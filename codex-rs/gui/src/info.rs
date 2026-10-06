//! Info pane for the active thread tab.
//!
//! Per-thread data ([`ThreadInfo`]) is collected from notifications whether
//! or not its tab is visible; only the active tab is rendered into
//! `InfoState`. Data that needs a request (queued messages, goal, existing
//! sub-agents) is fetched the first time the pane shows a thread, and the
//! account-wide pieces (rate limits, app-scoped MCP status) live in
//! [`InfoShared`].

mod terminals;

use std::collections::BTreeMap;
use std::rc::Rc;

use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CollabAgentState;
use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::CollabAgentTool;
use codex_app_server_protocol::GetAccountRateLimitsParams;
use codex_app_server_protocol::GetAccountRateLimitsResponse;
use codex_app_server_protocol::HookExecutionMode;
use codex_app_server_protocol::HookRunSummary;
use codex_app_server_protocol::ListMcpServerStatusParams;
use codex_app_server_protocol::ListMcpServerStatusResponse;
use codex_app_server_protocol::McpServerConnectionStatus;
use codex_app_server_protocol::McpServerStartupState;
use codex_app_server_protocol::McpServerStatus;
use codex_app_server_protocol::McpServerStatusDetail;
use codex_app_server_protocol::QueuedSubmission;
use codex_app_server_protocol::RateLimitSnapshot;
use codex_app_server_protocol::RateLimitWindow;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::SubAgentActivityKind;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadGoalClearParams;
use codex_app_server_protocol::ThreadGoalClearResponse;
use codex_app_server_protocol::ThreadGoalGetParams;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalMutationOrigin;
use codex_app_server_protocol::ThreadGoalSetParams;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadQueueDeleteParams;
use codex_app_server_protocol::ThreadQueueDeleteResponse;
use codex_app_server_protocol::ThreadQueueListParams;
use codex_app_server_protocol::ThreadQueueListResponse;
use codex_app_server_protocol::ThreadQueueReorderParams;
use codex_app_server_protocol::ThreadQueueReorderResponse;
use codex_app_server_protocol::ThreadQueueStartParams;
use codex_app_server_protocol::ThreadQueueStartResponse;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::TurnPlanStep;
use codex_app_server_protocol::TurnPlanStepStatus;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::VecModel;

use crate::app::AppController;
use crate::app::DialogRequest;
use crate::app::ThreadTab;
use crate::backend::BackendError;
use crate::sidebar::sync_model;
use crate::ui::InfoAgent;
use crate::ui::InfoAgentStatus;
use crate::ui::InfoDiffFile;
use crate::ui::InfoField;
use crate::ui::InfoHook;
use crate::ui::InfoLimit;
use crate::ui::InfoMail;
use crate::ui::InfoMcp;
use crate::ui::InfoMcpStatus;
use crate::ui::InfoPlanStep;
use crate::ui::InfoQueued;
use crate::ui::InfoState;
use crate::ui::InfoStepStatus;
use crate::ui::InfoTerminal;

use self::terminals::Terminals;

/// Changed files listed before "and N more".
const MAX_DIFF_FILES: usize = 20;
/// Cross-tab messages listed (newest kept).
const MAX_MAILBOX_ENTRIES: usize = 20;
const MAIL_PREVIEW_CHARS: usize = 160;
const QUEUE_PAGE_LIMIT: u32 = 100;
const SUB_AGENT_PAGE_LIMIT: u32 = 50;
const MCP_STATUS_PAGE_LIMIT: u32 = 100;
/// Context the model always uses (system prompt, tools); matches the TUI so
/// both report the same "% left".
const CONTEXT_BASELINE_TOKENS: i64 = 12_000;
const GOALS_DISABLED_MESSAGE: &str = "goals feature is disabled";

/// Progress of a one-shot fetch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Fetch {
    #[default]
    NotRequested,
    Loading,
    Done,
    /// The server does not support it here (feature off, no state DB, ...).
    Unavailable,
}

/// Goal of a thread as far as the GUI knows.
#[derive(Clone, Debug, Default, PartialEq)]
enum GoalState {
    #[default]
    Unknown,
    Loading,
    /// The `goals` feature is off: the section is hidden.
    Disabled,
    Unset,
    Set(ThreadGoal),
}

/// How a file appears in a diff.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum FileChangeKind {
    #[default]
    Modified,
    Added,
    Deleted,
}

/// Line counts of one file in a unified diff.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DiffFileStat {
    pub(crate) path: String,
    pub(crate) added: usize,
    pub(crate) removed: usize,
    pub(crate) kind: FileChangeKind,
}

/// Latest aggregated diff of a turn.
#[derive(Debug)]
struct TurnDiff {
    turn_id: String,
    raw: String,
    files: Vec<DiffFileStat>,
}

/// Status of a sub-agent, simplified for display.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentStatus {
    Starting,
    Running,
    Idle,
    Completed,
    Failed,
    Stopped,
}

/// A sub-agent spawned by the thread.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SubAgent {
    pub(crate) thread_id: String,
    pub(crate) nickname: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) status: AgentStatus,
    /// Last message reported for the agent (collab tool state).
    pub(crate) message: Option<String>,
    /// `thread/read` was sent to learn the nickname and role.
    metadata_requested: bool,
}

impl SubAgent {
    fn new(thread_id: &str, status: AgentStatus) -> Self {
        Self {
            thread_id: thread_id.to_string(),
            nickname: None,
            role: None,
            path: None,
            status,
            message: None,
            metadata_requested: false,
        }
    }

    fn label(&self) -> String {
        if let Some(nickname) = self.nickname.as_deref().filter(|name| !name.is_empty()) {
            return nickname.to_string();
        }
        if let Some(name) = self
            .path
            .as_deref()
            .and_then(|path| path.rsplit('/').find(|segment| !segment.is_empty()))
        {
            return name.to_string();
        }
        if let Some(role) = self.role.as_deref().filter(|role| !role.is_empty()) {
            return role.to_string();
        }
        let short: String = self.thread_id.chars().take(8).collect();
        format!("Agent {short}")
    }

    fn detail(&self) -> String {
        if let Some(message) = self
            .message
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            return first_line(message);
        }
        let label = self.label();
        self.role
            .as_deref()
            .filter(|role| *role != label)
            .unwrap_or_default()
            .to_string()
    }
}

/// What a notification says about one sub-agent.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AgentUpdate {
    pub(crate) thread_id: String,
    pub(crate) nickname: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) status: Option<AgentStatus>,
    pub(crate) message: Option<String>,
}

/// Adds or updates a sub-agent; fields missing from `update` are kept.
pub(crate) fn upsert_agent(agents: &mut Vec<SubAgent>, update: AgentUpdate) {
    let index = match agents
        .iter()
        .position(|agent| agent.thread_id == update.thread_id)
    {
        Some(index) => index,
        None => {
            agents.push(SubAgent::new(
                &update.thread_id,
                update.status.unwrap_or(AgentStatus::Starting),
            ));
            agents.len() - 1
        }
    };
    let agent = &mut agents[index];
    if update.nickname.is_some() {
        agent.nickname = update.nickname;
    }
    if update.role.is_some() {
        agent.role = update.role;
    }
    if update.path.is_some() {
        agent.path = update.path;
    }
    if let Some(status) = update.status {
        agent.status = status;
    }
    if update.message.is_some() {
        agent.message = update.message;
    }
}

fn collab_status(status: &CollabAgentStatus) -> AgentStatus {
    match status {
        CollabAgentStatus::PendingInit => AgentStatus::Starting,
        CollabAgentStatus::Running => AgentStatus::Running,
        CollabAgentStatus::Completed => AgentStatus::Completed,
        CollabAgentStatus::Errored | CollabAgentStatus::NotFound => AgentStatus::Failed,
        CollabAgentStatus::Interrupted | CollabAgentStatus::Shutdown => AgentStatus::Stopped,
    }
}

fn thread_status_agent(status: &ThreadStatus) -> AgentStatus {
    match status {
        ThreadStatus::Active { .. } => AgentStatus::Running,
        ThreadStatus::SystemError => AgentStatus::Failed,
        ThreadStatus::Idle | ThreadStatus::NotLoaded => AgentStatus::Idle,
    }
}

/// Sub-agent updates carried by an item of the parent thread.
pub(crate) fn agent_updates_from_item(item: &ThreadItem) -> Vec<AgentUpdate> {
    match item {
        ThreadItem::CollabAgentToolCall {
            tool,
            receiver_thread_ids,
            agents_states,
            ..
        } => receiver_thread_ids
            .iter()
            .map(|thread_id| {
                let state: Option<&CollabAgentState> = agents_states.get(thread_id);
                AgentUpdate {
                    thread_id: thread_id.clone(),
                    status: state.map(|state| collab_status(&state.status)).or_else(|| {
                        matches!(tool, CollabAgentTool::SpawnAgent).then_some(AgentStatus::Running)
                    }),
                    message: state.and_then(|state| state.message.clone()),
                    ..AgentUpdate::default()
                }
            })
            .collect(),
        ThreadItem::SubAgentActivity {
            kind,
            agent_thread_id,
            agent_path,
            ..
        } => vec![AgentUpdate {
            thread_id: agent_thread_id.clone(),
            path: (!agent_path.is_empty()).then(|| agent_path.clone()),
            status: Some(match kind {
                SubAgentActivityKind::Started | SubAgentActivityKind::Interacted => {
                    AgentStatus::Running
                }
                SubAgentActivityKind::Interrupted => AgentStatus::Stopped,
                SubAgentActivityKind::Completed => AgentStatus::Completed,
            }),
            ..AgentUpdate::default()
        }],
        _ => Vec::new(),
    }
}

fn agent_update_from_thread(thread: &Thread) -> AgentUpdate {
    AgentUpdate {
        thread_id: thread.id.clone(),
        nickname: thread.agent_nickname.clone(),
        role: thread.agent_role.clone(),
        path: None,
        status: Some(thread_status_agent(&thread.status)),
        message: None,
    }
}

/// MCP server startup state for display.
#[derive(Clone, Debug, PartialEq)]
struct McpEntry {
    state: McpServerStartupState,
    error: Option<String>,
}

/// The pane's view of an `mcpServerStatus/list` entry; `None` for servers
/// that are not started (disabled, or not connected by this thread).
fn mcp_entry_from_status(status: &McpServerStatus) -> Option<McpEntry> {
    let (state, error) = match status.runtime_status? {
        McpServerConnectionStatus::Starting => (McpServerStartupState::Starting, None),
        McpServerConnectionStatus::Connected => (McpServerStartupState::Ready, None),
        McpServerConnectionStatus::Failed => {
            (McpServerStartupState::Failed, status.tools_error.clone())
        }
        McpServerConnectionStatus::AuthenticationRequired => (
            McpServerStartupState::Failed,
            Some("Sign-in required".to_string()),
        ),
        McpServerConnectionStatus::Cancelled => (McpServerStartupState::Cancelled, None),
        McpServerConnectionStatus::NotStarted | McpServerConnectionStatus::Disabled => {
            return None;
        }
    };
    Some(McpEntry { state, error })
}

