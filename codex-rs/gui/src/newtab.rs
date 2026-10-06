//! New-tab page: recent folders, folder and file pickers, "resume a thread",
//! and a sign-in prompt when the provider needs an OpenAI login.
//!
//! Starting a thread from here (or from the sidebar) first runs the same
//! folder-trust check as the TUI: `config/read` with layers for the folder,
//! plus project-root discovery on disk. Untrusted project folders get a
//! "Trust this folder?" dialog; trusting writes
//! `projects."<key>".trust_level = "trusted"` through `config/batchWrite`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigBatchWriteParams;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigWriteResponse;
use codex_app_server_protocol::EnvironmentInfoParams;
use codex_app_server_protocol::EnvironmentInfoResponse;
use codex_app_server_protocol::FsReadDirectoryEntry;
use codex_app_server_protocol::FsReadDirectoryParams;
use codex_app_server_protocol::FsReadDirectoryResponse;
use codex_app_server_protocol::GetAccountParams;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::MergeStrategy;
use codex_app_server_protocol::ServerNotification;
use codex_config::default_project_root_markers;
use codex_config::loader::discover_project_root;
use codex_config::loader::normalized_project_trust_keys;
use codex_config::loader::project_trust_key;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::LOCAL_FS;
use codex_git_utils::resolve_root_git_project_for_trust;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use serde_json::Value as JsonValue;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::VecModel;

use crate::app::AppController;
use crate::app::TabKind;
use crate::backend::Backend;
use crate::backend::BackendError;
use crate::connection::ConnectionTarget;
use crate::sidebar::ThreadSummary;
use crate::threads::PickerButton;
use crate::threads::PickerController;
use crate::threads::PickerEvent;
use crate::threads::PickerFlow;
use crate::threads::PickerInputView;
use crate::threads::PickerListView;
use crate::threads::PickerOutcome;
use crate::threads::PickerRowView;
use crate::threads::PickerView;
use crate::ui::NewTabState;
use crate::ui::RecentFolder;

/// Folders listed on the page.
const MAX_FOLDERS: usize = 10;

/// Whether the configured provider needs a sign-in, from `account/read`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AccountState {
    #[default]
    Unknown,
    Loading,
    Known {
        sign_in_needed: bool,
    },
}

#[derive(Default)]
pub(crate) struct NewTabController {
    folders: Rc<VecModel<RecentFolder>>,
    /// Whether each candidate folder exists, filled in off the UI thread.
    exists: HashMap<PathBuf, bool>,
    /// Folders with an existence check in flight.
    checking: HashSet<PathBuf>,
    account: AccountState,
    /// Folders with a trust check in flight (ignores double clicks).
    trust_checks: HashSet<PathBuf>,
    /// The picker overlay (trust prompts, server folders, review targets).
    pub(crate) picker: PickerController,
}

/// A folder offered on the new-tab page.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FolderChoice {
    pub(crate) path: PathBuf,
    /// Listed threads whose cwd is this folder.
    pub(crate) thread_count: usize,
    /// In the GUI's recent-folder list.
    pub(crate) recent: bool,
}

/// Merges the GUI's recent folders (most recent first) with the folders of
/// listed threads (folder of the newest thread first), without duplicates.
pub(crate) fn merge_recent_folders(
    recent: &[PathBuf],
    threads: &[ThreadSummary],
    max: usize,
) -> Vec<FolderChoice> {
    let groups = crate::sidebar::group_by_folder(threads);
    let counts: HashMap<&Path, usize> = groups
        .iter()
        .map(|group| (group.cwd, group.threads.len()))
        .collect();
    let mut seen: HashSet<&Path> = HashSet::new();
    let recent_choices = recent.iter().map(|path| (path.as_path(), true));
    let thread_choices = groups.iter().map(|group| (group.cwd, false));
    recent_choices
        .chain(thread_choices)
        .filter(|(path, _)| seen.insert(path))
        .take(max)
        .map(|(path, recent)| FolderChoice {
            path: path.to_path_buf(),
            thread_count: counts.get(path).copied().unwrap_or(0),
            recent,
        })
        .collect()
}

/// `path` with the home directory shortened to `~`.
pub(crate) fn display_path(path: &Path) -> String {
    display_path_with_home(path, dirs::home_dir().as_deref())
}

fn display_path_with_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && !home.as_os_str().is_empty()
        && let Ok(rest) = path.strip_prefix(home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display());
    }
    path.display().to_string()
}

/// Config key path that marks a project trusted, for a key from
/// [`project_trust_key`].
pub(crate) fn trust_key_path(project_key: &str) -> String {
    let escaped = project_key.replace('\\', "\\\\").replace('"', "\\\"");
    format!("projects.\"{escaped}\".trust_level")
}

/// Project roots found on disk; only needed when the folder has no project
/// config layer yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LocalRoots {
    /// Trust keys of the nearest project root, or of the folder itself.
    pub(crate) project_root_keys: Vec<String>,
    /// Trust keys of the main git repository root (worktrees resolved).
    pub(crate) git_root_keys: Option<Vec<String>>,
    /// No project marker of any kind above the folder.
    pub(crate) projectless: bool,
}

/// Outcome of the folder-trust check.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TrustCheck {
    /// Trusted, outside any project, or project config already loads.
    Proceed,
    /// Ask first. `target` is the folder whose trust is saved (usually the
    /// repository root); `untrusted` when it is explicitly marked untrusted.
    Ask { target: PathBuf, untrusted: bool },
}

/// Where the folder of a trust check lives.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TrustHost<'a> {
    /// On this machine (embedded server or local daemon). Project roots are
    /// discovered on disk when the folder has no project config layer yet.
    Local(Option<&'a LocalRoots>),
    /// On a remote server: only the server's view is available.
    Remote,
}

