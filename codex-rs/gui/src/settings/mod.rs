//! Settings tab: config editing (Common, All settings, raw `config.toml`),
//! importing from other agents, account sign-in, MCP servers, skills,
//! plugins, hooks, feature flags, GUI preferences, keyboard shortcuts, the
//! server connection, the Windows sandbox, diagnostics, and feedback.
//!
//! Config pages read `config/read` (with layers, so each value can show the
//! layer it came from) and write through `config/batchWrite` with
//! `reload_user_config` so open threads pick changes up. Writes carry the
//! user layer's version for optimistic concurrency. Other writers (folder
//! trust, plugins, skills, the Providers page, other programs) change that
//! version, so the config is re-read whenever the tab is shown, and a
//! key-scoped write that still hits a version conflict is re-sent once with
//! the current version. The raw `config.toml` page reads and writes the file
//! directly so it keeps working when the embedded server failed to start.
//!
//! Model provider settings are fixed when the server starts; when the
//! configured provider no longer matches, the config pages offer a restart.
//!
//! Each page lives in its own module and loads lazily the first time it is
//! shown; a server (re)start invalidates everything.

pub(crate) mod bedrock;

mod account;
mod appearance;
mod connection;
mod diagnostics;
mod extensions;
mod feedback;
mod fields;
mod import;
mod keyboard;
mod mcp;
mod memories;
mod model;
mod raw;
mod schema;
mod toml_value;
mod windows_sandbox;
mod words;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigBatchWriteParams;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::ConfigLayer;
use codex_app_server_protocol::ConfigLayerMetadata;
use codex_app_server_protocol::ConfigLayerSource;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::ConfigRequirements;
use codex_app_server_protocol::ConfigRequirementsReadResponse;
use codex_app_server_protocol::ConfigWriteResponse;
use codex_app_server_protocol::MergeStrategy;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::WriteStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;
use slint::ComponentHandle;
use slint::Model as _;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use crate::app::AppController;
use crate::app::TabKind;
use crate::backend::BackendError;
use crate::ui::ItemRow;
use crate::ui::KeyValue;
use crate::ui::SettingsState;

/// Settings pages, by the id used in `settings_open_page`.
pub(crate) const PAGES: &[&str] = &[
    "common",
    "all",
    "raw",
    "import",
    "account",
    "bedrock",
    "mcp",
    "skills",
    "plugins",
    "hooks",
    "features",
    "memories",
    "appearance",
    "keyboard",
    "connection",
    "windows-sandbox",
    "diagnostics",
    "feedback",
];

/// Most pages fetched when a list RPC is paginated.
const MAX_PAGES: usize = 10;
const PAGE_SIZE: u32 = 100;

/// Effective config as last read from `config/read`.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConfigSnapshot {
    /// Effective config as one JSON object (keys match `config.toml`).
    pub(crate) effective: Value,
    pub(crate) origins: HashMap<String, ConfigLayerMetadata>,
    /// Highest precedence first.
    pub(crate) layers: Vec<ConfigLayer>,
    /// The base user layer's content and version.
    pub(crate) user_config: Value,
    pub(crate) user_version: Option<String>,
    pub(crate) user_file: Option<PathBuf>,
    /// `user_file` as the server reported it (the raw page edits a remote
    /// server's file through the server).
    pub(crate) user_file_abs: Option<AbsolutePathBuf>,
}

impl ConfigSnapshot {
    fn from_response(response: ConfigReadResponse) -> Self {
        let mut effective = serde_json::to_value(&response.config).unwrap_or(Value::Null);
        strip_nulls(&mut effective);
        let layers = response.layers.unwrap_or_default();
        let user_layer = layers
            .iter()
            .find(|layer| model::is_base_user_layer(&layer.name));
        let user_file_abs = layers.iter().find_map(|layer| match &layer.name {
            ConfigLayerSource::User {
                file,
                profile: None,
            } => Some(file.clone()),
            _ => None,
        });
        Self {
            effective,
            origins: response.origins,
            user_config: user_layer
                .map(|layer| layer.config.clone())
                .unwrap_or(Value::Null),
            user_version: user_layer.map(|layer| layer.version.clone()),
            user_file: user_file_abs.as_ref().map(AbsolutePathBuf::to_path_buf),
            user_file_abs,
            layers,
        }
    }
}

/// Provider settings the app-server fixes when it starts: the selected
/// provider and its `model_providers` entry. The model catalog and new
/// threads keep the startup provider until the server restarts.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProviderSettings {
    id: String,
    entry: Option<Value>,
}

impl ProviderSettings {
    pub(crate) fn of(config: &Value) -> Self {
        let id = model::lookup(config, &["model_provider"])
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or("openai")
            .to_string();
        let entry = model::lookup(config, &["model_providers", id.as_str()]).cloned();
        Self { id, entry }
    }
}

/// Tracks whether the configured provider still matches the one the server
/// started with. The first config read after a (re)start is the baseline;
/// reads for another project folder can differ through project layers, so
/// only reads for the baseline's folder update the decision.
#[derive(Debug, Default)]
pub(crate) struct ProviderWatch {
    baseline: Option<(Option<PathBuf>, ProviderSettings)>,
    restart_needed: bool,
}

impl ProviderWatch {
    /// Records the provider settings read for `context`; returns whether a
    /// restart is needed.
    pub(crate) fn observe(&mut self, context: &Option<PathBuf>, current: ProviderSettings) -> bool {
        match &self.baseline {
            None => self.baseline = Some((context.clone(), current)),
            Some((baseline_context, baseline)) if baseline_context == context => {
                self.restart_needed = *baseline != current;
            }
            Some(_) => {}
        }
        self.restart_needed
    }
}

fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, child| !child.is_null());
            for child in map.values_mut() {
                strip_nulls(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// A Slint list model plus the plain rows it was built from, so updates
/// touch only rows that changed (keeping scroll position and focus).
///
/// Each published row carries a revision; editable controls copy the row's
/// value into themselves when it changes (see `SyncedSwitch`). A row whose
/// control value is unchanged keeps its revision, so republishing it for a
/// busy flag, an error, or a note leaves what the user typed or toggled.
pub(crate) struct RowList<R, S: 'static> {
    pub(crate) model: Rc<VecModel<S>>,
    pub(crate) rows: Vec<R>,
    revisions: Vec<i32>,
}

impl<R, S: Clone + 'static> Default for RowList<R, S> {
    fn default() -> Self {
        Self {
            model: Rc::new(VecModel::default()),
            rows: Vec::new(),
            revisions: Vec::new(),
        }
    }
}