/// A hook that started and has not completed.
#[derive(Clone, Debug)]
struct RunningHook {
    turn_id: Option<String>,
    run: HookRunSummary,
}

/// Drops the hooks that cannot still run once turn `turn_id` completed:
/// synchronous hooks of that turn and of no turn (they block the turn or
/// precede it). Covers a dropped `hook/completed`; `turn/completed` itself
/// is always delivered. Asynchronous hooks may outlive their turn.
fn prune_hooks_after_turn(hooks: &mut Vec<RunningHook>, turn_id: &str) {
    hooks.retain(|hook| {
        let finished = hook.run.execution_mode == HookExecutionMode::Sync
            && hook.turn_id.as_deref().is_none_or(|id| id == turn_id);
        !finished
    });
}

/// Account rate limits shown in the pane and the `account/rateLimits/read`
/// that fills them.
#[derive(Default)]
struct RateLimits {
    fetch: Fetch,
    /// Latest snapshot per rate-limit bucket (`limit_id`, "codex" by default).
    snapshots: BTreeMap<String, RateLimitSnapshot>,
    /// Bumped per read and whenever the account or server changes; only the
    /// read with the current number is applied.
    seq: u64,
    /// What notifications delivered while a read was in flight (merged per
    /// bucket); these fields are newer than the read's.
    notified: BTreeMap<String, RateLimitSnapshot>,
}

impl RateLimits {
    /// The account or server changed: forget everything and ignore reads
    /// already in flight.
    fn reset(&mut self) {
        self.snapshots.clear();
        self.invalidate();
    }

    /// Keeps the snapshots but asks for a fresh read; reads in flight are
    /// ignored.
    fn invalidate(&mut self) {
        self.fetch = Fetch::NotRequested;
        self.seq = self.seq.wrapping_add(1);
        self.notified.clear();
    }

    /// Starts a read; `None` while one is in flight.
    fn begin_read(&mut self) -> Option<u64> {
        if self.fetch == Fetch::Loading {
            return None;
        }
        self.fetch = Fetch::Loading;
        self.seq = self.seq.wrapping_add(1);
        self.notified.clear();
        Some(self.seq)
    }

    /// A sparse `account/rateLimits/updated` snapshot.
    fn on_update(&mut self, snapshot: RateLimitSnapshot) {
        let key = limit_key(&snapshot);
        if self.fetch == Fetch::Loading {
            let pending = merge_rate_limit(self.notified.get(&key), snapshot.clone());
            self.notified.insert(key.clone(), pending);
        } else {
            self.fetch = Fetch::Done;
        }
        let merged = merge_rate_limit(self.snapshots.get(&key), snapshot);
        self.snapshots.insert(key, merged);
    }

    /// Applies the answer to read `seq` (`None` when it failed). Returns
    /// false for a stale answer, which is dropped.
    fn finish_read(&mut self, seq: u64, read: Option<BTreeMap<String, RateLimitSnapshot>>) -> bool {
        if seq != self.seq || self.fetch != Fetch::Loading {
            return false;
        }
        let notified = std::mem::take(&mut self.notified);
        match read {
            Some(mut read) => {
                for (key, update) in notified {
                    let merged = merge_rate_limit(read.get(&key), update);
                    read.insert(key, merged);
                }
                self.snapshots = read;
                self.fetch = Fetch::Done;
            }
            None if self.snapshots.is_empty() => self.fetch = Fetch::Unavailable,
            None => self.fetch = Fetch::Done,
        }
        true
    }
}

/// Per-thread info pane data.
#[derive(Debug, Default)]
pub(crate) struct ThreadInfo {
    usage: Option<ThreadTokenUsage>,
    diff: Option<TurnDiff>,
    plan: Option<(Option<String>, Vec<TurnPlanStep>)>,
    agents: Vec<SubAgent>,
    agents_backfill: Fetch,
    queue: Vec<QueuedSubmission>,
    queue_state: Fetch,
    /// Bumped per queue fetch so only the newest response is applied.
    queue_seq: u64,
    goal: GoalState,
    /// Hooks that started and have not completed.
    hooks: Vec<RunningHook>,
    mcp: BTreeMap<String, McpEntry>,
    /// Notifications were dropped: re-read the MCP states when shown.
    mcp_stale: bool,
    /// Commands left running in the background (`/ps`).
    terminals: Terminals,
}

/// Info pane state shared by all tabs.
#[derive(Default)]
pub(crate) struct InfoShared {
    rate_limits: RateLimits,
    /// MCP startup states not scoped to a thread.
    app_mcp: BTreeMap<String, McpEntry>,
    models: InfoModels,
    /// Refreshes background terminals while their section is shown.
    terminal_timer: slint::Timer,
}

#[derive(Default)]
struct InfoModels {
    fields: Rc<VecModel<InfoField>>,
    plan: Rc<VecModel<InfoPlanStep>>,
    diff_files: Rc<VecModel<InfoDiffFile>>,
    queue: Rc<VecModel<InfoQueued>>,
    agents: Rc<VecModel<InfoAgent>>,
    hooks: Rc<VecModel<InfoHook>>,
    mcp: Rc<VecModel<InfoMcp>>,
    mailbox: Rc<VecModel<InfoMail>>,
    limits: Rc<VecModel<InfoLimit>>,
    terminals: Rc<VecModel<InfoTerminal>>,
}

impl AppController {
    pub(crate) fn info_bind(&mut self) {
        let state = self.window.global::<InfoState>();
        let models = &self.info_shared.models;
        state.set_fields(ModelRc::from(models.fields.clone()));
        state.set_plan(ModelRc::from(models.plan.clone()));
        state.set_diff_files(ModelRc::from(models.diff_files.clone()));
        state.set_queue(ModelRc::from(models.queue.clone()));
        state.set_agents(ModelRc::from(models.agents.clone()));
        state.set_hooks(ModelRc::from(models.hooks.clone()));
        state.set_mcp(ModelRc::from(models.mcp.clone()));
        state.set_mailbox(ModelRc::from(models.mailbox.clone()));
        state.set_limits(ModelRc::from(models.limits.clone()));
        state.set_terminals(ModelRc::from(models.terminals.clone()));

        state.on_open_folder(|| {
            crate::ui_thread::with_app(|app| {
                if let Some(cwd) = app
                    .active_thread_index()
                    .and_then(|index| app.thread_tab(index))
                    .map(|thread| thread.cwd.clone())
                {
                    app.open_in_file_manager(&cwd);
                }
            });
        });
        state.on_open_diff(|| {
            crate::ui_thread::with_app(|app| {
                if let Some(index) = app.active_thread_index() {
                    app.info_open_diff(index);
                }
            });
        });
        state.on_open_agent(|thread_id| {
            let thread_id = thread_id.to_string();
            crate::ui_thread::with_app(move |app| app.open_thread(thread_id, None));
        });
        state.on_open_peer(|thread_id| {
            let thread_id = thread_id.to_string();
            crate::ui_thread::with_app(move |app| {
                if !thread_id.is_empty() {
                    app.open_thread(thread_id, None);
                }
            });
        });
        state.on_queue_action(|id, action| {
            let (id, action) = (id.to_string(), action.to_string());
            crate::ui_thread::with_app(move |app| app.info_queue_action(&id, &action));
        });
        state.on_goal_action(|action| {
            let action = action.to_string();
            crate::ui_thread::with_app(move |app| app.info_goal_action(&action));
        });
        state.on_terminal_action(|process_id, action| {
            let (process_id, action) = (process_id.to_string(), action.to_string());
            crate::ui_thread::with_app(move |app| app.info_terminal_action(&process_id, &action));
        });
        state.on_set_xtab(|enabled| {
            crate::ui_thread::with_app(move |app| {
                if let Some(index) = app.active_thread_index() {
                    app.xtab_set_enabled(index, enabled);
                    app.info_render();
                }
            });
        });
    }