/// Decides whether to ask before opening `cwd`, from a `config/read`
/// response with layers. Mirrors the TUI's folder-trust check
/// (`read_remote_project_trust`).
pub(crate) fn decide_trust(
    config_read: &JsonValue,
    cwd: &str,
    cwd_keys: &[String],
    host: TrustHost<'_>,
) -> TrustCheck {
    let local_roots = match host {
        TrustHost::Local(roots) => roots,
        TrustHost::Remote => None,
    };
    let project_layers = project_layers(config_read);
    let disabled_project = project_layers
        .iter()
        .rev()
        .find(|layer| {
            layer
                .get("disabledReason")
                .and_then(JsonValue::as_str)
                .is_some()
        })
        .copied();
    let disabled_reason = disabled_project
        .and_then(|layer| layer.get("disabledReason"))
        .and_then(JsonValue::as_str);
    let projects = config_read["config"]["projects"].as_object();

    let cwd_trust_target = saved_trust_target(projects, cwd_keys);
    let trust_target: String = cwd_trust_target
        .map(str::to_string)
        .or_else(|| {
            disabled_reason
                .and_then(|reason| reason.split_once(", add "))
                .and_then(|(_, reason)| reason.rsplit_once(" as a trusted project in "))
                .map(|(target, _)| target.to_string())
        })
        .or_else(|| {
            disabled_project
                .and_then(|layer| layer["name"]["dotCodexFolder"].as_str())
                .and_then(|path| {
                    path.strip_suffix("/.codex")
                        .or_else(|| path.strip_suffix("\\.codex"))
                })
                .map(str::to_string)
        })
        .or_else(|| {
            let roots = local_roots?;
            saved_trust_target(projects, &roots.project_root_keys)
                .or_else(|| {
                    roots
                        .git_root_keys
                        .as_deref()
                        .and_then(|keys| saved_trust_target(projects, keys))
                })
                .or_else(|| {
                    roots
                        .git_root_keys
                        .as_ref()
                        .and_then(|keys| keys.first())
                        .map(String::as_str)
                })
                .or_else(|| roots.project_root_keys.first().map(String::as_str))
                .map(str::to_string)
        })
        .unwrap_or_else(|| cwd.to_string());

    let trust_level = project_entry(projects, &trust_target)
        .and_then(|(_, project)| project.get("trust_level"))
        .and_then(JsonValue::as_str)
        .filter(|level| matches!(*level, "trusted" | "untrusted"));
    let explicitly_untrusted = cwd_trust_target.is_none()
        && disabled_reason.is_some_and(|reason| {
            projects.into_iter().flatten().any(|(path, project)| {
                project.get("trust_level").and_then(JsonValue::as_str) == Some("untrusted")
                    && reason
                        .strip_prefix(path.as_str())
                        .is_some_and(|suffix| suffix.starts_with(" is marked as untrusted"))
            })
        });
    let projectless = local_roots.is_some_and(|roots| roots.projectless);
    let project_config_loads = disabled_project.is_none()
        && project_layers
            .iter()
            .any(|layer| layer.get("disabledReason").is_none());
    if !explicitly_untrusted
        && (trust_level == Some("trusted")
            || (trust_level.is_none() && (projectless || project_config_loads)))
    {
        return TrustCheck::Proceed;
    }
    // Without project roots from disk, a folder inside an explicitly
    // untrusted project would otherwise be offered for trust on its own.
    // Like the TUI, never let a subfolder override its project's choice.
    if matches!(host, TrustHost::Remote)
        && trust_level.is_none()
        && !explicitly_untrusted
        && project_layers.is_empty()
        && let Some(project) = untrusted_ancestor(projects, cwd)
    {
        return TrustCheck::Ask {
            target: PathBuf::from(project),
            untrusted: true,
        };
    }
    TrustCheck::Ask {
        target: PathBuf::from(trust_target),
        untrusted: explicitly_untrusted || trust_level == Some("untrusted"),
    }
}

/// The deepest project marked untrusted that contains `cwd`, compared by
/// path segments in the path's own syntax (POSIX or Windows).
fn untrusted_ancestor<'a>(
    projects: Option<&'a serde_json::Map<String, JsonValue>>,
    cwd: &str,
) -> Option<&'a str> {
    let cwd = server_path_uri(cwd)?;
    projects
        .into_iter()
        .flatten()
        .filter(|(_, project)| {
            project.get("trust_level").and_then(JsonValue::as_str) == Some("untrusted")
        })
        .filter_map(|(path, _)| {
            let project = server_path_uri(path)?;
            cwd.starts_with(&project)
                .then(|| (project.lexical_depth().unwrap_or(0), path.as_str()))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, path)| path)
}

/// `path` as a URI, inferring its syntax from its spelling (a server path
/// may use another syntax than this machine). `None` unless absolute.
fn server_path_uri(path: &str) -> Option<PathUri> {
    let path = path.trim();
    // "/srv/app/" is "/srv/app"; roots ("/", "C:\") keep their separator.
    let trimmed = path.trim_end_matches(['/', '\\']);
    let path = if trimmed.is_empty() || trimmed.ends_with(':') {
        path
    } else {
        trimmed
    };
    LegacyAppPathString::from_string(path).to_inferred_path_uri()
}

/// `path` normalized in its own syntax, or `None` unless it is absolute.
pub(crate) fn normalize_server_path(path: &str) -> Option<String> {
    server_path_uri(path).map(|uri| uri.inferred_native_path_string())
}

/// The parent folder of server path `path`, if it is not a root.
pub(crate) fn server_parent(path: &str) -> Option<String> {
    server_path_uri(path)?
        .parent()
        .map(|parent| parent.inferred_native_path_string())
}

/// Server path of the entry `name` inside folder `path`.
pub(crate) fn server_child(path: &str, name: &str) -> Option<String> {
    if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
        return None;
    }
    server_path_uri(path)?
        .join(name)
        .ok()
        .map(|child| child.inferred_native_path_string())
}

fn project_layers(config_read: &JsonValue) -> Vec<&JsonValue> {
    config_read
        .get("layers")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter(|layer| layer["name"]["type"] == "project")
        .collect()
}

/// Saved `projects` entry with a trust level for `key`. Keys compare
/// case-insensitively on Windows.
fn project_entry<'a>(
    projects: Option<&'a serde_json::Map<String, JsonValue>>,
    key: &str,
) -> Option<(&'a String, &'a JsonValue)> {
    let projects = projects?;
    let has_level = |project: &JsonValue| project["trust_level"].as_str().is_some();
    projects
        .get_key_value(key)
        .filter(|(_, project)| has_level(project))
        .or_else(|| {
            if !cfg!(windows) {
                return None;
            }
            projects
                .iter()
                .filter(|(path, project)| path.eq_ignore_ascii_case(key) && has_level(project))
                .min_by_key(|(path, _)| *path)
        })
}