impl<R: Clone + PartialEq, S: Clone + 'static> RowList<R, S> {
    pub(crate) fn model_rc(&self) -> ModelRc<S> {
        ModelRc::from(self.model.clone())
    }

    /// Replaces the rows. `key` identifies rows; `force` names rows whose
    /// controls are reset to the row's value even when nothing changed
    /// (after a refused edit). Every change resyncs the controls.
    pub(crate) fn sync(
        &mut self,
        rows: Vec<R>,
        key: impl Fn(&R) -> &str,
        force: &HashSet<String>,
        revision: &mut i32,
        to_slint: impl Fn(&R, i32) -> S,
    ) {
        self.sync_controls(rows, key, |_, _| false, force, revision, to_slint);
    }

    /// Like [`Self::sync`], but rows for which `same_control(new, old)`
    /// holds are republished without resyncing their controls.
    pub(crate) fn sync_controls(
        &mut self,
        rows: Vec<R>,
        key: impl Fn(&R) -> &str,
        same_control: impl Fn(&R, &R) -> bool,
        force: &HashSet<String>,
        revision: &mut i32,
        to_slint: impl Fn(&R, i32) -> S,
    ) {
        let same_shape = rows.len() == self.rows.len()
            && rows
                .iter()
                .zip(&self.rows)
                .all(|(new, old)| key(new) == key(old));
        if same_shape {
            for (index, (new, old)) in rows.iter().zip(&self.rows).enumerate() {
                let forced = force.contains(key(new));
                if new == old && !forced {
                    continue;
                }
                if forced || !same_control(new, old) {
                    *revision += 1;
                    self.revisions[index] = *revision;
                }
                self.model
                    .set_row_data(index, to_slint(new, self.revisions[index]));
            }
        } else {
            self.revisions = rows
                .iter()
                .map(|_| {
                    *revision += 1;
                    *revision
                })
                .collect();
            let converted: Vec<S> = rows
                .iter()
                .zip(&self.revisions)
                .map(|(row, revision)| to_slint(row, *revision))
                .collect();
            self.model.set_vec(converted);
        }
        self.rows = rows;
    }
}

/// Values the user set that are still being saved, keyed by row. Rows show
/// them instead of the stored value so controls do not flick back while the
/// write and the follow-up re-read run.
#[derive(Debug)]
pub(crate) struct PendingValues<T> {
    /// Value, and once saved, the read generation it must be newer than.
    entries: HashMap<String, (T, Option<u64>)>,
}

impl<T> Default for PendingValues<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<T> PendingValues<T> {
    pub(crate) fn begin(&mut self, key: impl Into<String>, value: T) {
        self.entries.insert(key.into(), (value, None));
    }

    /// The write succeeded: keep the value until a read newer than
    /// `generation` shows it.
    pub(crate) fn saved(&mut self, key: &str, generation: u64) {
        if let Some((_, settle_after)) = self.entries.get_mut(key) {
            *settle_after = Some(generation);
        }
    }

    /// The write failed: show the stored value again.
    pub(crate) fn cancel(&mut self, key: &str) {
        self.entries.remove(key);
    }

    /// A read of `generation` arrived; drop the values it reflects.
    pub(crate) fn settle(&mut self, generation: u64) {
        self.entries.retain(|_, (_, settle_after)| {
            settle_after.is_none_or(|settle_after| generation <= settle_after)
        });
    }

    pub(crate) fn get(&self, key: &str) -> Option<&T> {
        self.entries.get(key).map(|(value, _)| value)
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Loading state shared by the lazily loaded pages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum LoadState {
    #[default]
    Stale,
    Loading,
    Loaded,
}

/// UI-thread state of the settings tab.
#[derive(Default)]
pub(crate) struct SettingsController {
    page: String,
    /// The user picked a page since the server last failed.
    page_chosen: bool,
    server_error: Option<String>,
    pub(crate) snapshot: Option<ConfigSnapshot>,
    snapshot_error: Option<String>,
    snapshot_generation: u64,
    /// A `config/read` for `snapshot_generation` is in flight.
    snapshot_loading: bool,
    provider_watch: ProviderWatch,
    pub(crate) requirements: Option<ConfigRequirements>,
    pub(crate) models: Vec<Model>,
    /// Folder whose project layers are included (`None` = user-level only).
    context: Option<PathBuf>,
    context_options: Vec<Option<PathBuf>>,
    /// Monotonic counter stamped on every republished row.
    revision: i32,
    pub(crate) fields: fields::FieldsState,
    pub(crate) raw: raw::RawState,
    pub(crate) account: account::AccountState,
    pub(crate) mcp: mcp::McpState,
    pub(crate) extensions: extensions::ExtensionsState,
    pub(crate) diagnostics: diagnostics::DiagnosticsState,
    pub(crate) keyboard: keyboard::KeyboardState,
    pub(crate) connection: connection::ConnectionState,
    pub(crate) import: import::ImportState,
    pub(crate) feedback: feedback::FeedbackState,
    pub(crate) sandbox: windows_sandbox::SandboxState,
    pub(crate) memories: memories::MemoriesState,
    pub(crate) renderer_in_use: String,
    write_queue: VecDeque<PendingWrite>,
    write_in_flight: bool,
    bound: bool,
}

/// Completion of a config write: `Ok` with the server response, or `Err`
/// with a message for the user.
pub(crate) type WriteDone =
    Box<dyn FnOnce(&mut AppController, Result<ConfigWriteResponse, String>) + Send>;

/// A config write waiting for the previous one to finish.
pub(crate) struct PendingWrite {
    edits: Vec<ConfigEdit>,
    done: WriteDone,
    /// Re-send once with the current version after a version conflict.
    retry_on_conflict: bool,
}

/// Whether `edits` keep their meaning when re-applied on top of a newer
/// config.toml: each sets one key or deep-merges into a table. Replacing a
/// whole table (an edited TOML snippet, a new MCP server) could drop what
/// the other writer added, so those report the conflict instead.
pub(crate) fn retry_safe(edits: &[ConfigEdit]) -> bool {
    edits
        .iter()
        .all(|edit| edit.merge_strategy != MergeStrategy::Replace || !edit.value.is_object())
}

/// Replace edit for `key_path`.
pub(crate) fn replace_edit(key_path: impl Into<String>, value: Value) -> ConfigEdit {
    ConfigEdit {
        key_path: key_path.into(),
        value,
        merge_strategy: MergeStrategy::Replace,
    }
}

/// Upsert (deep merge) edit for `key_path`.
pub(crate) fn upsert_edit(key_path: impl Into<String>, value: Value) -> ConfigEdit {
    ConfigEdit {
        key_path: key_path.into(),
        value,
        merge_strategy: MergeStrategy::Upsert,
    }
}

/// Human message for an `OkOverridden` write, if any.
pub(crate) fn overridden_message(response: &ConfigWriteResponse) -> Option<String> {
    if response.status != WriteStatus::OkOverridden {
        return None;
    }
    Some(match &response.overridden_metadata {
        Some(metadata) => format!(
            "Saved, but {} still wins: {}",
            model::origin_label(&metadata.overriding_layer.name),
            metadata.message
        ),
        None => "Saved, but a higher-precedence layer overrides this value.".to_string(),
    })
}

pub(crate) fn kv(key: impl Into<String>, value: impl Into<String>) -> KeyValue {
    KeyValue {
        key: SharedString::from(key.into()),
        value: SharedString::from(value.into()),
        mono: false,
    }
}

pub(crate) fn kv_mono(key: impl Into<String>, value: impl Into<String>) -> KeyValue {
    KeyValue {
        mono: true,
        ..kv(key, value)
    }
}

/// Plain mirror of the Slint `ItemRow` used to diff list pages.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ListItem {
    pub(crate) header: bool,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) description: String,
    pub(crate) status: String,
    pub(crate) status_tone: i32,
    pub(crate) tag: String,
    pub(crate) detail: String,
    pub(crate) error: String,
    pub(crate) toggle: Option<bool>,
    pub(crate) toggle_enabled: bool,
    pub(crate) action: Option<(String, String)>,
    pub(crate) secondary: Option<(String, String)>,
    pub(crate) secondary_danger: bool,
    pub(crate) busy: bool,
}

