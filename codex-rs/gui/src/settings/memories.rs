//! Settings › Memories, the GUI's `/memories`: the `memories` feature flag,
//! whether Codex uses memories in new threads and creates them from new
//! threads (`memories.use_memories`, `memories.generate_memories`), and
//! "Reset all memories" (`memory/reset`), like the TUI's memories popup.
//!
//! The switches are config rows (see [`super::fields`]), so they show their
//! origin layer, managed locks, and saving state like every other setting.

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::MemoryResetResponse;
use serde_json::Value;
use slint::ComponentHandle;
use slint::SharedString;

use super::fields::FieldInputs;
use super::fields::FieldRow;
use super::fields::schema_row;
use super::schema::FieldKind;
use super::schema::FieldSpec;
use super::tone;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::backend::BackendError;
use crate::ui::SettingsState;

/// Key of the feature flag the memory settings depend on.
pub(crate) const FEATURE_KEY: &str = "features.memories";
const DOCS_URL: &str = "https://developers.openai.com/codex/memories";

/// Shown under the memory settings while the feature is off.
const FEATURE_OFF_NOTE: &str = "Takes effect when Memories is turned on.";

#[derive(Debug, Default)]
pub(crate) struct MemoriesState {
    /// `memory/reset` is running.
    resetting: bool,
    /// Result of the last reset: text and `ToneBadge` tone.
    notice: Option<(String, i32)>,
}

/// One switch of the page, as a schema field so it gets the usual origin,
/// lock, and default handling.
fn switch_row(
    path: &str,
    title: &str,
    description: &str,
    default: Option<bool>,
    inputs: &FieldInputs<'_>,
) -> FieldRow {
    let spec = FieldSpec {
        segments: path.split('.').map(str::to_string).collect(),
        section: "memories".to_string(),
        description: description.to_string(),
        kind: FieldKind::Bool,
        default: default.map(Value::Bool),
    };
    let mut row = schema_row(&spec, inputs);
    row.title = title.to_string();
    row
}

/// Rows of the Memories page: the feature flag, then the two settings.
pub(crate) fn memory_rows(inputs: &FieldInputs<'_>) -> Vec<FieldRow> {
    let feature = switch_row(
        FEATURE_KEY,
        "Memories",
        "Let Codex remember useful context from your threads and bring it into new ones.",
        /*default*/ None,
        inputs,
    );
    let feature_on = feature.checked;
    let mut settings = [
        switch_row(
            "memories.use_memories",
            "Use memories",
            "New threads start with what Codex remembers.",
            /*default*/ Some(true),
            inputs,
        ),
        switch_row(
            "memories.generate_memories",
            "Generate memories",
            "Codex creates memories from new threads after they go idle.",
            /*default*/ Some(true),
            inputs,
        ),
    ];
    if !feature_on {
        for row in &mut settings {
            row.note = FEATURE_OFF_NOTE.to_string();
        }
    }
    std::iter::once(feature).chain(settings).collect()
}

impl AppController {
    pub(super) fn settings_memories_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.set_memory_fields(self.settings.fields.memories.model_rc());
        state.on_memories_reset(|| {
            crate::ui_thread::with_app(AppController::settings_memories_confirm_reset);
        });
        state.on_memories_learn_more(|| {
            crate::ui_thread::with_app(|app| app.settings_open_url(DOCS_URL));
        });
    }

    pub(super) fn settings_memories_activate(&mut self) {
        self.settings_fields_activate("memories");
        self.settings_memories_render();
    }

    /// A reset in flight on a server that stopped never reports back.
    pub(super) fn settings_memories_on_server_reset(&mut self) {
        self.settings.memories.resetting = false;
        self.settings_memories_render();
    }

    fn settings_memories_confirm_reset(&mut self) {
        if self.settings.memories.resetting || !self.backend.is_ready() {
            return;
        }
        let place = if self.backend.is_embedded() {
            "in your Codex home"
        } else {
            "on the app-server's machine"
        };
        self.show_dialog(
            DialogRequest::confirm(
                "Reset all memories?",
                format!(
                    "This deletes the memory files and thread summaries {place}. Your threads are kept. This cannot be undone."
                ),
            )
            .accept_label("Reset all memories")
            .destructive(),
            Box::new(|app, accepted| {
                if accepted.is_some() {
                    app.settings_memories_reset();
                }
            }),
        );
    }

    fn settings_memories_reset(&mut self) {
        self.settings.memories.resetting = true;
        self.settings.memories.notice = None;
        self.settings_memories_render();
        self.backend.call(
            |request_id| ClientRequest::MemoryReset {
                request_id,
                params: None,
            },
            |app, result: Result<MemoryResetResponse, BackendError>| {
                if !app.settings.memories.resetting {
                    return;
                }
                app.settings.memories.resetting = false;
                app.settings.memories.notice = Some(match result {
                    Ok(_) => ("All memories were reset.".to_string(), tone::SUCCESS),
                    Err(err) => (
                        format!("Could not reset memories: {}", err.user_message()),
                        tone::DANGER,
                    ),
                });
                app.settings_memories_render();
            },
        );
    }

    fn settings_memories_render(&self) {
        let memories = &self.settings.memories;
        let state = self.window.global::<SettingsState>();
        state.set_memories_resetting(memories.resetting);
        let (notice, notice_tone) = memories
            .notice
            .clone()
            .unwrap_or((String::new(), tone::NEUTRAL));
        state.set_memories_notice(SharedString::from(notice));
        state.set_memories_notice_tone(notice_tone);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ConfigRequirements;
    use codex_app_server_protocol::ExperimentalFeature;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::HashMap;

    fn memories_feature() -> ExperimentalFeature {
        serde_json::from_value(json!({
            "name": "memories",
            "stage": "stable",
            "displayName": null,
            "description": null,
            "announcement": null,
            "enabled": false,
            "defaultEnabled": false,
        }))
        .expect("feature")
    }

    fn rows(effective: Value, requirements: Option<&ConfigRequirements>) -> Vec<FieldRow> {
        let origins = HashMap::new();
        let features = vec![memories_feature()];
        memory_rows(&FieldInputs {
            effective: &effective,
            user: &effective,
            origins: &origins,
            requirements,
            features: &features,
        })
    }

    #[test]
    fn settings_default_on_and_note_the_feature_when_it_is_off() {
        let rows = rows(json!({}), None);
        let keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                FEATURE_KEY,
                "memories.use_memories",
                "memories.generate_memories"
            ]
        );
        assert_eq!(rows[0].title, "Memories");
        assert!(!rows[0].checked, "the memories feature is off by default");
        assert!(rows[1].checked && rows[2].checked);
        assert_eq!(rows[1].note, FEATURE_OFF_NOTE);
    }

    #[test]
    fn configured_values_and_managed_features_show() {
        let requirements: ConfigRequirements =
            serde_json::from_value(json!({"featureRequirements": {"memories": true}}))
                .expect("requirements");
        let rows = rows(
            json!({"features": {"memories": true}, "memories": {"generate_memories": false}}),
            Some(&requirements),
        );
        assert!(rows[0].checked);
        assert!(rows[0].locked, "managed feature flags cannot be changed");
        assert!(rows[1].checked);
        assert!(!rows[2].checked);
        assert_eq!(rows[2].note, "");
    }
}