fn saved_trust_target<'a>(
    projects: Option<&'a serde_json::Map<String, JsonValue>>,
    keys: &[String],
) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| project_entry(projects, key).map(|(path, _)| path.as_str()))
}

/// Runs the folder-trust check for `folder` (on the Tokio runtime).
///
/// `local` is false when the app-server's files are on another machine:
/// project roots cannot be discovered from here, so only the server's view
/// is used.
async fn check_folder_trust(
    backend: &Backend,
    folder: &Path,
    local: bool,
) -> anyhow::Result<TrustCheck> {
    let cwd = folder
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("the folder path is not valid UTF-8"))?
        .to_string();
    let response: JsonValue = backend
        .request(ClientRequest::ConfigRead {
            request_id: backend.next_request_id(),
            params: ConfigReadParams {
                include_layers: true,
                cwd: Some(cwd.clone()),
            },
        })
        .await?;
    if !local {
        return Ok(decide_trust(
            &response,
            &cwd,
            std::slice::from_ref(&cwd),
            TrustHost::Remote,
        ));
    }
    let local_roots = if project_layers(&response).is_empty() {
        Some(discover_local_roots(&response, folder).await?)
    } else {
        None
    };
    let cwd_keys = normalized_project_trust_keys(folder);
    Ok(decide_trust(
        &response,
        &cwd,
        &cwd_keys,
        TrustHost::Local(local_roots.as_ref()),
    ))
}

async fn discover_local_roots(
    config_read: &JsonValue,
    folder: &Path,
) -> anyhow::Result<LocalRoots> {
    let cwd = AbsolutePathBuf::from_absolute_path(folder)?;
    let markers = serde_json::from_value::<Option<Vec<String>>>(
        config_read["config"]["project_root_markers"].clone(),
    )?
    .unwrap_or_else(default_project_root_markers);
    let project_root = discover_project_root(LOCAL_FS.as_ref(), &cwd, &markers).await?;
    let git_root = resolve_root_git_project_for_trust(LOCAL_FS.as_ref(), &cwd).await;
    let projectless = project_root.is_none()
        && discover_project_root(LOCAL_FS.as_ref(), &cwd, &default_project_root_markers())
            .await?
            .is_none();
    Ok(LocalRoots {
        project_root_keys: normalized_project_trust_keys(
            project_root.as_ref().unwrap_or(&cwd).as_path(),
        ),
        git_root_keys: git_root.map(|root| normalized_project_trust_keys(root.as_path())),
        projectless,
    })
}

/// "Trust this folder?" before a thread starts in an untrusted project.
pub(crate) struct TrustPrompt {
    folder: PathBuf,
    /// The folder whose trust is saved (usually the repository root).
    target: PathBuf,
    /// Explicitly marked untrusted: only a restricted start is offered.
    untrusted: bool,
}

/// What the user chose in the trust prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrustChoice {
    Trust,
    OpenRestricted,
    Cancel,
}

/// Maps a button of the trust prompt to a choice. Anything but an explicit
/// choice (Cancel, Escape) opens nothing.
fn trust_choice(untrusted: bool, button: PickerButton) -> TrustChoice {
    match (button, untrusted) {
        (PickerButton::Accept, false) => TrustChoice::Trust,
        (PickerButton::Accept, true) | (PickerButton::Secondary, false) => {
            TrustChoice::OpenRestricted
        }
        _ => TrustChoice::Cancel,
    }
}

impl TrustPrompt {
    pub(crate) fn view(&self) -> PickerView {
        if self.untrusted {
            return PickerView {
                title: "Open untrusted folder?".to_string(),
                message: format!(
                    "{} is marked as untrusted. Its project config, hooks, and exec policies stay disabled; skills still load and tools follow your permission settings. Opening does not change the saved trust.",
                    display_path(&self.target)
                ),
                cancel_label: "Cancel".to_string(),
                accept_label: "Open restricted".to_string(),
                accept_enabled: true,
                ..PickerView::default()
            };
        }
        let mut message = format!(
            "Codex can read, edit, and run files in {}, subject to your permission settings. Project settings can run code automatically, even without a model request. Continue only if you trust these files.",
            display_path(&self.folder)
        );
        if self.target != self.folder {
            message.push_str(&format!(
                "\n\nTrust applies to the project root: {}",
                display_path(&self.target)
            ));
        }
        PickerView {
            title: "Trust this folder?".to_string(),
            message,
            cancel_label: "Cancel".to_string(),
            secondary_label: "Open restricted".to_string(),
            accept_label: "Trust and continue".to_string(),
            accept_enabled: true,
            ..PickerView::default()
        }
    }
}

/// Lists folders on a remote server (`fs/readDirectory`) so a thread can
/// start in one; the local file dialog would offer this machine's folders.
pub(crate) struct FolderBrowser {
    /// Folder shown, in the server's path syntax.
    path: String,
    listing: FolderListing,
    /// Error about the typed path.
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum FolderListing {
    Loading,
    /// Subfolder names, sorted for display.
    Ready(Vec<String>),
    Failed(String),
    /// The path cannot be sent from this machine (a Windows path from a
    /// POSIX machine or the reverse); it can still be typed.
    Unsupported,
}

/// Subfolders of a directory listing, sorted case-insensitively with
/// hidden folders last.
pub(crate) fn sorted_subfolders(entries: &[FsReadDirectoryEntry]) -> Vec<String> {
    let mut folders: Vec<String> = entries
        .iter()
        .filter(|entry| entry.is_directory)
        .map(|entry| entry.file_name.clone())
        .collect();
    folders.sort_by_cached_key(|name| (name.starts_with('.'), name.to_lowercase()));
    folders
}

impl FolderBrowser {
    fn new(path: String) -> Self {
        Self {
            path,
            listing: FolderListing::Loading,
            error: None,
        }
    }

    /// Rows: the parent folder (when there is one), then the subfolders.
    fn rows(&self) -> Vec<(String, PickerRowView)> {
        let mut rows = Vec::new();
        if let Some(parent) = server_parent(&self.path) {
            rows.push((
                parent.clone(),
                PickerRowView::new("..", format!("Up to {parent}")),
            ));
        }
        if let FolderListing::Ready(folders) = &self.listing {
            rows.extend(folders.iter().filter_map(|name| {
                let child = server_child(&self.path, name)?;
                Some((child, PickerRowView::new(name.clone(), "").trailing("›")))
            }));
        }
        rows
    }

