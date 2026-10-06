//! Settings › Import: bring setup over from other coding agents.
//!
//! Mirrors the TUI's `/import` flow: `externalAgentConfig/detect` runs once
//! per migration source (Claude Code, Cursor) for the home folder and the
//! open thread folders, the user picks items, and
//! `externalAgentConfig/import` starts the import. The server answers with
//! an import id right away and reports per-type results through
//! `externalAgentConfig/import/progress` and `.../completed`.
//! `externalAgentConfig/import/readHistories` lists finished imports from
//! every client.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ExternalAgentConfigDetectParams;
use codex_app_server_protocol::ExternalAgentConfigDetectResponse;
use codex_app_server_protocol::ExternalAgentConfigImportCompletedNotification;
use codex_app_server_protocol::ExternalAgentConfigImportHistoriesReadResponse;
use codex_app_server_protocol::ExternalAgentConfigImportHistory;
use codex_app_server_protocol::ExternalAgentConfigImportParams;
use codex_app_server_protocol::ExternalAgentConfigImportProgressNotification;
use codex_app_server_protocol::ExternalAgentConfigImportResponse;
use codex_app_server_protocol::ExternalAgentConfigImportTypeResult;
use codex_app_server_protocol::ExternalAgentConfigMigrationItem;
use codex_app_server_protocol::ExternalAgentConfigMigrationItemType as ItemType;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use super::ItemList;
use super::ListItem;
use super::LoadState;
use super::tone;
use crate::app::AppController;
use crate::backend::BackendError;
use crate::ui::SettingsState;

/// Migration sources the app-server understands: (selector, label).
const SOURCES: &[(&str, &str)] = &[("claude-code", "Claude Code"), ("cursor", "Cursor")];

/// Identifies this app in import analytics.
const IMPORT_CLIENT: &str = "codex-gui";

/// Notifications kept while the import response is still in flight.
const MAX_EARLY_NOTIFICATIONS: usize = 32;

/// Most past imports listed.
const MAX_HISTORY_ROWS: usize = 20;

const ITEM_PREFIX: &str = "item:";

/// Items one source offers.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DetectedSource {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) items: Vec<ExternalAgentConfigMigrationItem>,
}

/// A group of items shown under one heading.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ItemGroup {
    pub(crate) label: String,
    pub(crate) description: &'static str,
    pub(crate) indices: Vec<usize>,
}

/// Groups items like the TUI: home-level setup, per-project setup, then
/// chat sessions.
pub(crate) fn item_groups(items: &[ExternalAgentConfigMigrationItem]) -> Vec<ItemGroup> {
    let is_sessions =
        |item: &ExternalAgentConfigMigrationItem| item.item_type == ItemType::Sessions;
    let has_cwd = |item: &ExternalAgentConfigMigrationItem| {
        item.cwd
            .as_ref()
            .is_some_and(|cwd| !cwd.as_os_str().is_empty())
    };
    let indices = |keep: &dyn Fn(&ExternalAgentConfigMigrationItem) -> bool| -> Vec<usize> {
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| keep(item))
            .map(|(index, _)| index)
            .collect()
    };
    let setup = indices(&|item| !is_sessions(item) && !has_cwd(item));
    let projects = indices(&|item| !is_sessions(item) && has_cwd(item));
    let sessions = indices(&is_sessions);

    let mut groups = Vec::new();
    if !setup.is_empty() {
        groups.push(ItemGroup {
            label: "Tools and setup".to_string(),
            description: "Settings, instructions, integrations, agents, commands, and skills from your home folder.",
            indices: setup,
        });
    }
    if !projects.is_empty() {
        let folders: BTreeSet<&PathBuf> = projects
            .iter()
            .filter_map(|index| items[*index].cwd.as_ref())
            .collect();
        groups.push(ItemGroup {
            label: if folders.len() == 1 {
                "Project".to_string()
            } else {
                format!("Projects ({})", folders.len())
            },
            description: "Codex files added next to your existing project files.",
            indices: projects,
        });
    }
    if !sessions.is_empty() {
        let count: usize = sessions
            .iter()
            .map(|index| item_count(&items[*index]))
            .sum();
        groups.push(ItemGroup {
            label: format!("Chat sessions ({count})"),
            description: "Recent chats, imported as Codex threads.",
            indices: sessions,
        });
    }
    groups
}

/// User-facing name of an item type.
pub(crate) fn type_label(item_type: ItemType) -> &'static str {
    match item_type {
        ItemType::AgentsMd => "Instructions",
        ItemType::Config => "Settings",
        ItemType::Skills => "Skills",
        ItemType::Plugins => "Plugins",
        ItemType::McpServerConfig => "MCP servers",
        ItemType::Subagents => "Agents",
        ItemType::Hooks => "Hooks",
        ItemType::Commands => "Slash commands",
        ItemType::Memory => "Memory",
        ItemType::Sessions => "Chat sessions",
    }
}

