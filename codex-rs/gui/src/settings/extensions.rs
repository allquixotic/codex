//! Settings › Skills, Plugins, Hooks, and Features: list pages with
//! enable toggles and a few actions (install, uninstall, trust).
//!
//! Skill toggles and plugin installs are written by the server itself
//! (`skills/config/write`, `plugin/install`, `plugin/uninstall`), so the
//! config is re-read afterwards to keep the version later writes are checked
//! against current.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ExperimentalFeature;
use codex_app_server_protocol::ExperimentalFeatureListParams;
use codex_app_server_protocol::ExperimentalFeatureListResponse;
use codex_app_server_protocol::ExperimentalFeatureStage;
use codex_app_server_protocol::HookHandlerMetadata;
use codex_app_server_protocol::HookSource;
use codex_app_server_protocol::HookTrustStatus;
use codex_app_server_protocol::HooksListEntry;
use codex_app_server_protocol::HooksListParams;
use codex_app_server_protocol::HooksListResponse;
use codex_app_server_protocol::PluginAvailability;
use codex_app_server_protocol::PluginInstallParams;
use codex_app_server_protocol::PluginInstallPolicy;
use codex_app_server_protocol::PluginInstallResponse;
use codex_app_server_protocol::PluginListParams;
use codex_app_server_protocol::PluginListResponse;
use codex_app_server_protocol::PluginUninstallParams;
use codex_app_server_protocol::PluginUninstallResponse;
use codex_app_server_protocol::SkillScope;
use codex_app_server_protocol::SkillsConfigWriteParams;
use codex_app_server_protocol::SkillsConfigWriteResponse;
use codex_app_server_protocol::SkillsListEntry;
use codex_app_server_protocol::SkillsListParams;
use codex_app_server_protocol::SkillsListResponse;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;
use serde_json::json;
use slint::ComponentHandle;

use super::ItemList;
use super::ListItem;
use super::LoadState;
use super::MAX_PAGES;
use super::PAGE_SIZE;
use super::PendingValues;
use super::model;
use super::overridden_message;
use super::replace_edit;
use super::tone;
use super::upsert_edit;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::backend::BackendError;
use crate::ui::SettingsState;

/// Marketplaces the TUI hides from CLI users.
const HIDDEN_MARKETPLACES: &[&str] = &["openai-bundled"];

#[derive(Default)]
pub(crate) struct ExtensionsState {
    pub(crate) skills: ItemList,
    pub(crate) plugins: ItemList,
    pub(crate) hooks: ItemList,
    pub(crate) features: ItemList,
    pages: HashMap<&'static str, PageMeta>,
    skill_entries: Vec<SkillsListEntry>,
    plugin_list: Option<PluginListResponse>,
    hook_entries: Vec<HooksListEntry>,
    feature_list: Vec<ExperimentalFeature>,
}

/// Per-page bookkeeping (rows live in the `ItemList`s above so Slint models
/// can be bound once).
#[derive(Default)]
struct PageMeta {
    load: LoadState,
    generation: u64,
    error: Option<String>,
    busy: HashSet<String>,
    errors: HashMap<String, String>,
    notices: HashMap<String, String>,
    force: HashSet<String>,
    /// Switch values being saved, shown until the list is re-read.
    pending: PendingValues<bool>,
}

impl PageMeta {
    /// The switch value of row `id`: a value being saved, else `stored`.
    fn toggle(&self, id: &str, stored: bool) -> Option<bool> {
        Some(self.pending.get(id).copied().unwrap_or(stored))
    }
}

fn page_key(page: &str) -> Option<&'static str> {
    ["skills", "plugins", "hooks", "features"]
        .into_iter()
        .find(|known| *known == page)
}

impl ExtensionsState {
    pub(crate) fn invalidate(&mut self) {
        for meta in self.pages.values_mut() {
            meta.load = LoadState::Stale;
        }
    }

    fn meta(&mut self, page: &'static str) -> &mut PageMeta {
        self.pages.entry(page).or_default()
    }

    /// Feature flags from the last `experimentalFeature/list`.
    pub(crate) fn feature_flags(&self) -> &[ExperimentalFeature] {
        &self.feature_list
    }

    fn list(&mut self, page: &str) -> Option<&mut ItemList> {
        match page {
            "skills" => Some(&mut self.skills),
            "plugins" => Some(&mut self.plugins),
            "hooks" => Some(&mut self.hooks),
            "features" => Some(&mut self.features),
            _ => None,
        }
    }
}

fn skill_scope_title(scope: SkillScope) -> &'static str {
    match scope {
        SkillScope::Repo => "Project skills",
        SkillScope::User => "Your skills",
        SkillScope::Admin => "Managed skills",
        SkillScope::System => "Built-in skills",
    }
}

