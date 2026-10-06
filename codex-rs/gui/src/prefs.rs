//! GUI-only preferences stored in `$CODEX_HOME/gui.json`.
//!
//! These are settings that only make sense for this front end (renderer,
//! window geometry, recent folders). Everything that affects agent behavior
//! lives in `config.toml` and is edited through the app-server config RPCs.

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

const PREFS_FILE_NAME: &str = "gui.json";
const MAX_RECENT_FOLDERS: usize = 12;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RendererChoice {
    /// OpenGL renderer when available, software otherwise.
    #[default]
    Auto,
    /// Force the CPU renderer (least memory; no GPU needed).
    Software,
    /// WGPU renderer (Metal / D3D12 / Vulkan), also allowing software
    /// adapters such as Windows WARP on GPU-less VMs.
    Gpu,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

/// What typing + Enter does while a turn is running.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BusyInput {
    /// Inject the message into the running turn (`turn/steer`).
    #[default]
    Steer,
    /// Queue the message until the turn finishes (`thread/queue/add`).
    Queue,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub(crate) struct Prefs {
    pub(crate) renderer: RendererChoice,
    pub(crate) theme: ThemeChoice,
    /// Enter sends, Shift+Enter inserts a newline. When false, Ctrl/Cmd+Enter sends.
    pub(crate) enter_sends: bool,
    pub(crate) busy_input: BusyInput,
    pub(crate) font_size: f32,
    pub(crate) desktop_notifications: bool,
    /// Register cross-tab messaging tools on new threads.
    pub(crate) cross_tab_tools: bool,
    pub(crate) sidebar_visible: bool,
    pub(crate) info_pane_visible: bool,
    pub(crate) window_width: f32,
    pub(crate) window_height: f32,
    /// Most recent first.
    pub(crate) recent_folders: Vec<PathBuf>,
    /// App-server to use: empty for embedded (default), `unix://` for the
    /// local daemon, or `ws://host:port` / `wss://host:port`.
    pub(crate) app_server_address: String,
    /// Environment variable holding the remote app-server auth token.
    pub(crate) remote_auth_token_env: Option<String>,
    /// Shortcut overrides: action name → binding (see `shortcuts.rs`).
    pub(crate) keymap: std::collections::BTreeMap<String, String>,
    /// Keys this version does not know (for example written by a newer
    /// version), kept so saving does not drop them.
    #[serde(flatten)]
    pub(crate) unknown: serde_json::Map<String, serde_json::Value>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            renderer: RendererChoice::Auto,
            theme: ThemeChoice::System,
            enter_sends: true,
            busy_input: BusyInput::Steer,
            font_size: 13.0,
            desktop_notifications: true,
            cross_tab_tools: true,
            sidebar_visible: true,
            info_pane_visible: true,
            window_width: 1280.0,
            window_height: 820.0,
            recent_folders: Vec::new(),
            app_server_address: String::new(),
            remote_auth_token_env: None,
            keymap: std::collections::BTreeMap::new(),
            unknown: serde_json::Map::new(),
        }
    }
}

/// Preferences read at startup, plus problems to show the user.
#[derive(Debug)]
pub(crate) struct LoadedPrefs {
    pub(crate) prefs: Prefs,
    /// One message per problem; empty when the file was fine or missing.
    pub(crate) problems: Vec<String>,
}

impl Prefs {
    /// Loads prefs from `codex_home`. Invalid values fall back to their
    /// defaults one by one; an unreadable file falls back to all defaults.
    pub(crate) fn load(codex_home: Option<&Path>) -> Self {
        Self::load_checked(codex_home, /*backup_unreadable*/ false).prefs
    }