    pub(crate) fn info_show(&mut self) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        self.info_load_lazy(index);
        if self.info_shared.rate_limits.fetch == Fetch::NotRequested {
            self.info_fetch_rate_limits();
        }
        self.info_render();
    }

    /// `/status` and `/usage`: shows the info pane, or the same summary in
    /// a dialog when the window is too narrow for the pane (the pane
    /// preference is left alone then).
    pub(crate) fn info_show_status(&mut self, index: usize) {
        if self.info_window_is_wide() {
            self.show_info_pane();
            return;
        }
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let now = crate::sidebar::unix_now();
        let text = status_text(
            &summary_fields(thread),
            thread.info.usage.as_ref().map(context_view).as_ref(),
            &limit_row_views(&self.info_shared.rate_limits.snapshots, now),
        );
        let title = thread.title();
        self.show_dialog(
            DialogRequest {
                cancel_label: String::new(),
                ..DialogRequest::confirm(title, text)
            }
            .accept_label("Close"),
            Box::new(|_, _| {}),
        );
    }

    /// Some notifications were dropped: re-read what they would have
    /// updated (transcripts and the sidebar resync themselves).
    pub(crate) fn info_on_lagged(&mut self) {
        self.info_shared.rate_limits.invalidate();
        for index in 0..self.tabs.len() {
            if let Some(tab) = self.thread_tab_mut(index) {
                tab.info.queue_state = Fetch::NotRequested;
                tab.info.agents_backfill = Fetch::NotRequested;
                if tab.info.goal != GoalState::Disabled {
                    tab.info.goal = GoalState::Unknown;
                }
                // A dropped `hook/completed` would leave a hook "running".
                tab.info.hooks.clear();
                tab.info.mcp_stale = true;
            }
            self.info_terminals_reset(index);
        }
        if self.active_thread_index().is_some() {
            self.info_show();
        }
    }

    pub(crate) fn info_on_notification(&mut self, index: usize, notification: &ServerNotification) {
        let mut fetch_queue = false;
        let mut fetch_agent_metadata = false;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let info = &mut thread.info;
        let changed = match notification {
            ServerNotification::ThreadTokenUsageUpdated(updated) => {
                info.usage = Some(updated.token_usage.clone());
                true
            }
            ServerNotification::TurnDiffUpdated(updated) => {
                if updated.diff.trim().is_empty() {
                    // The turn's changes cancelled out; keep an older turn's diff.
                    let same_turn = info
                        .diff
                        .as_ref()
                        .is_some_and(|diff| diff.turn_id == updated.turn_id);
                    if same_turn {
                        info.diff = None;
                    }
                    same_turn
                } else {
                    info.diff = Some(TurnDiff {
                        turn_id: updated.turn_id.clone(),
                        files: parse_diff_stats(&updated.diff),
                        raw: updated.diff.clone(),
                    });
                    true
                }
            }
            ServerNotification::TurnPlanUpdated(updated) => {
                info.plan = Some((updated.explanation.clone(), updated.plan.clone()));
                true
            }
            ServerNotification::ItemStarted(started) => {
                let changed = apply_agent_updates(&mut info.agents, &started.item);
                fetch_agent_metadata |= changed;
                changed
            }
            ServerNotification::ItemCompleted(completed) => {
                let changed = apply_agent_updates(&mut info.agents, &completed.item);
                fetch_agent_metadata |= changed;
                changed
            }
            ServerNotification::ThreadGoalUpdated(updated) => {
                info.goal = GoalState::Set(updated.goal.clone());
                true
            }
            ServerNotification::ThreadGoalCleared(_) => {
                info.goal = GoalState::Unset;
                true
            }
            ServerNotification::ThreadQueueChanged(_) => {
                fetch_queue = true;
                false
            }
            ServerNotification::HookStarted(started) => {
                info.hooks.retain(|hook| hook.run.id != started.run.id);
                info.hooks.push(RunningHook {
                    turn_id: started.turn_id.clone(),
                    run: started.run.clone(),
                });
                true
            }
            ServerNotification::HookCompleted(completed) => {
                info.hooks.retain(|hook| hook.run.id != completed.run.id);
                true
            }
            ServerNotification::TurnCompleted(completed) => {
                prune_hooks_after_turn(&mut info.hooks, &completed.turn.id);
                true
            }
            ServerNotification::McpServerStatusUpdated(updated) => {
                info.mcp.insert(
                    updated.name.clone(),
                    McpEntry {
                        state: updated.status,
                        error: updated.error.clone(),
                    },
                );
                true
            }
            ServerNotification::TurnStarted(_)
            | ServerNotification::ThreadStatusChanged(_)
            | ServerNotification::ThreadSettingsUpdated(_)
            | ServerNotification::ThreadNameUpdated(_)
            | ServerNotification::ThreadClosed(_)
            | ServerNotification::ThreadArchived(_) => true,
            _ => false,
        };
        if fetch_queue {
            self.info_fetch_queue(index);
        }
        if fetch_agent_metadata {
            self.info_fetch_agent_metadata(index);
        }
        self.info_terminals_on_notification(index, notification);
        if changed && self.active == Some(index) {
            self.info_render();
        }
    }

    /// Notifications that are not routed to a tab: account rate limits,
    /// app-scoped MCP status, and sub-agent threads of open tabs.
    pub(crate) fn info_on_app_notification(&mut self, notification: &ServerNotification) {
        let changed = match notification {
            ServerNotification::AccountRateLimitsUpdated(updated) => {
                self.info_shared
                    .rate_limits
                    .on_update(updated.rate_limits.clone());
                true
            }
            ServerNotification::AccountUpdated(_) => {
                // A read still in flight is for the previous account.
                self.info_shared.rate_limits.reset();
                if self.active_thread_index().is_some() {
                    self.info_fetch_rate_limits();
                }
                true
            }
            ServerNotification::McpServerStatusUpdated(updated) if updated.thread_id.is_none() => {
                self.info_shared.app_mcp.insert(
                    updated.name.clone(),
                    McpEntry {
                        state: updated.status,
                        error: updated.error.clone(),
                    },
                );
                true
            }
            ServerNotification::ThreadStarted(started) => {
                let thread = &started.thread;
                match thread
                    .parent_thread_id
                    .as_deref()
                    .and_then(|parent| self.tab_index_for_thread(parent))
                {
                    Some(index) => {
                        if let Some(tab) = self.thread_tab_mut(index) {
                            upsert_agent(&mut tab.info.agents, agent_update_from_thread(thread));
                        }
                        self.active == Some(index)
                    }
                    None => false,
                }
            }
            ServerNotification::ThreadStatusChanged(changed) => {
                let status = thread_status_agent(&changed.status);
                let mut active_changed = false;
                for index in 0..self.tabs.len() {
                    let is_active = self.active == Some(index);
                    if let Some(tab) = self.thread_tab_mut(index)
                        && let Some(agent) = tab
                            .info
                            .agents
                            .iter_mut()
                            .find(|agent| agent.thread_id == changed.thread_id)
                        && agent.status != status
                    {
                        agent.status = status;
                        active_changed |= is_active;
                    }
                }
                active_changed
            }
            _ => false,
        };
        if changed && self.active_thread_index().is_some() {
            self.info_render();
        }
    }

    pub(crate) fn info_on_server_ready(&mut self) {
        // A read still in flight went to the old server.
        self.info_shared.rate_limits.reset();
        self.info_shared.app_mcp.clear();
        for index in 0..self.tabs.len() {
            if let Some(tab) = self.thread_tab_mut(index) {
                // A restarted server re-reports these; reload lazily.
                tab.info.queue_state = Fetch::NotRequested;
                tab.info.agents_backfill = Fetch::NotRequested;
                if tab.info.goal != GoalState::Disabled {
                    tab.info.goal = GoalState::Unknown;
                }
                tab.info.hooks.clear();
                tab.info.mcp.clear();
                tab.info.mcp_stale = false;
            }
            self.info_terminals_reset(index);
        }
        if self.active_thread_index().is_some() {
            self.info_show();
        }
    }

    /// Opens the latest aggregated turn diff of tab `index` in a diff tab.
    pub(crate) fn info_open_diff(&mut self, index: usize) {
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        match thread.info.diff.as_ref() {
            Some(diff) => {
                let title = format!("Changes · {}", thread.title());
                let raw = diff.raw.clone();
                self.open_diff_tab(title, raw);
            }
            None => self.toast("This thread has no changes yet"),
        }
    }

    // ----- lazy loads -------------------------------------------------------

    fn info_load_lazy(&mut self, index: usize) {
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        if thread.thread_id.is_none() {
            return;
        }
        let (queue, goal, agents, mcp) = (
            thread.info.queue_state == Fetch::NotRequested,
            thread.info.goal == GoalState::Unknown,
            thread.info.agents_backfill == Fetch::NotRequested,
            thread.info.mcp_stale,
        );
        if mcp {
            self.info_fetch_mcp_status(index);
        }
        if queue {
            self.info_fetch_queue(index);
        }
        if goal {
            self.info_fetch_goal(index);
        }
        if agents {
            self.info_fetch_sub_agents(index);
        }
        self.info_terminals_load_lazy(index);
    }

    fn info_fetch_queue(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        thread.info.queue_seq += 1;
        let seq = thread.info.queue_seq;
        if thread.info.queue_state != Fetch::Done {
            thread.info.queue_state = Fetch::Loading;
        }
        self.backend.call(
            |request_id| ClientRequest::ThreadQueueList {
                request_id,
                params: ThreadQueueListParams {
                    thread_id,
                    cursor: None,
                    limit: Some(QUEUE_PAGE_LIMIT),
                },
            },
            move |app, result: Result<ThreadQueueListResponse, BackendError>| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                let Some(thread) = app.thread_tab_mut(index) else {
                    return;
                };
                if thread.info.queue_seq != seq {
                    return;
                }
                match result {
                    Ok(response) => {
                        thread.info.queue = response.data;
                        thread.info.queue_state = Fetch::Done;
                    }
                    Err(err) => {
                        // Ephemeral threads and servers without a state DB
                        // have no queue; the section simply stays hidden.
                        tracing::debug!(error = %err.user_message(), "thread/queue/list failed");
                        thread.info.queue.clear();
                        thread.info.queue_state = Fetch::Unavailable;
                    }
                }
                if app.active == Some(index) {
                    app.info_render();
                }
            },
        );
    }

    fn info_fetch_goal(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        thread.info.goal = GoalState::Loading;
        self.backend.call(
            |request_id| ClientRequest::ThreadGoalGet {
                request_id,
                params: ThreadGoalGetParams { thread_id },
            },
            move |app, result: Result<ThreadGoalGetResponse, BackendError>| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                let Some(thread) = app.thread_tab_mut(index) else {
                    return;
                };
                if thread.info.goal != GoalState::Loading {
                    // A goal notification arrived first; it is newer.
                    return;
                }
                thread.info.goal = match result {
                    Ok(response) => response.goal.map_or(GoalState::Unset, GoalState::Set),
                    Err(err) if is_goals_disabled(&err) => GoalState::Disabled,
                    Err(err) => {
                        tracing::debug!(error = %err.user_message(), "thread/goal/get failed");
                        GoalState::Disabled
                    }
                };
                if app.active == Some(index) {
                    app.info_render();
                }
            },
        );
    }

    fn info_fetch_sub_agents(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        thread.info.agents_backfill = Fetch::Loading;
        let params = ThreadListParams {
            cursor: None,
            limit: Some(SUB_AGENT_PAGE_LIMIT),
            sort_key: Some(ThreadSortKey::CreatedAt),
            sort_direction: Some(SortDirection::Asc),
            model_providers: Some(Vec::new()),
            source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
            originators: None,
            archived: Some(false),
            section_id: None,
            project_id: None,
            cwd: None,
            use_state_db_only: true,
            search_term: None,
            parent_thread_id: Some(thread_id),
            ancestor_thread_id: None,
        };
        self.backend.call(
            |request_id| ClientRequest::ThreadList { request_id, params },
            move |app, result: Result<ThreadListResponse, BackendError>| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                let Some(thread) = app.thread_tab_mut(index) else {
                    return;
                };
                match result {
                    Ok(response) => {
                        for child in &response.data {
                            upsert_agent(&mut thread.info.agents, agent_update_from_thread(child));
                        }
                        thread.info.agents_backfill = Fetch::Done;
                    }
                    Err(err) => {
                        tracing::debug!(error = %err.user_message(), "sub-agent list failed");
                        thread.info.agents_backfill = Fetch::Unavailable;
                    }
                }
                if app.active == Some(index) {
                    app.info_render();
                }
            },
        );
    }

    /// Reads nickname and role of sub-agents only known from collab items.
    fn info_fetch_agent_metadata(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let agent_ids: Vec<String> = thread
            .info
            .agents
            .iter_mut()
            .filter(|agent| agent.nickname.is_none() && !agent.metadata_requested)
            .map(|agent| {
                agent.metadata_requested = true;
                agent.thread_id.clone()
            })
            .collect();
        for agent_id in agent_ids {
            self.backend.call(
                |request_id| crate::session::thread_read(request_id, &agent_id),
                move |app, result: Result<ThreadReadResponse, BackendError>| {
                    let response = match result {
                        Ok(response) => response,
                        Err(err) => {
                            tracing::debug!(error = %err.user_message(), "sub-agent thread/read failed");
                            return;
                        }
                    };
                    let Some(index) = app.tab_index_by_id(tab_id) else {
                        return;
                    };
                    if let Some(tab) = app.thread_tab_mut(index) {
                        upsert_agent(
                            &mut tab.info.agents,
                            agent_update_from_thread(&response.thread),
                        );
                    }
                    if app.active == Some(index) {
                        app.info_render();
                    }
                },
            );
        }
    }

    /// Re-reads the MCP server states of tab `index` (after dropped
    /// notifications).
    fn info_fetch_mcp_status(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        thread.info.mcp_stale = false;
        let params = ListMcpServerStatusParams {
            cursor: None,
            limit: Some(MCP_STATUS_PAGE_LIMIT),
            detail: Some(McpServerStatusDetail::ToolsAndAuthOnly),
            thread_id: Some(thread_id),
            server_name: None,
        };
        self.backend.call(
            |request_id| ClientRequest::McpServerStatusList { request_id, params },
            move |app, result: Result<ListMcpServerStatusResponse, BackendError>| {
                let response = match result {
                    Ok(response) => response,
                    Err(err) => {
                        tracing::debug!(error = %err.user_message(), "mcpServerStatus/list failed");
                        return;
                    }
                };
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                if let Some(thread) = app.thread_tab_mut(index) {
                    thread.info.mcp = response
                        .data
                        .iter()
                        .filter_map(|status| {
                            mcp_entry_from_status(status).map(|entry| (status.name.clone(), entry))
                        })
                        .collect();
                }
                if app.active == Some(index) {
                    app.info_render();
                }
            },
        );
    }

    fn info_fetch_rate_limits(&mut self) {
        let Some(seq) = self.info_shared.rate_limits.begin_read() else {
            return;
        };
        self.backend.call(
            |request_id| ClientRequest::GetAccountRateLimits {
                request_id,
                params: Some(GetAccountRateLimitsParams {
                    supports_luna_reserve: false,
                    exclude_reset_credit_details: true,
                }),
            },
            move |app, result: Result<GetAccountRateLimitsResponse, BackendError>| {
                let read = match result {
                    Ok(response) => Some(match response.rate_limits_by_limit_id {
                        Some(by_id) if !by_id.is_empty() => by_id.into_iter().collect(),
                        _ => {
                            let key = limit_key(&response.rate_limits);
                            BTreeMap::from([(key, response.rate_limits)])
                        }
                    }),
                    Err(err) => {
                        // API-key and non-OpenAI providers have no limits.
                        tracing::debug!(error = %err.user_message(), "account/rateLimits/read failed");
                        None
                    }
                };
                if app.info_shared.rate_limits.finish_read(seq, read) {
                    app.info_render();
                }
            },
        );
    }

    // ----- actions ----------------------------------------------------------

    fn info_queue_action(&mut self, submission_id: &str, action: &str) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        let Some(position) = thread
            .info
            .queue
            .iter()
            .position(|item| item.id == submission_id)
        else {
            return;
        };
        let submission_id = submission_id.to_string();
        match action {
            "up" | "down" => {
                let ids: Vec<String> = thread
                    .info
                    .queue
                    .iter()
                    .map(|item| item.id.clone())
                    .collect();
                let delta = if action == "up" { -1 } else { 1 };
                let Some(order) = reorder_ids(&ids, position, delta) else {
                    return;
                };
                // Optimistic; `thread/queue/changed` re-lists afterwards.
                thread.info.queue.sort_by_key(|item| {
                    order
                        .iter()
                        .position(|id| *id == item.id)
                        .unwrap_or(usize::MAX)
                });
                self.info_render();
                self.backend.call(
                    |request_id| ClientRequest::ThreadQueueReorder {
                        request_id,
                        params: ThreadQueueReorderParams {
                            thread_id,
                            queued_submission_ids: order,
                        },
                    },
                    move |app, result: Result<ThreadQueueReorderResponse, BackendError>| {
                        if let Err(err) = result {
                            app.toast(format!("Could not reorder: {}", err.user_message()));
                            app.info_refetch_queue(tab_id);
                        }
                    },
                );
            }
            "delete" => {
                let removed = thread.info.queue.remove(position);
                self.info_render();
                self.backend.call(
                    |request_id| ClientRequest::ThreadQueueDelete {
                        request_id,
                        params: ThreadQueueDeleteParams {
                            thread_id,
                            queued_submission_id: submission_id,
                        },
                    },
                    move |app, result: Result<ThreadQueueDeleteResponse, BackendError>| {
                        match result {
                            // The message will never be sent: drop its bubble.
                            Ok(_) => {
                                if let Some(index) = app.tab_index_by_id(tab_id) {
                                    app.transcript_remove_echo(
                                        index,
                                        &removed.client_user_message_id,
                                    );
                                }
                            }
                            Err(err) => {
                                app.toast(format!("Could not remove: {}", err.user_message()));
                                app.info_refetch_queue(tab_id);
                            }
                        }
                    },
                );
            }
            "send" => {
                if thread.is_busy() {
                    self.toast("Wait for the current turn to finish");
                    return;
                }
                self.backend.call(
                    |request_id| ClientRequest::ThreadQueueStart {
                        request_id,
                        params: ThreadQueueStartParams {
                            thread_id,
                            queued_submission_id: Some(submission_id),
                        },
                    },
                    move |app, result: Result<ThreadQueueStartResponse, BackendError>| {
                        if let Err(err) = result {
                            app.toast(format!("Could not send: {}", err.user_message()));
                        }
                    },
                );
            }
            other => tracing::debug!(action = other, "unknown queue action"),
        }
    }

    fn info_refetch_queue(&mut self, tab_id: crate::app::TabId) {
        if let Some(index) = self.tab_index_by_id(tab_id) {
            self.info_fetch_queue(index);
        }
    }

    fn info_goal_action(&mut self, action: &str) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        let tab_id = self.tabs[index].id;
        match action {
            "set" => {
                let current = match &thread.info.goal {
                    GoalState::Set(goal) => goal.objective.clone(),
                    _ => String::new(),
                };
                self.show_dialog(
                    DialogRequest {
                        input_placeholder: "What should the agent keep working toward?".to_string(),
                        ..DialogRequest::prompt("Set goal", current)
                    }
                    .accept_label("Set goal"),
                    Box::new(move |app, value| {
                        let Some(objective) = value.map(|value| value.trim().to_string()) else {
                            return;
                        };
                        if objective.is_empty() {
                            return;
                        }
                        app.info_set_goal(
                            tab_id,
                            ThreadGoalSetParams {
                                thread_id,
                                origin: Some(ThreadGoalMutationOrigin::User),
                                objective: Some(objective),
                                status: Some(ThreadGoalStatus::Active),
                                token_budget: None,
                            },
                        );
                    }),
                );
            }
            "pause" | "resume" => {
                let status = if action == "pause" {
                    ThreadGoalStatus::Paused
                } else {
                    ThreadGoalStatus::Active
                };
                self.info_set_goal(
                    tab_id,
                    ThreadGoalSetParams {
                        thread_id,
                        origin: Some(ThreadGoalMutationOrigin::User),
                        objective: None,
                        status: Some(status),
                        token_budget: None,
                    },
                );
            }
            "clear" => {
                self.backend.call(
                    |request_id| ClientRequest::ThreadGoalClear {
                        request_id,
                        params: ThreadGoalClearParams {
                            thread_id,
                            origin: Some(ThreadGoalMutationOrigin::User),
                        },
                    },
                    move |app, result: Result<ThreadGoalClearResponse, BackendError>| match result {
                        Ok(_) => app.info_update_goal(tab_id, GoalState::Unset),
                        Err(err) => {
                            app.toast(format!("Could not clear goal: {}", err.user_message()));
                        }
                    },
                );
            }
            other => tracing::debug!(action = other, "unknown goal action"),
        }
    }

    fn info_set_goal(&mut self, tab_id: crate::app::TabId, params: ThreadGoalSetParams) {
        self.backend.call(
            |request_id| ClientRequest::ThreadGoalSet { request_id, params },
            move |app, result: Result<ThreadGoalSetResponse, BackendError>| match result {
                Ok(response) => app.info_update_goal(tab_id, GoalState::Set(response.goal)),
                Err(err) if is_goals_disabled(&err) => {
                    app.info_update_goal(tab_id, GoalState::Disabled);
                }
                Err(err) => app.toast(format!("Could not update goal: {}", err.user_message())),
            },
        );
    }

    fn info_update_goal(&mut self, tab_id: crate::app::TabId, goal: GoalState) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        if let Some(thread) = self.thread_tab_mut(index) {
            thread.info.goal = goal;
        }
        if self.active == Some(index) {
            self.info_render();
        }
    }

    // ----- rendering --------------------------------------------------------

    /// Pushes the active thread's info into `InfoState`.
    fn info_render(&mut self) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let now = crate::sidebar::unix_now();
        let mailbox: Vec<InfoMail> = self
            .thread_tab(index)
            .and_then(|thread| thread.thread_id.as_deref())
            .map(|thread_id| self.xtab_mailbox_for_thread(thread_id))
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(MAX_MAILBOX_ENTRIES)
            .map(|entry| InfoMail {
                incoming: entry.direction == crate::xtab::MailDirection::Incoming,
                peer: entry.peer_title.into(),
                peer_thread_id: entry.peer_thread_id.into(),
                text: crate::app::truncate_chars(entry.text.trim(), MAIL_PREVIEW_CHARS).into(),
                time: relative_age(now, entry.timestamp).into(),
            })
            .collect();
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let state = self.window.global::<InfoState>();
        let models = &self.info_shared.models;
        let info = &thread.info;

        // Summary.
        state.set_title(thread.title().into());
        state.set_folder_name(crate::app::folder_label(&thread.cwd).into());
        state.set_folder_path(crate::newtab::display_path(&thread.cwd).into());
        sync_model(
            &models.fields,
            summary_fields(thread)
                .into_iter()
                .map(|(label, value)| field(label, value))
                .collect(),
        );

        // Context window.
        match info.usage.as_ref() {
            Some(usage) => {
                let view = context_view(usage);
                state.set_has_usage(true);
                state.set_has_window(view.used_fraction.is_some());
                state.set_context_used(view.used_fraction.unwrap_or(0.0));
                state.set_context_label(view.label.into());
                state.set_context_detail(view.detail.into());
                state.set_usage_detail(view.totals.into());
            }
            None => state.set_has_usage(false),
        }

        // Plan.
        match info.plan.as_ref() {
            Some((explanation, steps)) if !steps.is_empty() => {
                let done = steps
                    .iter()
                    .filter(|step| step.status == TurnPlanStepStatus::Completed)
                    .count();
                state.set_has_plan(true);
                state.set_plan_progress(format!("{done} of {}", steps.len()).into());
                state.set_plan_explanation(
                    explanation
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or_default()
                        .into(),
                );
                sync_model(
                    &models.plan,
                    steps
                        .iter()
                        .map(|step| InfoPlanStep {
                            text: step.step.as_str().into(),
                            status: match step.status {
                                TurnPlanStepStatus::Pending => InfoStepStatus::Pending,
                                TurnPlanStepStatus::InProgress => InfoStepStatus::InProgress,
                                TurnPlanStepStatus::Completed => InfoStepStatus::Completed,
                            },
                        })
                        .collect(),
                );
            }
            _ => state.set_has_plan(false),
        }

        // Turn changes.
        match info.diff.as_ref() {
            Some(diff) => {
                let added: usize = diff.files.iter().map(|file| file.added).sum();
                let removed: usize = diff.files.iter().map(|file| file.removed).sum();
                state.set_has_diff(true);
                state.set_diff_summary(diff_summary(diff.files.len(), added, removed).into());
                state.set_diff_added(i32::try_from(added).unwrap_or(i32::MAX));
                state.set_diff_removed(i32::try_from(removed).unwrap_or(i32::MAX));
                let hidden = diff.files.len().saturating_sub(MAX_DIFF_FILES);
                state.set_diff_more(if hidden > 0 {
                    format!("and {hidden} more").into()
                } else {
                    Default::default()
                });
                sync_model(
                    &models.diff_files,
                    diff.files
                        .iter()
                        .take(MAX_DIFF_FILES)
                        .map(|file| InfoDiffFile {
                            name: file_name(&file.path).into(),
                            path: file.path.as_str().into(),
                            added: i32::try_from(file.added).unwrap_or(i32::MAX),
                            removed: i32::try_from(file.removed).unwrap_or(i32::MAX),
                            note: match file.kind {
                                FileChangeKind::Added => "new",
                                FileChangeKind::Deleted => "deleted",
                                FileChangeKind::Modified => "",
                            }
                            .into(),
                        })
                        .collect(),
                );
            }
            None => state.set_has_diff(false),
        }

        // Goal.
        state.set_goal_available(matches!(info.goal, GoalState::Unset | GoalState::Set(_)));
        match &info.goal {
            GoalState::Set(goal) => {
                state.set_has_goal(true);
                state.set_goal_objective(goal.objective.as_str().into());
                state.set_goal_status(goal_status_label(goal.status).into());
                state.set_goal_detail(goal_detail(goal).into());
                state.set_goal_paused(goal.status == ThreadGoalStatus::Paused);
            }
            _ => state.set_has_goal(false),
        }

        // Queued messages.
        state.set_queue_can_send(!thread.is_busy() && thread.thread_id.is_some());
        sync_model(
            &models.queue,
            info.queue
                .iter()
                .map(|item| InfoQueued {
                    id: item.id.as_str().into(),
                    text: queued_preview(item).into(),
                })
                .collect(),
        );

        // Sub-agents.
        sync_model(
            &models.agents,
            info.agents
                .iter()
                .map(|agent| InfoAgent {
                    thread_id: agent.thread_id.as_str().into(),
                    label: agent.label().into(),
                    detail: agent.detail().into(),
                    status: match agent.status {
                        AgentStatus::Starting => InfoAgentStatus::Starting,
                        AgentStatus::Running => InfoAgentStatus::Running,
                        AgentStatus::Idle => InfoAgentStatus::Idle,
                        AgentStatus::Completed => InfoAgentStatus::Completed,
                        AgentStatus::Failed => InfoAgentStatus::Failed,
                        AgentStatus::Stopped => InfoAgentStatus::Stopped,
                    },
                    status_text: agent_status_label(agent.status).into(),
                })
                .collect(),
        );

        // Hooks.
        sync_model(
            &models.hooks,
            info.hooks
                .iter()
                .map(|hook| &hook.run)
                .map(|run| InfoHook {
                    label: format!("{:?}", run.event_name).into(),
                    detail: run
                        .status_message
                        .clone()
                        .filter(|message| !message.trim().is_empty())
                        .unwrap_or_else(|| file_name(&run.source_path.as_path().to_string_lossy()))
                        .into(),
                })
                .collect(),
        );

        // MCP servers: app-scoped states, overridden by the thread's own.
        let mut servers = self.info_shared.app_mcp.clone();
        servers.extend(
            info.mcp
                .iter()
                .map(|(name, entry)| (name.clone(), entry.clone())),
        );
        state.set_mcp_summary(mcp_summary(servers.values().map(|entry| entry.state)).into());
        sync_model(
            &models.mcp,
            servers
                .into_iter()
                .map(|(name, entry)| InfoMcp {
                    name: name.into(),
                    status: match entry.state {
                        McpServerStartupState::Starting => InfoMcpStatus::Starting,
                        McpServerStartupState::Ready => InfoMcpStatus::Ready,
                        McpServerStartupState::Failed => InfoMcpStatus::Failed,
                        McpServerStartupState::Cancelled => InfoMcpStatus::Cancelled,
                    },
                    status_text: mcp_state_label(entry.state).into(),
                    error: entry.error.unwrap_or_default().into(),
                })
                .collect(),
        );

        // Cross-tab.
        state.set_xtab_enabled(thread.xtab_enabled);
        sync_model(&models.mailbox, mailbox);

        // Account rate limits.
        let snapshots = &self.info_shared.rate_limits.snapshots;
        let limits = limit_rows(snapshots, now);
        state.set_limits_note(
            snapshots
                .values()
                .find_map(|snapshot| snapshot.plan_type)
                .map(|plan| format!("{plan:?} plan"))
                .unwrap_or_default()
                .into(),
        );
        sync_model(&models.limits, limits);
        self.info_render_terminals(index);
    }
}