/// Rows of the Skills page, grouped by scope.
fn skill_rows(entries: &[SkillsListEntry], meta: &PageMeta, ready: bool) -> Vec<ListItem> {
    let mut rows = Vec::new();
    for scope in [
        SkillScope::Repo,
        SkillScope::User,
        SkillScope::Admin,
        SkillScope::System,
    ] {
        let mut skills: Vec<_> = entries
            .iter()
            .flat_map(|entry| &entry.skills)
            .filter(|skill| skill.scope == scope)
            .collect();
        if skills.is_empty() {
            continue;
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        rows.push(ListItem::header(
            format!("§{}", model::wire_name(&scope)),
            skill_scope_title(scope),
        ));
        for skill in skills {
            let id = skill.path.display().to_string();
            let title = skill
                .interface
                .as_ref()
                .and_then(|interface| interface.display_name.clone())
                .unwrap_or_else(|| skill.name.clone());
            let description = skill
                .short_description
                .clone()
                .or_else(|| {
                    skill
                        .interface
                        .as_ref()
                        .and_then(|i| i.short_description.clone())
                })
                .unwrap_or_else(|| skill.description.clone());
            rows.push(ListItem {
                title,
                subtitle: super::short_path(&skill.path),
                description: crate::app::truncate_chars(&description, 280),
                tag: skill
                    .plugin_id
                    .as_ref()
                    .map(|plugin| format!("Plugin {plugin}"))
                    .unwrap_or_default(),
                error: meta.errors.get(&id).cloned().unwrap_or_default(),
                detail: meta.notices.get(&id).cloned().unwrap_or_default(),
                toggle: meta.toggle(&id, skill.enabled),
                toggle_enabled: ready,
                busy: meta.busy.contains(&id),
                id,
                ..ListItem::default()
            });
        }
    }
    let errors: Vec<_> = entries.iter().flat_map(|entry| &entry.errors).collect();
    if !errors.is_empty() {
        let mut header = ListItem::header("§errors", "Skills that could not be loaded");
        header.status = errors.len().to_string();
        header.status_tone = tone::DANGER;
        rows.push(header);
        for error in errors {
            rows.push(ListItem {
                id: format!("error:{}", error.path.display()),
                title: error.path.file_name().map_or_else(
                    || error.path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                subtitle: error.path.display().to_string(),
                error: error.message.clone(),
                ..ListItem::default()
            });
        }
    }
    rows
}

/// Rows of the Plugins page, one section per marketplace.
fn plugin_rows(response: &PluginListResponse, meta: &PageMeta, ready: bool) -> Vec<ListItem> {
    let mut rows = Vec::new();
    for marketplace in &response.marketplaces {
        if HIDDEN_MARKETPLACES.contains(&marketplace.name.as_str()) {
            continue;
        }
        let mut header = ListItem::header(
            format!("§{}", marketplace.name),
            marketplace
                .interface
                .as_ref()
                .and_then(|interface| interface.display_name.clone())
                .unwrap_or_else(|| marketplace.name.clone()),
        );
        header.description = match marketplace.plugins.len() {
            0 => "No plugins".to_string(),
            1 => "1 plugin".to_string(),
            count => format!("{count} plugins"),
        };
        rows.push(header);
        for plugin in &marketplace.plugins {
            let installable = plugin.install_policy != PluginInstallPolicy::NotAvailable
                && plugin.availability == PluginAvailability::Available;
            let (status, status_tone) = if plugin.availability != PluginAvailability::Available {
                ("Disabled by admin".to_string(), tone::WARNING)
            } else if !plugin.installed {
                ("Not installed".to_string(), tone::NEUTRAL)
            } else if plugin.enabled {
                ("Enabled".to_string(), tone::SUCCESS)
            } else {
                ("Installed".to_string(), tone::NEUTRAL)
            };
            let interface = plugin.interface.as_ref();
            let mut subtitle = plugin.id.clone();
            if let Some(version) = plugin.local_version.as_ref().or(plugin.version.as_ref()) {
                subtitle.push_str(&format!(" · v{version}"));
            }
            rows.push(ListItem {
                id: plugin.id.clone(),
                title: interface
                    .and_then(|interface| interface.display_name.clone())
                    .unwrap_or_else(|| plugin.name.clone()),
                subtitle,
                description: interface
                    .and_then(|interface| interface.short_description.clone())
                    .unwrap_or_default(),
                status,
                status_tone,
                tag: interface
                    .and_then(|interface| interface.developer_name.clone())
                    .unwrap_or_default(),
                detail: meta.notices.get(&plugin.id).cloned().unwrap_or_default(),
                error: meta.errors.get(&plugin.id).cloned().unwrap_or_default(),
                toggle: if plugin.installed {
                    meta.toggle(&plugin.id, plugin.enabled)
                } else {
                    None
                },
                toggle_enabled: ready,
                action: (!plugin.installed && installable && ready)
                    .then(|| ("Install".to_string(), "install".to_string())),
                secondary: (plugin.installed && ready)
                    .then(|| ("Uninstall".to_string(), "uninstall".to_string())),
                busy: meta.busy.contains(&plugin.id),
                ..ListItem::default()
            });
        }
    }
    if !response.marketplace_load_errors.is_empty() {
        rows.push(ListItem::header(
            "§errors",
            "Marketplaces that could not be loaded",
        ));
        for error in &response.marketplace_load_errors {
            rows.push(ListItem {
                id: format!("error:{}", error.marketplace_path.display()),
                title: error.marketplace_path.display().to_string(),
                error: error.message.clone(),
                ..ListItem::default()
            });
        }
    }
    rows
}

fn hook_source_label(source: HookSource) -> &'static str {
    match source {
        HookSource::System => "System",
        HookSource::User => "User",
        HookSource::Project => "Project",
        HookSource::Mdm => "MDM",
        HookSource::SessionFlags => "Command line",
        HookSource::Plugin => "Plugin",
        HookSource::CloudRequirements | HookSource::CloudManagedConfig => "Organization",
        HookSource::LegacyManagedConfigFile | HookSource::LegacyManagedConfigMdm => "Managed",
        HookSource::Unknown => "Unknown",
    }
}

/// Rows of the Hooks page.
fn hook_rows(entries: &[HooksListEntry], meta: &PageMeta, ready: bool) -> Vec<ListItem> {
    let mut rows = Vec::new();
    let mut hooks: Vec<_> = entries.iter().flat_map(|entry| &entry.hooks).collect();
    hooks.sort_by_key(|hook| (hook.display_order, hook.key.clone()));
    let mut seen = HashSet::new();
    for hook in hooks {
        if !seen.insert(hook.key.clone()) {
            continue;
        }
        let handler = match &hook.handler {
            HookHandlerMetadata::Command { command, r#async } => {
                if *r#async {
                    format!("{command} (async)")
                } else {
                    command.clone()
                }
            }
            HookHandlerMetadata::McpTool { server, tool } => format!("MCP tool {server}/{tool}"),
            HookHandlerMetadata::Prompt {} => "Prompt hook".to_string(),
            HookHandlerMetadata::Agent {} => "Agent hook".to_string(),
        };
        let (status, status_tone) = match hook.trust_status {
            HookTrustStatus::Managed => ("Managed", tone::NEUTRAL),
            HookTrustStatus::Trusted => ("Trusted", tone::SUCCESS),
            HookTrustStatus::Untrusted => ("Not trusted", tone::WARNING),
            HookTrustStatus::Modified => ("Changed since trusted", tone::WARNING),
        };
        let mut title = model::wire_name(&hook.event_name);
        title = capitalize(&title);
        if let Some(matcher) = hook.matcher.as_ref().filter(|matcher| !matcher.is_empty()) {
            title.push_str(&format!(" · {matcher}"));
        }
        let needs_trust = !hook.is_managed
            && matches!(
                hook.trust_status,
                HookTrustStatus::Untrusted | HookTrustStatus::Modified
            );
        let mut tag = hook_source_label(hook.source).to_string();
        if let Some(plugin) = hook.plugin_id.as_ref() {
            tag = format!("Plugin {plugin}");
        }
        rows.push(ListItem {
            id: hook.key.clone(),
            title,
            subtitle: handler,
            description: hook.status_message.clone().unwrap_or_default(),
            status: status.to_string(),
            status_tone,
            tag,
            detail: format!(
                "{} · timeout {}s",
                super::short_path(&hook.source_path),
                hook.timeout_sec
            ),
            error: meta.errors.get(&hook.key).cloned().unwrap_or_default(),
            toggle: meta.toggle(&hook.key, hook.enabled),
            toggle_enabled: ready && !hook.is_managed,
            action: (needs_trust && ready).then(|| ("Trust".to_string(), "trust".to_string())),
            busy: meta.busy.contains(&hook.key),
            ..ListItem::default()
        });
    }
    let warnings: Vec<&String> = entries.iter().flat_map(|entry| &entry.warnings).collect();
    let errors: Vec<_> = entries.iter().flat_map(|entry| &entry.errors).collect();
    if !warnings.is_empty() || !errors.is_empty() {
        rows.push(ListItem::header("§problems", "Problems"));
        for (index, warning) in warnings.into_iter().enumerate() {
            rows.push(ListItem {
                id: format!("warning:{index}"),
                title: "Warning".to_string(),
                description: warning.clone(),
                status: "Warning".to_string(),
                status_tone: tone::WARNING,
                ..ListItem::default()
            });
        }
        for error in errors {
            rows.push(ListItem {
                id: format!("error:{}", error.path.display()),
                title: error.path.display().to_string(),
                error: error.message.clone(),
                ..ListItem::default()
            });
        }
    }
    rows
}