/// How many objects an item imports (skills, servers, sessions, ...).
pub(crate) fn item_count(item: &ExternalAgentConfigMigrationItem) -> usize {
    let Some(details) = item.details.as_ref() else {
        return usize::from(item.item_type != ItemType::Memory);
    };
    match item.item_type {
        ItemType::Plugins => details
            .plugins
            .iter()
            .map(|group| group.plugin_names.len())
            .sum(),
        ItemType::McpServerConfig => details.mcp_servers.len(),
        ItemType::Subagents => details.subagents.len(),
        ItemType::Hooks => details.hooks.len(),
        ItemType::Commands => details.commands.len(),
        ItemType::Memory => details.memory.len(),
        ItemType::Sessions => details.sessions.len(),
        ItemType::Skills => details.skills.len(),
        ItemType::AgentsMd | ItemType::Config => 1,
    }
}

/// Names of the objects an item imports, for its detail line.
fn item_names(item: &ExternalAgentConfigMigrationItem) -> Vec<&str> {
    let Some(details) = item.details.as_ref() else {
        return Vec::new();
    };
    match item.item_type {
        ItemType::Plugins => details
            .plugins
            .iter()
            .flat_map(|group| group.plugin_names.iter().map(String::as_str))
            .collect(),
        ItemType::Skills => details
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect(),
        ItemType::McpServerConfig => details
            .mcp_servers
            .iter()
            .map(|server| server.name.as_str())
            .collect(),
        ItemType::Subagents => details
            .subagents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect(),
        ItemType::Hooks => details
            .hooks
            .iter()
            .map(|hook| hook.name.as_str())
            .collect(),
        ItemType::Commands => details
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect(),
        ItemType::Memory => details.memory.iter().map(String::as_str).collect(),
        ItemType::Sessions => details
            .sessions
            .iter()
            .filter_map(|session| session.title.as_deref())
            .collect(),
        ItemType::AgentsMd | ItemType::Config => Vec::new(),
    }
}

/// "3 skills: a, b, c, +1 more".
fn item_detail(item: &ExternalAgentConfigMigrationItem) -> String {
    const SHOWN: usize = 4;
    let count = item_count(item);
    let noun = match item.item_type {
        ItemType::Plugins => "plugin",
        ItemType::Skills => "skill",
        ItemType::McpServerConfig => "MCP server",
        ItemType::Subagents => "agent",
        ItemType::Hooks => "hook",
        ItemType::Commands => "slash command",
        ItemType::Memory => "memory",
        ItemType::Sessions => "chat",
        ItemType::AgentsMd | ItemType::Config => return String::new(),
    };
    let noun = match (count, noun) {
        (1, noun) => noun.to_string(),
        (_, "memory") => "memories".to_string(),
        (_, noun) => format!("{noun}s"),
    };
    let names = item_names(item);
    if names.is_empty() {
        return format!("{count} {noun}");
    }
    let mut shown = names
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        shown = format!("{shown}, +{} more", names.len() - SHOWN);
    }
    format!("{count} {noun}: {shown}")
}

/// The server's description in import wording ("Migrate X" → "Import X"),
/// with paths inside the item's project shown relative to it and the home
/// folder shown as `~`.
fn item_description(item: &ExternalAgentConfigMigrationItem, home: Option<&Path>) -> String {
    let mut text = match item.description.strip_prefix("Migrate ") {
        Some(rest) => format!("Import {rest}"),
        None => item.description.clone(),
    };
    let separator = std::path::MAIN_SEPARATOR;
    if let Some(cwd) = item
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.as_os_str().is_empty())
    {
        text = text.replace(&format!("{}{separator}", cwd.display()), "");
    }
    if let Some(home) = home.filter(|home| !home.as_os_str().is_empty()) {
        text = text.replace(
            &format!("{}{separator}", home.display()),
            &format!("~{separator}"),
        );
    }
    text
}

/// Detected items and which of them the user selected.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ImportSelection {
    items: Vec<ExternalAgentConfigMigrationItem>,
    selected: Vec<bool>,
}

impl ImportSelection {
    /// Every item starts selected, like the TUI.
    pub(crate) fn new(items: Vec<ExternalAgentConfigMigrationItem>) -> Self {
        let selected = vec![true; items.len()];
        Self { items, selected }
    }

