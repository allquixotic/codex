//! Raw `config.toml` page: edits the user config file directly.
//!
//! With the embedded server (running or not) the file is on this machine:
//! reads and writes happen on the blocking pool, saving writes atomically
//! (a private temp file renamed over the target, following a symlinked
//! `config.toml` to its target), and everything except the final reload
//! works while the server is down, which is how users fix a config that
//! keeps Codex from starting. With a daemon or remote app-server the file
//! lives on the server's machine, so it is read and written through the
//! server's `fs/readFile` and `fs/writeFile`.
//!
//! Either way, saving validates TOML syntax, refuses to clobber a file that
//! changed since it was loaded (unless the user confirms), and then asks the
//! running server to reload.

use std::hash::Hash;
use std::hash::Hasher;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use base64::Engine;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigBatchWriteParams;
use codex_app_server_protocol::ConfigWriteResponse;
use codex_app_server_protocol::FsCreateDirectoryParams;
use codex_app_server_protocol::FsCreateDirectoryResponse;
use codex_app_server_protocol::FsReadDirectoryParams;
use codex_app_server_protocol::FsReadDirectoryResponse;
use codex_app_server_protocol::FsReadFileParams;
use codex_app_server_protocol::FsReadFileResponse;
use codex_app_server_protocol::FsWriteFileParams;
use codex_app_server_protocol::FsWriteFileResponse;
use codex_utils_absolute_path::AbsolutePathBuf;
use slint::ComponentHandle;
use slint::SharedString;

use super::toml_value;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::backend::Backend;
use crate::backend::BackendError;
use crate::ui::SettingsState;

/// Shown when the app-server that owns `config.toml` is not connected.
const SERVER_NOT_CONNECTED: &str = "config.toml lives on the app-server's machine. It can be edited here once Codex is connected to it.";

/// Identity of the file contents a draft was based on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Fingerprint {
    exists: bool,
    hash: u64,
}

impl Fingerprint {
    fn of(contents: Option<&[u8]>) -> Self {
        let mut hasher = std::hash::DefaultHasher::new();
        contents.hash(&mut hasher);
        Self {
            exists: contents.is_some(),
            hash: hasher.finish(),
        }
    }
}

/// Where the page reads and writes `config.toml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RawLocation {
    /// On this machine.
    Local(PathBuf),
    /// On the app-server's machine, through its `fs/*` requests.
    Server(AbsolutePathBuf),
}

impl RawLocation {
    fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Server(path) => format!("{} (on the app-server)", path.display()),
        }
    }
}

/// Chooses where `config.toml` is edited. The embedded server shares this
/// machine's files; a daemon or remote server reports its own user config
/// file (`user_file`, from `config/read`) and is reached through `fs/*`
/// requests, so it must be connected.
pub(crate) fn raw_location(
    embedded: bool,
    connected: bool,
    user_file: Option<&AbsolutePathBuf>,
    codex_home: Option<&Path>,
) -> Result<RawLocation, String> {
    if embedded {
        return user_file
            .map(AbsolutePathBuf::to_path_buf)
            .or_else(|| codex_home.map(|home| home.join("config.toml")))
            .map(RawLocation::Local)
            .ok_or_else(|| "The Codex home folder could not be determined.".to_string());
    }
    if !connected {
        return Err(SERVER_NOT_CONNECTED.to_string());
    }
    user_file
        .cloned()
        .map(RawLocation::Server)
        .ok_or_else(|| "The app-server did not report where its config.toml is.".to_string())
}

/// Text and fingerprint of `contents`; a missing file reads as empty.
fn decode_config(contents: Option<Vec<u8>>) -> std::io::Result<(String, Fingerprint)> {
    let Some(bytes) = contents else {
        return Ok((String::new(), Fingerprint::of(None)));
    };
    let fingerprint = Fingerprint::of(Some(&bytes));
    let text = String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the file is not valid UTF-8",
        )
    })?;
    Ok((text, fingerprint))
}

/// Reads `path`; a missing file reads as empty.
pub(crate) fn read_config(path: &Path) -> std::io::Result<(String, Fingerprint)> {
    decode_config(read_local_bytes(path)?)
}