/// Section id and title for a feature stage.
fn stage_label(stage: &ExperimentalFeatureStage) -> (&'static str, &'static str) {
    match stage {
        ExperimentalFeatureStage::Beta => ("Beta", "Beta"),
        ExperimentalFeatureStage::Stable => ("Stable", "Stable"),
        ExperimentalFeatureStage::UnderDevelopment => {
            ("In development", "Under development (may be unstable)")
        }
        ExperimentalFeatureStage::Deprecated => ("Deprecated", "Deprecated"),
        ExperimentalFeatureStage::Removed => ("Removed", "Removed"),
    }
}

/// Rows of the Features page, grouped by stage. Removed flags are hidden.
fn feature_rows(
    features: &[ExperimentalFeature],
    locked: &HashMap<String, bool>,
    meta: &PageMeta,
    ready: bool,
) -> Vec<ListItem> {
    let mut rows = Vec::new();
    for stage in [
        ExperimentalFeatureStage::Beta,
        ExperimentalFeatureStage::Stable,
        ExperimentalFeatureStage::UnderDevelopment,
        ExperimentalFeatureStage::Deprecated,
    ] {
        let mut group: Vec<_> = features
            .iter()
            .filter(|feature| feature.stage == stage)
            .collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by(|a, b| a.name.cmp(&b.name));
        let (label, title) = stage_label(&stage);
        rows.push(ListItem::header(format!("§{label}"), title));
        for feature in group {
            let managed = locked.get(&feature.name);
            let default = if feature.default_enabled { "on" } else { "off" };
            let mut detail = format!("features.{} · default {default}", feature.name);
            if let Some(required) = managed {
                detail.push_str(&format!(
                    " · managed: always {}",
                    if *required { "on" } else { "off" }
                ));
            }
            rows.push(ListItem {
                id: feature.name.clone(),
                title: feature
                    .display_name
                    .clone()
                    .unwrap_or_else(|| feature.name.clone()),
                description: feature
                    .description
                    .clone()
                    .or_else(|| feature.announcement.clone())
                    .unwrap_or_default(),
                status: if feature.enabled == feature.default_enabled {
                    String::new()
                } else {
                    "Changed".to_string()
                },
                status_tone: tone::ACCENT,
                tag: if managed.is_some() {
                    "Managed".to_string()
                } else {
                    String::new()
                },
                detail,
                error: meta
                    .errors
                    .get(&feature.name)
                    .or_else(|| meta.notices.get(&feature.name))
                    .cloned()
                    .unwrap_or_default(),
                toggle: meta.toggle(&feature.name, feature.enabled),
                toggle_enabled: ready && managed.is_none(),
                busy: meta.busy.contains(&feature.name),
                ..ListItem::default()
            });
        }
    }
    rows
}