    /// Keeps earlier choices for items that were detected again.
    pub(crate) fn carry_over(&mut self, previous: &ImportSelection) {
        for (index, item) in self.items.iter().enumerate() {
            if let Some(old) = previous.items.iter().position(|old| old == item) {
                self.selected[index] = previous.selected[old];
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn toggle(&mut self, index: usize, on: bool) {
        if let Some(selected) = self.selected.get_mut(index) {
            *selected = on;
        }
    }

    pub(crate) fn select_all(&mut self, on: bool) {
        self.selected.iter_mut().for_each(|selected| *selected = on);
    }

    pub(crate) fn selected_count(&self) -> usize {
        self.selected.iter().filter(|selected| **selected).count()
    }

    pub(crate) fn selected_items(&self) -> Vec<ExternalAgentConfigMigrationItem> {
        self.items
            .iter()
            .zip(&self.selected)
            .filter(|(_, selected)| **selected)
            .map(|(item, _)| item.clone())
            .collect()
    }

    /// Group headers followed by their items. Item ids are `item:<index>`.
    pub(crate) fn rows(&self, enabled: bool, home: Option<&Path>) -> Vec<ListItem> {
        let mut rows = Vec::new();
        for (group_index, group) in item_groups(&self.items).into_iter().enumerate() {
            rows.push(ListItem {
                description: group.description.to_string(),
                ..ListItem::header(format!("group:{group_index}"), group.label)
            });
            for index in group.indices {
                let item = &self.items[index];
                let folder = item
                    .cwd
                    .as_deref()
                    .filter(|cwd| !cwd.as_os_str().is_empty())
                    .map(super::short_path)
                    .unwrap_or_default();
                rows.push(ListItem {
                    id: format!("{ITEM_PREFIX}{index}"),
                    title: type_label(item.item_type).to_string(),
                    subtitle: folder,
                    description: item_description(item, home),
                    detail: item_detail(item),
                    toggle: Some(self.selected[index]),
                    toggle_enabled: enabled,
                    ..ListItem::default()
                });
            }
        }
        rows
    }
}

/// Index from an item row id.
fn item_index(id: &str) -> Option<usize> {
    id.strip_prefix(ITEM_PREFIX)?.parse().ok()
}

/// Folds a progress notification into the results so far.
pub(crate) fn merge_results(
    results: &mut Vec<ExternalAgentConfigImportTypeResult>,
    update: Vec<ExternalAgentConfigImportTypeResult>,
) {
    for result in update {
        match results
            .iter_mut()
            .find(|existing| existing.item_type == result.item_type)
        {
            Some(existing) => {
                existing.successes.extend(result.successes);
                existing.failures.extend(result.failures);
            }
            None => results.push(result),
        }
    }
}

/// (imported, failed) totals.
fn result_totals(results: &[ExternalAgentConfigImportTypeResult]) -> (usize, usize) {
    results.iter().fold((0, 0), |(ok, failed), result| {
        (ok + result.successes.len(), failed + result.failures.len())
    })
}

/// "3 imported · 1 failed".
fn counts_text(imported: usize, failed: usize) -> String {
    match failed {
        0 => format!("{imported} imported"),
        failed => format!("{imported} imported · {failed} failed"),
    }
}

/// One row per item type with counts, targets, and failure messages.
pub(crate) fn result_rows(results: &[ExternalAgentConfigImportTypeResult]) -> Vec<ListItem> {
    results
        .iter()
        .map(|result| {
            let imported = result.successes.len();
            let failed = result.failures.len();
            let targets: Vec<String> = result
                .successes
                .iter()
                .filter_map(|success| success.title.as_deref().or(success.target.as_deref()))
                .take(3)
                .map(|target| {
                    let path = Path::new(target);
                    if path.is_absolute() {
                        super::short_path(path)
                    } else {
                        target.to_string()
                    }
                })
                .collect();
            let errors: Vec<String> = result
                .failures
                .iter()
                .take(3)
                .map(|failure| failure.message.clone())
                .collect();
            ListItem {
                id: format!("result:{}", super::model::wire_name(&result.item_type)),
                title: type_label(result.item_type).to_string(),
                status: counts_text(imported, failed),
                status_tone: match (imported, failed) {
                    (_, 0) => tone::SUCCESS,
                    (0, _) => tone::DANGER,
                    _ => tone::WARNING,
                },
                detail: if targets.is_empty() {
                    String::new()
                } else {
                    targets.join(", ")
                },
                error: errors.join("\n"),
                ..ListItem::default()
            }
        })
        .collect()
}

/// Past imports, newest first.
pub(crate) fn history_rows(
    history: &[ExternalAgentConfigImportHistory],
    now_secs: i64,
) -> Vec<ListItem> {
    let mut history: Vec<&ExternalAgentConfigImportHistory> = history.iter().collect();
    history.sort_by_key(|entry| std::cmp::Reverse(entry.completed_at_ms));
    history
        .into_iter()
        .take(MAX_HISTORY_ROWS)
        .map(|entry| {
            let source = entry
                .provider_id
                .as_deref()
                .map(|provider| {
                    SOURCES
                        .iter()
                        .find(|(id, _)| *id == provider)
                        .map_or_else(|| provider.to_string(), |(_, label)| (*label).to_string())
                })
                .unwrap_or_else(|| "Another agent".to_string());
            let mut types: Vec<ItemType> = Vec::new();
            for item_type in entry
                .successes
                .iter()
                .map(|success| success.item_type)
                .chain(entry.failures.iter().map(|failure| failure.item_type))
            {
                if !types.contains(&item_type) {
                    types.push(item_type);
                }
            }
            let types: Vec<&str> = types.into_iter().map(type_label).collect();
            let imported = entry.successes.len();
            let failed = entry.failures.len();
            let age = crate::sidebar::relative_time(now_secs, entry.completed_at_ms / 1000);
            ListItem {
                id: entry.import_id.clone(),
                title: format!("From {source}"),
                status: counts_text(imported, failed),
                status_tone: if failed == 0 {
                    tone::SUCCESS
                } else {
                    tone::WARNING
                },
                tag: if age == "now" {
                    "just now".to_string()
                } else {
                    format!("{age} ago")
                },
                description: types.join(", "),
                ..ListItem::default()
            }
        })
        .collect()
}

/// An import the server accepted (or is about to accept).
#[derive(Clone, Debug, Default)]
struct RunningImport {
    /// Known once `externalAgentConfig/import` returns.
    import_id: Option<String>,
    source_label: String,
    requested: usize,
    results: Vec<ExternalAgentConfigImportTypeResult>,
    /// Sessions were part of the import (the thread list changes).
    has_sessions: bool,
}

/// Notifications received before the import id was known.
#[derive(Clone, Debug)]
enum EarlyNotification {
    Progress(ExternalAgentConfigImportProgressNotification),
    Completed(ExternalAgentConfigImportCompletedNotification),
}

#[derive(Default)]
pub(crate) struct ImportState {
    load: LoadState,
    detect_generation: u64,
    sources: Vec<DetectedSource>,
    source_index: usize,
    selection: ImportSelection,
    status: String,
    running: Option<RunningImport>,
    early: Vec<EarlyNotification>,
    /// (text, tone) of the banner about the last import.
    banner: Option<(String, i32)>,
    results: Vec<ExternalAgentConfigImportTypeResult>,
    history: Vec<ExternalAgentConfigImportHistory>,
    history_status: String,
    revision: i32,
    pub(crate) items: ItemList,
    pub(crate) result_list: ItemList,
    pub(crate) history_list: ItemList,
}

impl ImportState {
    pub(super) fn invalidate(&mut self) {
        self.load = LoadState::Stale;
    }
}

impl AppController {
    pub(super) fn settings_import_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.set_import_items(self.settings.import.items.model_rc());
        state.set_import_results(self.settings.import.result_list.model_rc());
        state.set_import_history(self.settings.import.history_list.model_rc());
        state.on_import_detect(|| {
            crate::ui_thread::with_app(|app| app.settings_import_detect(/*force*/ true));
        });
        state.on_import_source_selected(|index| {
            crate::ui_thread::with_app(move |app| {
                if let Ok(index) = usize::try_from(index) {
                    app.settings_import_select_source(index);
                }
            });
        });
        state.on_import_toggle(|id, on| {
            let id = id.to_string();
            crate::ui_thread::with_app(move |app| {
                if let Some(index) = item_index(&id) {
                    app.settings.import.selection.toggle(index, on);
                    app.settings_import_refresh();
                }
            });
        });
        state.on_import_select_all(|on| {
            crate::ui_thread::with_app(move |app| {
                app.settings.import.selection.select_all(on);
                app.settings_import_refresh();
            });
        });
        state.on_import_start(|| crate::ui_thread::with_app(AppController::settings_import_start));
    }

    /// The server restarted or stopped: a running import will never report
    /// back, and detection results may be stale.
    pub(super) fn settings_import_on_server_reset(&mut self) {
        let import = &mut self.settings.import;
        import.invalidate();
        import.detect_generation += 1;
        import.early.clear();
        if let Some(running) = import.running.take() {
            import.banner = Some((
                format!(
                    "Codex stopped before the import from {} finished. Check again to see what is left to import.",
                    running.source_label
                ),
                tone::WARNING,
            ));
        }
        // `settings_on_server_ready` reactivates the visible page.
        self.settings_import_refresh();
    }

    pub(super) fn settings_import_activate(&mut self) {
        self.settings_import_detect(/*force*/ false);
        self.settings_import_load_history();
        self.settings_import_refresh();
    }

    /// Folders checked for project-level setup: open thread folders and the
    /// folder chosen on the Diagnostics page.
    fn settings_import_folders(&self) -> Vec<PathBuf> {
        let mut folders: Vec<PathBuf> = Vec::new();
        let open = self
            .tabs
            .iter()
            .filter_map(|tab| tab.thread().map(|thread| thread.cwd.clone()));
        for folder in open.chain(self.settings_context_cwd()) {
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        folders
    }

    /// Runs detection for every source unless results are current.
    fn settings_import_detect(&mut self, force: bool) {
        let import = &mut self.settings.import;
        if import.running.is_some()
            || self.settings.server_error.is_some()
            || (!force && import.load != LoadState::Stale)
        {
            return;
        }
        import.load = LoadState::Loading;
        import.detect_generation += 1;
        let generation = import.detect_generation;
        import.status = "Looking for setup from other agents…".to_string();
        let folders = self.settings_import_folders();
        let cwds = (!folders.is_empty()).then_some(folders);
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let mut sources = Vec::new();
            let mut errors = Vec::new();
            for (id, label) in SOURCES {
                let request = ClientRequest::ExternalAgentConfigDetect {
                    request_id: backend.next_request_id(),
                    params: ExternalAgentConfigDetectParams {
                        include_home: true,
                        cwds: cwds.clone(),
                        max_session_age_days: None,
                        max_sessions: None,
                        source: None,
                        migration_source: Some((*id).to_string()),
                    },
                };
                match backend
                    .request::<ExternalAgentConfigDetectResponse>(request)
                    .await
                {
                    Ok(response) if !response.items.is_empty() => sources.push(DetectedSource {
                        id,
                        label,
                        items: response.items,
                    }),
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(%err, source = *id, "externalAgentConfig/detect failed");
                        errors.push(format!("{label}: {}", err.user_message()));
                    }
                }
            }
            crate::ui_thread::post(move |app| {
                app.settings_import_detected(generation, sources, errors);
            });
        });
        self.settings_import_refresh();
    }