    pub(crate) fn view(&self, connection: &str) -> PickerView {
        let placeholder = match &self.listing {
            FolderListing::Loading => "Loading folders…".to_string(),
            FolderListing::Ready(folders) if folders.is_empty() => {
                "This folder has no subfolders.".to_string()
            }
            FolderListing::Ready(_) => String::new(),
            FolderListing::Failed(error) => format!("Could not list this folder: {error}"),
            FolderListing::Unsupported => {
                "This server's folders cannot be listed from here. Type the full path of a folder on the server.".to_string()
            }
        };
        let connection = if connection.is_empty() {
            "the server".to_string()
        } else {
            connection.to_string()
        };
        PickerView {
            title: "Choose a folder on the server".to_string(),
            message: format!(
                "The thread runs on {connection}, so its folder must exist there. Click a folder to open it, or type a path and press Enter."
            ),
            input: Some(PickerInputView {
                label: "Folder".to_string(),
                placeholder: "/home/me/project".to_string(),
                text: self.path.clone(),
            }),
            list: Some(PickerListView {
                rows: self.rows().into_iter().map(|(_, row)| row).collect(),
                placeholder,
                click_activates: true,
                ..PickerListView::default()
            }),
            note: self.error.clone().unwrap_or_default(),
            note_error: self.error.is_some(),
            cancel_label: "Cancel".to_string(),
            accept_label: "Start thread here".to_string(),
            accept_enabled: true,
            ..PickerView::default()
        }
    }
}

fn thread_count_label(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "1 thread".to_string(),
        count => format!("{count} threads"),
    }
}

impl AppController {
    pub(crate) fn newtab_bind(&mut self) {
        let state = self.window.global::<NewTabState>();
        state.set_folders(ModelRc::from(self.newtab.folders.clone()));
        state.on_folderless(|| crate::ui_thread::with_app(AppController::start_folderless_thread));
        state.on_choose_folder(|| {
            crate::ui_thread::with_app(AppController::newtab_pick_folder);
        });
        state.on_open_file(|| crate::ui_thread::with_app(AppController::files_pick_and_open));
        state.on_resume_thread(|| crate::ui_thread::with_app(AppController::sidebar_focus_search));
        state.on_sign_in(|| crate::ui_thread::with_app(|app| app.open_settings(Some("account"))));
        state.on_open_folder(|path| {
            let path = PathBuf::from(path.as_str());
            crate::ui_thread::with_app(move |app| app.start_thread_checked(path));
        });
        state.on_folder_action(|path, action| {
            let (path, action) = (PathBuf::from(path.as_str()), action.to_string());
            crate::ui_thread::with_app(move |app| app.newtab_folder_action(path, &action));
        });
        self.picker_bind();
    }

    pub(crate) fn newtab_show(&mut self) {
        let candidates: Vec<PathBuf> = self
            .newtab_choices()
            .into_iter()
            .map(|choice| choice.path)
            .collect();
        // Re-validate known folders too; the cached answer shows meanwhile.
        self.newtab_check_folders(candidates);
        self.newtab_render();
        if self.newtab.account == AccountState::Unknown {
            self.newtab_fetch_account();
        }
    }

    pub(crate) fn newtab_on_server_ready(&mut self) {
        self.newtab.account = AccountState::Unknown;
        if self.newtab_visible() {
            self.newtab_fetch_account();
        }
    }

    pub(crate) fn newtab_on_notification(&mut self, notification: &ServerNotification) {
        if matches!(
            notification,
            ServerNotification::AccountUpdated(_) | ServerNotification::AccountLoginCompleted(_)
        ) {
            self.newtab.account = AccountState::Unknown;
            if self.newtab_visible() {
                self.newtab_fetch_account();
            }
        }
    }

    /// The sidebar's thread list changed (its folders feed the page).
    pub(crate) fn newtab_on_threads_changed(&mut self) {
        if self.newtab_visible() {
            self.newtab_render();
        }
    }

    /// Folderless conversations use the user's home as their working directory.
    pub(crate) fn start_folderless_thread(&mut self) {
        if !self.server_files_are_local() {
            self.toast("Choose a folder on the remote server to start a conversation.");
            return;
        }
        match dirs::home_dir() {
            Some(home) => self.start_thread_checked(home),
            None => self.toast("Could not locate your home directory."),
        }
    }