/// Status tones understood by `ToneBadge`.
pub(crate) mod tone {
    pub(crate) const NEUTRAL: i32 = 0;
    pub(crate) const SUCCESS: i32 = 1;
    pub(crate) const WARNING: i32 = 2;
    pub(crate) const DANGER: i32 = 3;
    pub(crate) const ACCENT: i32 = 4;
}

impl ListItem {
    pub(crate) fn header(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            header: true,
            id: id.into(),
            title: title.into(),
            ..Self::default()
        }
    }

    pub(crate) fn to_slint(&self, revision: i32) -> ItemRow {
        let (action, action_id) = self.action.clone().unwrap_or_default();
        let (secondary, secondary_id) = self.secondary.clone().unwrap_or_default();
        ItemRow {
            kind: i32::from(self.header),
            id: self.id.as_str().into(),
            title: self.title.as_str().into(),
            subtitle: self.subtitle.as_str().into(),
            description: self.description.as_str().into(),
            status: self.status.as_str().into(),
            status_tone: self.status_tone,
            tag: self.tag.as_str().into(),
            detail: self.detail.as_str().into(),
            error: model::display_error(&self.error).into(),
            has_toggle: self.toggle.is_some(),
            toggle_on: self.toggle.unwrap_or(false),
            toggle_enabled: self.toggle_enabled,
            action: action.into(),
            action_id: action_id.into(),
            secondary: secondary.into(),
            secondary_id: secondary_id.into(),
            secondary_danger: self.secondary_danger,
            busy: self.busy,
            revision,
        }
    }
}

pub(crate) type ItemList = RowList<ListItem, ItemRow>;

impl ItemList {
    /// Republishes list items; only a changed switch value (or `force`)
    /// resyncs a row's switch.
    pub(crate) fn sync_items(
        &mut self,
        items: Vec<ListItem>,
        force: &HashSet<String>,
        revision: &mut i32,
    ) {
        fn item_key(item: &ListItem) -> &str {
            &item.id
        }
        fn same_toggle(new: &ListItem, old: &ListItem) -> bool {
            new.toggle == old.toggle
        }
        self.sync_controls(
            items,
            item_key,
            same_toggle,
            force,
            revision,
            ListItem::to_slint,
        );
    }
}

/// Normalizes a page id; `providers` is an alias of the Bedrock page.
fn normalize_page(page: &str) -> Option<&'static str> {
    let page = match page.trim() {
        "providers" | "provider" => "bedrock",
        "toml" | "config" | "config.toml" => "raw",
        "keymap" | "shortcuts" => "keyboard",
        "memory" => "memories",
        "daemon" | "remote" => "connection",
        "sandbox" => "windows-sandbox",
        other => other,
    };
    PAGES.iter().copied().find(|known| *known == page)
}

impl AppController {
    pub(crate) fn settings_bind(&mut self) {
        if self.settings.bound {
            return;
        }
        self.settings.bound = true;
        self.settings.page = "common".to_string();
        self.settings_detect_renderer();
        let state = self.window.global::<SettingsState>();
        state.set_common_fields(self.settings.fields.common.model_rc());
        state.set_all_fields(self.settings.fields.all.model_rc());
        state.set_mcp_items(self.settings.mcp.items.model_rc());
        state.set_skill_items(self.settings.extensions.skills.model_rc());
        state.set_plugin_items(self.settings.extensions.plugins.model_rc());
        state.set_hook_items(self.settings.extensions.hooks.model_rc());
        state.set_feature_items(self.settings.extensions.features.model_rc());
        state.set_diag_layers(self.settings.diagnostics.layers.model_rc());

        state.on_open_page(|page| {
            let page = page.to_string();
            crate::ui_thread::with_app(move |app| app.settings_open_page(&page));
        });
        state.on_restart_server(|| {
            crate::ui_thread::with_app(AppController::settings_restart_server);
        });
        state.on_copy_text(|text| {
            let text = text.to_string();
            crate::ui_thread::with_app(move |app| app.copy_to_clipboard(&text));
        });
        state.on_open_url(|url| {
            let url = url.to_string();
            crate::ui_thread::with_app(move |app| app.settings_open_url(&url));
        });
        state.on_open_path(|which| {
            let which = which.to_string();
            crate::ui_thread::with_app(move |app| app.settings_open_path(&which));
        });
        state.on_reload_config(|| {
            crate::ui_thread::with_app(|app| {
                app.settings_reload_config();
                app.settings_load_requirements();
            });
        });
        state.on_page_refresh(|page| {
            let page = page.to_string();
            crate::ui_thread::with_app(move |app| app.settings_refresh_page(&page));
        });
        state.on_item_toggle(|page, id, on| {
            let (page, id) = (page.to_string(), id.to_string());
            crate::ui_thread::with_app(move |app| app.settings_item_toggle(&page, &id, on));
        });
        state.on_item_action(|page, id, action| {
            let (page, id, action) = (page.to_string(), id.to_string(), action.to_string());
            crate::ui_thread::with_app(move |app| app.settings_item_action(&page, &id, &action));
        });
        self.settings_fields_bind();
        self.settings_raw_bind();
        self.settings_account_bind();
        self.settings_mcp_bind();
        self.settings_appearance_bind();
        self.settings_diagnostics_bind();
        self.settings_keyboard_bind();
        self.settings_connection_bind();
        self.settings_import_bind();
        self.settings_feedback_bind();
        self.settings_sandbox_bind();
        self.settings_memories_bind();
        self.settings_appearance_show();
    }