/// The thread's settings as the next turn will use them: pending composer
/// overrides first (the same rules as the composer toolbar), then what the
/// server reported.
fn summary_fields(thread: &ThreadTab) -> Vec<(&'static str, String)> {
    let approval = crate::composer::effective_approval(thread);
    let reviewer = crate::composer::effective_reviewer(thread);
    let sandbox = crate::composer::effective_sandbox(thread);
    let mut fields = vec![
        (
            "Model",
            crate::composer::effective_model(thread).unwrap_or_else(|| "—".to_string()),
        ),
        (
            "Effort",
            crate::composer::effective_effort(thread)
                .map(|effort| effort_label(&effort))
                .unwrap_or_else(|| "default".to_string()),
        ),
        (
            "Provider",
            thread
                .model_provider
                .clone()
                .unwrap_or_else(|| "—".to_string()),
        ),
        (
            "Approval",
            approval.map_or_else(
                || "—".to_string(),
                |policy| approval_with_reviewer(policy, reviewer),
            ),
        ),
        (
            "Sandbox",
            sandbox
                .as_ref()
                .map_or_else(|| "—".to_string(), sandbox_label),
        ),
    ];
    if let Some(parent) = thread.extras.side_parent.as_ref() {
        fields.insert(
            0,
            ("Side chat", format!("of “{}”, not saved", parent.title)),
        );
    }
    fields
}