    /// Like [`Prefs::load`], but describes what was ignored. With
    /// `backup_unreadable`, a file that is not valid JSON is copied to
    /// `gui.json.invalid-<unix time>` first, because the next save replaces
    /// it.
    pub(crate) fn load_checked(codex_home: Option<&Path>, backup_unreadable: bool) -> LoadedPrefs {
        let Some(path) = codex_home.map(prefs_path) else {
            return LoadedPrefs {
                prefs: Self::default(),
                problems: Vec::new(),
            };
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return LoadedPrefs {
                    prefs: Self::default(),
                    problems: Vec::new(),
                };
            }
            Err(err) => {
                return LoadedPrefs {
                    prefs: Self::default(),
                    problems: vec![format!(
                        "{} could not be read ({err}); default preferences are in use.",
                        path.display()
                    )],
                };
            }
        };
        match parse_lenient(&bytes) {
            Ok((prefs, invalid)) => {
                let problems = if invalid.is_empty() {
                    Vec::new()
                } else {
                    vec![format!(
                        "{}: ignored invalid values for {}; their defaults are in use.",
                        path.display(),
                        invalid.join(", ")
                    )]
                };
                LoadedPrefs {
                    prefs: prefs.sanitized(),
                    problems,
                }
            }
            Err(err) => {
                let backup = if backup_unreadable {
                    backup_file(&path)
                } else {
                    None
                };
                let kept = match backup {
                    Some(backup) => format!(" The file was copied to {}.", backup.display()),
                    None => String::new(),
                };
                LoadedPrefs {
                    prefs: Self::default(),
                    problems: vec![format!(
                        "{} is not valid ({err}); default preferences are in use.{kept}",
                        path.display()
                    )],
                }
            }
        }
    }

    /// Writes prefs atomically: a temp file named after this process (so
    /// two running instances never share one), then a rename.
    pub(crate) fn save(&self, codex_home: &Path) -> std::io::Result<()> {
        let path = prefs_path(codex_home);
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::create_dir_all(codex_home)?;
        let tmp = codex_home.join(format!("{PREFS_FILE_NAME}.{}.tmp", std::process::id()));
        let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Moves `folder` to the front of the recent list.
    pub(crate) fn remember_folder(&mut self, folder: &Path) {
        self.recent_folders.retain(|existing| existing != folder);
        self.recent_folders.insert(0, folder.to_path_buf());
        self.recent_folders.truncate(MAX_RECENT_FOLDERS);
    }

    fn sanitized(mut self) -> Self {
        if !(8.0..=32.0).contains(&self.font_size) {
            self.font_size = Self::default().font_size;
        }
        if !(400.0..=10_000.0).contains(&self.window_width) {
            self.window_width = Self::default().window_width;
        }
        if !(300.0..=10_000.0).contains(&self.window_height) {
            self.window_height = Self::default().window_height;
        }
        self.recent_folders.truncate(MAX_RECENT_FOLDERS);
        self
    }
}

fn prefs_path(codex_home: &Path) -> PathBuf {
    codex_home.join(PREFS_FILE_NAME)
}

/// Parses `gui.json`, keeping every valid value. Returns the names of the
/// ignored values (for example `renderer` or `keymap.new-tab`); fails only
/// when the file is not a JSON object at all.
fn parse_lenient(bytes: &[u8]) -> Result<(Prefs, Vec<String>), String> {
    use serde_json::Map;
    use serde_json::Value;

    let user = match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(user)) => user,
        Ok(_) => return Err("expected a JSON object".to_string()),
        Err(err) => return Err(err.to_string()),
    };
    let defaults = match serde_json::to_value(Prefs::default()) {
        Ok(Value::Object(defaults)) => defaults,
        _ => return Err("internal error: default preferences".to_string()),
    };
    // Whether `defaults` with `key` set to `value` deserializes.
    let accepts = |key: &str, value: &Value| {
        let mut candidate = defaults.clone();
        candidate.insert(key.to_string(), value.clone());
        serde_json::from_value::<Prefs>(Value::Object(candidate)).is_ok()
    };
    let mut merged = defaults.clone();
    let mut invalid = Vec::new();
    for (key, value) in user {
        let Some(default) = defaults.get(&key) else {
            merged.insert(key, value);
            continue;
        };
        if accepts(&key, &value) {
            merged.insert(key, value);
            continue;
        }
        // Keep the valid entries of a map (keymap) or list (recent folders).
        match (value, default) {
            (Value::Object(entries), Value::Object(_)) => {
                let mut kept = Map::new();
                for (name, entry) in entries {
                    let single = Value::Object(Map::from_iter([(name.clone(), entry.clone())]));
                    if accepts(&key, &single) {
                        kept.insert(name, entry);
                    } else {
                        invalid.push(format!("`{key}.{name}`"));
                    }
                }
                merged.insert(key, Value::Object(kept));
            }
            (Value::Array(items), Value::Array(_)) => {
                let (kept, dropped): (Vec<Value>, Vec<Value>) = items
                    .into_iter()
                    .partition(|item| accepts(&key, &Value::Array(vec![item.clone()])));
                match dropped.len() {
                    0 => {}
                    1 => invalid.push(format!("1 entry of `{key}`")),
                    count => invalid.push(format!("{count} entries of `{key}`")),
                }
                merged.insert(key, Value::Array(kept));
            }
            _ => invalid.push(format!("`{key}`")),
        }
    }
    serde_json::from_value(Value::Object(merged))
        .map(|prefs| (prefs, invalid))
        .map_err(|err| err.to_string())
}