    /// Called when the settings tab becomes the active tab.
    pub(crate) fn settings_show(&mut self) {
        // A recording does not survive leaving the tab.
        self.settings_keyboard_stop_recording();
        let generation = self.settings.snapshot_generation;
        self.settings_refresh_context_options();
        if self.settings.server_error.is_some() && !self.settings.page_chosen {
            self.settings.page = "raw".to_string();
        }
        // Other parts of the app and other programs write config.toml while
        // the tab is hidden: show current values, and keep the version that
        // writes are checked against current.
        if self.settings.snapshot_generation == generation
            && self.settings.server_error.is_none()
            && self.backend.is_ready()
        {
            self.settings_reload_config();
        }
        let page = self.settings.page.clone();
        self.settings_activate_page(&page);
    }

    pub(crate) fn settings_open_page(&mut self, page: &str) {
        let Some(page) = normalize_page(page) else {
            tracing::warn!(page, "unknown settings page");
            return;
        };
        self.settings.page_chosen = true;
        self.settings.page = page.to_string();
        self.settings_activate_page(page);
    }

    fn settings_activate_page(&mut self, page: &str) {
        if page.is_empty() {
            return;
        }
        if page != "keyboard" {
            self.settings_keyboard_stop_recording();
        }
        self.window
            .global::<SettingsState>()
            .set_page(SharedString::from(page));
        // Data loads only while the settings tab is visible.
        if !self.settings_tab_active() {
            return;
        }
        match page {
            "common" | "all" => self.settings_fields_activate(page),
            "raw" => self.settings_raw_activate(),
            "account" => self.settings_account_activate(),
            "mcp" => self.settings_mcp_activate(),
            "skills" | "plugins" | "hooks" | "features" => self.settings_extensions_activate(page),
            "memories" => self.settings_memories_activate(),
            "appearance" => self.settings_appearance_show(),
            "diagnostics" => self.settings_diagnostics_activate(),
            "keyboard" => self.settings_keyboard_activate(),
            "connection" => self.settings_connection_activate(),
            "import" => self.settings_import_activate(),
            "feedback" => self.settings_feedback_activate(),
            "windows-sandbox" => self.settings_sandbox_activate(),
            _ => {}
        }
    }

    fn settings_tab_active(&self) -> bool {
        self.active
            .and_then(|index| self.tabs.get(index))
            .is_some_and(|tab| matches!(tab.kind, TabKind::Settings))
    }

    pub(crate) fn settings_on_server_ready(&mut self) {
        self.settings.server_error = None;
        let state = self.window.global::<SettingsState>();
        state.set_server_ready(true);
        state.set_server_error(SharedString::new());
        // The new server started with the provider now in config.toml.
        self.settings.provider_watch = ProviderWatch::default();
        self.settings_refresh_restart_note();
        self.settings_invalidate_pages();
        self.settings_mcp_on_server_reset();
        self.settings_memories_on_server_reset();
        self.settings_reload_config();
        self.settings_load_requirements();
        self.settings_load_models();
        self.settings_account_on_server_ready();
        self.settings_import_on_server_reset();
        self.settings_sandbox_reset();
        self.settings_sandbox_check();
        self.settings_connection_refresh_status();
        if let Some(config) = self.config.as_ref() {
            self.settings.diagnostics.startup_warnings = config.startup_warnings.clone();
        }
        if self.settings_tab_active() {
            let page = self.settings.page.clone();
            self.settings_activate_page(&page);
        }
    }

    pub(crate) fn settings_on_server_failed(&mut self, message: &str) {
        self.settings.server_error = Some(message.to_string());
        self.settings.page_chosen = false;
        let state = self.window.global::<SettingsState>();
        state.set_server_ready(false);
        state.set_server_error(SharedString::from(first_line(message)));
        self.settings.snapshot_loading = false;
        self.settings.provider_watch = ProviderWatch::default();
        self.settings_refresh_restart_note();
        self.settings_account_on_server_failed();
        self.settings_mcp_on_server_reset();
        self.settings_memories_on_server_reset();
        self.settings_import_on_server_reset();
        self.settings_sandbox_reset();
        self.settings_connection_refresh_status();
        if self.settings_tab_active() {
            self.settings.page = "raw".to_string();
            self.settings_activate_page("raw");
        }
    }

