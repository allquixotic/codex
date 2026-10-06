//! Settings › Diagnostics: config layers, where a key's value comes from,
//! managed requirements, startup warnings, and app facts for bug reports.

use std::collections::HashMap;
use std::collections::HashSet;

use codex_app_server_protocol::ConfigLayer;
use codex_app_server_protocol::ConfigLayerMetadata;
use codex_app_server_protocol::ConfigLayerSource;
use codex_app_server_protocol::ConfigRequirements;
use serde_json::Value;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use super::ItemList;
use super::ListItem;
use super::kv;
use super::kv_mono;
use super::model;
use super::tone;
use crate::app::AppController;
use crate::ui::KeyValue;
use crate::ui::SettingsState;

#[derive(Default)]
pub(crate) struct DiagnosticsState {
    pub(crate) layers: ItemList,
    pub(super) startup_warnings: Vec<String>,
    /// `configWarning` notifications received while running.
    warnings: Vec<String>,
}

/// Where a layer lives: its file, folder, or MDM domain.
fn layer_location(source: &ConfigLayerSource) -> String {
    match source {
        ConfigLayerSource::PackagedDefaults { file }
        | ConfigLayerSource::System { file }
        | ConfigLayerSource::User { file, .. }
        | ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => super::short_path(file),
        ConfigLayerSource::Project { dot_codex_folder } => super::short_path(dot_codex_folder),
        ConfigLayerSource::Mdm { domain, key } => format!("{domain} ({key})"),
        ConfigLayerSource::EnterpriseManaged { id, .. } => format!("Delivered layer {id}"),
        ConfigLayerSource::SessionFlags => "-c flags passed to codex-gui".to_string(),
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => "MDM".to_string(),
    }
}

fn count_leaves(value: &Value) -> usize {
    match value {
        Value::Object(map) => map.values().map(count_leaves).sum(),
        Value::Null => 0,
        _ => 1,
    }
}

/// Rows describing config layers, highest precedence first.
pub(crate) fn layer_rows(layers: &[ConfigLayer]) -> Vec<ListItem> {
    layers
        .iter()
        .enumerate()
        .map(|(index, layer)| {
            let version = layer
                .version
                .strip_prefix("sha256:")
                .unwrap_or(&layer.version);
            let short: String = version.chars().take(12).collect();
            let settings = match count_leaves(&layer.config) {
                1 => "1 setting".to_string(),
                count => format!("{count} settings"),
            };
            let disabled = layer.disabled_reason.is_some();
            ListItem {
                id: format!("{index}"),
                title: model::origin_label(&layer.name),
                subtitle: layer_location(&layer.name),
                status: if disabled { "Ignored" } else { "Active" }.to_string(),
                status_tone: if disabled {
                    tone::WARNING
                } else {
                    tone::SUCCESS
                },
                tag: format!("Precedence {}", layer.name.precedence()),
                detail: format!("{settings} · version {short}"),
                error: layer.disabled_reason.clone().unwrap_or_default(),
                ..ListItem::default()
            }
        })
        .collect()
}

/// Flattens non-null requirement fields into readable key/value pairs.
pub(crate) fn requirement_rows(requirements: &ConfigRequirements) -> Vec<(String, String)> {
    let value = serde_json::to_value(requirements).unwrap_or(Value::Null);
    let mut rows = Vec::new();
    flatten(&value, "", &mut rows);
    rows
}

fn flatten(value: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Null => {}
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                let label = humanize(key);
                let path = if prefix.is_empty() {
                    label
                } else {
                    format!("{prefix} › {label}")
                };
                flatten(&map[key], &path, out);
            }
        }
        Value::Array(items) => {
            let rendered: Vec<String> = items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect();
            out.push((
                prefix.to_string(),
                if rendered.is_empty() {
                    "none allowed".to_string()
                } else {
                    rendered.join(", ")
                },
            ));
        }
        other => out.push((prefix.to_string(), model::display_value(other))),
    }
}