    fn settings_import_detected(
        &mut self,
        generation: u64,
        sources: Vec<DetectedSource>,
        errors: Vec<String>,
    ) {
        let import = &mut self.settings.import;
        if import.detect_generation != generation {
            return;
        }
        import.load = LoadState::Loaded;
        let previous_source = import
            .sources
            .get(import.source_index)
            .map(|source| source.id);
        let previous_selection = std::mem::take(&mut import.selection);
        import.source_index = previous_source
            .and_then(|id| sources.iter().position(|source| source.id == id))
            .unwrap_or(0);
        import.sources = sources;
        import.selection = import
            .sources
            .get(import.source_index)
            .map(|source| ImportSelection::new(source.items.clone()))
            .unwrap_or_default();
        if previous_source
            == import
                .sources
                .get(import.source_index)
                .map(|source| source.id)
        {
            import.selection.carry_over(&previous_selection);
        }
        import.status = match (import.sources.is_empty(), errors.is_empty()) {
            (true, true) => {
                "Nothing to import: no setup from Claude Code or Cursor was found in your home folder or the open project folders.".to_string()
            }
            (true, false) => format!("Could not check for setup to import. {}", errors.join(" ")),
            (false, true) => String::new(),
            (false, false) => format!("Some agents could not be checked. {}", errors.join(" ")),
        };
        self.settings_import_refresh();
    }