/// Copies an unreadable `gui.json` aside before it gets overwritten.
fn backup_file(path: &Path) -> Option<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let backup = path.with_file_name(format!("{PREFS_FILE_NAME}.invalid-{stamp}"));
    match std::fs::copy(path, &backup) {
        Ok(_) => Some(backup),
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "could not back up gui.json");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn round_trips_and_fills_missing_fields() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join(PREFS_FILE_NAME),
            br#"{"renderer":"software"}"#,
        )?;
        let prefs = Prefs::load(Some(dir.path()));
        assert_eq!(prefs.renderer, RendererChoice::Software);
        assert!(prefs.enter_sends);

        let mut changed = prefs;
        changed.theme = ThemeChoice::Dark;
        changed.save(dir.path())?;
        assert_eq!(Prefs::load(Some(dir.path())), changed);
        Ok(())
    }

    #[test]
    fn invalid_values_fall_back_one_by_one() -> Result<(), String> {
        let (prefs, invalid) = parse_lenient(
            br#"{
                "renderer": "vulkan",
                "theme": "dark",
                "keymap": {"new-tab": "Mod+N", "close-tab": ["Mod+W"]},
                "recent_folders": ["/a", 3],
                "font_size": "big"
            }"#,
        )?;
        assert_eq!(prefs.renderer, RendererChoice::Auto);
        assert_eq!(prefs.theme, ThemeChoice::Dark);
        assert_eq!(
            prefs.keymap,
            std::collections::BTreeMap::from([("new-tab".to_string(), "Mod+N".to_string())])
        );
        assert_eq!(prefs.recent_folders, vec![PathBuf::from("/a")]);
        assert_eq!(prefs.font_size, Prefs::default().font_size);
        // Key order depends on serde_json's map features; compare sorted.
        let mut invalid = invalid;
        invalid.sort();
        assert_eq!(
            invalid,
            vec![
                "1 entry of `recent_folders`".to_string(),
                "`font_size`".to_string(),
                "`keymap.close-tab`".to_string(),
                "`renderer`".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn unknown_keys_survive_a_save() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join(PREFS_FILE_NAME),
            br#"{"theme":"light","future_setting":{"a":1}}"#,
        )?;
        let loaded = Prefs::load_checked(Some(dir.path()), /*backup_unreadable*/ true);
        assert_eq!(loaded.problems, Vec::<String>::new());
        loaded.prefs.save(dir.path())?;
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(PREFS_FILE_NAME))?)?;
        assert_eq!(saved["future_setting"], serde_json::json!({"a": 1}));
        assert_eq!(saved["theme"], serde_json::json!("light"));
        // Only gui.json is left; the per-process temp file was renamed.
        let names: Vec<String> = std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![PREFS_FILE_NAME.to_string()]);
        Ok(())
    }

    #[test]
    fn unreadable_file_is_backed_up_and_reported() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(PREFS_FILE_NAME);
        std::fs::write(&path, b"{\"theme\": \"dark\",}")?;
        let loaded = Prefs::load_checked(Some(dir.path()), /*backup_unreadable*/ true);
        assert_eq!(loaded.prefs, Prefs::default());
        assert_eq!(loaded.problems.len(), 1);
        assert!(
            loaded.problems[0].contains("copied to"),
            "{:?}",
            loaded.problems
        );
        let backups: Vec<PathBuf> = std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().contains(".invalid-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0])?, b"{\"theme\": \"dark\",}");
        // Without a backup request (the early renderer lookup) nothing is copied.
        let quiet = Prefs::load_checked(Some(dir.path()), /*backup_unreadable*/ false);
        assert!(!quiet.problems[0].contains("copied to"));
        Ok(())
    }

    #[test]
    fn missing_file_is_not_a_problem() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let loaded = Prefs::load_checked(Some(dir.path()), /*backup_unreadable*/ true);
        assert_eq!(loaded.prefs, Prefs::default());
        assert!(loaded.problems.is_empty());
        Ok(())
    }

    #[test]
    fn recent_folders_are_deduplicated_and_capped() {
        let mut prefs = Prefs::default();
        for index in 0..20 {
            prefs.remember_folder(Path::new(&format!("/f{index}")));
        }
        prefs.remember_folder(Path::new("/f5"));
        assert_eq!(prefs.recent_folders.len(), MAX_RECENT_FOLDERS);
        assert_eq!(prefs.recent_folders[0], PathBuf::from("/f5"));
        assert_eq!(
            prefs
                .recent_folders
                .iter()
                .filter(|folder| folder.as_path() == Path::new("/f5"))
                .count(),
            1
        );
    }

    #[test]
    fn out_of_range_values_fall_back_to_defaults() {
        let prefs = Prefs {
            font_size: 200.0,
            window_width: 1.0,
            ..Prefs::default()
        }
        .sanitized();
        assert_eq!(prefs.font_size, Prefs::default().font_size);
        assert_eq!(prefs.window_width, Prefs::default().window_width);
    }
}