    pub(crate) fn settings_on_notification(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::AccountLoginCompleted(completed) => {
                self.settings_account_on_login_completed(completed);
            }
            ServerNotification::AccountUpdated(_) => self.settings_account_on_updated(),
            ServerNotification::AccountRateLimitsUpdated(updated) => {
                self.settings_account_on_rate_limits(&updated.rate_limits);
            }
            ServerNotification::McpServerStatusUpdated(updated) => {
                self.settings_mcp_on_status(updated);
            }
            ServerNotification::McpServerOauthLoginCompleted(completed) => {
                self.settings_mcp_on_oauth_completed(completed);
            }
            ServerNotification::SkillsChanged(_) => self.settings_skills_changed(),
            ServerNotification::ExternalAgentConfigImportProgress(progress) => {
                self.settings_import_on_progress(progress);
            }
            ServerNotification::ExternalAgentConfigImportCompleted(completed) => {
                self.settings_import_on_completed(completed);
            }
            ServerNotification::WindowsSandboxSetupCompleted(completed) => {
                self.settings_sandbox_on_completed(completed);
            }
            ServerNotification::TurnStarted(started) => {
                self.settings.feedback.note_turn_started(&started.thread_id);
            }
            ServerNotification::ConfigWarning(warning) => {
                let text = match &warning.details {
                    Some(details) => format!("{}: {details}", warning.summary),
                    None => warning.summary.clone(),
                };
                self.settings_diagnostics_add_warning(text);
            }
            _ => {}
        }
    }

    fn settings_invalidate_pages(&mut self) {
        self.settings.mcp.load = LoadState::Stale;
        self.settings.extensions.invalidate();
        self.settings.account.load = LoadState::Stale;
        self.settings.import.invalidate();
    }

    fn settings_refresh_page(&mut self, page: &str) {
        match page {
            "mcp" => self.settings_mcp_reload_servers(),
            "skills" | "plugins" | "hooks" | "features" => {
                self.settings_extensions_load(page, /*force*/ true);
            }
            "diagnostics" => {
                self.settings_reload_config();
                self.settings_load_requirements();
            }
            "import" => self.settings_import_activate(),
            "windows-sandbox" => self.settings_sandbox_check(),
            _ => {}
        }
    }

    fn settings_item_toggle(&mut self, page: &str, id: &str, on: bool) {
        match page {
            "mcp" => self.settings_mcp_toggle(id, on),
            "skills" | "plugins" | "hooks" | "features" => {
                self.settings_extensions_toggle(page, id, on);
            }
            _ => {}
        }
    }

    fn settings_item_action(&mut self, page: &str, id: &str, action: &str) {
        match page {
            "mcp" => self.settings_mcp_action(id, action),
            "skills" | "plugins" | "hooks" | "features" => {
                self.settings_extensions_action(page, id, action);
            }
            _ => {}
        }
    }

    fn settings_restart_server(&mut self) {
        self.settings.server_error = None;
        let state = self.window.global::<SettingsState>();
        state.set_server_error(SharedString::new());
        state.set_server_ready(false);
        self.settings.snapshot_error = None;
        self.settings_fields_refresh();
        self.toast("Restarting Codex…");
        self.backend.restart();
    }

    pub(crate) fn settings_open_url(&mut self, url: &str) {
        if let Err(err) = webbrowser::open(url) {
            self.toast(format!("Could not open the browser: {err}"));
        }
    }

    fn settings_open_path(&mut self, which: &str) {
        let path = match which {
            "logs" => self.settings_log_dir(),
            "home" => self.codex_home.clone(),
            _ => None,
        };
        let Some(path) = path else {
            self.toast("That folder is not known yet.");
            return;
        };
        // Local folders: a file manager, never a browser.
        let _ = std::fs::create_dir_all(&path);
        self.open_local_folder(&path);
    }

    pub(crate) fn settings_log_dir(&self) -> Option<PathBuf> {
        match self.config.as_ref() {
            Some(config) => Some(config.log_dir.clone()),
            None => self.codex_home.as_ref().map(|home| home.join("log")),
        }
    }

    // ----- shared config state ---------------------------------------------

    /// The folder whose project config layers are shown, if any.
    pub(crate) fn settings_context_cwd(&self) -> Option<PathBuf> {
        self.settings.context.clone()
    }

    /// Folder used by pages that list per-project items (skills, hooks).
    pub(crate) fn settings_list_cwd(&self) -> Option<PathBuf> {
        self.settings
            .context
            .clone()
            .or_else(|| self.config.as_ref().map(|config| config.cwd.to_path_buf()))
            .or_else(dirs::home_dir)
    }

    /// Rebuilds the folder choices: no project, open thread folders, then
    /// recent folders. Keeps the current choice when still offered.
    fn settings_refresh_context_options(&mut self) {
        let mut options: Vec<Option<PathBuf>> = vec![None];
        for tab in &self.tabs {
            if let Some(thread) = tab.thread()
                && !options.contains(&Some(thread.cwd.clone()))
            {
                options.push(Some(thread.cwd.clone()));
            }
        }
        for folder in self.prefs.recent_folders.iter().take(6) {
            if !options.contains(&Some(folder.clone())) {
                options.push(Some(folder.clone()));
            }
        }
        let first_open = self.settings.context_options.is_empty();
        let mut context_changed = false;
        if first_open && self.settings.context.is_none() {
            // Default to the first open thread's folder.
            self.settings.context = self
                .tabs
                .iter()
                .find_map(|tab| tab.thread().map(|thread| thread.cwd.clone()));
            context_changed = self.settings.context.is_some();
        }
        if !options.contains(&self.settings.context) {
            options.push(self.settings.context.clone());
        }
        let index = options
            .iter()
            .position(|option| *option == self.settings.context)
            .unwrap_or(0);
        let labels: Vec<SharedString> = options
            .iter()
            .map(|option| match option {
                None => SharedString::from("No project (user settings only)"),
                Some(path) => SharedString::from(home_relative(path)),
            })
            .collect();
        let state = self.window.global::<SettingsState>();
        state.set_context_options(ModelRc::new(VecModel::from(labels)));
        state.set_context_index(i32::try_from(index).unwrap_or(0));
        let list_context = match self.settings_list_cwd() {
            Some(path) => format!("Project items come from {}.", short_path(&path)),
            None => String::new(),
        };
        state.set_list_context(list_context.into());
        self.settings.context_options = options;
        if context_changed {
            self.settings.extensions.invalidate();
            self.settings_reload_config();
        }
    }

    fn settings_select_context(&mut self, index: i32) {
        let Some(choice) = usize::try_from(index)
            .ok()
            .and_then(|index| self.settings.context_options.get(index).cloned())
        else {
            return;
        };
        if choice == self.settings.context {
            return;
        }
        self.settings.context = choice;
        self.settings_refresh_context_options();
        self.settings.extensions.invalidate();
        self.settings_reload_config();
    }

    /// Re-reads the effective config with layers and origins.
    pub(crate) fn settings_reload_config(&mut self) {
        if self.settings.server_error.is_some() {
            return;
        }
        self.settings.snapshot_generation += 1;
        self.settings.snapshot_loading = true;
        let generation = self.settings.snapshot_generation;
        let context = self.settings_context_cwd();
        let cwd = context
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        self.backend.call(
            move |request_id| ClientRequest::ConfigRead {
                request_id,
                params: ConfigReadParams {
                    include_layers: true,
                    cwd,
                },
            },
            move |app, result: Result<ConfigReadResponse, BackendError>| {
                if app.settings.snapshot_generation != generation {
                    return;
                }
                app.settings.snapshot_loading = false;
                match result {
                    Ok(response) => {
                        let mut snapshot = ConfigSnapshot::from_response(response);
                        app.settings
                            .provider_watch
                            .observe(&context, ProviderSettings::of(&snapshot.effective));
                        // A write that finished after this read started
                        // has the newer version.
                        if app.settings.write_in_flight || !app.settings.write_queue.is_empty() {
                            snapshot.user_version = app
                                .settings
                                .snapshot
                                .as_ref()
                                .and_then(|old| old.user_version.clone())
                                .or(snapshot.user_version);
                        }
                        app.settings.snapshot = Some(snapshot);
                        app.settings.snapshot_error = None;
                    }
                    Err(err) => {
                        tracing::warn!(%err, "config/read failed");
                        app.settings.snapshot_error = Some(err.user_message());
                    }
                }
                app.settings_on_snapshot_changed();
            },
        );
    }

    fn settings_on_snapshot_changed(&mut self) {
        // Values shown while writes were saving are now in the snapshot.
        let generation = self.settings.snapshot_generation;
        self.settings.fields.settle_pending(generation);
        self.settings.mcp.settle_pending(generation);
        self.settings_refresh_restart_note();
        self.settings_fields_refresh();
        self.settings_raw_on_snapshot();
        self.settings_mcp_refresh_rows();
        self.settings_diagnostics_refresh();
        self.settings_feedback_refresh_disabled();
        self.settings_sandbox_refresh();
    }

    /// Shows or clears the "restart to use the new provider" banner of the
    /// config pages.
    fn settings_refresh_restart_note(&mut self) {
        let (note, offer_restart) = if self.settings.provider_watch.restart_needed
            && self.settings.server_error.is_none()
        {
            if self.backend.is_embedded() {
                (
                        "The model provider settings changed. Restart Codex to use them; until then, new threads keep the current provider.".to_string(),
                        true,
                    )
            } else {
                (
                    format!(
                        "The model provider settings changed. Restart the app-server ({}) to use them.",
                        self.connection_label
                    ),
                    false,
                )
            }
        } else {
            (String::new(), false)
        };
        let state = self.window.global::<SettingsState>();
        state.set_restart_note(note.into());
        state.set_restart_offer(offer_restart);
    }

    fn settings_load_requirements(&mut self) {
        self.backend.call(
            |request_id| ClientRequest::ConfigRequirementsRead {
                request_id,
                params: None,
            },
            |app, result: Result<ConfigRequirementsReadResponse, BackendError>| {
                match result {
                    Ok(response) => app.settings.requirements = response.requirements,
                    Err(err) => tracing::warn!(%err, "configRequirements/read failed"),
                }
                app.settings_account_apply_requirements();
                app.settings_fields_refresh();
                app.settings_diagnostics_refresh();
                app.settings_extensions_refresh_rows("features");
                app.settings_feedback_refresh_disabled();
                app.settings_sandbox_refresh();
            },
        );
    }

    fn settings_load_models(&mut self) {
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let mut models = Vec::new();
            let mut cursor = None;
            let mut error = None;
            for _ in 0..MAX_PAGES {
                let request = ClientRequest::ModelList {
                    request_id: backend.next_request_id(),
                    params: ModelListParams {
                        cursor: cursor.take(),
                        limit: Some(PAGE_SIZE),
                        include_hidden: Some(true),
                    },
                };
                match backend.request::<ModelListResponse>(request).await {
                    Ok(page) => {
                        models.extend(page.data);
                        cursor = page.next_cursor;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    Err(err) => {
                        error = Some(err.user_message());
                        break;
                    }
                }
            }
            crate::ui_thread::post(move |app| {
                if let Some(error) = error {
                    tracing::warn!(error, "model/list failed");
                }
                app.settings.models = models;
                app.settings_fields_refresh();
            });
        });
    }

    /// Writes `edits` to the user config through `config/batchWrite` with
    /// `reload_user_config`, then re-reads the config. A version conflict
    /// (config.toml changed since it was read) re-sends key-scoped edits once
    /// with the current version; otherwise it reloads and reports it.
    pub(crate) fn settings_write(&mut self, edits: Vec<ConfigEdit>, done: WriteDone) {
        let retry_on_conflict = retry_safe(&edits);
        self.settings.write_queue.push_back(PendingWrite {
            edits,
            done,
            retry_on_conflict,
        });
        self.settings_pump_writes();
    }

    /// Sends the next queued write. Writes go one at a time so each carries
    /// the version returned by the previous one; concurrent writes with the
    /// same expected version would conflict.
    fn settings_pump_writes(&mut self) {
        if self.settings.write_in_flight {
            return;
        }
        let Some(write) = self.settings.write_queue.pop_front() else {
            return;
        };
        self.settings.write_in_flight = true;
        let expected_version = self
            .settings
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.user_version.clone());
        self.settings_send_write(write, expected_version);
    }

    fn settings_send_write(&mut self, write: PendingWrite, expected_version: Option<String>) {
        let PendingWrite {
            edits,
            done,
            retry_on_conflict,
        } = write;
        let request_edits = edits.clone();
        self.backend.call(
            move |request_id| ClientRequest::ConfigBatchWrite {
                request_id,
                params: ConfigBatchWriteParams {
                    edits: request_edits,
                    file_path: None,
                    expected_version,
                    reload_user_config: true,
                },
            },
            move |app, result: Result<ConfigWriteResponse, BackendError>| {
                let reload = match result {
                    Ok(response) => {
                        app.settings.write_in_flight = false;
                        if let Some(snapshot) = app.settings.snapshot.as_mut() {
                            snapshot.user_version = Some(response.version.clone());
                        }
                        app.settings_raw_on_config_written();
                        done(app, Ok(response));
                        true
                    }
                    Err(err) => {
                        let conflict = err
                            .server_error()
                            .and_then(model::config_write_error_code)
                            == Some("configVersionConflict");
                        if conflict && retry_on_conflict {
                            tracing::info!("config.toml changed since it was read; retrying the write");
                            app.settings_retry_write(PendingWrite {
                                edits,
                                done,
                                retry_on_conflict: false,
                            });
                            return;
                        }
                        app.settings.write_in_flight = false;
                        let message = if conflict {
                            "config.toml changed on disk since it was loaded. The latest values were reloaded; make the change again.".to_string()
                        } else {
                            err.user_message()
                        };
                        done(app, Err(message));
                        conflict
                    }
                };
                app.settings_write_finished(reload);
            },
        );
    }

    /// Reads the user config's current version and sends `write` again.
    /// The write stays in flight meanwhile, so queued writes wait.
    fn settings_retry_write(&mut self, write: PendingWrite) {
        self.backend.call(
            |request_id| ClientRequest::ConfigRead {
                request_id,
                params: ConfigReadParams {
                    include_layers: true,
                    cwd: None,
                },
            },
            move |app, result: Result<ConfigReadResponse, BackendError>| match result {
                Ok(response) => {
                    let version = ConfigSnapshot::from_response(response).user_version;
                    if let Some(snapshot) = app.settings.snapshot.as_mut() {
                        snapshot.user_version = version.clone();
                    }
                    app.settings_send_write(write, version);
                }
                Err(err) => {
                    app.settings.write_in_flight = false;
                    (write.done)(app, Err(err.user_message()));
                    app.settings_write_finished(/*reload*/ true);
                }
            },
        );
    }

    /// Sends the next queued write, or re-reads the config after the last.
    fn settings_write_finished(&mut self, reload: bool) {
        if self.settings.write_queue.is_empty() {
            if reload {
                self.settings_reload_config();
            }
        } else {
            self.settings_pump_writes();
        }
    }

    /// Scripted settings input for UI automation (`{"settings": [...]}`).
    /// Goes through the same Slint callbacks as real input.
    pub(crate) fn settings_automation(&mut self, args: &[String]) {
        let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
        let text = |index: usize| SharedString::from(arg(index));
        let state = self.window.global::<SettingsState>();
        match arg(0) {
            "toggle" => state.invoke_field_toggle(text(1), text(2), arg(3) == "true"),
            "select" => state.invoke_field_select(text(1), text(2), arg(3).parse().unwrap_or(0)),
            "apply" => state.invoke_field_apply(text(1), text(2), text(3)),
            "reset" => state.invoke_field_reset(text(1), text(2)),
            "expand" => state.invoke_field_expand(text(1), text(2), arg(3) != "false"),
            "search" => {
                state.set_all_search(text(1));
                state.invoke_all_search_edited(text(1));
            }
            "item-toggle" => state.invoke_item_toggle(text(1), text(2), arg(3) == "true"),
            "item-action" => state.invoke_item_action(text(1), text(2), text(3)),
            "raw-draft" => state.set_raw_draft(text(1)),
            "raw-save" => state.invoke_raw_save(state.get_raw_draft()),
            "mcp-form" => {
                state.set_mcp_form_open(true);
                state.set_mcp_form_name(text(1));
                state.set_mcp_form_command(text(2));
                state.set_mcp_form_args(text(3));
            }
            "mcp-submit" => state.invoke_mcp_form_submit(),
            "restart" => state.invoke_restart_server(),
            "memories-reset" => state.invoke_memories_reset(),
            "diag" => {
                state.set_diag_key(text(1));
                state.invoke_diag_lookup(text(1));
            }
            "keyboard" => self.settings_keyboard_automation(&args[1..]),
            "connection" => self.settings_connection_automation(&args[1..]),
            "import" => self.settings_import_automation(&args[1..]),
            "feedback" => self.settings_feedback_automation(&args[1..]),
            other => tracing::warn!(action = other, "unknown settings automation action"),
        }
    }

    /// Detects the active renderer: only GPU renderers accept a rendering
    /// notifier.
    fn settings_detect_renderer(&mut self) {
        let result = self.window.window().set_rendering_notifier(|_, _| {});
        self.settings.renderer_in_use = match result {
            Ok(()) => "GPU (FemtoVG)".to_string(),
            Err(slint::SetRenderingNotifierError::Unsupported) => "Software".to_string(),
            Err(_) => "GPU".to_string(),
        };
    }
}