    /// Starts a thread in `folder` after the folder-trust check, asking the
    /// user first when the folder is an untrusted project.
    pub(crate) fn start_thread_checked(&mut self, folder: PathBuf) {
        if !self.newtab.trust_checks.insert(folder.clone()) {
            return;
        }
        let local = self.server_files_are_local();
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let result = check_folder_trust(&backend, &folder, local).await;
            crate::ui_thread::post(move |app| app.newtab_on_trust_checked(folder, result));
        });
    }

    /// Opens `path` in the system file manager.
    pub(crate) fn open_in_file_manager(&mut self, path: &Path) {
        if !self.server_files_are_local() {
            self.toast(format!("{} is on the server's machine", path.display()));
            return;
        }
        self.open_local_folder(path);
    }

    fn newtab_on_trust_checked(&mut self, folder: PathBuf, result: anyhow::Result<TrustCheck>) {
        self.newtab.trust_checks.remove(&folder);
        let check = match result {
            Ok(check) => check,
            Err(err) => {
                // Without an answer the server treats the folder as
                // untrusted, which is the safe default.
                tracing::warn!(error = %format!("{err:#}"), folder = %folder.display(), "folder trust check failed");
                TrustCheck::Proceed
            }
        };
        match check {
            TrustCheck::Proceed => self.start_thread_in_folder(folder),
            // Prompts queue behind one another; nothing opens unless the
            // user picks an option.
            TrustCheck::Ask { target, untrusted } => {
                self.picker_open(PickerFlow::Trust(TrustPrompt {
                    folder,
                    target,
                    untrusted,
                }));
            }
        }
    }

    /// Handles the trust prompt's buttons.
    pub(crate) fn newtab_trust_event(
        &mut self,
        prompt: &TrustPrompt,
        event: &PickerEvent,
    ) -> PickerOutcome {
        let PickerEvent::Button { button, .. } = event else {
            return PickerOutcome::Unchanged;
        };
        match trust_choice(prompt.untrusted, *button) {
            TrustChoice::Trust => {
                self.newtab_trust_and_start(prompt.folder.clone(), prompt.target.clone());
            }
            TrustChoice::OpenRestricted => self.start_thread_in_folder(prompt.folder.clone()),
            TrustChoice::Cancel => {}
        }
        PickerOutcome::Close
    }

    fn newtab_trust_and_start(&mut self, folder: PathBuf, target: PathBuf) {
        let local = self.server_files_are_local();
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            // Canonicalizes on disk, so it stays off the UI thread. A remote
            // server reported the key itself.
            let key = if local {
                project_trust_key(&target)
            } else {
                target.to_string_lossy().into_owned()
            };
            let request = ClientRequest::ConfigBatchWrite {
                request_id: backend.next_request_id(),
                params: ConfigBatchWriteParams {
                    edits: vec![ConfigEdit {
                        key_path: trust_key_path(&key),
                        value: JsonValue::from("trusted"),
                        merge_strategy: MergeStrategy::Replace,
                    }],
                    file_path: None,
                    expected_version: None,
                    reload_user_config: true,
                },
            };
            let result = backend.request::<ConfigWriteResponse>(request).await;
            crate::ui_thread::post(move |app| {
                if let Err(err) = result {
                    app.toast(format!(
                        "Could not save folder trust: {}",
                        err.user_message()
                    ));
                }
                app.start_thread_in_folder(folder);
            });
        });
    }

    fn newtab_pick_folder(&mut self) {
        if !self.server_files_are_local() {
            self.newtab_browse_server_folders(/*initial*/ None);
            return;
        }
        let mut dialog =
            rfd::AsyncFileDialog::new().set_title("Choose a folder for the new thread");
        if let Some(recent) = self.prefs.recent_folders.first() {
            dialog = dialog.set_directory(recent);
        }
        let future = dialog.pick_folder();
        let spawned = slint::spawn_local(async move {
            if let Some(folder) = future.await {
                let path = folder.path().to_path_buf();
                crate::ui_thread::with_app(move |app| app.start_thread_checked(path));
            }
        });
        if let Err(err) = spawned {
            tracing::warn!(%err, "could not open folder picker");
        }
    }

    fn newtab_folder_action(&mut self, path: PathBuf, action: &str) {
        match action {
            "open-folder" => self.open_in_file_manager(&path),
            "copy-path" => self.copy_to_clipboard(&path.to_string_lossy()),
            "forget" => {
                self.prefs.recent_folders.retain(|folder| folder != &path);
                self.save_prefs();
                self.newtab_render();
            }
            other => tracing::debug!(action = other, "unknown new-tab folder action"),
        }
    }

    /// Whether the app-server's files are on this machine: the embedded
    /// server or a local daemon (`unix://`), but not a WebSocket server.
    /// Known from startup, so it does not depend on the connection state.
    pub(crate) fn server_files_are_local(&self) -> bool {
        files_are_local(&self.backend.connection())
    }

    /// Opens the server folder browser at `initial`, or at the server's
    /// working directory.
    pub(crate) fn newtab_browse_server_folders(&mut self, initial: Option<String>) {
        let initial = initial.and_then(|path| normalize_server_path(&path));
        let start = initial.clone().unwrap_or_else(|| "/".to_string());
        let id = self.picker_open(PickerFlow::Browse(FolderBrowser::new(start.clone())));
        if initial.is_some() {
            self.newtab_browse_list(id, start);
            return;
        }
        self.backend.call(
            |request_id| ClientRequest::EnvironmentInfo {
                request_id,
                params: EnvironmentInfoParams {
                    environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                },
            },
            move |app, result: Result<EnvironmentInfoResponse, BackendError>| {
                let cwd = result
                    .ok()
                    .and_then(|info| info.cwd)
                    .map(|cwd| cwd.inferred_native_path_string())
                    .and_then(|cwd| normalize_server_path(&cwd))
                    .unwrap_or(start);
                let path = cwd.clone();
                app.picker_update(id, move |_, flow| {
                    let PickerFlow::Browse(browser) = flow else {
                        return PickerOutcome::Unchanged;
                    };
                    *browser = FolderBrowser::new(path);
                    PickerOutcome::NewStep
                });
                app.newtab_browse_list(id, cwd);
            },
        );
    }

    /// Lists the subfolders of `path` for browser `id`.
    fn newtab_browse_list(&mut self, id: u64, path: String) {
        let Ok(absolute) = AbsolutePathBuf::from_absolute_path_checked(&path) else {
            self.picker_update(id, move |_, flow| {
                if let PickerFlow::Browse(browser) = flow
                    && browser.path == path
                {
                    browser.listing = FolderListing::Unsupported;
                    return PickerOutcome::Refresh;
                }
                PickerOutcome::Unchanged
            });
            return;
        };
        self.backend.call(
            |request_id| ClientRequest::FsReadDirectory {
                request_id,
                params: FsReadDirectoryParams { path: absolute },
            },
            move |app, result: Result<FsReadDirectoryResponse, BackendError>| {
                app.picker_update(id, move |_, flow| {
                    let PickerFlow::Browse(browser) = flow else {
                        return PickerOutcome::Unchanged;
                    };
                    // A listing for a folder the user already left.
                    if browser.path != path {
                        return PickerOutcome::Unchanged;
                    }
                    browser.listing = match result {
                        Ok(response) => FolderListing::Ready(sorted_subfolders(&response.entries)),
                        Err(err) => FolderListing::Failed(err.user_message()),
                    };
                    PickerOutcome::Refresh
                });
            },
        );
    }

    /// Handles the server folder browser's events.
    pub(crate) fn newtab_browse_event(
        &mut self,
        browser: &mut FolderBrowser,
        id: u64,
        event: PickerEvent,
    ) -> PickerOutcome {
        let target = match event {
            PickerEvent::Activated(row) => {
                browser.rows().into_iter().nth(row).map(|(path, _)| path)
            }
            // Enter on a row chosen with the arrow keys opens that row; with
            // an edited path it goes to the path.
            PickerEvent::InputAccepted {
                text,
                selected: Some(row),
            } if normalize_server_path(&text).as_deref() == Some(browser.path.as_str()) => {
                browser.rows().into_iter().nth(row).map(|(path, _)| path)
            }
            PickerEvent::InputAccepted { text, .. } => match normalize_server_path(&text) {
                Some(path) => Some(path),
                None => {
                    browser.error = Some(not_absolute_message(&text));
                    return PickerOutcome::Refresh;
                }
            },
            PickerEvent::InputEdited(_) => {
                return if browser.error.take().is_some() {
                    PickerOutcome::Refresh
                } else {
                    PickerOutcome::Unchanged
                };
            }
            PickerEvent::Button {
                button: PickerButton::Accept,
                input,
                ..
            } => {
                let Some(path) = normalize_server_path(&input) else {
                    browser.error = Some(not_absolute_message(&input));
                    return PickerOutcome::Refresh;
                };
                self.start_thread_checked(PathBuf::from(path));
                return PickerOutcome::Close;
            }
            PickerEvent::Button { .. } => return PickerOutcome::Close,
        };
        let Some(target) = target else {
            return PickerOutcome::Unchanged;
        };
        *browser = FolderBrowser::new(target.clone());
        self.newtab_browse_list(id, target);
        PickerOutcome::NewStep
    }

    /// Whether the app-server's files are on another machine (the inverse of
    /// [`Self::server_files_are_local`]).
    pub(crate) fn server_is_remote(&self) -> bool {
        !self.server_files_are_local()
    }

    fn newtab_visible(&self) -> bool {
        match self.active.and_then(|index| self.tabs.get(index)) {
            Some(tab) => matches!(tab.kind, TabKind::NewTab),
            None => true,
        }
    }

    fn newtab_choices(&self) -> Vec<FolderChoice> {
        merge_recent_folders(
            &self.prefs.recent_folders,
            self.sidebar.recent_threads(),
            MAX_FOLDERS,
        )
    }

    fn newtab_render(&mut self) {
        let choices = self.newtab_choices();
        let unknown: Vec<PathBuf> = choices
            .iter()
            .filter(|choice| !self.newtab.exists.contains_key(&choice.path))
            .map(|choice| choice.path.clone())
            .collect();
        // Local checks answer later and re-render; remote ones answer now.
        self.newtab_check_folders(unknown);
        let rows: Vec<RecentFolder> = choices
            .into_iter()
            .filter(|choice| self.newtab.exists.get(&choice.path) == Some(&true))
            .map(|choice| RecentFolder {
                name: crate::app::folder_label(&choice.path).into(),
                detail: display_path(&choice.path).into(),
                path: choice.path.to_string_lossy().into_owned().into(),
                threads: thread_count_label(choice.thread_count).into(),
                removable: choice.recent,
            })
            .collect();
        crate::sidebar::sync_model(&self.newtab.folders, rows);
        let sign_in_needed = matches!(
            self.newtab.account,
            AccountState::Known {
                sign_in_needed: true
            }
        );
        self.window
            .global::<NewTabState>()
            .set_sign_in_needed(sign_in_needed);
    }

    /// Checks off the UI thread which of `paths` are existing directories.
    fn newtab_check_folders(&mut self, paths: Vec<PathBuf>) {
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|path| self.newtab.checking.insert(path.clone()))
            .collect();
        if paths.is_empty() {
            return;
        }
        if !self.server_files_are_local() {
            // Remote folders cannot be checked from here; offer them all.
            for path in paths {
                self.newtab.checking.remove(&path);
                self.newtab.exists.insert(path, true);
            }
            return;
        }
        self.backend.runtime().spawn_blocking(move || {
            let results: Vec<(PathBuf, bool)> = paths
                .into_iter()
                .map(|path| {
                    let exists = path.is_dir();
                    (path, exists)
                })
                .collect();
            crate::ui_thread::post(move |app| {
                let mut changed = false;
                for (path, exists) in results {
                    app.newtab.checking.remove(&path);
                    changed |= app.newtab.exists.insert(path, exists) != Some(exists);
                }
                if changed && app.newtab_visible() {
                    app.newtab_render();
                }
            });
        });
    }

    fn newtab_fetch_account(&mut self) {
        if self.newtab.account == AccountState::Loading {
            return;
        }
        self.newtab.account = AccountState::Loading;
        self.backend.call(
            |request_id| ClientRequest::GetAccount {
                request_id,
                params: GetAccountParams {
                    refresh_token: false,
                },
            },
            |app, result: Result<GetAccountResponse, BackendError>| {
                if app.newtab.account != AccountState::Loading {
                    // Invalidated while in flight; a newer request follows.
                    return;
                }
                app.newtab.account = match result {
                    Ok(response) => AccountState::Known {
                        sign_in_needed: sign_in_needed(&response),
                    },
                    Err(err) => {
                        tracing::warn!(error = %err.user_message(), "account/read failed");
                        AccountState::Known {
                            sign_in_needed: false,
                        }
                    }
                };
                if app.newtab_visible() {
                    app.newtab_render();
                }
            },
        );
    }
}