fn read_local_bytes(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Outcome of [`save_config`].
#[derive(Debug)]
pub(crate) enum SaveOutcome {
    Saved(Fingerprint),
    /// The file no longer matches `expected`; nothing was written.
    ChangedOnDisk,
}

/// Writes `text` to `path` atomically when the file still matches
/// `expected` (any content when `expected` is `None`).
pub(crate) fn save_config(
    path: &Path,
    text: &str,
    expected: Option<Fingerprint>,
) -> std::io::Result<SaveOutcome> {
    if let Some(expected) = expected {
        let current = read_local_bytes(path)?;
        if Fingerprint::of(current.as_deref()) != expected {
            return Ok(SaveOutcome::ChangedOnDisk);
        }
    }
    // Write through a symlinked config.toml instead of replacing the link.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let existing = std::fs::metadata(&target).ok();
    let (temp, mut file) = create_private_temp(&dir, &file_name, existing.as_ref())?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        if let Some(metadata) = existing.as_ref() {
            // The umask may have narrowed the mode the temp file was
            // created with; match the file being replaced exactly.
            std::fs::set_permissions(&temp, metadata.permissions())?;
        }
        std::fs::rename(&temp, &target)
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    Ok(SaveOutcome::Saved(Fingerprint::of(Some(text.as_bytes()))))
}

/// Creates a new, uniquely named temp file next to the config. It never
/// reuses an existing file (stale or planted), and on Unix it is created
/// with the replaced file's mode (`0600` for a new config) before any
/// contents are written, since `config.toml` may hold secrets.
fn create_private_temp(
    dir: &Path,
    file_name: &str,
    existing: Option<&std::fs::Metadata>,
) -> std::io::Result<(PathBuf, std::fs::File)> {
    #[cfg(not(unix))]
    let _ = existing;
    const ATTEMPTS: usize = 8;
    for _ in 0..ATTEMPTS {
        let temp = dir.join(format!(
            ".{file_name}.codex-gui-{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::fs::PermissionsExt;
            let mode = existing.map_or(0o600, |metadata| metadata.permissions().mode() & 0o777);
            options.mode(mode);
        }
        match options.open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a temporary file next to config.toml",
    ))
}

/// Reads `path` on the app-server's machine; `None` when it does not exist.
async fn read_server_bytes(
    backend: &Backend,
    path: &AbsolutePathBuf,
) -> Result<Option<Vec<u8>>, String> {
    let read = backend
        .request::<FsReadFileResponse>(ClientRequest::FsReadFile {
            request_id: backend.next_request_id(),
            params: FsReadFileParams { path: path.clone() },
        })
        .await;
    match read {
        Ok(response) => base64::engine::general_purpose::STANDARD
            .decode(response.data_base64)
            .map(Some)
            .map_err(|err| format!("the app-server returned invalid file data: {err}")),
        // `fs/readFile` reports a missing file only as a message; a listing
        // of the folder without the file means it does not exist yet.
        Err(err) => match server_file_exists(backend, path).await {
            Some(false) => Ok(None),
            Some(true) | None => Err(err.user_message()),
        },
    }
}

/// Whether `path` is listed in its folder on the app-server; `None` when the
/// folder cannot be listed.
async fn server_file_exists(backend: &Backend, path: &AbsolutePathBuf) -> Option<bool> {
    let parent = path.parent()?;
    let name = path.as_path().file_name()?.to_string_lossy().into_owned();
    let listing = backend
        .request::<FsReadDirectoryResponse>(ClientRequest::FsReadDirectory {
            request_id: backend.next_request_id(),
            params: FsReadDirectoryParams { path: parent },
        })
        .await
        .ok()?;
    Some(listing.entries.iter().any(|entry| entry.file_name == name))
}

/// [`save_config`] for a file on the app-server's machine.
async fn save_server_config(
    backend: &Backend,
    path: &AbsolutePathBuf,
    text: &str,
    expected: Option<Fingerprint>,
) -> Result<SaveOutcome, String> {
    if let Some(expected) = expected {
        let current = read_server_bytes(backend, path).await?;
        if Fingerprint::of(current.as_deref()) != expected {
            return Ok(SaveOutcome::ChangedOnDisk);
        }
    }
    if let Some(parent) = path.parent()
        && let Err(err) = backend
            .request::<FsCreateDirectoryResponse>(ClientRequest::FsCreateDirectory {
                request_id: backend.next_request_id(),
                params: FsCreateDirectoryParams {
                    path: parent,
                    recursive: Some(true),
                },
            })
            .await
    {
        // The folder usually exists; the write reports a real problem.
        tracing::debug!(%err, "fs/createDirectory before saving config.toml failed");
    }
    backend
        .request::<FsWriteFileResponse>(ClientRequest::FsWriteFile {
            request_id: backend.next_request_id(),
            params: FsWriteFileParams {
                path: path.clone(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(text),
            },
        })
        .await
        .map_err(|err| err.user_message())?;
    Ok(SaveOutcome::Saved(Fingerprint::of(Some(text.as_bytes()))))
}

#[derive(Default)]
pub(crate) struct RawState {
    location: Option<RawLocation>,
    fingerprint: Option<Fingerprint>,
    busy: bool,
    /// The status line shows that the page waits for the server's config.
    waiting: bool,
}

impl AppController {
    pub(super) fn settings_raw_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.on_raw_save(|text| {
            let text = text.to_string();
            crate::ui_thread::with_app(move |app| {
                app.settings_raw_save(text, /*force*/ false)
            });
        });
        state.on_raw_reload(|| {
            crate::ui_thread::with_app(|app| {
                if app.settings_raw_dirty() {
                    app.show_dialog(
                        DialogRequest::confirm(
                            "Discard unsaved changes?",
                            "Reloading replaces your edits with the file on disk.",
                        )
                        .accept_label("Discard and reload")
                        .destructive(),
                        Box::new(|app, accepted| {
                            if accepted.is_some() {
                                app.settings_raw_load(/*keep_dirty_draft*/ false);
                            }
                        }),
                    );
                } else {
                    app.settings_raw_load(/*keep_dirty_draft*/ false);
                }
            });
        });
    }

    fn settings_raw_location(&self) -> Result<RawLocation, String> {
        let snapshot = self.settings.snapshot.as_ref();
        raw_location(
            self.backend.is_embedded(),
            self.backend.is_ready() && self.settings.server_error.is_none(),
            snapshot.and_then(|snapshot| snapshot.user_file_abs.as_ref()),
            self.codex_home.as_deref(),
        )
    }

    /// A daemon or remote server whose config location is not known yet:
    /// the page waits for `config/read`.
    fn settings_raw_waiting_for_server(&self) -> bool {
        !self.backend.is_embedded()
            && self.backend.is_ready()
            && self.settings.server_error.is_none()
            && self.settings.snapshot.is_none()
    }

    fn settings_raw_dirty(&self) -> bool {
        let state = self.window.global::<SettingsState>();
        state.get_raw_draft() != state.get_raw_text()
    }

    /// Refreshes from disk each time the page is shown, keeping a dirty draft.
    pub(super) fn settings_raw_activate(&mut self) {
        if !self.settings.raw.busy {
            self.settings_raw_load(/*keep_dirty_draft*/ true);
        }
    }

    pub(super) fn settings_raw_on_snapshot(&mut self) {
        let Ok(location) = self.settings_raw_location() else {
            return;
        };
        if self.settings.raw.location.as_ref() == Some(&location) {
            return;
        }
        let state = self.window.global::<SettingsState>();
        state.set_raw_path(location.display().into());
        // The page was waiting for the server's config location, or the
        // location moved (another server): load the file now.
        if self.settings.page == "raw"
            && self.settings_tab_active()
            && !self.settings.raw.busy
            && (!state.get_raw_loaded() || !self.settings_raw_dirty())
        {
            self.settings_raw_load(/*keep_dirty_draft*/ true);
        }
    }

    /// Another page changed config.toml: refresh a visible, clean editor.
    /// Otherwise the next activation reloads it.
    pub(super) fn settings_raw_on_config_written(&mut self) {
        if self.settings.page == "raw"
            && self.settings_tab_active()
            && !self.settings.raw.busy
            && !self.settings_raw_dirty()
        {
            self.settings_raw_load(/*keep_dirty_draft*/ true);
        }
    }

    fn settings_raw_load(&mut self, keep_dirty_draft: bool) {
        let state = self.window.global::<SettingsState>();
        if self.settings_raw_waiting_for_server() {
            self.settings.raw.waiting = true;
            state.set_raw_status("Loading the app-server's configuration…".into());
            self.settings_reload_config();
            return;
        }
        let location = match self.settings_raw_location() {
            Ok(location) => location,
            Err(message) => {
                self.settings.raw.location = None;
                state.set_raw_loaded(false);
                state.set_raw_path(SharedString::new());
                self.settings_raw_set_error(&message);
                return;
            }
        };
        self.settings.raw.location = Some(location.clone());
        self.settings.raw.busy = true;
        state.set_raw_path(location.display().into());
        state.set_raw_busy(true);
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let result = match &location {
                RawLocation::Local(path) => {
                    let read_path = path.clone();
                    tokio::task::spawn_blocking(move || read_config(&read_path))
                        .await
                        .unwrap_or_else(|err| Err(std::io::Error::other(err.to_string())))
                        .map_err(|err| err.to_string())
                }
                RawLocation::Server(path) => read_server_bytes(&backend, path)
                    .await
                    .and_then(|bytes| decode_config(bytes).map_err(|err| err.to_string())),
            };
            crate::ui_thread::post(move |app| {
                app.settings_raw_loaded(&location, result, keep_dirty_draft)
            });
        });
    }

    fn settings_raw_loaded(
        &mut self,
        location: &RawLocation,
        result: Result<(String, Fingerprint), String>,
        keep_dirty_draft: bool,
    ) {
        self.settings.raw.busy = false;
        let dirty = self.settings_raw_dirty();
        let state = self.window.global::<SettingsState>();
        state.set_raw_busy(false);
        match result {
            Ok((text, fingerprint)) => {
                let changed = self.settings.raw.fingerprint != Some(fingerprint);
                if keep_dirty_draft && dirty && state.get_raw_loaded() {
                    if changed {
                        state.set_raw_status(
                            "config.toml changed on disk after you started editing. Saving will ask before overwriting it."
                                .into(),
                        );
                    }
                    return;
                }
                self.settings.raw.fingerprint = Some(fingerprint);
                state.set_raw_text(SharedString::from(text.as_str()));
                state.set_raw_draft(SharedString::from(text));
                state.set_raw_loaded(true);
                state.set_raw_error(SharedString::new());
                let waited = std::mem::take(&mut self.settings.raw.waiting);
                if !keep_dirty_draft || waited {
                    state.set_raw_status(SharedString::new());
                }
            }
            Err(err) => {
                self.settings_raw_set_error(&format!(
                    "Could not read {}: {err}",
                    location.display()
                ));
            }
        }
    }

    fn settings_raw_set_error(&mut self, message: &str) {
        self.settings.raw.waiting = false;
        let state = self.window.global::<SettingsState>();
        state.set_raw_status(SharedString::new());
        state.set_raw_error(message.into());
    }

    fn settings_raw_save(&mut self, text: String, force: bool) {
        if self.settings.raw.busy {
            return;
        }
        // Save where the draft was loaded from.
        let location = match self.settings.raw.location.clone() {
            Some(location) => Ok(location),
            None => self.settings_raw_location(),
        };
        let location = match location {
            Ok(location) => location,
            Err(message) => {
                self.settings_raw_set_error(&message);
                return;
            }
        };
        if matches!(location, RawLocation::Server(_)) && !self.backend.is_ready() {
            self.settings_raw_set_error(SERVER_NOT_CONNECTED);
            return;
        }
        let state = self.window.global::<SettingsState>();
        if let Err(problem) = toml_value::parse_document(&text) {
            state.set_raw_error(
                format!("Not saved: config.toml has a syntax error. {problem}").into(),
            );
            return;
        }
        state.set_raw_error(SharedString::new());
        state.set_raw_status(SharedString::new());
        state.set_raw_busy(true);
        self.settings.raw.busy = true;
        let expected = if force {
            None
        } else {
            self.settings.raw.fingerprint
        };
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let result = match &location {
                RawLocation::Local(path) => {
                    let (write_path, write_text) = (path.clone(), text.clone());
                    tokio::task::spawn_blocking(move || {
                        save_config(&write_path, &write_text, expected)
                    })
                    .await
                    .unwrap_or_else(|err| Err(std::io::Error::other(err.to_string())))
                    .map_err(|err| err.to_string())
                }
                RawLocation::Server(path) => {
                    save_server_config(&backend, path, &text, expected).await
                }
            };
            crate::ui_thread::post(move |app| app.settings_raw_saved(&location, text, result));
        });
    }

    fn settings_raw_saved(
        &mut self,
        location: &RawLocation,
        text: String,
        result: Result<SaveOutcome, String>,
    ) {
        self.settings.raw.busy = false;
        let state = self.window.global::<SettingsState>();
        state.set_raw_busy(false);
        match result {
            Ok(SaveOutcome::ChangedOnDisk) => {
                self.show_dialog(
                    DialogRequest::confirm(
                        "config.toml changed on disk",
                        "The file was modified outside this editor after you opened it. Overwrite it with your version? Choose Cancel, then Reload from disk, to keep the other changes instead.",
                    )
                    .accept_label("Overwrite")
                    .destructive(),
                    Box::new(move |app, accepted| {
                        if accepted.is_some() {
                            app.settings_raw_save(text, /*force*/ true);
                        }
                    }),
                );
            }
            Ok(SaveOutcome::Saved(fingerprint)) => {
                self.settings.raw.fingerprint = Some(fingerprint);
                state.set_raw_text(SharedString::from(text));
                tracing::info!(path = %location.display(), "saved config.toml");
                self.settings_raw_apply();
            }
            Err(err) => {
                self.settings_raw_set_error(&format!(
                    "Could not save {}: {err}",
                    location.display()
                ));
            }
        }
    }

    /// Asks the running server to reload the saved file. The re-read config
    /// then shows whether the provider changed and needs a restart.
    fn settings_raw_apply(&mut self) {
        let state = self.window.global::<SettingsState>();
        if self.settings.server_error.is_some() || !self.backend.is_ready() {
            state.set_raw_status("Saved. Restart Codex to load the new configuration.".into());
            return;
        }
        state.set_raw_status("Saved. Applying…".into());
        self.backend.call(
            |request_id| ClientRequest::ConfigBatchWrite {
                request_id,
                params: ConfigBatchWriteParams {
                    edits: Vec::new(),
                    file_path: None,
                    expected_version: None,
                    reload_user_config: true,
                },
            },
            move |app, result: Result<ConfigWriteResponse, BackendError>| {
                let state = app.window.global::<SettingsState>();
                match result {
                    Ok(response) => {
                        if let Some(snapshot) = app.settings.snapshot.as_mut() {
                            snapshot.user_version = Some(response.version);
                        }
                        state.set_raw_status("Saved. Open threads picked up the changes.".into());
                    }
                    Err(err) => {
                        state.set_raw_status(SharedString::new());
                        state.set_raw_error(
                            format!(
                                "Saved, but Codex could not apply the new configuration: {}",
                                err.user_message()
                            )
                            .into(),
                        );
                    }
                }
                app.settings_reload_config();
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn abs(path: &str) -> AbsolutePathBuf {
        let path = if cfg!(windows) {
            format!("C:{}", path.replace('/', "\\"))
        } else {
            path.to_string()
        };
        AbsolutePathBuf::from_absolute_path(path).unwrap_or_else(|err| panic!("{err}"))
    }

    #[test]
    fn missing_file_reads_as_empty() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let (text, fingerprint) = read_config(&dir.path().join("config.toml"))?;
        assert_eq!(text, "");
        assert!(!fingerprint.exists);
        Ok(())
    }

    #[test]
    fn save_detects_external_changes() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"a\"\n")?;
        let (_, loaded) = read_config(&path)?;

        std::fs::write(&path, "model = \"b\"\n")?;
        assert!(matches!(
            save_config(&path, "model = \"c\"\n", Some(loaded))?,
            SaveOutcome::ChangedOnDisk
        ));
        assert_eq!(std::fs::read_to_string(&path)?, "model = \"b\"\n");

        let (_, current) = read_config(&path)?;
        let SaveOutcome::Saved(saved) = save_config(&path, "model = \"c\"\n", Some(current))?
        else {
            panic!("expected a save");
        };
        assert_eq!(std::fs::read_to_string(&path)?, "model = \"c\"\n");
        assert_eq!(read_config(&path)?.1, saved);
        // No temp files are left behind.
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn save_creates_missing_folders_and_files() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("home").join("config.toml");
        let (_, missing) = read_config(&path)?;
        assert!(matches!(
            save_config(&path, "a = 1\n", Some(missing))?,
            SaveOutcome::Saved(_)
        ));
        assert_eq!(std::fs::read_to_string(&path)?, "a = 1\n");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_through_symlinks_and_keeps_permissions() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let real = dir.path().join("dotfiles-config.toml");
        std::fs::write(&real, "a = 1\n")?;
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600))?;
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link)?;
        save_config(&link, "a = 2\n", None)?;
        assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real)?, "a = 2\n");
        assert_eq!(
            std::fs::metadata(&real)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn temp_files_are_private_before_anything_is_written() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let (fresh, _file) = create_private_temp(dir.path(), "config.toml", None)?;
        assert_eq!(
            std::fs::metadata(&fresh)?.permissions().mode() & 0o777,
            0o600,
            "a new config is created owner-only"
        );
        let secret = dir.path().join("config.toml");
        std::fs::write(&secret, "token = \"x\"\n")?;
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))?;
        let existing = std::fs::metadata(&secret)?;
        let (copy, _file) = create_private_temp(dir.path(), "config.toml", Some(&existing))?;
        assert_eq!(
            std::fs::metadata(&copy)?.permissions().mode() & 0o777,
            0o600
        );
        assert_ne!(fresh, copy, "every save uses a new temp file");

        // A brand-new config.toml stays owner-only after the save.
        let new_config = dir.path().join("new").join("config.toml");
        save_config(&new_config, "a = 1\n", None)?;
        assert_eq!(
            std::fs::metadata(&new_config)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[test]
    fn remote_servers_edit_their_own_config_file() {
        let server_file = abs("/home/sean/.codex/config.toml");
        let home = PathBuf::from("/Users/me/.codex");
        assert_eq!(
            raw_location(
                /*embedded*/ true,
                /*connected*/ false,
                /*user_file*/ None,
                Some(&home)
            ),
            Ok(RawLocation::Local(home.join("config.toml")))
        );
        assert_eq!(
            raw_location(
                /*embedded*/ true,
                /*connected*/ true,
                Some(&server_file),
                Some(&home)
            ),
            Ok(RawLocation::Local(server_file.to_path_buf()))
        );
        // A daemon or remote server's file is reached through the server,
        // never through this machine's file system.
        assert_eq!(
            raw_location(
                /*embedded*/ false,
                /*connected*/ true,
                Some(&server_file),
                Some(&home)
            ),
            Ok(RawLocation::Server(server_file.clone()))
        );
        assert_eq!(
            raw_location(
                /*embedded*/ false,
                /*connected*/ false,
                Some(&server_file),
                Some(&home)
            ),
            Err(SERVER_NOT_CONNECTED.to_string())
        );
        assert!(
            raw_location(
                /*embedded*/ false,
                /*connected*/ true,
                /*user_file*/ None,
                Some(&home)
            )
            .is_err()
        );
        assert!(
            RawLocation::Server(server_file)
                .display()
                .ends_with("(on the app-server)")
        );
    }

    #[test]
    fn decoded_configs_require_utf8() {
        assert_eq!(
            decode_config(None)
                .map(|(text, fingerprint)| (text, fingerprint.exists))
                .ok(),
            Some((String::new(), false))
        );
        let (text, fingerprint) =
            decode_config(Some(b"a = 1\n".to_vec())).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(text, "a = 1\n");
        assert_eq!(fingerprint, Fingerprint::of(Some(b"a = 1\n")));
        assert!(decode_config(Some(vec![0xff, 0xfe])).is_err());
    }
}