/// Plain-text status summary for `/status` in a narrow window.
fn status_text(
    fields: &[(&'static str, String)],
    context: Option<&ContextView>,
    limits: &[LimitRowView],
) -> String {
    let mut lines: Vec<String> = fields
        .iter()
        .map(|(label, value)| format!("{label}: {value}"))
        .collect();
    if let Some(context) = context {
        let mut line = format!("Context: {}", context.label);
        if !context.detail.is_empty() {
            line.push_str(&format!(" ({})", context.detail));
        }
        lines.push(line);
        lines.push(context.totals.clone());
    }
    for limit in limits {
        let mut line = format!("{}: {}", limit.label, limit.percent);
        if !limit.detail.is_empty() {
            line.push_str(&format!(" · {}", limit.detail));
        }
        lines.push(line);
    }
    lines.push(String::new());
    lines.push("Widen the window to see the info pane.".to_string());
    lines.join("\n")
}

fn apply_agent_updates(agents: &mut Vec<SubAgent>, item: &ThreadItem) -> bool {
    let updates = agent_updates_from_item(item);
    let changed = !updates.is_empty();
    for update in updates {
        upsert_agent(agents, update);
    }
    changed
}

fn is_goals_disabled(err: &BackendError) -> bool {
    err.user_message().contains(GOALS_DISABLED_MESSAGE)
}

fn field(label: &str, value: String) -> InfoField {
    InfoField {
        label: label.into(),
        value: value.into(),
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn queued_preview(item: &QueuedSubmission) -> String {
    let text = crate::threads::user_input_preview(&item.input);
    if text.is_empty() {
        "(attachment)".to_string()
    } else {
        first_line(&text)
    }
}

/// "5m ago"-style age for list entries.
fn relative_age(now: i64, then: i64) -> String {
    match crate::sidebar::relative_time(now, then) {
        age if age == "now" => age,
        age => format!("{age} ago"),
    }
}

fn effort_label(effort: &codex_protocol::openai_models::ReasoningEffort) -> String {
    serde_json::to_value(effort)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{effort:?}").to_lowercase())
}

fn approval_label(policy: AskForApproval) -> String {
    match policy {
        AskForApproval::UnlessTrusted => "Untrusted commands",
        AskForApproval::OnRequest => "On request",
        AskForApproval::Granular { .. } => "Granular",
        AskForApproval::Never => "Never ask",
    }
    .to_string()
}

/// Approval mode, noting when the auto-review agent answers the requests.
fn approval_with_reviewer(policy: AskForApproval, reviewer: Option<ApprovalsReviewer>) -> String {
    let label = approval_label(policy);
    match (policy, reviewer) {
        (AskForApproval::Never, _) | (_, None | Some(ApprovalsReviewer::User)) => label,
        (_, Some(ApprovalsReviewer::AutoReview)) => format!("{label} · auto-review"),
    }
}

fn sandbox_label(policy: &SandboxPolicy) -> String {
    let (label, network) = match policy {
        SandboxPolicy::DangerFullAccess => ("Full access", false),
        SandboxPolicy::ReadOnly { network_access } => ("Read only", *network_access),
        SandboxPolicy::ExternalSandbox { .. } => ("External sandbox", false),
        SandboxPolicy::WorkspaceWrite { network_access, .. } => {
            ("Workspace write", *network_access)
        }
    };
    if network {
        format!("{label} + network")
    } else {
        label.to_string()
    }
}

fn agent_status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => "starting",
        AgentStatus::Running => "running",
        AgentStatus::Idle => "idle",
        AgentStatus::Completed => "done",
        AgentStatus::Failed => "failed",
        AgentStatus::Stopped => "stopped",
    }
}

fn mcp_state_label(state: McpServerStartupState) -> &'static str {
    match state {
        McpServerStartupState::Starting => "starting",
        McpServerStartupState::Ready => "ready",
        McpServerStartupState::Failed => "failed",
        McpServerStartupState::Cancelled => "cancelled",
    }
}

/// "3 ready · 1 failed".
fn mcp_summary(states: impl Iterator<Item = McpServerStartupState>) -> String {
    let mut counts: Vec<(McpServerStartupState, usize)> = Vec::new();
    for state in states {
        match counts.iter_mut().find(|(existing, _)| *existing == state) {
            Some((_, count)) => *count += 1,
            None => counts.push((state, 1)),
        }
    }
    counts.sort_by_key(|(state, _)| match state {
        McpServerStartupState::Ready => 0,
        McpServerStartupState::Starting => 1,
        McpServerStartupState::Failed => 2,
        McpServerStartupState::Cancelled => 3,
    });
    counts
        .into_iter()
        .map(|(state, count)| format!("{count} {}", mcp_state_label(state)))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn goal_status_label(status: ThreadGoalStatus) -> &'static str {
    match status {
        ThreadGoalStatus::Active => "Active",
        ThreadGoalStatus::Paused => "Paused",
        ThreadGoalStatus::Blocked => "Blocked",
        ThreadGoalStatus::UsageLimited => "Usage limited",
        ThreadGoalStatus::BudgetLimited => "Budget reached",
        ThreadGoalStatus::Complete => "Complete",
    }
}

fn goal_detail(goal: &ThreadGoal) -> String {
    let tokens = match goal.token_budget {
        Some(budget) => format!(
            "{} of {} tokens",
            format_tokens(goal.tokens_used),
            format_tokens(budget)
        ),
        None => format!("{} tokens", format_tokens(goal.tokens_used)),
    };
    if goal.time_used_seconds > 0 {
        format!("{tokens} · {}", format_duration(goal.time_used_seconds))
    } else {
        tokens
    }
}

/// "2 files · +12 −3".
fn diff_summary(files: usize, added: usize, removed: usize) -> String {
    let noun = if files == 1 { "file" } else { "files" };
    format!("{files} {noun} · +{added} −{removed}")
}

/// Compact token count: 950, 1.2K, 272K, 1.5M.
pub(crate) fn format_tokens(count: i64) -> String {
    fn scaled(count: i64, unit: f64, suffix: &str) -> String {
        let value = count as f64 / unit;
        let text = if value < 10.0 {
            let text = format!("{value:.1}");
            text.strip_suffix(".0").map(str::to_string).unwrap_or(text)
        } else {
            format!("{value:.0}")
        };
        format!("{text}{suffix}")
    }
    let count = count.max(0);
    match count {
        count if count < 1_000 => count.to_string(),
        count if count < 999_500 => scaled(count, 1e3, "K"),
        count if count < 999_500_000 => scaled(count, 1e6, "M"),
        count => scaled(count, 1e9, "B"),
    }
}

/// Short duration: "45s", "12m", "2h 5m", "3d 4h".
pub(crate) fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, minutes) => format!("{minutes}m"),
        (0, hours, 0) => format!("{hours}h"),
        (0, hours, minutes) => format!("{hours}h {minutes}m"),
        (days, 0, _) => format!("{days}d"),
        (days, hours, _) => format!("{days}d {hours}h"),
    }
}