/// `path` with the home directory shown as `~`.
pub(crate) fn home_relative(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// Path for display: the home folder as `~`, and long paths cut down to
/// their last components (Slint can only elide at the end).
pub(crate) fn short_path(path: &Path) -> String {
    const MAX_CHARS: usize = 64;
    let full = home_relative(path);
    if full.chars().count() <= MAX_CHARS {
        return full;
    }
    let components: Vec<_> = path.components().collect();
    let tail: PathBuf = components[components.len().saturating_sub(3)..]
        .iter()
        .collect();
    format!("…{}{}", std::path::MAIN_SEPARATOR, tail.display())
}

fn first_line(message: &str) -> String {
    let line = message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(message);
    crate::app::truncate_chars(line.trim(), 300)
}

/// Updates a `[FieldData]`-like model in place, used by tests to observe
/// conversions without a window.
#[cfg(test)]
pub(crate) fn rows_of<S: Clone + 'static>(model: &VecModel<S>) -> Vec<S> {
    (0..model.row_count())
        .filter_map(|row| model.row_data(row))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn page_ids_normalize_aliases() {
        assert_eq!(normalize_page("providers"), Some("bedrock"));
        assert_eq!(normalize_page("raw"), Some("raw"));
        assert_eq!(normalize_page("config.toml"), Some("raw"));
        assert_eq!(normalize_page(" mcp "), Some("mcp"));
        assert_eq!(normalize_page("nope"), None);
        assert_eq!(normalize_page("keymap"), Some("keyboard"));
        assert_eq!(normalize_page("sandbox"), Some("windows-sandbox"));
        assert_eq!(normalize_page("remote"), Some("connection"));
        assert_eq!(normalize_page("memory"), Some("memories"));
        for page in [
            "import",
            "keyboard",
            "connection",
            "windows-sandbox",
            "feedback",
            "memories",
        ] {
            assert_eq!(normalize_page(page), Some(page));
        }
        assert!(PAGES.contains(&"diagnostics"));
    }

    #[test]
    fn snapshot_extracts_user_layer_and_strips_nulls() {
        let response: ConfigReadResponse = serde_json::from_value(json!({
            "config": {"model": "gpt", "model_provider": null, "features": {"x": true}},
            "origins": {},
            "layers": [
                {"name": {"type": "project", "dotCodexFolder": abs_str("/r/.codex")}, "version": "p", "config": {}},
                {"name": {"type": "user", "file": abs_str("/h/config.toml"), "profile": null}, "version": "sha256:u", "config": {"model": "gpt"}},
            ],
        }))
        .expect("valid response");
        let snapshot = ConfigSnapshot::from_response(response);
        assert_eq!(snapshot.user_version.as_deref(), Some("sha256:u"));
        assert_eq!(snapshot.user_config, json!({"model": "gpt"}));
        assert_eq!(
            snapshot.user_file,
            Some(PathBuf::from(abs_str("/h/config.toml")))
        );
        assert_eq!(
            snapshot.user_file_abs.map(AbsolutePathBuf::into_path_buf),
            Some(PathBuf::from(abs_str("/h/config.toml")))
        );
        assert_eq!(
            model::lookup(&snapshot.effective, &["model_provider"]),
            None
        );
        assert_eq!(snapshot.layers.len(), 2);
    }

    fn abs_str(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", path.replace('/', "\\"))
        } else {
            path.to_string()
        }
    }

    #[test]
    fn row_list_updates_changed_rows_in_place() {
        let mut list: RowList<(String, i32), (String, i32, i32)> = RowList::default();
        let mut revision = 0;
        let to_slint = |row: &(String, i32), revision: i32| (row.0.clone(), row.1, revision);
        fn key(row: &(String, i32)) -> &str {
            &row.0
        }
        list.sync(
            vec![("a".to_string(), 1), ("b".to_string(), 2)],
            key,
            &HashSet::new(),
            &mut revision,
            to_slint,
        );
        assert_eq!(
            rows_of(&list.model),
            vec![("a".to_string(), 1, 1), ("b".to_string(), 2, 2)]
        );
        list.sync(
            vec![("a".to_string(), 1), ("b".to_string(), 3)],
            key,
            &HashSet::new(),
            &mut revision,
            to_slint,
        );
        assert_eq!(
            rows_of(&list.model),
            vec![("a".to_string(), 1, 1), ("b".to_string(), 3, 3)]
        );
        let force: HashSet<String> = ["a".to_string()].into_iter().collect();
        list.sync(
            vec![("a".to_string(), 1), ("b".to_string(), 3)],
            key,
            &force,
            &mut revision,
            to_slint,
        );
        assert_eq!(rows_of(&list.model)[0], ("a".to_string(), 1, 4));
        list.sync(
            vec![("c".to_string(), 0)],
            key,
            &HashSet::new(),
            &mut revision,
            to_slint,
        );
        assert_eq!(rows_of(&list.model), vec![("c".to_string(), 0, 5)]);
    }

    #[test]
    fn pending_values_last_until_a_newer_read() {
        let mut pending: PendingValues<bool> = PendingValues::default();
        pending.begin("a", true);
        pending.begin("b", false);
        // Still saving: no read settles it.
        pending.settle(100);
        assert_eq!(pending.get("a"), Some(&true));
        pending.saved("a", 7);
        pending.settle(7);
        assert_eq!(pending.get("a"), Some(&true), "a read sent before the save");
        pending.settle(8);
        assert_eq!(pending.get("a"), None);
        assert_eq!(pending.get("b"), Some(&false));
        pending.cancel("b");
        assert_eq!(pending.get("b"), None);
    }

    #[test]
    fn home_relative_abbreviates_the_home_folder() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(home_relative(&home), "~");
        assert_eq!(
            home_relative(&home.join("src")),
            format!("~{}src", std::path::MAIN_SEPARATOR)
        );
    }

    #[test]
    fn short_paths_keep_the_tail() {
        let long = PathBuf::from(
            "/a/very/long/path/that/keeps/going/and/going/for/a/while/longer/project/.codex/config.toml",
        );
        assert_eq!(
            short_path(&long),
            format!(
                "…{sep}project{sep}.codex{sep}config.toml",
                sep = std::path::MAIN_SEPARATOR
            )
        );
        assert_eq!(
            short_path(Path::new("/etc/codex/config.toml")),
            "/etc/codex/config.toml"
        );
    }

    #[test]
    fn first_line_skips_blank_lines() {
        assert_eq!(
            first_line("\n  Error loading config.toml:\nline 3"),
            "Error loading config.toml:"
        );
    }

    fn edit(key_path: &str, value: Value, merge_strategy: MergeStrategy) -> ConfigEdit {
        ConfigEdit {
            key_path: key_path.to_string(),
            value,
            merge_strategy,
        }
    }

    #[test]
    fn only_key_scoped_writes_retry_after_a_version_conflict() {
        assert!(retry_safe(&[
            edit("sandbox_mode", json!("read-only"), MergeStrategy::Replace),
            edit(
                "model_reasoning_effort",
                Value::Null,
                MergeStrategy::Replace
            ),
            edit("notify", json!(["say", "done"]), MergeStrategy::Replace),
            edit(
                "hooks.state",
                json!({"k": {"enabled": false}}),
                MergeStrategy::Upsert
            ),
        ]));
        // Replacing a whole table could drop what the other writer added.
        assert!(!retry_safe(&[edit(
            "mcp_servers",
            json!({"docs": {"command": "x"}}),
            MergeStrategy::Replace
        )]));
    }

    fn provider(config: Value) -> ProviderSettings {
        ProviderSettings::of(&config)
    }

    #[test]
    fn provider_changes_since_the_server_started_need_a_restart() {
        let mut watch = ProviderWatch::default();
        let user_only = None;
        let project = Some(PathBuf::from("/work/repo"));
        // The first read after a (re)start is the baseline.
        assert!(!watch.observe(&user_only, provider(json!({}))));
        assert_eq!(
            provider(json!({})),
            provider(json!({"model_provider": "openai"}))
        );
        assert!(!watch.observe(&user_only, provider(json!({"model": "x"}))));
        // Selecting another provider, or editing the active one, needs one.
        assert!(watch.observe(
            &user_only,
            provider(json!({"model_provider": "amazon-bedrock-runtime"}))
        ));
        assert!(
            !watch.observe(&user_only, provider(json!({}))),
            "changed back"
        );
        assert!(watch.observe(
            &user_only,
            provider(json!({"model_providers": {"openai": {"base_url": "https://proxy"}}}))
        ));
        // Reads for another folder keep the decision (project layers differ).
        assert!(watch.observe(&project, provider(json!({"model_provider": "corp"}))));
        assert!(!watch.observe(&user_only, provider(json!({}))));
        assert!(!watch.observe(&project, provider(json!({"model_provider": "corp"}))));
        // Entries of providers that are not selected do not matter.
        assert!(!watch.observe(
            &user_only,
            provider(json!({"model_providers": {"ollama": {"base_url": "http://x"}}}))
        ));
    }
}