    fn settings_import_select_source(&mut self, index: usize) {
        let import = &mut self.settings.import;
        if import.running.is_some() || index == import.source_index {
            return;
        }
        let Some(source) = import.sources.get(index) else {
            return;
        };
        import.source_index = index;
        import.selection = ImportSelection::new(source.items.clone());
        self.settings_import_refresh();
    }

    fn settings_import_load_history(&mut self) {
        if self.settings.server_error.is_some() {
            return;
        }
        self.backend.call(
            |request_id| ClientRequest::ExternalAgentConfigImportHistoriesRead {
                request_id,
                params: None,
            },
            |app, result: Result<ExternalAgentConfigImportHistoriesReadResponse, BackendError>| {
                let import = &mut app.settings.import;
                match result {
                    Ok(response) => {
                        import.history = response.data;
                        import.history_status = String::new();
                    }
                    Err(err) => {
                        tracing::warn!(%err, "externalAgentConfig/import/readHistories failed");
                        import.history_status =
                            format!("Past imports are unavailable: {}", err.user_message());
                    }
                }
                app.settings_import_refresh();
            },
        );
    }

    fn settings_import_start(&mut self) {
        let import = &mut self.settings.import;
        if import.running.is_some() {
            return;
        }
        let Some(source) = import.sources.get(import.source_index) else {
            return;
        };
        let items = import.selection.selected_items();
        if items.is_empty() {
            return;
        }
        let source_id = source.id;
        import.running = Some(RunningImport {
            import_id: None,
            source_label: source.label.to_string(),
            requested: items.len(),
            results: Vec::new(),
            has_sessions: items
                .iter()
                .any(|item| item.item_type == ItemType::Sessions),
        });
        import.early.clear();
        import.results.clear();
        import.banner = None;
        self.backend.call(
            move |request_id| ClientRequest::ExternalAgentConfigImport {
                request_id,
                params: ExternalAgentConfigImportParams {
                    migration_items: items,
                    source: Some(IMPORT_CLIENT.to_string()),
                    provider_id: Some(source_id.to_string()),
                    migration_source: Some(source_id.to_string()),
                },
            },
            |app, result: Result<ExternalAgentConfigImportResponse, BackendError>| {
                app.settings_import_started(result);
            },
        );
        self.settings_import_refresh();
    }