/// Whether a server reached through `target` shares this machine's files.
/// A local daemon does (like the TUI, which treats it as a local workspace);
/// a WebSocket server is treated as remote even on a loopback address,
/// which may be a tunnel to another machine.
pub(crate) fn files_are_local(target: &ConnectionTarget) -> bool {
    !matches!(
        target,
        ConnectionTarget::Remote(RemoteAppServerEndpoint::WebSocket { .. })
    )
}

fn not_absolute_message(text: &str) -> String {
    if text.trim().is_empty() {
        "Type the full path of a folder on the server.".to_string()
    } else {
        format!(
            "“{}” is not a full path. Start from the root, for example /home/me/project.",
            text.trim()
        )
    }
}

/// Same rule as the TUI's login screen: the provider needs OpenAI auth and
/// there is no API-key or ChatGPT login.
pub(crate) fn sign_in_needed(response: &GetAccountResponse) -> bool {
    response.requires_openai_auth
        && !matches!(
            response.account,
            Some(Account::ApiKey {} | Account::Chatgpt { .. })
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn summary(id: &str, cwd: &str, updated_at: i64) -> ThreadSummary {
        ThreadSummary {
            id: id.to_string(),
            title: id.to_string(),
            cwd: PathBuf::from(cwd),
            updated_at,
        }
    }

    #[test]
    fn recent_folders_come_first_then_thread_folders() {
        let recent = vec![PathBuf::from("/r1"), PathBuf::from("/a")];
        let threads = vec![
            summary("1", "/a", 10),
            summary("2", "/b", 30),
            summary("3", "/a", 20),
            summary("4", "/c", 5),
        ];
        let merged = merge_recent_folders(&recent, &threads, /*max*/ 4);
        assert_eq!(
            merged,
            vec![
                FolderChoice {
                    path: PathBuf::from("/r1"),
                    thread_count: 0,
                    recent: true,
                },
                FolderChoice {
                    path: PathBuf::from("/a"),
                    thread_count: 2,
                    recent: true,
                },
                FolderChoice {
                    path: PathBuf::from("/b"),
                    thread_count: 1,
                    recent: false,
                },
                FolderChoice {
                    path: PathBuf::from("/c"),
                    thread_count: 1,
                    recent: false,
                },
            ]
        );
        assert_eq!(merge_recent_folders(&recent, &threads, /*max*/ 1).len(), 1);
        assert_eq!(merge_recent_folders(&[], &[], /*max*/ 5), Vec::new());
    }

    #[test]
    fn trust_key_paths_are_quoted_and_escaped() {
        assert_eq!(
            trust_key_path("/Users/me/repo"),
            r#"projects."/Users/me/repo".trust_level"#
        );
        assert_eq!(
            trust_key_path(r#"C:\work\say "hi""#),
            r#"projects."C:\\work\\say \"hi\"".trust_level"#
        );
    }

    #[test]
    fn display_path_shortens_home() {
        let home = Path::new("/Users/me");
        assert_eq!(
            display_path_with_home(Path::new("/Users/me"), Some(home)),
            "~"
        );
        assert_eq!(
            display_path_with_home(Path::new("/Users/me/dev/x"), Some(home)),
            format!("~{}dev/x", std::path::MAIN_SEPARATOR)
        );
        assert_eq!(
            display_path_with_home(Path::new("/opt/x"), Some(home)),
            "/opt/x"
        );
        assert_eq!(display_path_with_home(Path::new("/opt/x"), None), "/opt/x");
    }

    fn keys(path: &str) -> Vec<String> {
        vec![path.to_string()]
    }

    #[test]
    fn trusted_folder_proceeds() {
        let response = json!({
            "config": { "projects": { "/repo": { "trust_level": "trusted" } } },
            "layers": [],
        });
        let roots = LocalRoots {
            project_root_keys: keys("/repo"),
            git_root_keys: Some(keys("/repo")),
            projectless: false,
        };
        assert_eq!(
            decide_trust(
                &response,
                "/repo/sub",
                &keys("/repo/sub"),
                TrustHost::Local(Some(&roots))
            ),
            TrustCheck::Proceed
        );
    }

    #[test]
    fn unknown_git_repo_asks_for_the_repo_root() {
        let response = json!({ "config": {}, "layers": [] });
        let roots = LocalRoots {
            project_root_keys: keys("/repo"),
            git_root_keys: Some(keys("/repo")),
            projectless: false,
        };
        assert_eq!(
            decide_trust(
                &response,
                "/repo/sub",
                &keys("/repo/sub"),
                TrustHost::Local(Some(&roots))
            ),
            TrustCheck::Ask {
                target: PathBuf::from("/repo"),
                untrusted: false,
            }
        );
    }

    #[test]
    fn projectless_folder_proceeds() {
        let response = json!({ "config": {}, "layers": [] });
        let roots = LocalRoots {
            project_root_keys: keys("/tmp/x"),
            git_root_keys: None,
            projectless: true,
        };
        assert_eq!(
            decide_trust(
                &response,
                "/tmp/x",
                &keys("/tmp/x"),
                TrustHost::Local(Some(&roots))
            ),
            TrustCheck::Proceed
        );
    }

    #[test]
    fn enabled_project_layer_proceeds() {
        let response = json!({
            "config": {},
            "layers": [
                { "name": { "type": "project", "dotCodexFolder": "/repo/.codex" } },
                { "name": { "type": "user", "file": "/home/config.toml" } },
            ],
        });
        assert_eq!(
            decide_trust(&response, "/repo", &keys("/repo"), TrustHost::Local(None)),
            TrustCheck::Proceed
        );
    }

    #[test]
    fn disabled_project_layer_asks_for_its_folder() {
        let response = json!({
            "config": {},
            "layers": [{
                "name": { "type": "project", "dotCodexFolder": "/repo/.codex" },
                "disabledReason": "project config is disabled",
            }],
        });
        assert_eq!(
            decide_trust(
                &response,
                "/repo/a",
                &keys("/repo/a"),
                TrustHost::Local(None)
            ),
            TrustCheck::Ask {
                target: PathBuf::from("/repo"),
                untrusted: false,
            }
        );
    }

    #[test]
    fn explicitly_untrusted_project_is_flagged() {
        let response = json!({
            "config": { "projects": { "/repo": { "trust_level": "untrusted" } } },
            "layers": [{
                "name": { "type": "project", "dotCodexFolder": "/repo/.codex" },
                "disabledReason": "/repo is marked as untrusted in config.toml",
            }],
        });
        assert_eq!(
            decide_trust(
                &response,
                "/repo/a",
                &keys("/repo/a"),
                TrustHost::Local(None)
            ),
            TrustCheck::Ask {
                target: PathBuf::from("/repo"),
                untrusted: true,
            }
        );
    }

    #[test]
    fn remote_folder_without_project_layers_asks_for_itself() {
        let response = json!({ "config": {}, "layers": [] });
        assert_eq!(
            decide_trust(&response, "/srv/app", &keys("/srv/app"), TrustHost::Remote),
            TrustCheck::Ask {
                target: PathBuf::from("/srv/app"),
                untrusted: false,
            }
        );
    }

    #[test]
    fn remote_subfolder_of_an_untrusted_project_cannot_be_trusted_alone() {
        let response = json!({
            "config": { "projects": {
                "/srv": { "trust_level": "trusted" },
                "/srv/repo": { "trust_level": "untrusted" },
                "/srv/repository": { "trust_level": "untrusted" },
            } },
            "layers": [],
        });
        // Only "Open restricted" is offered, naming the untrusted project.
        assert_eq!(
            decide_trust(
                &response,
                "/srv/repo/sub",
                &keys("/srv/repo/sub"),
                TrustHost::Remote
            ),
            TrustCheck::Ask {
                target: PathBuf::from("/srv/repo"),
                untrusted: true,
            }
        );
        // Path segments, not string prefixes: /srv/repo-x is outside /srv/repo.
        assert_eq!(
            decide_trust(
                &response,
                "/srv/repo-x",
                &keys("/srv/repo-x"),
                TrustHost::Remote
            ),
            TrustCheck::Ask {
                target: PathBuf::from("/srv/repo-x"),
                untrusted: false,
            }
        );
        // Windows paths on the server compare case-insensitively.
        let windows = json!({
            "config": { "projects": { "C:\\work\\repo": { "trust_level": "untrusted" } } },
            "layers": [],
        });
        assert_eq!(
            decide_trust(
                &windows,
                "c:\\Work\\Repo\\sub",
                &keys("c:\\Work\\Repo\\sub"),
                TrustHost::Remote
            ),
            TrustCheck::Ask {
                target: PathBuf::from("C:\\work\\repo"),
                untrusted: true,
            }
        );
    }

    #[test]
    fn trust_prompt_opens_nothing_unless_chosen() {
        assert_eq!(
            trust_choice(/*untrusted*/ false, PickerButton::Accept),
            TrustChoice::Trust
        );
        assert_eq!(
            trust_choice(/*untrusted*/ false, PickerButton::Secondary),
            TrustChoice::OpenRestricted
        );
        assert_eq!(
            trust_choice(/*untrusted*/ true, PickerButton::Accept),
            TrustChoice::OpenRestricted
        );
        for untrusted in [false, true] {
            assert_eq!(
                trust_choice(untrusted, PickerButton::Cancel),
                TrustChoice::Cancel
            );
            assert_eq!(
                trust_choice(untrusted, PickerButton::Back),
                TrustChoice::Cancel
            );
        }
        // An explicitly untrusted project never offers "Trust".
        assert_eq!(
            trust_choice(/*untrusted*/ true, PickerButton::Secondary),
            TrustChoice::Cancel
        );
        let prompt = TrustPrompt {
            folder: PathBuf::from("/repo/sub"),
            target: PathBuf::from("/repo"),
            untrusted: false,
        };
        let view = prompt.view();
        assert_eq!(view.cancel_label, "Cancel");
        assert_eq!(view.secondary_label, "Open restricted");
        assert_eq!(view.accept_label, "Trust and continue");
        assert!(view.message.contains("project root"));
    }

    #[test]
    fn server_paths_keep_their_own_syntax() {
        assert_eq!(
            normalize_server_path(" /srv/app/ ").as_deref(),
            Some("/srv/app")
        );
        assert_eq!(normalize_server_path("srv/app"), None);
        assert_eq!(normalize_server_path("~/app"), None);
        assert_eq!(server_parent("/srv/app").as_deref(), Some("/srv"));
        assert_eq!(server_parent("/srv").as_deref(), Some("/"));
        assert_eq!(server_parent("/"), None);
        assert_eq!(server_child("/srv", "app").as_deref(), Some("/srv/app"));
        assert_eq!(server_child("/", "srv").as_deref(), Some("/srv"));
        assert_eq!(server_child("/srv", ".."), None);
        assert_eq!(server_child("/srv", "a/b"), None);
        assert_eq!(server_parent("C:\\work\\app").as_deref(), Some("C:\\work"));
        assert_eq!(
            server_child("C:\\work", "app").as_deref(),
            Some("C:\\work\\app")
        );
    }

    #[test]
    fn folder_browser_lists_the_parent_then_subfolders() {
        let entry = |name: &str, is_directory: bool| FsReadDirectoryEntry {
            file_name: name.to_string(),
            is_directory,
            is_file: !is_directory,
        };
        let folders = sorted_subfolders(&[
            entry("zeta", true),
            entry(".git", true),
            entry("README.md", false),
            entry("Alpha", true),
        ]);
        assert_eq!(folders, vec!["Alpha", "zeta", ".git"]);
        let browser = FolderBrowser {
            path: "/srv".to_string(),
            listing: FolderListing::Ready(folders),
            error: None,
        };
        let targets: Vec<String> = browser.rows().into_iter().map(|(path, _)| path).collect();
        assert_eq!(targets, vec!["/", "/srv/Alpha", "/srv/zeta", "/srv/.git"]);
        let view = browser.view("Remote (wss://box:4500)");
        assert!(view.message.contains("Remote (wss://box:4500)"));
        assert_eq!(view.input.map(|input| input.text).as_deref(), Some("/srv"));
        let unsupported = FolderBrowser {
            listing: FolderListing::Unsupported,
            ..FolderBrowser::new("C:\\work".to_string())
        };
        assert!(
            unsupported
                .view("")
                .list
                .is_some_and(|list| list.placeholder.contains("Type the full path"))
        );
    }

    #[test]
    fn only_websocket_servers_have_remote_files() {
        assert!(files_are_local(&ConnectionTarget::Embedded));
        assert!(files_are_local(&ConnectionTarget::Remote(
            RemoteAppServerEndpoint::UnixSocket {
                socket_path: AbsolutePathBuf::from_absolute_path_checked("/tmp/codex.sock")
                    .unwrap_or_else(|err| panic!("{err}")),
            }
        )));
        assert!(!files_are_local(&ConnectionTarget::Remote(
            RemoteAppServerEndpoint::WebSocket {
                websocket_url: "ws://127.0.0.1:4500/".to_string(),
                auth_token: None,
            }
        )));
    }

    #[test]
    fn sign_in_needed_only_without_openai_login() {
        let response = |account: Option<Account>, requires_openai_auth: bool| GetAccountResponse {
            account,
            requires_openai_auth,
            workspace_routing: None,
        };
        assert!(sign_in_needed(&response(None, true)));
        assert!(!sign_in_needed(&response(None, false)));
        assert!(!sign_in_needed(&response(Some(Account::ApiKey {}), true)));
    }
}