/// Context-window gauge contents.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ContextView {
    /// 0..1 of the window in use; `None` when the window size is unknown.
    pub(crate) used_fraction: Option<f32>,
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) totals: String,
}

/// Percent of the context window in use, by the TUI's rule: the fixed
/// baseline (system prompt, tools) is excluded from both sides, so a fresh
/// thread reads 0% and a full one 100%.
pub(crate) fn context_used_percent(tokens_in_context: i64, context_window: i64) -> i64 {
    if context_window <= CONTEXT_BASELINE_TOKENS {
        return 100;
    }
    let effective_window = context_window - CONTEXT_BASELINE_TOKENS;
    let used = (tokens_in_context - CONTEXT_BASELINE_TOKENS).max(0);
    let remaining = (effective_window - used).max(0);
    let left = ((remaining as f64 / effective_window as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round() as i64;
    100 - left
}

pub(crate) fn context_view(usage: &ThreadTokenUsage) -> ContextView {
    let in_context = usage.last.total_tokens.max(0);
    let total = &usage.total;
    let mut totals = format!(
        "Thread total {} · {} in",
        format_tokens(total.total_tokens),
        format_tokens(total.input_tokens)
    );
    if total.cached_input_tokens > 0 {
        totals.push_str(&format!(
            " ({} cached)",
            format_tokens(total.cached_input_tokens)
        ));
    }
    totals.push_str(&format!(" · {} out", format_tokens(total.output_tokens)));
    match usage.model_context_window.filter(|window| *window > 0) {
        Some(window) => {
            let used = context_used_percent(in_context, window);
            ContextView {
                used_fraction: Some(used as f32 / 100.0),
                label: format!("{used}% used"),
                detail: format!(
                    "{} of {} tokens in context",
                    format_tokens(in_context),
                    format_tokens(window)
                ),
                totals,
            }
        }
        None => ContextView {
            used_fraction: None,
            label: format!("{} tokens", format_tokens(in_context)),
            detail: String::new(),
            totals,
        },
    }
}

/// New order after moving `ids[index]` by `delta` places, or `None` when it
/// would leave the list.
pub(crate) fn reorder_ids(ids: &[String], index: usize, delta: isize) -> Option<Vec<String>> {
    let target = index.checked_add_signed(delta)?;
    if index >= ids.len() || target >= ids.len() || target == index {
        return None;
    }
    let mut order = ids.to_vec();
    let moved = order.remove(index);
    order.insert(target, moved);
    Some(order)
}

/// Lines added and removed per file of a unified diff (git or plain).
pub(crate) fn parse_diff_stats(diff: &str) -> Vec<DiffFileStat> {
    #[derive(Default)]
    struct Pending {
        stat: DiffFileStat,
        old_path: Option<String>,
        new_path: Option<String>,
        git_path: Option<String>,
        saw_hunk: bool,
    }
    impl Pending {
        fn finish(self) -> DiffFileStat {
            let path = self
                .new_path
                .or(self.old_path)
                .or(self.git_path)
                .unwrap_or_default();
            DiffFileStat { path, ..self.stat }
        }
    }
    fn header_path(rest: &str, prefix: &str) -> Option<String> {
        // `similar` may append a tab and a timestamp to header paths.
        let rest = rest.split('\t').next().unwrap_or(rest).trim_end();
        if rest == "/dev/null" {
            return None;
        }
        Some(rest.strip_prefix(prefix).unwrap_or(rest).to_string())
    }
    fn hunk_counts(line: &str) -> Option<(u64, u64)> {
        let mut parts = line.strip_prefix("@@ ")?.split(' ');
        let count = |range: &str| match range.split_once(',') {
            Some((_, count)) => count.parse::<u64>().ok(),
            None => Some(1),
        };
        let old = count(parts.next()?.strip_prefix('-')?)?;
        let new = count(parts.next()?.strip_prefix('+')?)?;
        Some((old, new))
    }

    let mut files = Vec::new();
    let mut current: Option<Pending> = None;
    // Lines left in the current hunk (old side, new side).
    let mut remaining: Option<(u64, u64)> = None;
    for line in diff.lines() {
        if let Some((old, new)) = remaining.as_mut() {
            if let Some(file) = current.as_mut() {
                match line.as_bytes().first() {
                    Some(b'+') => {
                        file.stat.added += 1;
                        *new = new.saturating_sub(1);
                    }
                    Some(b'-') => {
                        file.stat.removed += 1;
                        *old = old.saturating_sub(1);
                    }
                    Some(b'\\') => {}
                    _ => {
                        *old = old.saturating_sub(1);
                        *new = new.saturating_sub(1);
                    }
                }
            }
            if *old == 0 && *new == 0 {
                remaining = None;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(file.finish());
            }
            current = Some(Pending {
                git_path: rest.rsplit_once(" b/").map(|(_, path)| path.to_string()),
                ..Pending::default()
            });
        } else if let Some(rest) = line.strip_prefix("--- ") {
            // Plain unified diffs start each file with `---`.
            let starts_new_file = current
                .as_ref()
                .is_none_or(|file| file.saw_hunk || file.old_path.is_some());
            if starts_new_file {
                if let Some(file) = current.take() {
                    files.push(file.finish());
                }
                current = Some(Pending::default());
            }
            if let Some(file) = current.as_mut() {
                file.old_path = header_path(rest, "a/");
                if file.old_path.is_none() {
                    file.stat.kind = FileChangeKind::Added;
                }
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(file) = current.as_mut() {
                file.new_path = header_path(rest, "b/");
                if file.new_path.is_none() {
                    file.stat.kind = FileChangeKind::Deleted;
                }
            }
        } else if line.starts_with("new file mode")
            && let Some(file) = current.as_mut()
        {
            file.stat.kind = FileChangeKind::Added;
        } else if line.starts_with("deleted file mode")
            && let Some(file) = current.as_mut()
        {
            file.stat.kind = FileChangeKind::Deleted;
        } else if line.starts_with("@@ ")
            && let Some(file) = current.as_mut()
        {
            file.saw_hunk = true;
            remaining = hunk_counts(line).filter(|(old, new)| *old > 0 || *new > 0);
        }
    }
    if let Some(file) = current.take() {
        files.push(file.finish());
    }
    files
}

fn limit_key(snapshot: &RateLimitSnapshot) -> String {
    snapshot
        .limit_id
        .clone()
        .unwrap_or_else(|| "codex".to_string())
}

/// Applies a sparse `account/rateLimits/updated` snapshot over the previous
/// one: fields the update leaves out keep their last known value.
pub(crate) fn merge_rate_limit(
    previous: Option<&RateLimitSnapshot>,
    update: RateLimitSnapshot,
) -> RateLimitSnapshot {
    let Some(previous) = previous else {
        return update;
    };
    let previous = previous.clone();
    RateLimitSnapshot {
        limit_id: update.limit_id.or(previous.limit_id),
        limit_name: update.limit_name.or(previous.limit_name),
        normal_model_slug: update.normal_model_slug.or(previous.normal_model_slug),
        primary: update.primary.or(previous.primary),
        secondary: update.secondary.or(previous.secondary),
        credits: update.credits.or(previous.credits),
        individual_limit: update.individual_limit.or(previous.individual_limit),
        spend_control_reached: update
            .spend_control_reached
            .or(previous.spend_control_reached),
        plan_type: update.plan_type.or(previous.plan_type),
        rate_limit_reached_type: update.rate_limit_reached_type,
    }
}

/// Name of a rate-limit window ("5-hour limit", "Weekly limit").
pub(crate) fn window_label(minutes: Option<i64>, secondary: bool) -> String {
    const HOUR: i64 = 60;
    const NAMED: [(i64, &str); 5] = [
        (5 * HOUR, "5-hour limit"),
        (24 * HOUR, "Daily limit"),
        (7 * 24 * HOUR, "Weekly limit"),
        (30 * 24 * HOUR, "Monthly limit"),
        (365 * 24 * HOUR, "Annual limit"),
    ];
    let Some(minutes) = minutes.filter(|minutes| *minutes > 0) else {
        return if secondary {
            "Secondary limit"
        } else {
            "Usage limit"
        }
        .to_string();
    };
    if let Some((_, name)) = NAMED
        .iter()
        .find(|(window, _)| (minutes - window).abs() * 20 <= *window)
    {
        return (*name).to_string();
    }
    if minutes % HOUR == 0 {
        format!("{}-hour limit", minutes / HOUR)
    } else {
        format!("{minutes}-minute limit")
    }
}

/// One rendered usage-limit row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LimitRowView {
    pub(crate) label: String,
    pub(crate) used: f32,
    pub(crate) percent: String,
    pub(crate) detail: String,
}

/// Rows for every window of every bucket; the default "codex" bucket first.
pub(crate) fn limit_row_views(
    snapshots: &BTreeMap<String, RateLimitSnapshot>,
    now: i64,
) -> Vec<LimitRowView> {
    let mut buckets: Vec<(&String, &RateLimitSnapshot)> = snapshots.iter().collect();
    buckets.sort_by_key(|(id, _)| !id.eq_ignore_ascii_case("codex"));
    let mut rows = Vec::new();
    for (id, snapshot) in buckets {
        let prefix = (!id.eq_ignore_ascii_case("codex"))
            .then(|| snapshot.limit_name.clone().unwrap_or_else(|| id.clone()));
        let windows: [(Option<&RateLimitWindow>, bool); 2] = [
            (snapshot.primary.as_ref(), false),
            (snapshot.secondary.as_ref(), true),
        ];
        for (window, secondary) in windows {
            let Some(window) = window else {
                continue;
            };
            let name = window_label(window.window_duration_mins, secondary);
            let label = match &prefix {
                Some(prefix) => format!("{prefix} · {name}"),
                None => name,
            };
            let used = window.used_percent.clamp(0, 100);
            let detail = window
                .resets_at
                .filter(|resets_at| *resets_at > now)
                .map(|resets_at| format!("Resets in {}", format_duration(resets_at - now)))
                .unwrap_or_default();
            rows.push(LimitRowView {
                label,
                used: used as f32 / 100.0,
                percent: format!("{used}% used"),
                detail,
            });
        }
    }
    rows
}

fn limit_rows(snapshots: &BTreeMap<String, RateLimitSnapshot>, now: i64) -> Vec<InfoLimit> {
    limit_row_views(snapshots, now)
        .into_iter()
        .map(|row| InfoLimit {
            label: row.label.into(),
            used: row.used,
            percent: row.percent.into(),
            detail: row.detail.into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::TokenUsageBreakdown;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn breakdown(total: i64) -> TokenUsageBreakdown {
        TokenUsageBreakdown {
            total_tokens: total,
            input_tokens: total - total / 10,
            cached_input_tokens: total / 4,
            cache_write_input_tokens: 0,
            output_tokens: total / 10,
            reasoning_output_tokens: 0,
        }
    }

    #[test]
    fn token_counts_are_compact() {
        assert_eq!(format_tokens(-5), "0");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(1_000), "1K");
        assert_eq!(format_tokens(1_234), "1.2K");
        assert_eq!(format_tokens(12_345), "12K");
        assert_eq!(format_tokens(272_000), "272K");
        assert_eq!(format_tokens(999_499), "999K");
        assert_eq!(format_tokens(999_500), "1M");
        assert_eq!(format_tokens(1_540_000), "1.5M");
        assert_eq!(format_tokens(25_000_000), "25M");
    }

    #[test]
    fn context_gauge_matches_tui_baseline() {
        assert_eq!(context_used_percent(0, 272_000), 0);
        assert_eq!(context_used_percent(12_000, 272_000), 0);
        assert_eq!(context_used_percent(142_000, 272_000), 50);
        assert_eq!(context_used_percent(272_000, 272_000), 100);
        assert_eq!(context_used_percent(400_000, 272_000), 100);
        assert_eq!(context_used_percent(5_000, 10_000), 100);
    }

    #[test]
    fn context_view_reports_window_and_totals() {
        let usage = ThreadTokenUsage {
            total: breakdown(40_000),
            last: breakdown(142_000),
            model_context_window: Some(272_000),
        };
        let view = context_view(&usage);
        assert_eq!(view.used_fraction, Some(0.5));
        assert_eq!(view.label, "50% used");
        assert_eq!(view.detail, "142K of 272K tokens in context");
        assert_eq!(
            view.totals,
            "Thread total 40K · 36K in (10K cached) · 4K out"
        );

        let no_window = ThreadTokenUsage {
            model_context_window: None,
            ..usage
        };
        let view = context_view(&no_window);
        assert_eq!(view.used_fraction, None);
        assert_eq!(view.label, "142K tokens");
    }

    #[test]
    fn reorder_moves_one_place_within_bounds() {
        let ids: Vec<String> = ["a", "b", "c"].iter().map(ToString::to_string).collect();
        assert_eq!(
            reorder_ids(&ids, 1, -1),
            Some(vec!["b".to_string(), "a".to_string(), "c".to_string()])
        );
        assert_eq!(
            reorder_ids(&ids, 1, 1),
            Some(vec!["a".to_string(), "c".to_string(), "b".to_string()])
        );
        assert_eq!(reorder_ids(&ids, 0, -1), None);
        assert_eq!(reorder_ids(&ids, 2, 1), None);
        assert_eq!(reorder_ids(&ids, 5, -1), None);
    }

    #[test]
    fn parses_git_turn_diff() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn a() {}
-fn b() {}
+fn b() { 1 }
+fn c() {}
 fn d() {}
diff --git a/hello file.txt b/hello file.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/hello file.txt
@@ -0,0 +1,2 @@
+--- looks like a header
+second
diff --git a/old.txt b/old.txt
deleted file mode 100644
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-gone
";
        assert_eq!(
            parse_diff_stats(diff),
            vec![
                DiffFileStat {
                    path: "src/lib.rs".to_string(),
                    added: 2,
                    removed: 1,
                    kind: FileChangeKind::Modified,
                },
                DiffFileStat {
                    path: "hello file.txt".to_string(),
                    added: 2,
                    removed: 0,
                    kind: FileChangeKind::Added,
                },
                DiffFileStat {
                    path: "old.txt".to_string(),
                    added: 0,
                    removed: 1,
                    kind: FileChangeKind::Deleted,
                },
            ]
        );
    }

    #[test]
    fn parses_plain_unified_diff_and_empty_input() {
        let diff = "\
--- a/one.txt
+++ b/one.txt
@@ -1,2 +1,2 @@
-x
+y
 z
--- a/two.txt
+++ b/two.txt
@@ -1 +1,2 @@
 keep
+add
";
        let stats = parse_diff_stats(diff);
        assert_eq!(
            stats
                .iter()
                .map(|stat| (stat.path.as_str(), stat.added, stat.removed))
                .collect::<Vec<_>>(),
            vec![("one.txt", 1, 1), ("two.txt", 1, 0)]
        );
        assert_eq!(parse_diff_stats(""), Vec::new());
    }

    #[test]
    fn sparse_rate_limit_updates_keep_known_fields() {
        let window = |used: i32| RateLimitWindow {
            used_percent: used,
            window_duration_mins: Some(300),
            resets_at: Some(1_000),
        };
        let full = RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            normal_model_slug: None,
            primary: Some(window(10)),
            secondary: Some(window(20)),
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        };
        let update = RateLimitSnapshot {
            primary: Some(window(15)),
            secondary: None,
            ..full.clone()
        };
        let merged = merge_rate_limit(Some(&full), update);
        assert_eq!(merged.primary, Some(window(15)));
        assert_eq!(merged.secondary, Some(window(20)));
    }

    #[test]
    fn rate_limit_rows_and_labels() {
        assert_eq!(window_label(Some(300), /*secondary*/ false), "5-hour limit");
        assert_eq!(
            window_label(Some(10_080), /*secondary*/ true),
            "Weekly limit"
        );
        assert_eq!(window_label(Some(120), /*secondary*/ false), "2-hour limit");
        assert_eq!(window_label(None, /*secondary*/ true), "Secondary limit");

        let snapshot = RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            normal_model_slug: None,
            primary: Some(RateLimitWindow {
                used_percent: 42,
                window_duration_mins: Some(300),
                resets_at: Some(1_000 + 7_500),
            }),
            secondary: None,
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        };
        let snapshots = BTreeMap::from([("codex".to_string(), snapshot)]);
        assert_eq!(
            limit_row_views(&snapshots, 1_000),
            vec![LimitRowView {
                label: "5-hour limit".to_string(),
                used: 0.42,
                percent: "42% used".to_string(),
                detail: "Resets in 2h 5m".to_string(),
            }]
        );
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(720), "12m");
        assert_eq!(format_duration(7_200), "2h");
        assert_eq!(format_duration(7_500), "2h 5m");
        assert_eq!(format_duration(86_400 * 3 + 3_600 * 4 + 60), "3d 4h");
        assert_eq!(format_duration(86_400), "1d");
    }

    #[test]
    fn sub_agents_merge_updates_from_items_and_threads() {
        let mut agents = Vec::new();
        let spawn = ThreadItem::CollabAgentToolCall {
            id: "call".to_string(),
            tool: CollabAgentTool::SpawnAgent,
            status: codex_app_server_protocol::CollabAgentToolCallStatus::Completed,
            sender_thread_id: "parent".to_string(),
            receiver_thread_ids: vec!["child".to_string()],
            prompt: Some("look into tests".to_string()),
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        assert!(apply_agent_updates(&mut agents, &spawn));
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, AgentStatus::Running);
        assert_eq!(agents[0].label(), "Agent child");

        let activity = ThreadItem::SubAgentActivity {
            id: "act".to_string(),
            kind: SubAgentActivityKind::Completed,
            agent_thread_id: "child".to_string(),
            agent_path: "/root/tester".to_string(),
        };
        apply_agent_updates(&mut agents, &activity);
        assert_eq!(agents[0].status, AgentStatus::Completed);
        assert_eq!(agents[0].label(), "tester");

        upsert_agent(
            &mut agents,
            AgentUpdate {
                thread_id: "child".to_string(),
                nickname: Some("Ada".to_string()),
                role: Some("reviewer".to_string()),
                ..AgentUpdate::default()
            },
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].label(), "Ada");
        assert_eq!(agents[0].detail(), "reviewer");
        assert_eq!(agents[0].status, AgentStatus::Completed);
    }

    #[test]
    fn labels_summaries_and_previews() {
        assert_eq!(diff_summary(1, 2, 0), "1 file · +2 −0");
        assert_eq!(diff_summary(3, 10, 4), "3 files · +10 −4");
        assert_eq!(
            mcp_summary(
                [
                    McpServerStartupState::Failed,
                    McpServerStartupState::Ready,
                    McpServerStartupState::Ready,
                ]
                .into_iter()
            ),
            "2 ready · 1 failed"
        );
        assert_eq!(file_name("src/ui/app.slint"), "app.slint");
        assert_eq!(file_name(r"C:\x\y.txt"), "y.txt");
        assert_eq!(relative_age(100, 100), "now");
        assert_eq!(relative_age(1_000, 400), "10m ago");
        assert_eq!(
            sandbox_label(&SandboxPolicy::ReadOnly {
                network_access: true
            }),
            "Read only + network"
        );
    }

    fn snapshot(limit_id: &str, used: i32) -> RateLimitSnapshot {
        RateLimitSnapshot {
            limit_id: Some(limit_id.to_string()),
            limit_name: None,
            normal_model_slug: None,
            primary: Some(RateLimitWindow {
                used_percent: used,
                window_duration_mins: Some(300),
                resets_at: None,
            }),
            secondary: None,
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        }
    }

    fn used(limits: &RateLimits, key: &str) -> Option<i32> {
        limits
            .snapshots
            .get(key)
            .and_then(|snapshot| snapshot.primary.as_ref())
            .map(|window| window.used_percent)
    }

    #[test]
    fn rate_limit_reads_from_before_an_account_change_are_dropped() {
        let mut limits = RateLimits::default();
        let first = limits.begin_read().expect("read starts");
        // Only one read at a time.
        assert_eq!(limits.begin_read(), None);
        // Login, logout or account switch: the first read is for the old
        // account, and a new read starts.
        limits.reset();
        let second = limits.begin_read().expect("new read starts");
        assert_ne!(first, second);
        // The old account's answer arrives first and is dropped...
        assert!(!limits.finish_read(
            first,
            Some(BTreeMap::from([(
                "codex".to_string(),
                snapshot("codex", 90)
            )]))
        ));
        assert_eq!(limits.fetch, Fetch::Loading);
        assert_eq!(used(&limits, "codex"), None);
        // ...and the new one is applied.
        assert!(limits.finish_read(
            second,
            Some(BTreeMap::from([(
                "codex".to_string(),
                snapshot("codex", 5)
            )]))
        ));
        assert_eq!(limits.fetch, Fetch::Done);
        assert_eq!(used(&limits, "codex"), Some(5));
        // A failure from a superseded read (old server) cannot mark the
        // limits unavailable either.
        limits.invalidate();
        assert!(!limits.finish_read(second, None));
        assert_eq!(limits.fetch, Fetch::NotRequested);
        assert_eq!(used(&limits, "codex"), Some(5));
    }

    #[test]
    fn rate_limit_updates_during_a_read_win_over_it() {
        let mut limits = RateLimits::default();
        // Known before the read: a stale secondary window.
        limits.on_update(RateLimitSnapshot {
            secondary: Some(RateLimitWindow {
                used_percent: 99,
                window_duration_mins: Some(10_080),
                resets_at: None,
            }),
            ..snapshot("codex", 1)
        });
        let seq = limits.begin_read().expect("read starts");
        // A sparse update (primary only) while the read is in flight.
        limits.on_update(snapshot("codex", 40));
        // A notification does not end the read.
        assert_eq!(limits.fetch, Fetch::Loading);
        assert!(limits.finish_read(
            seq,
            Some(BTreeMap::from([
                ("codex".to_string(), snapshot("codex", 30)),
                ("other".to_string(), snapshot("other", 7)),
            ]))
        ));
        assert_eq!(used(&limits, "codex"), Some(40));
        assert_eq!(used(&limits, "other"), Some(7));
        // The read's (missing) secondary window is not replaced by the stale
        // one known before the read.
        assert_eq!(
            limits
                .snapshots
                .get("codex")
                .and_then(|snapshot| snapshot.secondary.as_ref()),
            None
        );
        // A failed read keeps what notifications delivered.
        let seq = limits.begin_read().expect("read starts");
        assert!(limits.finish_read(seq, None));
        assert_eq!(limits.fetch, Fetch::Done);
        let mut empty = RateLimits::default();
        let seq = empty.begin_read().expect("read starts");
        assert!(empty.finish_read(seq, None));
        assert_eq!(empty.fetch, Fetch::Unavailable);
    }

    fn hook(id: &str, turn_id: Option<&str>, mode: &str) -> RunningHook {
        let run: HookRunSummary = serde_json::from_value(serde_json::json!({
            "id": id,
            "eventName": "preToolUse",
            "handlerType": "command",
            "executionMode": mode,
            "scope": "turn",
            "sourcePath": std::env::temp_dir().join("hooks.json"),
            "displayOrder": 0,
            "status": "running",
            "statusMessage": null,
            "startedAt": 0,
            "completedAt": null,
            "durationMs": null,
            "entries": [],
        }))
        .expect("valid hook json");
        RunningHook {
            turn_id: turn_id.map(str::to_string),
            run,
        }
    }

    #[test]
    fn completed_turns_clear_their_synchronous_hooks() {
        let mut hooks = vec![
            hook("sync-this-turn", Some("t1"), "sync"),
            hook("async-this-turn", Some("t1"), "async"),
            hook("sync-other-turn", Some("t2"), "sync"),
            hook("sync-no-turn", None, "sync"),
        ];
        prune_hooks_after_turn(&mut hooks, "t1");
        let left: Vec<&str> = hooks.iter().map(|hook| hook.run.id.as_str()).collect();
        assert_eq!(left, vec!["async-this-turn", "sync-other-turn"]);
    }

    #[test]
    fn mcp_status_maps_to_startup_states() {
        let status = |runtime: Option<&str>, tools_error: Option<&str>| -> McpServerStatus {
            serde_json::from_value(serde_json::json!({
                "name": "docs",
                "runtimeStatus": runtime,
                "pluginId": null,
                "httpOrigin": null,
                "serverInfo": null,
                "serverCapabilities": null,
                "tools": {},
                "toolsError": tools_error,
                "resources": [],
                "resourceTemplates": [],
                "authStatus": "unsupported",
            }))
            .expect("valid status json")
        };
        assert_eq!(
            mcp_entry_from_status(&status(Some("connected"), None)),
            Some(McpEntry {
                state: McpServerStartupState::Ready,
                error: None,
            })
        );
        assert_eq!(
            mcp_entry_from_status(&status(Some("failed"), Some("spawn failed"))),
            Some(McpEntry {
                state: McpServerStartupState::Failed,
                error: Some("spawn failed".to_string()),
            })
        );
        assert_eq!(
            mcp_entry_from_status(&status(Some("authenticationRequired"), None))
                .map(|entry| entry.state),
            Some(McpServerStartupState::Failed)
        );
        assert_eq!(mcp_entry_from_status(&status(Some("disabled"), None)), None);
        assert_eq!(mcp_entry_from_status(&status(None, None)), None);
    }

    #[test]
    fn summary_uses_pending_overrides_for_every_setting() {
        let mut thread = ThreadTab::new(std::path::PathBuf::from("/repo"));
        thread.model = Some("server-model".to_string());
        thread.approval_policy = Some(AskForApproval::OnRequest);
        thread.sandbox = Some(SandboxPolicy::ReadOnly {
            network_access: false,
        });
        // "Full access" chosen before the thread started (or without
        // thread/settings/update): only the turn overrides know it.
        thread.turn_overrides.approval_policy = Some(AskForApproval::Never);
        thread.turn_overrides.sandbox_policy = Some(SandboxPolicy::DangerFullAccess);
        let fields = summary_fields(&thread);
        let value = |label: &str| {
            fields
                .iter()
                .find(|(name, _)| *name == label)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value("Model"), Some("server-model".to_string()));
        assert_eq!(value("Approval"), Some("Never ask".to_string()));
        assert_eq!(value("Sandbox"), Some("Full access".to_string()));

        thread.turn_overrides = crate::session::TurnOverrides::default();
        thread.approvals_reviewer = Some(ApprovalsReviewer::AutoReview);
        let fields = summary_fields(&thread);
        assert!(
            fields
                .iter()
                .any(|(name, value)| *name == "Approval" && value == "On request · auto-review")
        );
    }

    #[test]
    fn status_text_lists_settings_context_and_limits() {
        let fields = vec![
            ("Model", "gpt-test".to_string()),
            ("Sandbox", "Read only".to_string()),
        ];
        let context = ContextView {
            used_fraction: Some(0.25),
            label: "25% used".to_string(),
            detail: "50K of 200K tokens in context".to_string(),
            totals: "Thread total 80K · 70K in · 10K out".to_string(),
        };
        let limits = vec![LimitRowView {
            label: "5-hour limit".to_string(),
            used: 0.1,
            percent: "10% used".to_string(),
            detail: "Resets in 2h".to_string(),
        }];
        assert_eq!(
            status_text(&fields, Some(&context), &limits),
            "Model: gpt-test\n\
             Sandbox: Read only\n\
             Context: 25% used (50K of 200K tokens in context)\n\
             Thread total 80K · 70K in · 10K out\n\
             5-hour limit: 10% used · Resets in 2h\n\
             \n\
             Widen the window to see the info pane."
        );
        assert!(
            status_text(&fields, None, &[]).starts_with("Model: gpt-test\nSandbox: Read only\n\n")
        );
    }
}