    fn settings_import_started(
        &mut self,
        result: Result<ExternalAgentConfigImportResponse, BackendError>,
    ) {
        let import = &mut self.settings.import;
        let Some(running) = import.running.as_mut() else {
            return;
        };
        match result {
            Ok(response) => {
                running.import_id = Some(response.import_id);
                // Apply anything that raced ahead of the response.
                for early in std::mem::take(&mut import.early) {
                    match early {
                        EarlyNotification::Progress(progress) => {
                            self.settings_import_on_progress(&progress);
                        }
                        EarlyNotification::Completed(completed) => {
                            self.settings_import_on_completed(&completed);
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(%err, "externalAgentConfig/import failed");
                import.running = None;
                import.early.clear();
                import.banner = Some((
                    format!("Import failed: {}", err.user_message()),
                    tone::DANGER,
                ));
            }
        }
        self.settings_import_refresh();
    }

    /// Whether `import_id` belongs to the running import; buffers the
    /// notification when the id is not known yet.
    fn settings_import_claims(
        &mut self,
        import_id: &str,
        early: impl FnOnce() -> EarlyNotification,
    ) -> bool {
        let import = &mut self.settings.import;
        match import.running.as_ref() {
            Some(running) => match running.import_id.as_deref() {
                Some(id) => id == import_id,
                None => {
                    if import.early.len() < MAX_EARLY_NOTIFICATIONS {
                        import.early.push(early());
                    }
                    false
                }
            },
            None => false,
        }
    }

    pub(super) fn settings_import_on_progress(
        &mut self,
        progress: &ExternalAgentConfigImportProgressNotification,
    ) {
        if !self.settings_import_claims(&progress.import_id, || {
            EarlyNotification::Progress(progress.clone())
        }) {
            return;
        }
        if let Some(running) = self.settings.import.running.as_mut() {
            merge_results(&mut running.results, progress.item_type_results.clone());
        }
        self.settings_import_refresh();
    }

    pub(super) fn settings_import_on_completed(
        &mut self,
        completed: &ExternalAgentConfigImportCompletedNotification,
    ) {
        if !self.settings_import_claims(&completed.import_id, || {
            EarlyNotification::Completed(completed.clone())
        }) {
            // Another client's import (shared daemon) still changes history.
            if self.settings.import.running.is_none() {
                self.settings.import.invalidate();
                self.settings_import_load_history();
            }
            return;
        }
        let import = &mut self.settings.import;
        let Some(running) = import.running.take() else {
            return;
        };
        import.results = completed.item_type_results.clone();
        let (imported, failed) = result_totals(&import.results);
        let tone = match (imported, failed) {
            (_, 0) => tone::SUCCESS,
            (0, _) => tone::DANGER,
            _ => tone::WARNING,
        };
        import.banner = Some((
            format!(
                "Import from {} finished: {}. Imported setup applies to new threads.",
                running.source_label,
                counts_text(imported, failed)
            ),
            tone,
        ));
        import.invalidate();
        // Imports change config.toml, skills, plugins, MCP servers, and hooks.
        self.settings_invalidate_pages();
        self.settings_reload_config();
        if running.has_sessions {
            self.sidebar_refresh();
        }
        self.settings_import_load_history();
        if self.settings_tab_active() && self.settings.page == "import" {
            self.settings_import_detect(/*force*/ false);
        }
        self.settings_import_refresh();
    }

    pub(super) fn settings_import_refresh(&mut self) {
        let server_ready = self.settings.server_error.is_none() && self.backend.is_ready();
        let import = &mut self.settings.import;
        let running = import.running.is_some();
        let mut revision = import.revision;
        import.items.sync_items(
            import.selection.rows(!running, dirs::home_dir().as_deref()),
            &HashSet::new(),
            &mut revision,
        );
        let results = match import.running.as_ref() {
            Some(running) => result_rows(&running.results),
            None => result_rows(&import.results),
        };
        import
            .result_list
            .sync_items(results, &HashSet::new(), &mut revision);
        import.history_list.sync_items(
            history_rows(&import.history, crate::sidebar::unix_now()),
            &HashSet::new(),
            &mut revision,
        );
        import.revision = revision;

        let state = self.window.global::<SettingsState>();
        let labels: Vec<SharedString> = import
            .sources
            .iter()
            .map(|source| {
                let count = source.items.len();
                let noun = if count == 1 { "item" } else { "items" };
                SharedString::from(format!("{} ({count} {noun})", source.label))
            })
            .collect();
        state.set_import_sources(ModelRc::new(VecModel::from(labels)));
        state.set_import_source_index(i32::try_from(import.source_index).unwrap_or(0));
        state.set_import_detecting(import.load == LoadState::Loading);
        let status = if !server_ready && import.sources.is_empty() {
            String::new()
        } else {
            import.status.clone()
        };
        state.set_import_status(status.into());
        state.set_import_selected(i32::try_from(import.selection.selected_count()).unwrap_or(0));
        state.set_import_total(i32::try_from(import.selection.len()).unwrap_or(0));
        state.set_import_running(running);
        let (banner, banner_tone) = match import.running.as_ref() {
            Some(running) => (
                format!(
                    "Importing {} item{} from {}… You can keep working while it finishes.",
                    running.requested,
                    if running.requested == 1 { "" } else { "s" },
                    running.source_label
                ),
                tone::NEUTRAL,
            ),
            None => import.banner.clone().unwrap_or_default(),
        };
        state.set_import_run_status(banner.into());
        state.set_import_run_tone(banner_tone);
        state.set_import_history_status(import.history_status.clone().into());
    }

    /// Scripted input for UI automation: `["toggle", index, "true"]`,
    /// `["all", "false"]`, `["source", index]`, `["start"]`, `["detect"]`.
    pub(super) fn settings_import_automation(&mut self, args: &[String]) {
        let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
        let state = self.window.global::<SettingsState>();
        match arg(0) {
            "toggle" => state
                .invoke_import_toggle(format!("{ITEM_PREFIX}{}", arg(1)).into(), arg(2) != "false"),
            "all" => state.invoke_import_select_all(arg(1) != "false"),
            "source" => state.invoke_import_source_selected(arg(1).parse().unwrap_or(0)),
            "start" => state.invoke_import_start(),
            "detect" => state.invoke_import_detect(),
            other => tracing::warn!(action = other, "unknown import automation action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ExternalAgentConfigImportItemTypeFailure;
    use codex_app_server_protocol::ExternalAgentConfigImportItemTypeSuccess;
    use codex_app_server_protocol::MigrationDetails;
    use codex_app_server_protocol::SessionMigration;
    use codex_app_server_protocol::SkillMigration;
    use pretty_assertions::assert_eq;

    fn item(item_type: ItemType, cwd: Option<&str>) -> ExternalAgentConfigMigrationItem {
        ExternalAgentConfigMigrationItem {
            item_type,
            description: format!("Migrate {}", type_label(item_type)),
            cwd: cwd.map(PathBuf::from),
            details: None,
        }
    }

    fn skills(names: &[&str]) -> ExternalAgentConfigMigrationItem {
        ExternalAgentConfigMigrationItem {
            details: Some(MigrationDetails {
                skills: names
                    .iter()
                    .map(|name| SkillMigration {
                        name: (*name).to_string(),
                    })
                    .collect(),
                ..MigrationDetails::default()
            }),
            ..item(ItemType::Skills, None)
        }
    }

    fn sessions(count: usize) -> ExternalAgentConfigMigrationItem {
        ExternalAgentConfigMigrationItem {
            details: Some(MigrationDetails {
                sessions: (0..count)
                    .map(|index| SessionMigration {
                        path: PathBuf::from(format!("/s/{index}.jsonl")),
                        cwd: PathBuf::from("/p"),
                        title: Some(format!("chat {index}")),
                    })
                    .collect(),
                ..MigrationDetails::default()
            }),
            ..item(ItemType::Sessions, None)
        }
    }

    #[test]
    fn items_group_by_scope() {
        let items = vec![
            item(ItemType::Config, None),
            item(ItemType::AgentsMd, Some("/repo-a")),
            sessions(3),
            skills(&["a"]),
            item(ItemType::AgentsMd, Some("/repo-b")),
            item(ItemType::Commands, Some("")),
        ];
        let groups = item_groups(&items);
        let summary: Vec<(String, Vec<usize>)> = groups
            .into_iter()
            .map(|group| (group.label, group.indices))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Tools and setup".to_string(), vec![0, 3, 5]),
                ("Projects (2)".to_string(), vec![1, 4]),
                ("Chat sessions (3)".to_string(), vec![2]),
            ]
        );
    }

    #[test]
    fn selection_starts_full_and_tracks_toggles() {
        let mut selection = ImportSelection::new(vec![
            item(ItemType::Config, None),
            skills(&["a", "b"]),
            sessions(2),
        ]);
        assert_eq!(selection.selected_count(), 3);
        selection.toggle(1, /*on*/ false);
        selection.toggle(9, /*on*/ false);
        assert_eq!(selection.selected_count(), 2);
        let types: Vec<ItemType> = selection
            .selected_items()
            .iter()
            .map(|item| item.item_type)
            .collect();
        assert_eq!(types, vec![ItemType::Config, ItemType::Sessions]);
        selection.select_all(/*on*/ false);
        assert!(selection.selected_items().is_empty());
        selection.select_all(/*on*/ true);
        assert_eq!(selection.selected_count(), 3);
    }

    #[test]
    fn selection_survives_detecting_again() {
        let mut old = ImportSelection::new(vec![item(ItemType::Config, None), skills(&["a"])]);
        old.toggle(0, /*on*/ false);
        let mut new = ImportSelection::new(vec![
            skills(&["a"]),
            item(ItemType::Config, None),
            item(ItemType::Hooks, None),
        ]);
        new.carry_over(&old);
        assert_eq!(new.selected, vec![true, false, true]);
    }

    #[test]
    fn rows_list_headers_then_items() {
        let selection = ImportSelection::new(vec![skills(&["a", "b", "c", "d", "e"]), sessions(1)]);
        let rows = selection.rows(/*enabled*/ true, /*home*/ None);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["group:0", "item:0", "group:1", "item:1"]);
        assert!(rows[0].header);
        assert_eq!(rows[1].title, "Skills");
        assert_eq!(rows[1].description, "Import Skills");
        assert_eq!(rows[1].detail, "5 skills: a, b, c, d, +1 more");
        assert_eq!(rows[1].toggle, Some(true));
        assert_eq!(rows[3].detail, "1 chat: chat 0");
        assert_eq!(item_index("item:12"), Some(12));
        assert_eq!(item_index("group:1"), None);
    }

    fn success(item_type: ItemType, target: &str) -> ExternalAgentConfigImportItemTypeSuccess {
        ExternalAgentConfigImportItemTypeSuccess {
            item_type,
            cwd: None,
            source: None,
            target: Some(target.to_string()),
            title: None,
        }
    }

    fn failure(item_type: ItemType, message: &str) -> ExternalAgentConfigImportItemTypeFailure {
        ExternalAgentConfigImportItemTypeFailure {
            item_type,
            error_type: None,
            sub_error_type: None,
            failure_stage: "import".to_string(),
            message: message.to_string(),
            cwd: None,
            source: None,
        }
    }

    #[test]
    fn progress_merges_per_type_and_rows_report_counts() {
        let mut results = Vec::new();
        merge_results(
            &mut results,
            vec![ExternalAgentConfigImportTypeResult {
                item_type: ItemType::Skills,
                successes: vec![success(ItemType::Skills, "skills/a")],
                failures: Vec::new(),
            }],
        );
        merge_results(
            &mut results,
            vec![
                ExternalAgentConfigImportTypeResult {
                    item_type: ItemType::Skills,
                    successes: Vec::new(),
                    failures: vec![failure(ItemType::Skills, "b exists")],
                },
                ExternalAgentConfigImportTypeResult {
                    item_type: ItemType::Config,
                    successes: vec![success(ItemType::Config, "config.toml")],
                    failures: Vec::new(),
                },
            ],
        );
        assert_eq!(result_totals(&results), (2, 1));
        let rows = result_rows(&results);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Skills");
        assert_eq!(rows[0].status, "1 imported · 1 failed");
        assert_eq!(rows[0].status_tone, tone::WARNING);
        assert_eq!(rows[0].detail, "skills/a");
        assert_eq!(rows[0].error, "b exists");
        assert_eq!(rows[1].status, "1 imported");
        assert_eq!(rows[1].status_tone, tone::SUCCESS);
    }

    #[test]
    fn history_lists_newest_first_with_source_names() {
        let entry =
            |id: &str, provider: Option<&str>, at_secs: i64| ExternalAgentConfigImportHistory {
                import_id: id.to_string(),
                provider_id: provider.map(str::to_string),
                completed_at_ms: at_secs * 1000,
                successes: vec![success(ItemType::Config, "config.toml")],
                failures: Vec::new(),
            };
        let rows = history_rows(
            &[
                entry("old", Some("cursor"), 1_000),
                entry("new", Some("claude-code"), 9_000),
                entry("other", None, 5_000),
            ],
            10_000,
        );
        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["From Claude Code", "From Another agent", "From Cursor"]
        );
        assert_eq!(rows[0].tag, "16m ago");
        assert_eq!(rows[0].description, "Settings");
        assert_eq!(rows[0].status, "1 imported");
    }

    #[test]
    fn descriptions_shorten_project_and_home_paths() {
        let sep = std::path::MAIN_SEPARATOR;
        let home = PathBuf::from(format!("{sep}home{sep}me"));
        let project = home.join("repo");
        let project_item = ExternalAgentConfigMigrationItem {
            description: format!(
                "Migrate {} to {}",
                project.join("CLAUDE.md").display(),
                project.join("AGENTS.md").display()
            ),
            cwd: Some(project),
            ..item(ItemType::AgentsMd, None)
        };
        assert_eq!(
            item_description(&project_item, Some(&home)),
            "Import CLAUDE.md to AGENTS.md"
        );
        let home_item = ExternalAgentConfigMigrationItem {
            description: format!(
                "Migrate {} into {}",
                home.join(".claude").join("settings.json").display(),
                home.join(".codex").join("config.toml").display()
            ),
            ..item(ItemType::Config, None)
        };
        assert_eq!(
            item_description(&home_item, Some(&home)),
            format!("Import ~{sep}.claude{sep}settings.json into ~{sep}.codex{sep}config.toml")
        );
    }

    #[test]
    fn item_counts_follow_details() {
        assert_eq!(item_count(&item(ItemType::Config, None)), 1);
        assert_eq!(item_count(&item(ItemType::Memory, None)), 0);
        assert_eq!(item_count(&skills(&["a", "b"])), 2);
        assert_eq!(item_detail(&item(ItemType::Config, None)), "");
        assert_eq!(item_detail(&skills(&["a"])), "1 skill: a");
    }
}