/// Edit persisting a feature toggle, like the TUI: an explicit value when
/// enabling or when the default is on, otherwise the key is removed.
pub(crate) fn feature_edit(
    name: &str,
    enabled: bool,
    default_enabled: bool,
) -> codex_app_server_protocol::ConfigEdit {
    let value = if enabled || default_enabled {
        Value::Bool(enabled)
    } else {
        Value::Null
    };
    replace_edit(model::feature_key_path(name), value)
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl AppController {
    pub(super) fn settings_extensions_activate(&mut self, page: &str) {
        let Some(key) = page_key(page) else {
            return;
        };
        if self.settings.extensions.meta(key).load == LoadState::Stale {
            self.settings_extensions_load(page, /*force*/ false);
        }
        self.settings_extensions_refresh_rows(page);
    }

    /// Loads `page`'s list unless it is loaded or loading (other pages use
    /// the feature list for defaults).
    pub(super) fn settings_extensions_ensure_loaded(&mut self, page: &str) {
        if let Some(key) = page_key(page)
            && self.settings.extensions.meta(key).load == LoadState::Stale
        {
            self.settings_extensions_load(page, /*force*/ false);
        }
    }

    pub(super) fn settings_skills_changed(&mut self) {
        let meta = self.settings.extensions.meta("skills");
        if meta.load == LoadState::Stale {
            return;
        }
        meta.load = LoadState::Stale;
        if self.settings.page == "skills" && self.settings_tab_active() {
            self.settings_extensions_load("skills", /*force*/ true);
        }
    }

    pub(super) fn settings_extensions_load(&mut self, page: &str, force: bool) {
        let Some(key) = page_key(page) else {
            return;
        };
        if self.settings.server_error.is_some() {
            return;
        }
        let meta = self.settings.extensions.meta(key);
        meta.load = LoadState::Loading;
        meta.generation += 1;
        let generation = meta.generation;
        self.settings_extensions_refresh_rows(page);
        let cwd = self.settings_list_cwd();
        match key {
            "skills" => self.backend.call(
                move |request_id| ClientRequest::SkillsList {
                    request_id,
                    params: SkillsListParams {
                        cwds: cwd.into_iter().collect(),
                        force_reload: force,
                    },
                },
                move |app, result: Result<SkillsListResponse, BackendError>| {
                    let data = app.settings_extensions_loaded(
                        "skills",
                        generation,
                        result.map(|r| r.data),
                    );
                    if let Some(data) = data {
                        app.settings.extensions.skill_entries = data;
                    }
                    app.settings_extensions_refresh_rows("skills");
                },
            ),
            "plugins" => {
                let cwds = cwd
                    .and_then(|cwd| AbsolutePathBuf::from_absolute_path(cwd).ok())
                    .map(|cwd| vec![cwd]);
                self.backend.call(
                    move |request_id| ClientRequest::PluginList {
                        request_id,
                        params: PluginListParams {
                            cwds,
                            marketplace_kinds: None,
                            force_refetch: force,
                        },
                    },
                    move |app, result: Result<PluginListResponse, BackendError>| {
                        if let Some(list) =
                            app.settings_extensions_loaded("plugins", generation, result)
                        {
                            app.settings.extensions.plugin_list = Some(list);
                        }
                        app.settings_extensions_refresh_rows("plugins");
                    },
                );
            }
            "hooks" => self.backend.call(
                move |request_id| ClientRequest::HooksList {
                    request_id,
                    params: HooksListParams {
                        cwds: cwd.into_iter().collect(),
                    },
                },
                move |app, result: Result<HooksListResponse, BackendError>| {
                    if let Some(data) =
                        app.settings_extensions_loaded("hooks", generation, result.map(|r| r.data))
                    {
                        app.settings.extensions.hook_entries = data;
                    }
                    app.settings_extensions_refresh_rows("hooks");
                },
            ),
            _ => self.settings_features_load(generation),
        }
    }

    fn settings_features_load(&mut self, generation: u64) {
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let mut features = Vec::new();
            let mut cursor = None;
            let mut error = None;
            for _ in 0..MAX_PAGES {
                let request = ClientRequest::ExperimentalFeatureList {
                    request_id: backend.next_request_id(),
                    params: ExperimentalFeatureListParams {
                        cursor: cursor.take(),
                        limit: Some(PAGE_SIZE),
                        thread_id: None,
                    },
                };
                match backend
                    .request::<ExperimentalFeatureListResponse>(request)
                    .await
                {
                    Ok(page) => {
                        features.extend(page.data);
                        cursor = page.next_cursor;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    Err(err) => {
                        error = Some(err);
                        break;
                    }
                }
            }
            crate::ui_thread::post(move |app| {
                let result = match error {
                    Some(err) if features.is_empty() => Err(err),
                    _ => Ok(features),
                };
                if let Some(features) =
                    app.settings_extensions_loaded("features", generation, result)
                {
                    app.settings.extensions.feature_list = features;
                    // All settings and Memories show feature defaults.
                    app.settings_fields_refresh();
                }
                app.settings_extensions_refresh_rows("features");
            });
        });
    }

    /// Records a list result; returns the data when it is still current.
    fn settings_extensions_loaded<T>(
        &mut self,
        page: &'static str,
        generation: u64,
        result: Result<T, BackendError>,
    ) -> Option<T> {
        let meta = self.settings.extensions.meta(page);
        if meta.generation != generation {
            return None;
        }
        meta.load = LoadState::Loaded;
        // Switches saved before this list was requested now show its value.
        meta.pending.settle(generation);
        match result {
            Ok(data) => {
                meta.error = None;
                Some(data)
            }
            Err(err) => {
                meta.error = Some(err.user_message());
                None
            }
        }
    }

    pub(super) fn settings_extensions_refresh_rows(&mut self, page: &str) {
        let Some(key) = page_key(page) else {
            return;
        };
        let ready = self.settings.server_error.is_none() && self.backend.is_ready();
        let locked: HashMap<String, bool> = self
            .settings
            .requirements
            .as_ref()
            .and_then(|requirements| requirements.feature_requirements.clone())
            .map(|features| features.into_iter().collect())
            .unwrap_or_default();
        let extensions = &mut self.settings.extensions;
        let meta = extensions.pages.entry(key).or_default();
        let rows = match key {
            "skills" => skill_rows(&extensions.skill_entries, meta, ready),
            "plugins" => extensions
                .plugin_list
                .as_ref()
                .map(|list| plugin_rows(list, meta, ready))
                .unwrap_or_default(),
            "hooks" => hook_rows(&extensions.hook_entries, meta, ready),
            _ => feature_rows(&extensions.feature_list, &locked, meta, ready),
        };
        let noun = match key {
            "skills" => "skills",
            "plugins" => "plugins",
            "hooks" => "hooks",
            _ => "features",
        };
        let status = match (&meta.error, meta.load) {
            (Some(error), _) => format!("Could not load {noun}: {error}"),
            (None, LoadState::Loading) if rows.is_empty() => format!("Loading {noun}…"),
            _ => String::new(),
        };
        let force = std::mem::take(&mut meta.force);
        let mut revision = self.settings.revision;
        if let Some(list) = self.settings.extensions.list(key) {
            list.sync_items(rows, &force, &mut revision);
        }
        self.settings.revision = revision;
        let state = self.window.global::<SettingsState>();
        let status = status.into();
        match key {
            "skills" => state.set_skill_status(status),
            "plugins" => state.set_plugin_status(status),
            "hooks" => state.set_hook_status(status),
            _ => state.set_feature_status(status),
        }
    }

    /// Marks row `id` busy; `toggle` is the switch value being saved.
    fn settings_extensions_begin(&mut self, page: &'static str, id: &str, toggle: Option<bool>) {
        let meta = self.settings.extensions.meta(page);
        meta.busy.insert(id.to_string());
        meta.errors.remove(id);
        meta.notices.remove(id);
        if let Some(on) = toggle {
            meta.pending.begin(id, on);
        }
        self.settings_extensions_refresh_rows(page);
    }

    /// Finishes an action on row `id`: records the error (the switch shows
    /// the stored value again) or reloads the list (the switch keeps the
    /// saved value until the list has it).
    fn settings_extensions_finish(
        &mut self,
        page: &'static str,
        id: String,
        error: Option<String>,
    ) {
        let meta = self.settings.extensions.meta(page);
        meta.busy.remove(&id);
        match error {
            Some(error) => {
                meta.pending.cancel(&id);
                meta.force.insert(id.clone());
                meta.errors.insert(id, error);
                self.settings_extensions_refresh_rows(page);
            }
            None => {
                let generation = meta.generation;
                meta.pending.saved(&id, generation);
                self.settings_extensions_load(page, /*force*/ true);
            }
        }
    }

    pub(super) fn settings_extensions_toggle(&mut self, page: &str, id: &str, on: bool) {
        let Some(page) = page_key(page) else {
            return;
        };
        let id = id.to_string();
        self.settings_extensions_begin(page, &id, Some(on));
        match page {
            "skills" => {
                let path = PathBuf::from(&id);
                let Ok(path) = AbsolutePathBuf::from_absolute_path(path) else {
                    self.settings_extensions_finish(
                        page,
                        id,
                        Some("The skill path is not absolute.".to_string()),
                    );
                    return;
                };
                self.backend.call(
                    move |request_id| ClientRequest::SkillsConfigWrite {
                        request_id,
                        params: SkillsConfigWriteParams {
                            path: Some(path),
                            name: None,
                            enabled: on,
                        },
                    },
                    move |app, result: Result<SkillsConfigWriteResponse, BackendError>| {
                        if result.is_ok() {
                            // The server wrote config.toml.
                            app.settings_reload_config();
                        }
                        let error = match result {
                            Ok(response) if response.effective_enabled != on => Some(format!(
                                "Saved, but a higher-precedence setting keeps this skill {}.",
                                if response.effective_enabled {
                                    "on"
                                } else {
                                    "off"
                                }
                            )),
                            Ok(_) => None,
                            Err(err) => Some(err.user_message()),
                        };
                        app.settings_extensions_finish("skills", id, error);
                    },
                );
            }
            "plugins" => {
                let edit = upsert_edit(
                    model::key_path(&["plugins", id.as_str()]),
                    json!({ "enabled": on }),
                );
                self.settings_write(
                    vec![edit],
                    Box::new(move |app, result| {
                        let error = result.err();
                        app.settings_extensions_finish("plugins", id, error);
                    }),
                );
            }
            "hooks" => {
                let edit = upsert_edit("hooks.state", json!({ id.as_str(): { "enabled": on } }));
                self.settings_write(
                    vec![edit],
                    Box::new(move |app, result| {
                        let error = result.err();
                        app.settings_extensions_finish("hooks", id, error);
                    }),
                );
            }
            _ => {
                let default_enabled = self
                    .settings
                    .extensions
                    .feature_list
                    .iter()
                    .find(|feature| feature.name == id)
                    .is_some_and(|feature| feature.default_enabled);
                let edit = feature_edit(&id, on, default_enabled);
                self.settings_write(
                    vec![edit],
                    Box::new(move |app, result| {
                        let meta = app.settings.extensions.meta("features");
                        let error = match result {
                            Ok(response) => {
                                if let Some(message) = overridden_message(&response) {
                                    meta.notices.insert(id.clone(), message);
                                }
                                None
                            }
                            Err(message) => Some(message),
                        };
                        app.settings_extensions_finish("features", id, error);
                    }),
                );
            }
        }
    }

    pub(super) fn settings_extensions_action(&mut self, page: &str, id: &str, action: &str) {
        match (page, action) {
            ("plugins", "install") => self.settings_plugin_install(id),
            ("plugins", "uninstall") => {
                let id = id.to_string();
                self.show_dialog(
                    DialogRequest::confirm(
                        "Uninstall plugin?",
                        format!("{id} and its skills, MCP servers, and hooks are removed."),
                    )
                    .accept_label("Uninstall")
                    .destructive(),
                    Box::new(move |app, accepted| {
                        if accepted.is_some() {
                            app.settings_plugin_uninstall(id);
                        }
                    }),
                );
            }
            ("hooks", "trust") => self.settings_hook_trust(id),
            _ => {}
        }
    }

    fn settings_plugin_install(&mut self, id: &str) {
        let Some(list) = self.settings.extensions.plugin_list.as_ref() else {
            return;
        };
        let Some((marketplace, plugin)) = list.marketplaces.iter().find_map(|marketplace| {
            marketplace
                .plugins
                .iter()
                .find(|plugin| plugin.id == id)
                .map(|plugin| (marketplace, plugin))
        }) else {
            return;
        };
        let (marketplace_path, remote_marketplace_name) = match &marketplace.path {
            Some(path) => (Some(path.clone()), None),
            None => (None, Some(marketplace.name.clone())),
        };
        let plugin_name = plugin.name.clone();
        let id = id.to_string();
        self.settings_extensions_begin("plugins", &id, /*toggle*/ None);
        self.backend.call(
            move |request_id| ClientRequest::PluginInstall {
                request_id,
                params: PluginInstallParams {
                    marketplace_path,
                    remote_marketplace_name,
                    install_attempt_id: None,
                    plugin_name,
                },
            },
            move |app, result: Result<PluginInstallResponse, BackendError>| match result {
                Ok(response) => {
                    // The server wrote config.toml.
                    app.settings_reload_config();
                    if response.apps_needing_auth.is_empty() {
                        app.toast(format!("Installed {id}"));
                    } else {
                        app.settings.extensions.meta("plugins").notices.insert(
                            id.clone(),
                            format!(
                                "Installed. {} connected app(s) need sign-in before use.",
                                response.apps_needing_auth.len()
                            ),
                        );
                    }
                    app.settings_extensions_finish("plugins", id, None);
                }
                Err(err) => {
                    let message = format!("Could not install: {}", err.user_message());
                    app.settings_extensions_finish("plugins", id, Some(message));
                }
            },
        );
    }

    fn settings_plugin_uninstall(&mut self, id: String) {
        self.settings_extensions_begin("plugins", &id, /*toggle*/ None);
        let plugin_id = id.clone();
        self.backend.call(
            move |request_id| ClientRequest::PluginUninstall {
                request_id,
                params: PluginUninstallParams { plugin_id },
            },
            move |app, result: Result<PluginUninstallResponse, BackendError>| {
                let error = result
                    .err()
                    .map(|err| format!("Could not uninstall: {}", err.user_message()));
                if error.is_none() {
                    // The server wrote config.toml.
                    app.settings_reload_config();
                    app.toast(format!("Uninstalled {id}"));
                }
                app.settings_extensions_finish("plugins", id, error);
            },
        );
    }

    fn settings_hook_trust(&mut self, key: &str) {
        let Some(hash) = self
            .settings
            .extensions
            .hook_entries
            .iter()
            .flat_map(|entry| &entry.hooks)
            .find(|hook| hook.key == key)
            .map(|hook| hook.current_hash.clone())
        else {
            return;
        };
        let key = key.to_string();
        self.settings_extensions_begin("hooks", &key, /*toggle*/ None);
        let edit = upsert_edit(
            "hooks.state",
            json!({ key.as_str(): { "trusted_hash": hash } }),
        );
        self.settings_write(
            vec![edit],
            Box::new(move |app, result| {
                let error = result.err();
                app.settings_extensions_finish("hooks", key, error);
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn feature_edits_match_the_tui() {
        let edit = feature_edit("x", true, false);
        assert_eq!(edit.key_path, r#"features."x""#);
        assert_eq!(edit.value, json!(true));
        assert_eq!(feature_edit("x", false, false).value, Value::Null);
        assert_eq!(feature_edit("x", false, true).value, json!(false));
    }

    fn feature(name: &str, stage: &str, enabled: bool) -> ExperimentalFeature {
        serde_json::from_value(json!({
            "name": name,
            "stage": stage,
            "displayName": null,
            "description": "Does things.",
            "announcement": null,
            "enabled": enabled,
            "defaultEnabled": false,
        }))
        .expect("feature")
    }

    #[test]
    fn features_group_by_stage_and_respect_requirements() {
        let features = vec![
            feature("b", "beta", true),
            feature("gone", "removed", false),
            feature("a", "underDevelopment", false),
        ];
        let mut locked = HashMap::new();
        locked.insert("a".to_string(), false);
        let rows = feature_rows(&features, &locked, &PageMeta::default(), true);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["§Beta", "b", "§In development", "a"]);
        assert_eq!(rows[1].toggle, Some(true));
        assert!(rows[1].toggle_enabled);
        assert_eq!(rows[3].tag, "Managed");
        assert!(!rows[3].toggle_enabled);
        assert_eq!(
            rows[3].detail,
            "features.a · default off · managed: always off"
        );
    }

    #[test]
    fn hooks_offer_trust_for_untrusted_user_hooks() {
        let entries: Vec<HooksListEntry> = serde_json::from_value(json!([{
            "cwd": "/r",
            "hooks": [{
                "key": "k1",
                "eventName": "preToolUse",
                "handlerType": "command",
                "command": "lint.sh",
                "matcher": "shell",
                "timeoutSec": 30,
                "statusMessage": null,
                "sourcePath": abs("/h/config.toml"),
                "source": "user",
                "pluginId": null,
                "displayOrder": 0,
                "enabled": true,
                "isManaged": false,
                "currentHash": "h",
                "trustStatus": "untrusted"
            }],
            "warnings": ["slow hook"],
            "errors": []
        }]))
        .expect("hooks");
        let rows = hook_rows(&entries, &PageMeta::default(), true);
        assert_eq!(rows[0].title, "PreToolUse · shell");
        assert_eq!(rows[0].subtitle, "lint.sh");
        assert_eq!(rows[0].status, "Not trusted");
        assert_eq!(
            rows[0].action,
            Some(("Trust".to_string(), "trust".to_string()))
        );
        assert_eq!(rows[0].tag, "User");
        assert_eq!(rows[1].id, "§problems");
        assert_eq!(rows[2].description, "slow hook");
    }

    fn abs(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", path.replace('/', "\\"))
        } else {
            path.to_string()
        }
    }

    #[test]
    fn skills_group_by_scope() {
        let entries: Vec<SkillsListEntry> = serde_json::from_value(json!([{
            "cwd": "/r",
            "skills": [
                {"name": "zeta", "description": "Z.", "path": abs("/h/skills/zeta/SKILL.md"), "scope": "user", "enabled": true},
                {"name": "alpha", "description": "A.", "path": abs("/r/.codex/skills/alpha/SKILL.md"), "scope": "repo", "enabled": false}
            ],
            "errors": [{"path": "/h/skills/bad/SKILL.md", "message": "missing name"}]
        }]))
        .expect("skills");
        let rows = skill_rows(&entries, &PageMeta::default(), true);
        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Project skills",
                "alpha",
                "Your skills",
                "zeta",
                "Skills that could not be loaded",
                "SKILL.md"
            ]
        );
        assert_eq!(rows[1].toggle, Some(false));
        assert_eq!(rows[5].error, "missing name");
    }

    #[test]
    fn switches_show_values_being_saved_until_the_list_has_them() {
        let features = vec![feature("b", "beta", false)];
        let mut meta = PageMeta::default();
        meta.pending.begin("b", true);
        let toggle =
            |meta: &PageMeta| feature_rows(&features, &HashMap::new(), meta, true)[1].toggle;
        assert_eq!(toggle(&meta), Some(true));
        // Saved while list 3 was current: list 3 does not have it yet.
        meta.pending.saved("b", 3);
        meta.pending.settle(3);
        assert_eq!(toggle(&meta), Some(true));
        meta.pending.settle(4);
        assert_eq!(toggle(&meta), Some(false));
    }
}