/// `allowedSandboxModes` → `Allowed sandbox modes`.
fn humanize(key: &str) -> String {
    let mut out = String::new();
    for (index, ch) in key.chars().enumerate() {
        if ch == '_' {
            out.push(' ');
        } else if ch.is_ascii_uppercase() && index > 0 {
            out.push(' ');
            out.push(ch.to_ascii_lowercase());
        } else if index == 0 {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// "Defaults", "3 customized", "2 customized, 1 problem (see warnings)".
fn keymap_summary(custom: usize, problems: usize) -> String {
    let custom = match custom {
        0 => "Defaults".to_string(),
        count => format!("{count} customized"),
    };
    match problems {
        0 => custom,
        1 => format!("{custom}, 1 problem (see Warnings)"),
        count => format!("{custom}, {count} problems (see Warnings)"),
    }
}

/// Explains the effective value of `key` and the layers that set it.
pub(crate) fn origin_rows(
    key: &str,
    effective: &Value,
    user: &Value,
    origins: &HashMap<String, ConfigLayerMetadata>,
    requirements: Option<&ConfigRequirements>,
) -> Vec<(String, String, bool)> {
    let segments = match model::parse_key_path(key.trim()) {
        Ok(segments) => segments,
        Err(err) => return vec![("Key".to_string(), format!("Invalid key path: {err}"), false)],
    };
    let mut rows = vec![("Key".to_string(), model::key_path(&segments), true)];
    let effective_text = match model::lookup(effective, &segments) {
        Some(value) => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        None => "Not set; the built-in default applies".to_string(),
    };
    rows.push(("Effective value".to_string(), effective_text, true));
    let found = model::origins_for(origins, &segments);
    if found.is_empty() {
        rows.push((
            "Set by".to_string(),
            "No config file sets this key".to_string(),
            false,
        ));
    }
    for (index, metadata) in found.iter().enumerate() {
        let label = if index == 0 { "Set by" } else { "Also set by" };
        rows.push((
            label.to_string(),
            model::origin_detail(&metadata.name),
            false,
        ));
    }
    let user_text = match model::lookup(user, &segments) {
        Some(value) => value.to_string(),
        None => "Not set".to_string(),
    };
    rows.push(("Your config.toml".to_string(), user_text, true));
    if let Some(field) =
        requirements.and_then(|requirements| model::requirement_pin(requirements, &segments))
    {
        rows.push((
            "Managed".to_string(),
            format!("Pinned by requirements ({field})"),
            false,
        ));
    }
    rows
}

impl AppController {
    pub(super) fn settings_diagnostics_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.on_diag_lookup(|key| {
            let key = key.to_string();
            crate::ui_thread::with_app(move |app| app.settings_diagnostics_lookup(&key));
        });
        state.on_context_selected(|index| {
            crate::ui_thread::with_app(move |app| app.settings_select_context(index));
        });
    }

    pub(super) fn settings_diagnostics_activate(&mut self) {
        if self.settings.snapshot.is_none()
            && !self.settings.snapshot_loading
            && self.settings.server_error.is_none()
        {
            self.settings_reload_config();
        }
        self.settings_diagnostics_refresh();
    }

    pub(super) fn settings_diagnostics_add_warning(&mut self, warning: String) {
        let diagnostics = &mut self.settings.diagnostics;
        if !diagnostics.warnings.contains(&warning) {
            diagnostics.warnings.push(warning);
            self.settings_diagnostics_refresh();
        }
    }

    pub(super) fn settings_diagnostics_refresh(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.set_diag_facts(ModelRc::new(VecModel::from(
            self.settings_diagnostics_facts(),
        )));

        let layers = self
            .settings
            .snapshot
            .as_ref()
            .map(|snapshot| layer_rows(&snapshot.layers))
            .unwrap_or_default();
        let mut revision = self.settings.revision;
        self.settings
            .diagnostics
            .layers
            .sync_items(layers, &HashSet::new(), &mut revision);
        self.settings.revision = revision;

        let requirements: Vec<KeyValue> = self
            .settings
            .requirements
            .as_ref()
            .map(requirement_rows)
            .unwrap_or_default()
            .into_iter()
            .map(|(key, value)| kv(key, value))
            .collect();
        state.set_diag_requirements(ModelRc::new(VecModel::from(requirements)));

        let mut warnings: Vec<SharedString> = Vec::new();
        let keymap_problems = self.settings_keyboard_problems();
        for warning in self
            .settings
            .diagnostics
            .startup_warnings
            .iter()
            .chain(&self.settings.diagnostics.warnings)
            .chain(&keymap_problems)
        {
            let warning = SharedString::from(warning.as_str());
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
        state.set_diag_warnings(ModelRc::new(VecModel::from(warnings)));

        let key = state.get_diag_key().to_string();
        if !key.trim().is_empty() {
            self.settings_diagnostics_lookup(&key);
        }
    }

    fn settings_diagnostics_facts(&self) -> Vec<KeyValue> {
        let mut facts = vec![kv("App version", env!("CARGO_PKG_VERSION"))];
        let server = match (&self.settings.server_error, self.backend.is_ready()) {
            (Some(error), _) => {
                format!("Not running: {}", error.lines().next().unwrap_or_default())
            }
            (None, true) => "Running".to_string(),
            (None, false) => "Starting".to_string(),
        };
        facts.push(kv("Codex server", server));
        if !self.connection_label.is_empty() {
            facts.push(kv("Connection", self.connection_label.as_str()));
        }
        if let Some(config) = self.config.as_ref() {
            facts.push(kv_mono("Model provider", config.model_provider_id.as_str()));
        }
        if let Some(home) = self.codex_home.as_ref() {
            facts.push(kv_mono("Codex home", home.display().to_string()));
        }
        if let Some(file) = self
            .settings
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.user_file.as_ref())
        {
            facts.push(kv_mono("User config", file.display().to_string()));
        }
        if let Some(logs) = self.settings_log_dir() {
            facts.push(kv_mono("Log folder", logs.display().to_string()));
        }
        facts.push(kv(
            "Renderer",
            format!(
                "{} (preference: {})",
                self.settings.renderer_in_use,
                super::appearance::renderer_label(self.prefs.renderer)
            ),
        ));
        facts.push(kv(
            "Platform",
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        ));
        let custom = self.settings_keyboard_custom_count();
        let problems = self.settings_keyboard_problems().len();
        facts.push(kv("Keyboard shortcuts", keymap_summary(custom, problems)));
        facts
    }

    fn settings_diagnostics_lookup(&mut self, key: &str) {
        let state = self.window.global::<SettingsState>();
        let rows: Vec<KeyValue> = match self.settings.snapshot.as_ref() {
            _ if key.trim().is_empty() => Vec::new(),
            None => vec![kv("Key", "The configuration is not loaded yet.")],
            Some(snapshot) => origin_rows(
                key,
                &snapshot.effective,
                &snapshot.user_config,
                &snapshot.origins,
                self.settings.requirements.as_ref(),
            )
            .into_iter()
            .map(|(key, value, mono)| {
                if mono {
                    kv_mono(key, value)
                } else {
                    kv(key, value)
                }
            })
            .collect(),
        };
        state.set_diag_origins(ModelRc::new(VecModel::from(rows)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn abs(path: &str) -> AbsolutePathBuf {
        let path = if cfg!(windows) {
            format!("C:{path}")
        } else {
            path.to_string()
        };
        AbsolutePathBuf::from_absolute_path(path).expect("absolute")
    }

    #[test]
    fn layers_describe_location_state_and_version() {
        let layers = vec![
            ConfigLayer {
                name: ConfigLayerSource::Project {
                    dot_codex_folder: abs("/r/.codex"),
                },
                version: "sha256:0123456789abcdef".to_string(),
                config: json!({"model": "x", "history": {"max_bytes": 1}}),
                disabled_reason: Some("Untrusted project".to_string()),
            },
            ConfigLayer {
                name: ConfigLayerSource::SessionFlags,
                version: "sha256:ff".to_string(),
                config: json!({"model": "y"}),
                disabled_reason: None,
            },
        ];
        let rows = layer_rows(&layers);
        assert_eq!(rows[0].title, "Project config");
        assert!(rows[0].subtitle.ends_with(".codex"), "{}", rows[0].subtitle);
        assert_eq!(rows[0].status, "Ignored");
        assert_eq!(rows[0].error, "Untrusted project");
        assert_eq!(rows[0].detail, "2 settings · version 0123456789ab");
        assert_eq!(rows[0].tag, "Precedence 25");
        assert_eq!(rows[1].detail, "1 setting · version ff");
        assert_eq!(rows[1].status, "Active");
    }

    #[test]
    fn requirements_flatten_to_readable_rows() {
        let requirements: ConfigRequirements = serde_json::from_value(json!({
            "allowedSandboxModes": ["read-only", "workspace-write"],
            "allowedLoginMethods": [],
            "feedback": {"enabled": false},
            "modelProvider": null,
        }))
        .expect("requirements");
        assert_eq!(
            requirement_rows(&requirements),
            vec![
                (
                    "Allowed login methods".to_string(),
                    "none allowed".to_string()
                ),
                (
                    "Allowed sandbox modes".to_string(),
                    "read-only, workspace-write".to_string()
                ),
                ("Feedback › Enabled".to_string(), "false".to_string()),
            ]
        );
    }

    #[test]
    fn origin_rows_explain_a_key() {
        let effective = json!({"model": "gpt", "features": {"x": true}});
        let user = json!({"model": "gpt"});
        let mut origins = HashMap::new();
        origins.insert(
            "model".to_string(),
            ConfigLayerMetadata {
                name: ConfigLayerSource::User {
                    file: abs("/h/config.toml"),
                    profile: None,
                },
                version: "v".to_string(),
            },
        );
        let rows = origin_rows("model", &effective, &user, &origins, None);
        assert_eq!(rows[0], ("Key".to_string(), "model".to_string(), true));
        assert_eq!(
            rows[1],
            ("Effective value".to_string(), "\"gpt\"".to_string(), true)
        );
        assert_eq!(rows[2].0, "Set by");
        assert!(rows[2].1.starts_with("User config ("));
        assert_eq!(
            rows[3],
            ("Your config.toml".to_string(), "\"gpt\"".to_string(), true)
        );
        let rows = origin_rows(r#"features."x""#, &effective, &user, &origins, None);
        assert_eq!(rows[0].1, "features.x");
        assert_eq!(rows[2].1, "No config file sets this key");
        let rows = origin_rows("a..b", &effective, &user, &origins, None);
        assert!(rows[0].1.starts_with("Invalid key path"));
    }

    #[test]
    fn keymap_summary_counts_customizations_and_problems() {
        assert_eq!(keymap_summary(0, 0), "Defaults");
        assert_eq!(keymap_summary(2, 0), "2 customized");
        assert_eq!(
            keymap_summary(1, 1),
            "1 customized, 1 problem (see Warnings)"
        );
        assert_eq!(keymap_summary(0, 3), "Defaults, 3 problems (see Warnings)");
    }

    #[test]
    fn humanizes_wire_keys() {
        assert_eq!(humanize("allowedSandboxModes"), "Allowed sandbox modes");
        assert_eq!(humanize("new_thread"), "New thread");
    }
}
