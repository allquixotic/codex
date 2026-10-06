//! Approvals and other server requests that need a user decision.
//!
//! Every server request except dynamic tool calls lands in
//! [`AppController::approvals_on_request`]. Requests are routed to the tab of
//! the thread that sent them; requests from threads without a tab (sub-agents)
//! go to the tab of their parent, found with `thread/read`, and fall back to
//! the active thread tab. Each tab queues its requests in [`PendingApprovals`]
//! and the active tab's oldest request is shown as a card between the
//! transcript and the composer.
//!
//! Every request is answered exactly once: by the user, automatically for the
//! kinds codex-gui cannot show, or dropped when the server reports it resolved
//! (`serverRequest/resolved`, turn end, server restart). Closing a tab cancels
//! its own thread's requests and moves requests from other threads (sub-agents)
//! to another open tab, or cancels them when none is left.
//!
//! The same card asks for consent before a cross-tab message from another
//! tab's agent is delivered ([`DeliveryRequest`]); that answer goes to
//! `crate::xtab` instead of the server.
//!
//! The card takes keyboard focus only from an empty composer or from nothing,
//! never from another input or from under a dialog, and it ignores keys and
//! clicks for [`INPUT_GUARD`] after it appears or changes, so typing meant for
//! something else cannot answer it.

mod delivery;
mod elicitation;
mod format;
mod request;
mod user_input;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::McpServerElicitationAction;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemsListParams;
use codex_app_server_protocol::ThreadItemsListResponse;
use codex_app_server_protocol::ThreadReadResponse;
use codex_protocol::protocol::SubAgentSource;
use slint::ComponentHandle;
use slint::Model;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

pub(crate) use self::delivery::DeliveryChoice;
pub(crate) use self::delivery::DeliveryRequest;
use self::format::DiffLine;
use self::format::DiffLineKind;
use self::request::Answer;
use self::request::ApprovalOption;
use self::request::CardBody;
use self::request::CardContext;
use self::request::CardView;
use self::request::Decision;
use self::request::FieldKind;
use self::request::FieldView;
use self::request::Intake;
use self::request::PendingRequest;
use self::request::RequestKind;
use self::request::Tone;
use crate::app::AppController;
use crate::app::TabId;
use crate::backend::BackendError;
use crate::transcript::NoticeKind;
use crate::ui::ApprovalCardKind;
use crate::ui::ApprovalChoiceData;
use crate::ui::ApprovalDiffKind;
use crate::ui::ApprovalDiffLine;
use crate::ui::ApprovalFieldData;
use crate::ui::ApprovalFieldKind;
use crate::ui::ApprovalOptionData;
use crate::ui::ApprovalTone;
use crate::ui::ApprovalsState;

/// Sub-agent parents followed when routing a request to a tab.
const MAX_ROUTE_DEPTH: usize = 8;
/// File-change items whose changes are remembered for approval cards.
const FILE_CHANGE_CACHE_CAPACITY: usize = 64;
/// Longest origin label shown in the card header.
const ORIGIN_LABEL_CHARS: usize = 40;
/// Keys and clicks are ignored this long after the card appears, changes or
/// regains focus with the window.
const INPUT_GUARD: Duration = Duration::from_millis(400);

/// Holds off answers for [`INPUT_GUARD`] after the card was (re)armed, so a
/// key or click meant for something else (a word being typed elsewhere, a
/// double click, a held Enter on the previous card) does not answer it.
#[derive(Debug, Default)]
struct InputGuard {
    armed_at: Option<Instant>,
}

impl InputGuard {
    fn arm(&mut self, now: Instant) {
        self.armed_at = Some(now);
    }

    fn is_ready(&self, now: Instant) -> bool {
        self.armed_at
            .is_none_or(|armed_at| now.saturating_duration_since(armed_at) >= INPUT_GUARD)
    }
}

/// What closing a tab does with one of the requests it shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OnClose {
    /// The tab's own thread asked; the thread is going away.
    Cancel,
    /// Another thread asked (a sub-agent, or a thread without a tab).
    Move,
    /// A cross-tab message waiting to be delivered to the closed tab.
    Decline,
}

fn on_close(request: &PendingRequest, closed_thread_id: Option<&str>) -> OnClose {
    if matches!(request.kind, RequestKind::Delivery(_)) {
        OnClose::Decline
    } else if closed_thread_id == Some(request.thread_id.as_str()) {
        OnClose::Cancel
    } else {
        OnClose::Move
    }
}

/// Confirmation text for closing a tab that still shows requests: what
/// happens to each kind.
fn close_warning(requests: &[OnClose], other_thread_tabs: bool) -> Option<String> {
    if requests.is_empty() {
        return None;
    }
    let count = |kind: OnClose| requests.iter().filter(|request| **request == kind).count();
    let total = requests.len();
    let mut text = if total == 1 {
        "A request in this tab is waiting for your answer.".to_string()
    } else {
        format!("{total} requests in this tab are waiting for your answer.")
    };
    if count(OnClose::Cancel) > 0 {
        text.push_str(" Closing the tab cancels the ones from this thread.");
    }
    if count(OnClose::Move) > 0 {
        text.push_str(if other_thread_tabs {
            " Requests from sub-agents and other threads move to another tab."
        } else {
            " Requests from sub-agents and other threads are cancelled, because no other thread tab is open."
        });
    }
    if count(OnClose::Decline) > 0 {
        text.push_str(" Messages other tabs want to send here are declined.");
    }
    Some(text)
}

/// Requests waiting for a decision on one thread tab, oldest first.
#[derive(Debug, Default)]
pub(crate) struct PendingApprovals {
    requests: Vec<PendingRequest>,
}

impl PendingApprovals {
    pub(crate) fn len(&self) -> usize {
        self.requests.len()
    }

    fn contains(&self, request_id: &RequestId) -> bool {
        self.requests
            .iter()
            .any(|request| &request.request_id == request_id)
    }

    /// Queues `request`; a replayed request id is ignored.
    fn push(&mut self, request: PendingRequest) -> bool {
        if self.contains(&request.request_id) {
            return false;
        }
        self.requests.push(request);
        true
    }

    fn take(&mut self, request_id: &RequestId) -> Option<PendingRequest> {
        let position = self
            .requests
            .iter()
            .position(|request| &request.request_id == request_id)?;
        Some(self.requests.remove(position))
    }

    fn front(&self) -> Option<&PendingRequest> {
        self.requests.first()
    }

    fn front_mut(&mut self) -> Option<&mut PendingRequest> {
        self.requests.first_mut()
    }

    /// Drops requests matching `predicate`; returns how many were dropped.
    fn drop_where(&mut self, predicate: impl Fn(&PendingRequest) -> bool) -> usize {
        let before = self.requests.len();
        self.requests.retain(|request| !predicate(request));
        before - self.requests.len()
    }
}

/// Where a request from `thread_id` should be shown.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Placement {
    Tab {
        index: usize,
        /// Label of the requesting thread when it is not the tab's own.
        origin: Option<String>,
    },
    /// The owner of this thread is unknown; read it first.
    Lookup(String),
    /// No tab shows this thread or any of its ancestors.
    NoTab,
}

/// Parent of a thread that has no tab of its own.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Route {
    parent_thread_id: Option<String>,
    label: String,
}

/// Follows sub-agent parents from `thread_id` to the first thread with a tab.
fn place(
    thread_id: &str,
    routes: &HashMap<String, Route>,
    unreadable: &HashSet<String>,
    tab_for_thread: impl Fn(&str) -> Option<usize>,
) -> Placement {
    let mut current = thread_id.to_string();
    let mut origin: Option<String> = None;
    for _ in 0..MAX_ROUTE_DEPTH {
        if let Some(index) = tab_for_thread(&current) {
            return Placement::Tab { index, origin };
        }
        if unreadable.contains(&current) {
            return Placement::NoTab;
        }
        match routes.get(&current) {
            None => return Placement::Lookup(current),
            Some(route) => {
                if origin.is_none() {
                    origin = Some(route.label.clone());
                }
                match route.parent_thread_id.as_ref() {
                    Some(parent) => current = parent.clone(),
                    None => return Placement::NoTab,
                }
            }
        }
    }
    Placement::NoTab
}

/// Parent and display label of `thread`, from its session source.
fn route_for_thread(thread: &Thread) -> Route {
    let (spawn_parent, spawn_nickname, spawn_role) = match &thread.source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_nickname,
            agent_role,
            ..
        }) => (
            Some(parent_thread_id.to_string()),
            agent_nickname.clone(),
            agent_role.clone(),
        ),
        _ => (None, None, None),
    };
    let nickname = thread
        .agent_nickname
        .clone()
        .or(spawn_nickname)
        .filter(|name| !name.trim().is_empty());
    let role = thread
        .agent_role
        .clone()
        .or(spawn_role)
        .filter(|role| !role.trim().is_empty());
    let parent_thread_id = spawn_parent.or_else(|| thread.parent_thread_id.clone());
    let label = match (nickname, role, parent_thread_id.is_some()) {
        (Some(nickname), Some(role), _) => format!("Sub-agent {nickname} ({role})"),
        (Some(nickname), None, _) => format!("Sub-agent {nickname}"),
        (None, Some(role), _) => format!("Sub-agent ({role})"),
        (None, None, true) => "Sub-agent".to_string(),
        (None, None, false) => other_thread_label(&thread.id),
    };
    Route {
        parent_thread_id,
        label: crate::app::truncate_chars(&label, ORIGIN_LABEL_CHARS),
    }
}

fn other_thread_label(thread_id: &str) -> String {
    let short: String = thread_id.chars().take(8).collect();
    format!("Thread {short}")
}

/// Changes of recent file-change items, keyed by (thread id, item id). The
/// approval request does not carry the diff, so it is remembered from the
/// item notifications.
#[derive(Debug, Default)]
struct FileChangeCache {
    entries: VecDeque<((String, String), Vec<FileUpdateChange>)>,
}

impl FileChangeCache {
    fn insert(&mut self, thread_id: &str, item_id: &str, changes: Vec<FileUpdateChange>) {
        self.remove(thread_id, item_id);
        if self.entries.len() >= FILE_CHANGE_CACHE_CAPACITY {
            self.entries.pop_front();
        }
        self.entries
            .push_back(((thread_id.to_string(), item_id.to_string()), changes));
    }

    fn get(&self, thread_id: &str, item_id: &str) -> Option<&[FileUpdateChange]> {
        self.entries
            .iter()
            .find(|((thread, item), _)| thread == thread_id && item == item_id)
            .map(|(_, changes)| changes.as_slice())
    }

    fn remove(&mut self, thread_id: &str, item_id: &str) {
        self.entries
            .retain(|((thread, item), _)| !(thread == thread_id && item == item_id));
    }
}

/// Slint models behind the card.
struct CardModels {
    options: Rc<VecModel<ApprovalOptionData>>,
    details: Rc<VecModel<slint::StyledText>>,
    diff: Rc<VecModel<ApprovalDiffLine>>,
    fields: Rc<VecModel<ApprovalFieldData>>,
}

/// App-wide approvals state (the per-tab queues live in [`PendingApprovals`]).
#[derive(Default)]
pub(crate) struct ApprovalsController {
    file_changes: FileChangeCache,
    /// File-change items being fetched because their changes were not cached.
    fetching: HashSet<(String, String)>,
    /// Parents of threads that have no tab (filled by `thread/read`).
    routes: HashMap<String, Route>,
    /// Requests whose owner tab is still being looked up.
    awaiting_route: Vec<PendingRequest>,
    /// Threads with a `thread/read` in flight.
    lookups: HashSet<String>,
    /// Request on screen: the active tab and the request id.
    shown: Option<(TabId, RequestId)>,
    /// Options of the card on screen, in display order.
    options: Vec<ApprovalOption>,
    rendered_diff: Vec<DiffLine>,
    /// The card on screen, to arm the guard when it changes.
    rendered_card: Option<CardView>,
    guard: InputGuard,
    focus_serial: i32,
    models: Option<CardModels>,
}

impl AppController {
    pub(crate) fn approvals_bind(&mut self) {
        let models = CardModels {
            options: Rc::new(VecModel::default()),
            details: Rc::new(VecModel::default()),
            diff: Rc::new(VecModel::default()),
            fields: Rc::new(VecModel::default()),
        };
        let state = self.window.global::<ApprovalsState>();
        state.set_options(ModelRc::from(models.options.clone()));
        state.set_details(ModelRc::from(models.details.clone()));
        state.set_diff(ModelRc::from(models.diff.clone()));
        state.set_fields(ModelRc::from(models.fields.clone()));
        self.approvals.models = Some(models);

        state.on_choose(|index| {
            crate::ui_thread::with_app(move |app| {
                if let Ok(index) = usize::try_from(index)
                    && app.approvals_input_ready()
                {
                    app.approvals_choose(index);
                }
            });
        });
        state.on_key(|text| {
            let mut handled = false;
            crate::ui_thread::with_app_now(|app| handled = app.approvals_key(text.as_str()));
            handled
        });
        state.on_toggle_choice(|field, choice| {
            crate::ui_thread::with_app(move |app| {
                if let (Ok(field), Ok(choice)) = (usize::try_from(field), usize::try_from(choice)) {
                    app.approvals_toggle_choice(field, choice);
                }
            });
        });
        state.on_edit_text(|field, text| {
            let text = text.to_string();
            crate::ui_thread::with_app(move |app| {
                if let Ok(field) = usize::try_from(field) {
                    app.approvals_edit_text(field, text);
                }
            });
        });
        state.on_submit(|| {
            crate::ui_thread::with_app(|app| {
                if app.approvals_input_ready() {
                    app.approvals_submit();
                }
            });
        });
        state.on_secondary(|| {
            crate::ui_thread::with_app(|app| {
                if app.approvals_input_ready() {
                    app.approvals_secondary();
                }
            });
        });
        state.on_tertiary(|| {
            crate::ui_thread::with_app(|app| {
                if app.approvals_input_ready() {
                    app.approvals_tertiary();
                }
            });
        });
        state.on_rearm(|| {
            crate::ui_thread::with_app(|app| app.approvals.guard.arm(Instant::now()));
        });
        state.on_open_link(|| {
            crate::ui_thread::with_app(|app| {
                let link = app.window.global::<ApprovalsState>().get_link().to_string();
                app.approvals_open_url(&link);
            });
        });
        state.on_copy_code(|| {
            crate::ui_thread::with_app(|app| {
                let code = app.window.global::<ApprovalsState>().get_code().to_string();
                let code = code.strip_prefix("$ ").unwrap_or(&code).to_string();
                app.copy_to_clipboard(&code);
            });
        });
    }

    /// Number of undecided requests shown on tab `index`.
    pub(crate) fn approvals_pending_count(&self, index: usize) -> usize {
        self.thread_tab(index)
            .map_or(0, |thread| thread.approvals.len())
    }

    /// What closing tab `index` would do to the requests it shows, for the
    /// close confirmation; `None` when it shows none.
    pub(crate) fn approvals_close_warning(&self, index: usize) -> Option<String> {
        let thread = self.thread_tab(index)?;
        let closing: Vec<OnClose> = thread
            .approvals
            .requests
            .iter()
            .map(|request| on_close(request, thread.thread_id.as_deref()))
            .collect();
        let other_thread_tabs = self
            .tabs
            .iter()
            .enumerate()
            .any(|(other, tab)| other != index && tab.thread().is_some());
        close_warning(&closing, other_thread_tabs)
    }

    /// Answers or moves the requests of a thread tab that was just removed
    /// (`pending` is its queue), so no request is left without a card.
    pub(crate) fn approvals_on_tab_closed(
        &mut self,
        closed_thread_id: Option<&str>,
        pending: PendingApprovals,
    ) {
        for mut request in pending.requests {
            match on_close(&request, closed_thread_id) {
                OnClose::Decline => {
                    if let RequestKind::Delivery(delivery) = &request.kind {
                        self.xtab_delivery_dropped(
                            delivery.id,
                            "The receiving tab was closed before the user decided.",
                        );
                    }
                }
                OnClose::Cancel => self.approvals_auto_cancel(request),
                OnClose::Move => match self.approvals_reroute_target(&request.thread_id) {
                    Some((index, origin)) => {
                        request.origin = origin;
                        self.approvals_attach(index, request);
                    }
                    None => {
                        tracing::info!(
                            thread_id = %request.thread_id,
                            "no thread tab is left for a server request; cancelling it"
                        );
                        self.approvals_auto_cancel(request);
                    }
                },
            }
        }
        // The card on screen may have belonged to the closed tab.
        if self
            .approvals
            .shown
            .as_ref()
            .is_some_and(|(tab, _)| self.tab_index_by_id(*tab).is_none())
        {
            self.approvals.shown = None;
        }
    }

    /// Tab for a request whose tab was closed: the nearest open ancestor of
    /// its thread, else the active or any thread tab. Never opens a tab.
    fn approvals_reroute_target(&self, thread_id: &str) -> Option<(usize, Option<String>)> {
        if let Placement::Tab { index, origin } =
            self.approvals_placement(thread_id, &HashSet::new())
        {
            return Some((index, origin));
        }
        let index = self
            .active_thread_index()
            .or_else(|| self.tabs.iter().position(|tab| tab.thread().is_some()))?;
        let own_tab = self
            .thread_tab(index)
            .and_then(|thread| thread.thread_id.as_deref())
            == Some(thread_id);
        let origin = (!own_tab).then(|| {
            self.approvals
                .routes
                .get(thread_id)
                .map(|route| route.label.clone())
                .unwrap_or_else(|| other_thread_label(thread_id))
        });
        Some((index, origin))
    }

    /// Shows the consent card for a cross-tab message in its target tab
    /// `index`. `request_id`, `caller_thread_id` and `caller_turn_id` are the
    /// sender's tool call, so the card goes away with it.
    pub(crate) fn approvals_push_delivery(
        &mut self,
        index: usize,
        request_id: RequestId,
        caller_thread_id: String,
        caller_turn_id: String,
        delivery: DeliveryRequest,
    ) {
        let request = PendingRequest {
            request_id,
            thread_id: caller_thread_id,
            turn_id: Some(caller_turn_id),
            origin: None,
            kind: RequestKind::Delivery(Box::new(delivery)),
        };
        self.approvals_attach(index, request);
        self.refresh_tabs();
    }

    /// Removes consent cards of cross-tab messages the sender gave up on.
    pub(crate) fn approvals_remove_deliveries(&mut self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        self.approvals_dismiss(|request| {
            matches!(&request.kind, RequestKind::Delivery(delivery) if ids.contains(&delivery.id))
        });
    }

    /// Whether keys and clicks may answer the card now: no dialog covers
    /// it and the guard after its last change has passed.
    fn approvals_input_ready(&self) -> bool {
        !self.approvals_modal_open() && self.approvals.guard.is_ready(Instant::now())
    }

    /// A modal dialog or overlay is open. The card may still hold keyboard
    /// focus underneath it, but must not take keys meant for the dialog.
    fn approvals_modal_open(&self) -> bool {
        let app = self.window.global::<crate::ui::AppState>();
        app.get_dialog_open()
            || app.get_about_open()
            || self
                .window
                .global::<crate::ui::XtabState>()
                .get_dialog_open()
    }

    // ----- intake and routing -----------------------------------------------

    pub(crate) fn approvals_on_request(&mut self, server_request: ServerRequest) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or_default();
        match request::intake(server_request, now) {
            Intake::Pending(request) => self.approvals_route(*request),
            Intake::Resolve {
                request_id,
                thread_id,
                result,
                notice,
            } => {
                self.backend.resolve(request_id, result);
                self.approvals_note(thread_id.as_deref(), notice);
            }
            Intake::Reject {
                request_id,
                thread_id,
                error,
                notice,
            } => {
                tracing::info!(%request_id, message = %error.message, "rejected server request");
                self.backend.reject(request_id, error);
                self.approvals_note(thread_id.as_deref(), notice);
            }
        }
    }

    /// Records an automatic answer in the transcript of the owning tab.
    fn approvals_note(&mut self, thread_id: Option<&str>, notice: Option<String>) {
        let Some(text) = notice else {
            return;
        };
        let index = thread_id
            .and_then(|thread_id| self.tab_index_for_thread(thread_id))
            .or_else(|| self.active_thread_index());
        if let Some(index) = index {
            self.transcript_push_notice(index, NoticeKind::Warning, text);
        }
    }

    fn approvals_placement(&self, thread_id: &str, unreadable: &HashSet<String>) -> Placement {
        place(thread_id, &self.approvals.routes, unreadable, |thread| {
            self.tab_index_for_thread(thread)
        })
    }

    fn approvals_route(&mut self, request: PendingRequest) {
        self.approvals_route_with(request, &HashSet::new());
    }

    fn approvals_route_with(&mut self, mut request: PendingRequest, unreadable: &HashSet<String>) {
        match self.approvals_placement(&request.thread_id, unreadable) {
            Placement::Tab { index, origin } => {
                request.origin = origin;
                self.approvals_attach(index, request);
            }
            Placement::Lookup(thread_id) => {
                let pending = &mut self.approvals.awaiting_route;
                if !pending
                    .iter()
                    .any(|waiting| waiting.request_id == request.request_id)
                {
                    pending.push(request);
                }
                self.approvals_lookup(thread_id);
            }
            Placement::NoTab => {
                let label = self
                    .approvals
                    .routes
                    .get(&request.thread_id)
                    .map(|route| route.label.clone());
                match self.approvals_fallback_tab(&request.thread_id) {
                    Some(index) => {
                        let own_tab = self
                            .thread_tab(index)
                            .and_then(|thread| thread.thread_id.as_deref())
                            == Some(request.thread_id.as_str());
                        request.origin = if own_tab {
                            None
                        } else {
                            Some(label.unwrap_or_else(|| other_thread_label(&request.thread_id)))
                        };
                        self.approvals_attach(index, request);
                    }
                    None => {
                        // Not even a tab could be opened: answer so the turn
                        // does not hang.
                        tracing::warn!(
                            thread_id = %request.thread_id,
                            "no tab can show a server request; cancelling it"
                        );
                        self.approvals_auto_cancel(request);
                    }
                }
            }
        }
    }

    /// Tab for a request whose thread and ancestors have no tab: the active
    /// thread tab, any thread tab, or a new tab for the thread itself.
    fn approvals_fallback_tab(&mut self, thread_id: &str) -> Option<usize> {
        if let Some(index) = self
            .active_thread_index()
            .or_else(|| self.tabs.iter().position(|tab| tab.thread().is_some()))
        {
            return Some(index);
        }
        self.open_thread(thread_id.to_string(), /*cwd_hint*/ None);
        self.tab_index_for_thread(thread_id)
    }

    /// Reads `thread_id` to find the tab of its parent thread.
    fn approvals_lookup(&mut self, thread_id: String) {
        if !self.approvals.lookups.insert(thread_id.clone()) {
            return;
        }
        let read_id = thread_id.clone();
        self.backend.call(
            |request_id| crate::session::thread_read(request_id, &read_id),
            move |app, result: Result<ThreadReadResponse, BackendError>| {
                app.approvals_on_thread_read(thread_id, result);
            },
        );
    }

    fn approvals_on_thread_read(
        &mut self,
        thread_id: String,
        result: Result<ThreadReadResponse, BackendError>,
    ) {
        self.approvals.lookups.remove(&thread_id);
        let mut unreadable = HashSet::new();
        match result {
            Ok(response) => {
                let route = route_for_thread(&response.thread);
                self.approvals.routes.insert(thread_id, route);
            }
            Err(err) => {
                tracing::warn!(%thread_id, %err, "could not read thread to route a server request");
                unreadable.insert(thread_id);
            }
        }
        for request in std::mem::take(&mut self.approvals.awaiting_route) {
            self.approvals_route_with(request, &unreadable);
        }
        self.refresh_tabs();
    }

    /// Queues `request` on tab `index` and tells the user.
    fn approvals_attach(&mut self, index: usize, request: PendingRequest) {
        let file_change = request
            .file_change_item()
            .map(|(thread_id, item_id)| (thread_id.to_string(), item_id.to_string()));
        let turn_id = request.turn_id.clone();
        let body = match request.origin.as_deref() {
            Some(origin) => format!("{origin}: {}", request.notification_body()),
            None => request.notification_body(),
        };
        let Some(thread) = self.thread_tab_mut(index) else {
            self.approvals_auto_cancel(request);
            return;
        };
        let title = thread.title();
        if !thread.approvals.push(request) {
            return;
        }
        if let Some((thread_id, item_id)) = file_change
            && self
                .approvals
                .file_changes
                .get(&thread_id, &item_id)
                .is_none()
            && let Some(turn_id) = turn_id
        {
            self.approvals_fetch_file_change(thread_id, turn_id, item_id);
        }
        let background = !self.window_in_foreground() || self.active != Some(index);
        if self.active != Some(index) {
            self.tabs[index].unread = true;
        }
        if background {
            self.notify_desktop(&title, &body);
        }
        if self.active == Some(index) {
            self.approvals_show();
        }
    }

    /// Answers a request nobody can see with its cancel choice (or the
    /// equivalent empty answer) so the agent is never left waiting.
    fn approvals_auto_cancel(&mut self, request: PendingRequest) {
        let answer = match &request.kind {
            RequestKind::Delivery(delivery) => {
                // Not a server request: the sender's tool call is answered
                // by the cross-tab controller.
                self.xtab_delivery_dropped(
                    delivery.id,
                    "codex-gui could not show the message to the user.",
                );
                return;
            }
            RequestKind::UserInput(form) => form.skip_answer(),
            RequestKind::Elicitation(elicitation) if elicitation.is_form() => {
                elicitation.dismiss_answer(McpServerElicitationAction::Cancel)
            }
            _ => match request.cancel_option() {
                Some(option) => request.answer(&option.decision),
                None => Err(serde::ser::Error::custom("request has no cancel choice")),
            },
        };
        match answer {
            Ok(answer) => self.backend.resolve(request.request_id, answer.result),
            Err(err) => self.backend.reject(
                request.request_id,
                JSONRPCErrorError {
                    code: request::UNSUPPORTED_ERROR_CODE,
                    message: format!("codex-gui could not show this request: {err}"),
                    data: None,
                },
            ),
        }
    }

    fn approvals_fetch_file_change(&mut self, thread_id: String, turn_id: String, item_id: String) {
        let key = (thread_id.clone(), item_id.clone());
        if !self.approvals.fetching.insert(key.clone()) {
            return;
        }
        let params = ThreadItemsListParams {
            thread_id: thread_id.clone(),
            turn_id: Some(turn_id),
            cursor: None,
            limit: Some(crate::session::HISTORY_ITEM_PAGE_LIMIT),
            sort_direction: Some(SortDirection::Desc),
        };
        self.backend.call(
            |request_id| ClientRequest::ThreadItemsList { request_id, params },
            move |app, result: Result<ThreadItemsListResponse, BackendError>| {
                app.approvals.fetching.remove(&key);
                let response = match result {
                    Ok(response) => response,
                    Err(err) => {
                        tracing::debug!(%err, "could not fetch file changes for an approval");
                        return;
                    }
                };
                let changes = response
                    .data
                    .into_iter()
                    .find_map(|entry| match entry.item {
                        ThreadItem::FileChange { id, changes, .. } if id == item_id => {
                            Some(changes)
                        }
                        _ => None,
                    });
                if let Some(changes) = changes
                    && app
                        .approvals
                        .file_changes
                        .get(&thread_id, &item_id)
                        .is_none()
                {
                    app.approvals
                        .file_changes
                        .insert(&thread_id, &item_id, changes);
                    app.approvals_show();
                }
            },
        );
    }

    // ----- notifications ----------------------------------------------------

    pub(crate) fn approvals_on_notification(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::ItemStarted(started) => {
                if let ThreadItem::FileChange { id, changes, .. } = &started.item {
                    self.approvals_cache_changes(&started.thread_id, id, changes.clone());
                }
            }
            ServerNotification::FileChangePatchUpdated(updated) => {
                self.approvals_cache_changes(
                    &updated.thread_id,
                    &updated.item_id,
                    updated.changes.clone(),
                );
            }
            ServerNotification::ItemCompleted(completed) => {
                if let ThreadItem::FileChange { id, .. } = &completed.item {
                    self.approvals.file_changes.remove(&completed.thread_id, id);
                }
            }
            ServerNotification::ServerRequestResolved(resolved) => {
                self.approvals_dismiss(|request| {
                    request.request_id == resolved.request_id
                        && request.thread_id == resolved.thread_id
                });
            }
            ServerNotification::TurnCompleted(completed) => {
                // The server abandons a turn's requests when it ends.
                let turn_id = completed.turn.id.as_str();
                self.approvals_dismiss(|request| {
                    request.thread_id == completed.thread_id
                        && request.turn_id.as_deref() == Some(turn_id)
                });
            }
            ServerNotification::ThreadClosed(closed) => {
                self.approvals_dismiss(|request| request.thread_id == closed.thread_id);
            }
            _ => {}
        }
    }

    fn approvals_cache_changes(
        &mut self,
        thread_id: &str,
        item_id: &str,
        changes: Vec<FileUpdateChange>,
    ) {
        self.approvals
            .file_changes
            .insert(thread_id, item_id, changes);
        let shown = self
            .approvals_active_front()
            .is_some_and(|(_, request)| request.file_change_item() == Some((thread_id, item_id)));
        if shown {
            self.approvals_show();
        }
    }

    /// Drops requests the server no longer waits for.
    fn approvals_dismiss(&mut self, predicate: impl Fn(&PendingRequest) -> bool) {
        let mut dropped = 0;
        for tab in &mut self.tabs {
            if let Some(thread) = tab.thread_mut() {
                dropped += thread.approvals.drop_where(&predicate);
            }
        }
        let before = self.approvals.awaiting_route.len();
        self.approvals
            .awaiting_route
            .retain(|request| !predicate(request));
        dropped += before - self.approvals.awaiting_route.len();
        if dropped > 0 {
            self.approvals_show();
            self.refresh_tabs();
        }
    }

    /// Forgets every pending request; their server is gone (restart or
    /// failure), so they can no longer be answered.
    pub(crate) fn approvals_reset(&mut self) {
        for tab in &mut self.tabs {
            if let Some(thread) = tab.thread_mut() {
                thread.approvals = PendingApprovals::default();
            }
        }
        self.approvals.awaiting_route.clear();
        self.approvals.lookups.clear();
        self.approvals.fetching.clear();
        self.approvals.file_changes = FileChangeCache::default();
        self.approvals_show();
    }

    // ----- the card ---------------------------------------------------------

    /// The active tab's index and its oldest pending request.
    fn approvals_active_front(&self) -> Option<(usize, &PendingRequest)> {
        let index = self.active_thread_index()?;
        let request = self.thread_tab(index)?.approvals.front()?;
        Some((index, request))
    }

    /// Like [`Self::approvals_active_front`], but only when that request is
    /// the one on screen (guards against clicks on a stale card).
    fn approvals_shown_front(&self) -> Option<(usize, &PendingRequest)> {
        let (index, request) = self.approvals_active_front()?;
        let shown = self.approvals.shown.as_ref()?;
        (shown.0 == self.tabs[index].id && shown.1 == request.request_id)
            .then_some((index, request))
    }

    /// Renders the active tab's oldest request (or hides the card).
    pub(crate) fn approvals_show(&mut self) {
        let Some((index, request)) = self.approvals_active_front() else {
            self.approvals.shown = None;
            self.approvals.rendered_card = None;
            self.approvals.options.clear();
            let state = self.window.global::<ApprovalsState>();
            state.set_pending_count(0);
            // The card is going away; its focus handlers will not run.
            state.set_card_focused(false);
            state.set_focused_field(-1);
            return;
        };
        let count = self.approvals_pending_count(index);
        let key = (self.tabs[index].id, request.request_id.clone());
        let previous_tab = self.approvals.shown.as_ref().map(|(tab, _)| *tab);
        let is_new = self.approvals.shown.as_ref() != Some(&key);
        let cwd = self.thread_tab(index).map(|thread| thread.cwd.clone());
        let file_changes = request
            .file_change_item()
            .and_then(|(thread_id, item_id)| self.approvals.file_changes.get(thread_id, item_id));
        let card = request.card(&CardContext {
            cwd: cwd.as_deref(),
            file_changes,
        });
        let origin = request.origin.clone().unwrap_or_default();
        let hint = approvals_hint(request);
        if is_new || self.approvals.rendered_card.as_ref() != Some(&card) {
            // A new or reflowed card (a diff arriving pushes the options
            // down) is not answerable until the user can have seen it.
            self.approvals.guard.arm(Instant::now());
        }
        if is_new {
            // Take keyboard focus for a new request only from an empty
            // composer (a typed "y" must not approve), and never while a
            // dialog is open. The card also leaves focus alone when another
            // text input has it (checked in approvals.slint).
            let composer_empty = self
                .window
                .global::<crate::ui::ComposerState>()
                .get_text()
                .trim()
                .is_empty();
            let grab = composer_empty && !self.approvals_modal_open();
            let state = self.window.global::<ApprovalsState>();
            state.set_grab_focus(grab);
            if grab {
                self.approvals.focus_serial = self.approvals.focus_serial.wrapping_add(1);
                state.set_focus_serial(self.approvals.focus_serial);
            }
            // The next request of the same tab starts with nothing
            // highlighted, so an Enter meant for the previous card cannot
            // pick its first option.
            let replaces_previous = previous_tab == Some(key.0);
            state.set_highlighted(if replaces_previous { -1 } else { 0 });
            // Form rows are rebuilt below; their old inputs cannot report
            // losing focus.
            state.set_focused_field(-1);
        }
        self.approvals_render(&card, count, origin, hint, is_new);
        self.approvals.rendered_card = Some(card);
        self.approvals.shown = Some(key);
        // Set last so the card is created with every property in place.
        self.window
            .global::<ApprovalsState>()
            .set_pending_count(i32::try_from(count).unwrap_or(i32::MAX));
    }

    fn approvals_render(
        &mut self,
        card: &CardView,
        count: usize,
        origin: String,
        hint: String,
        is_new: bool,
    ) {
        let Some(models) = self.approvals.models.as_ref() else {
            return;
        };
        let state = self.window.global::<ApprovalsState>();
        state.set_title(card.title.as_str().into());
        state.set_reason(card.reason.as_str().into());
        state.set_code(card.code.as_str().into());
        state.set_code_caption(card.code_caption.as_str().into());
        state.set_link(card.link.as_str().into());
        state.set_origin(origin.into());
        state.set_hint(hint.into());
        state.set_position(if count > 1 {
            format!("1 of {count}").into()
        } else {
            SharedString::new()
        });
        let details: Vec<slint::StyledText> = card
            .details
            .iter()
            .map(|detail| {
                slint::StyledText::from_markdown(detail)
                    .unwrap_or_else(|_| slint::StyledText::from_plain_text(detail))
            })
            .collect();
        if !model_equals(&models.details, &details) {
            models.details.set_vec(details);
        }
        if self.approvals.rendered_diff != card.diff {
            models
                .diff
                .set_vec(card.diff.iter().map(diff_line_data).collect::<Vec<_>>());
            self.approvals.rendered_diff = card.diff.clone();
        }
        match &card.body {
            CardBody::Choices(options) => {
                state.set_kind(ApprovalCardKind::Choices);
                let rows: Vec<ApprovalOptionData> = options.iter().map(option_data).collect();
                if !model_equals(&models.options, &rows) {
                    models.options.set_vec(rows);
                }
                if is_new {
                    models.fields.set_vec(Vec::new());
                }
                self.approvals.options = options.clone();
            }
            CardBody::Form(form) => {
                state.set_kind(ApprovalCardKind::Form);
                state.set_submit_label(form.submit_label.as_str().into());
                state.set_secondary_label(form.secondary_label.as_str().into());
                state.set_tertiary_label(form.tertiary_label.as_str().into());
                state.set_form_error(form.error.as_str().into());
                if models.options.row_count() > 0 {
                    models.options.set_vec(Vec::new());
                }
                if is_new {
                    // Fresh rows (and text inputs) for a new request only;
                    // later updates go row by row to keep focus and cursor.
                    models
                        .fields
                        .set_vec(form.fields.iter().map(field_data).collect::<Vec<_>>());
                } else {
                    for (row, field) in form.fields.iter().enumerate() {
                        update_field_row(&models.fields, row, field);
                    }
                }
                self.approvals.options.clear();
            }
        }
    }

    // ----- user actions -----------------------------------------------------

    fn approvals_choose(&mut self, option_index: usize) {
        let Some((index, request)) = self.approvals_shown_front() else {
            return;
        };
        let Some(option) = self.approvals.options.get(option_index) else {
            return;
        };
        let request_id = request.request_id.clone();
        if let Decision::Delivery(choice) = option.decision {
            self.approvals_finish_delivery(index, &request_id, choice);
            return;
        }
        let answer = request.answer(&option.decision);
        self.approvals_finish(index, &request_id, answer);
    }

    /// Letter, digit, Enter and Esc keys while the card has focus.
    fn approvals_key(&mut self, text: &str) -> bool {
        let mut chars = text.chars();
        let (Some(key), None) = (chars.next(), chars.next()) else {
            return false;
        };
        if self.approvals_shown_front().is_none() || !self.approvals_input_ready() {
            return false;
        }
        if key == '\u{1b}' {
            // Esc reaches the card when no global shortcut claimed it.
            return self.approvals_cancel_shown();
        }
        if key == '\n' {
            if self.approvals.options.is_empty() {
                self.approvals_submit();
                return true;
            }
            let highlighted = self.window.global::<ApprovalsState>().get_highlighted();
            return match usize::try_from(highlighted) {
                Ok(position) if position < self.approvals.options.len() => {
                    self.approvals_choose(position);
                    true
                }
                // Nothing highlighted yet: Enter does nothing.
                _ => false,
            };
        }
        let key = key.to_ascii_lowercase();
        let position = match key.to_digit(10) {
            Some(digit @ 1..=9) => usize::try_from(digit - 1)
                .ok()
                .filter(|position| *position < self.approvals.options.len()),
            _ => self
                .approvals
                .options
                .iter()
                .position(|option| option.key == Some(key)),
        };
        match position {
            Some(position) => {
                self.approvals_choose(position);
                true
            }
            None => false,
        }
    }

    /// Esc while the card has focus cancels the request. Returns false when
    /// Esc should keep its usual meaning (interrupting the turn).
    pub(crate) fn approvals_on_escape(&mut self) -> bool {
        let state = self.window.global::<ApprovalsState>();
        let focus_within = state.get_card_focused() || state.get_focused_field() >= 0;
        focus_within && self.approvals_input_ready() && self.approvals_cancel_shown()
    }

    /// Answers the card on screen with its cancel choice. User-input
    /// questions have none (Esc interrupts the turn instead, like the TUI).
    fn approvals_cancel_shown(&mut self) -> bool {
        let Some((index, request)) = self.approvals_shown_front() else {
            return false;
        };
        let request_id = request.request_id.clone();
        let answer = match &request.kind {
            RequestKind::Delivery(_) => {
                self.approvals_finish_delivery(index, &request_id, DeliveryChoice::Decline);
                return true;
            }
            RequestKind::UserInput(_) => return false,
            RequestKind::Elicitation(elicitation) if elicitation.is_form() => {
                elicitation.dismiss_answer(McpServerElicitationAction::Cancel)
            }
            _ => match request.cancel_option() {
                Some(option) => request.answer(&option.decision),
                None => return false,
            },
        };
        self.approvals_finish(index, &request_id, answer);
        true
    }

    fn approvals_toggle_choice(&mut self, field: usize, choice: usize) {
        if self.approvals_shown_front().is_none() {
            return;
        }
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let Some(request) = self
            .thread_tab_mut(index)
            .and_then(|thread| thread.approvals.front_mut())
        else {
            return;
        };
        let changed = match &mut request.kind {
            RequestKind::UserInput(form) => form.select(field, choice),
            RequestKind::Elicitation(elicitation) => elicitation.toggle_choice(field, choice),
            _ => false,
        };
        if changed {
            self.approvals_refresh_field(index, field);
        }
    }

    /// Re-renders one form row (and the form error) of the active card.
    fn approvals_refresh_field(&mut self, index: usize, field: usize) {
        let Some(request) = self
            .thread_tab(index)
            .and_then(|thread| thread.approvals.front())
        else {
            return;
        };
        let (view, form_error) = match &request.kind {
            RequestKind::UserInput(form) => (form.field(field), String::new()),
            RequestKind::Elicitation(elicitation) => (
                elicitation.field(field),
                elicitation.form_error().to_string(),
            ),
            _ => return,
        };
        if let (Some(view), Some(models)) = (view, self.approvals.models.as_ref()) {
            update_field_row(&models.fields, field, &view);
        }
        self.window
            .global::<ApprovalsState>()
            .set_form_error(form_error.into());
    }

    fn approvals_edit_text(&mut self, field: usize, text: String) {
        if self.approvals_shown_front().is_none() {
            return;
        }
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let Some(request) = self
            .thread_tab_mut(index)
            .and_then(|thread| thread.approvals.front_mut())
        else {
            return;
        };
        let error_cleared = match &mut request.kind {
            RequestKind::UserInput(form) => {
                form.set_text(field, text);
                false
            }
            RequestKind::Elicitation(elicitation) => elicitation.set_text(field, text),
            _ => false,
        };
        if error_cleared {
            self.approvals_refresh_field(index, field);
        }
    }

    fn approvals_submit(&mut self) {
        if self.approvals_shown_front().is_none() {
            return;
        }
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let Some(request) = self
            .thread_tab_mut(index)
            .and_then(|thread| thread.approvals.front_mut())
        else {
            return;
        };
        let request_id = request.request_id.clone();
        let answer = match &mut request.kind {
            RequestKind::UserInput(form) => Some(form.submit_answer()),
            RequestKind::Elicitation(elicitation) => elicitation.submit(),
            _ => return,
        };
        match answer {
            Some(answer) => self.approvals_finish(index, &request_id, answer),
            // Validation failed: show the errors in place.
            None => self.approvals_show(),
        }
    }

    fn approvals_secondary(&mut self) {
        let Some((index, request)) = self.approvals_shown_front() else {
            return;
        };
        let request_id = request.request_id.clone();
        let answer = match &request.kind {
            RequestKind::UserInput(form) => form.skip_answer(),
            RequestKind::Elicitation(elicitation) => {
                elicitation.dismiss_answer(McpServerElicitationAction::Decline)
            }
            _ => return,
        };
        self.approvals_finish(index, &request_id, answer);
    }

    fn approvals_tertiary(&mut self) {
        let Some((index, request)) = self.approvals_shown_front() else {
            return;
        };
        let request_id = request.request_id.clone();
        let answer = match &request.kind {
            RequestKind::Elicitation(elicitation) => {
                elicitation.dismiss_answer(McpServerElicitationAction::Cancel)
            }
            _ => return,
        };
        self.approvals_finish(index, &request_id, answer);
    }

    /// Sends the answer, removes the request, and records the decision.
    fn approvals_finish(
        &mut self,
        index: usize,
        request_id: &RequestId,
        answer: serde_json::Result<Answer>,
    ) {
        let Some(request) = self
            .thread_tab_mut(index)
            .and_then(|thread| thread.approvals.take(request_id))
        else {
            return;
        };
        match answer {
            Ok(answer) => {
                self.backend.resolve(request.request_id, answer.result);
                if let Some(url) = answer.open_url.as_deref() {
                    self.approvals_open_url(url);
                }
                if let Some((kind, text)) = answer.notice {
                    let text = match request.origin.as_deref() {
                        Some(origin) => format!("{text} ({origin})"),
                        None => text,
                    };
                    self.transcript_push_notice(index, kind, text);
                }
            }
            Err(err) => {
                tracing::error!(%err, "failed to encode a server request response");
                self.backend.reject(
                    request.request_id,
                    JSONRPCErrorError {
                        code: -32603,
                        message: format!("codex-gui failed to encode the response: {err}"),
                        data: None,
                    },
                );
                self.transcript_push_notice(
                    index,
                    NoticeKind::Error,
                    format!("Could not send your answer: {err}"),
                );
            }
        }
        self.approvals_show();
        self.refresh_tabs();
    }

    /// Records the user's decision on a cross-tab message and hands it to
    /// the cross-tab controller, which delivers it or tells the sender.
    fn approvals_finish_delivery(
        &mut self,
        index: usize,
        request_id: &RequestId,
        choice: DeliveryChoice,
    ) {
        let Some(request) = self
            .thread_tab_mut(index)
            .and_then(|thread| thread.approvals.take(request_id))
        else {
            return;
        };
        if let RequestKind::Delivery(delivery) = request.kind {
            let (kind, text) = delivery.notice(choice);
            self.transcript_push_notice(index, kind, text);
            self.xtab_delivery_decided(delivery.id, choice);
        }
        self.approvals_show();
        self.refresh_tabs();
    }

    /// Opens an http(s) link in the browser, off the UI thread (launching a
    /// browser can block for a while). Other schemes are refused: links come
    /// from MCP servers, and the OS would hand them to any registered app.
    fn approvals_open_url(&mut self, url: &str) {
        if url.is_empty() {
            return;
        }
        if !format::is_openable_url(url) {
            tracing::warn!(%url, "refused to open a non-http link from a request");
            self.toast("Only http and https links can be opened");
            return;
        }
        let url = url.to_string();
        self.backend.spawn(async move {
            let opened = tokio::task::spawn_blocking(move || webbrowser::open(&url))
                .await
                .map_err(std::io::Error::other)
                .and_then(|result| result);
            if let Err(err) = opened {
                crate::ui_thread::post(move |app| {
                    app.toast(format!("Could not open the link: {err}"));
                });
            }
        });
    }

    // ----- automation -------------------------------------------------------

    /// Scripted answers for UI tests: "accept", "decline", "cancel", "skip",
    /// "submit", "key:<k>", "option:<n>", "select:<field>:<choice>",
    /// "text:<field>:<value>", "escape", "press:<key>" (a real key event
    /// through the window: a character, "escape", "enter", "up", "down"), and
    /// "click:<x>,<y>" (a real left click at logical window coordinates).
    pub(crate) fn approvals_automation(&mut self, action: &str) {
        let (verb, rest) = action.split_once(':').unwrap_or((action, ""));
        if verb != "press" && verb != "click" {
            // Scripted answers are not stray input; only real key presses and
            // clicks go through the guard.
            self.approvals.guard = InputGuard::default();
        }
        match verb {
            "accept" => {
                if self.approvals.options.is_empty() {
                    self.approvals_submit();
                } else {
                    self.approvals_choose(/*option_index*/ 0);
                }
            }
            "decline" => {
                let options = &self.approvals.options;
                let decline = options
                    .iter()
                    .position(|option| option.tone == Tone::Negative && !option.cancels)
                    .or_else(|| options.iter().position(|option| option.cancels));
                match decline {
                    Some(position) => self.approvals_choose(position),
                    None => self.approvals_secondary(),
                }
            }
            "cancel" => {
                match self
                    .approvals
                    .options
                    .iter()
                    .position(|option| option.cancels)
                {
                    Some(position) => self.approvals_choose(position),
                    None => self.approvals_tertiary(),
                }
            }
            "skip" => self.approvals_secondary(),
            "submit" => self.approvals_submit(),
            "escape" => {
                self.window
                    .global::<ApprovalsState>()
                    .set_card_focused(true);
                if !self.approvals_on_escape() {
                    tracing::info!("automation: escape did not cancel the request");
                }
            }
            "key" => {
                if !self.approvals_key(rest) {
                    tracing::info!(key = rest, "automation: key not handled");
                }
            }
            "press" => {
                // A real key press through the window, after this tick so the
                // handlers can reach the controller.
                let text: SharedString = match rest {
                    "escape" => slint::platform::Key::Escape.into(),
                    "enter" => slint::platform::Key::Return.into(),
                    "up" => slint::platform::Key::UpArrow.into(),
                    "down" => slint::platform::Key::DownArrow.into(),
                    "tab" => slint::platform::Key::Tab.into(),
                    other => other.into(),
                };
                let window = self.window.as_weak();
                slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                    if let Some(window) = window.upgrade() {
                        let window = window.window();
                        window.dispatch_event(slint::platform::WindowEvent::KeyPressed {
                            text: text.clone(),
                        });
                        window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
                    }
                });
            }
            "click" => {
                // A real left click at logical "x,y" (e.g. to focus another
                // input before a card arrives).
                let Some((x, y)) = rest.split_once(',').and_then(|(x, y)| {
                    Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
                }) else {
                    tracing::warn!(rest, "automation: click needs x,y");
                    return;
                };
                let window = self.window.as_weak();
                slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                    if let Some(window) = window.upgrade() {
                        let window = window.window();
                        let position = slint::LogicalPosition::new(x, y);
                        let button = slint::platform::PointerEventButton::Left;
                        window.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                            position,
                        });
                        window.dispatch_event(slint::platform::WindowEvent::PointerPressed {
                            position,
                            button,
                        });
                        window.dispatch_event(slint::platform::WindowEvent::PointerReleased {
                            position,
                            button,
                        });
                    }
                });
            }
            "option" => {
                if let Ok(position) = rest.parse::<usize>() {
                    self.approvals_choose(position);
                }
            }
            "select" => {
                if let Some((field, choice)) = rest.split_once(':')
                    && let (Ok(field), Ok(choice)) = (field.parse(), choice.parse())
                {
                    self.approvals_toggle_choice(field, choice);
                }
            }
            "text" => {
                if let Some((field, value)) = rest.split_once(':')
                    && let Ok(field) = field.parse()
                {
                    self.approvals_edit_text(field, value.to_string());
                    // Mirror the edit into the view, as typing would.
                    if let Some(models) = self.approvals.models.as_ref()
                        && let Some(mut row) = models.fields.row_data(field)
                    {
                        row.text = value.into();
                        models.fields.set_row_data(field, row);
                    }
                }
            }
            other => tracing::warn!(action = other, "unknown approvals automation action"),
        }
    }

    /// Injects a server request given as JSON (UI tests of cards the mock
    /// server cannot trigger). Answers go to the real server, which ignores
    /// unknown request ids.
    pub(crate) fn approvals_automation_inject(&mut self, request: serde_json::Value) {
        match serde_json::from_value::<ServerRequest>(request) {
            Ok(request) => {
                self.approvals_on_request(request);
                self.refresh_tabs();
            }
            Err(err) => tracing::warn!(%err, "invalid injected server request"),
        }
    }
}

/// Keyboard hint under a choices card (forms show their own text).
fn approvals_hint(request: &PendingRequest) -> String {
    match &request.kind {
        RequestKind::UserInput(_) => {
            "Unanswered questions are sent without an answer. Skip sends no answers.".to_string()
        }
        RequestKind::Elicitation(elicitation) if elicitation.is_form() => String::new(),
        RequestKind::Delivery(_) => "↑ ↓ to move · Enter to choose · Esc to decline".to_string(),
        _ => {
            if request.cancel_option().is_some() {
                "↑ ↓ to move · Enter to choose · Esc to cancel".to_string()
            } else {
                "↑ ↓ to move · Enter to choose".to_string()
            }
        }
    }
}

fn model_equals<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: &[T]) -> bool {
    model.row_count() == rows.len()
        && rows
            .iter()
            .enumerate()
            .all(|(index, row)| model.row_data(index).as_ref() == Some(row))
}

fn option_data(option: &ApprovalOption) -> ApprovalOptionData {
    ApprovalOptionData {
        label: option.label.as_str().into(),
        key: option.key_label().into(),
        tone: match option.tone {
            Tone::Positive => ApprovalTone::Positive,
            Tone::Negative => ApprovalTone::Negative,
        },
    }
}

fn diff_line_data(line: &DiffLine) -> ApprovalDiffLine {
    ApprovalDiffLine {
        text: line.text.as_str().into(),
        kind: match line.kind {
            DiffLineKind::Context => ApprovalDiffKind::Context,
            DiffLineKind::Added => ApprovalDiffKind::Added,
            DiffLineKind::Removed => ApprovalDiffKind::Removed,
            DiffLineKind::Hunk => ApprovalDiffKind::Hunk,
            DiffLineKind::File => ApprovalDiffKind::File,
            DiffLineKind::Note => ApprovalDiffKind::Note,
        },
    }
}

fn field_data(field: &FieldView) -> ApprovalFieldData {
    ApprovalFieldData {
        kind: match field.kind {
            FieldKind::SingleChoice => ApprovalFieldKind::SingleChoice,
            FieldKind::MultiChoice => ApprovalFieldKind::MultiChoice,
            FieldKind::Text => ApprovalFieldKind::Text,
            FieldKind::Number => ApprovalFieldKind::Number,
            FieldKind::Boolean => ApprovalFieldKind::Boolean,
        },
        header: field.header.as_str().into(),
        prompt: field.prompt.as_str().into(),
        required: field.required,
        choices: ModelRc::new(VecModel::from(
            field
                .choices
                .iter()
                .map(|choice| ApprovalChoiceData {
                    label: choice.label.as_str().into(),
                    description: choice.description.as_str().into(),
                    checked: choice.checked,
                })
                .collect::<Vec<_>>(),
        )),
        show_text: field.show_text,
        text: field.text.as_str().into(),
        placeholder: field.placeholder.as_str().into(),
        secret: field.secret,
        error: field.error.as_str().into(),
    }
}

/// Updates one form row in place, keeping its text input alive.
fn update_field_row(model: &VecModel<ApprovalFieldData>, row: usize, field: &FieldView) {
    let Some(current) = model.row_data(row) else {
        return;
    };
    let choices: Vec<ApprovalChoiceData> = current.choices.iter().collect();
    let next = field_data(field);
    let next_choices: Vec<ApprovalChoiceData> = next.choices.iter().collect();
    let unchanged = choices == next_choices
        && current.error == next.error
        && current.placeholder == next.placeholder
        && current.header == next.header;
    if unchanged {
        return;
    }
    model.set_row_data(
        row,
        ApprovalFieldData {
            // The text input owns what the user typed.
            text: current.text,
            ..next
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn route(parent: Option<&str>, label: &str) -> Route {
        Route {
            parent_thread_id: parent.map(ToString::to_string),
            label: label.to_string(),
        }
    }

    #[test]
    fn requests_from_open_threads_stay_on_their_tab() {
        let routes = HashMap::new();
        let placement = place("t1", &routes, &HashSet::new(), |thread| {
            (thread == "t1").then_some(3)
        });
        assert_eq!(
            placement,
            Placement::Tab {
                index: 3,
                origin: None
            }
        );
    }

    #[test]
    fn sub_agent_requests_route_to_the_nearest_open_ancestor() {
        let routes = HashMap::from([
            (
                "child".to_string(),
                route(Some("middle"), "Sub-agent Ada (explorer)"),
            ),
            ("middle".to_string(), route(Some("root"), "Sub-agent Bo")),
        ]);
        let placement = place("child", &routes, &HashSet::new(), |thread| {
            (thread == "root").then_some(0)
        });
        assert_eq!(
            placement,
            Placement::Tab {
                index: 0,
                origin: Some("Sub-agent Ada (explorer)".to_string())
            }
        );
    }

    #[test]
    fn unknown_threads_are_looked_up_and_dead_ends_have_no_tab() {
        let routes = HashMap::from([("child".to_string(), route(Some("parent"), "Sub-agent"))]);
        assert_eq!(
            place("child", &routes, &HashSet::new(), |_| None),
            Placement::Lookup("parent".to_string())
        );
        let unreadable = HashSet::from(["parent".to_string()]);
        assert_eq!(
            place("child", &routes, &unreadable, |_| None),
            Placement::NoTab
        );
        let orphan = HashMap::from([("root".to_string(), route(None, "Thread root"))]);
        assert_eq!(
            place("root", &orphan, &HashSet::new(), |_| None),
            Placement::NoTab
        );
    }

    #[test]
    fn routing_stops_on_parent_cycles() {
        let routes = HashMap::from([
            ("a".to_string(), route(Some("b"), "A")),
            ("b".to_string(), route(Some("a"), "B")),
        ]);
        assert_eq!(
            place("a", &routes, &HashSet::new(), |_| None),
            Placement::NoTab
        );
    }

    fn thread(source: serde_json::Value, extra: serde_json::Value) -> Thread {
        let mut base = json!({
            "id": "child-thread",
            "sessionId": "s",
            "forkedFromId": null,
            "parentThreadId": null,
            "preview": "",
            "ephemeral": false,
            "modelProvider": "mock",
            "createdAt": 0,
            "updatedAt": 0,
            "status": {"type": "idle"},
            "path": null,
            "cwd": "/repo",
            "cliVersion": "0",
            "source": source,
            "gitInfo": null,
            "name": null,
            "turns": [],
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        match serde_json::from_value(base) {
            Ok(thread) => thread,
            Err(err) => panic!("invalid test thread: {err}"),
        }
    }

    #[test]
    fn routes_come_from_the_spawn_source() {
        let spawned = thread(
            json!({"subAgent": {"thread_spawn": {
                "parent_thread_id": "67e55044-10b1-426f-9247-bb680e5fe0c8",
                "depth": 1,
                "agent_nickname": "Ada",
                "agent_role": "explorer",
            }}}),
            json!({}),
        );
        assert_eq!(
            route_for_thread(&spawned),
            route(
                Some("67e55044-10b1-426f-9247-bb680e5fe0c8"),
                "Sub-agent Ada (explorer)"
            )
        );

        let by_field = thread(
            json!("cli"),
            json!({"parentThreadId": "parent", "agentNickname": "Bo"}),
        );
        assert_eq!(
            route_for_thread(&by_field),
            route(Some("parent"), "Sub-agent Bo")
        );

        let root = thread(json!("cli"), json!({}));
        assert_eq!(route_for_thread(&root), route(None, "Thread child-th"));
    }

    #[test]
    fn pending_approvals_dedupe_and_drop() {
        let mut pending = PendingApprovals::default();
        let request = |id: i64, turn: &str| PendingRequest {
            request_id: RequestId::Integer(id),
            thread_id: "t".to_string(),
            turn_id: Some(turn.to_string()),
            origin: None,
            kind: RequestKind::FileChange(
                match serde_json::from_value(json!({
                    "threadId": "t", "turnId": turn, "itemId": "i", "startedAtMs": 0,
                    "reason": null, "grantRoot": null,
                })) {
                    Ok(params) => params,
                    Err(err) => panic!("invalid params: {err}"),
                },
            ),
        };
        assert!(pending.push(request(1, "u1")));
        assert!(!pending.push(request(1, "u1")), "replayed ids are ignored");
        assert!(pending.push(request(2, "u2")));
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending.drop_where(|request| request.turn_id.as_deref() == Some("u1")),
            1
        );
        assert_eq!(
            pending.front().map(|request| request.request_id.clone()),
            Some(RequestId::Integer(2))
        );
        assert!(pending.take(&RequestId::Integer(2)).is_some());
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn the_input_guard_holds_answers_briefly_after_each_change() {
        let start = Instant::now();
        let mut guard = InputGuard::default();
        assert!(guard.is_ready(start), "an unarmed guard lets input through");
        guard.arm(start);
        assert!(!guard.is_ready(start));
        assert!(!guard.is_ready(start + INPUT_GUARD / 2));
        assert!(guard.is_ready(start + INPUT_GUARD));
        // Re-arming (a new card, a reflow, the window regaining focus)
        // starts the wait over.
        guard.arm(start + INPUT_GUARD);
        assert!(!guard.is_ready(start + INPUT_GUARD + INPUT_GUARD / 2));
    }

    fn pending(id: i64, thread_id: &str, kind: RequestKind) -> PendingRequest {
        PendingRequest {
            request_id: RequestId::Integer(id),
            thread_id: thread_id.to_string(),
            turn_id: Some("turn".to_string()),
            origin: None,
            kind,
        }
    }

    fn file_change(thread_id: &str) -> RequestKind {
        match serde_json::from_value(json!({
            "threadId": thread_id, "turnId": "turn", "itemId": "i", "startedAtMs": 0,
            "reason": null, "grantRoot": null,
        })) {
            Ok(params) => RequestKind::FileChange(params),
            Err(err) => panic!("invalid params: {err}"),
        }
    }

    fn delivery() -> RequestKind {
        RequestKind::Delivery(Box::new(DeliveryRequest {
            id: 9,
            source_title: "A".to_string(),
            target_title: "B".to_string(),
            message: "hi".to_string(),
            wait_for_reply: false,
            always_ask: None,
        }))
    }

    #[test]
    fn closing_a_tab_cancels_its_own_requests_and_moves_the_rest() {
        let own = pending(1, "parent", file_change("parent"));
        let sub_agent = pending(2, "child", file_change("child"));
        // A consent card carries the sender's thread, but belongs to the
        // closed (receiving) tab.
        let message = pending(3, "sender", delivery());
        assert_eq!(on_close(&own, Some("parent")), OnClose::Cancel);
        assert_eq!(on_close(&sub_agent, Some("parent")), OnClose::Move);
        assert_eq!(on_close(&message, Some("parent")), OnClose::Decline);
        assert_eq!(
            on_close(&own, /*closed_thread_id*/ None),
            OnClose::Move,
            "a tab whose thread never started owns no request"
        );
    }

    #[test]
    fn close_warnings_say_what_happens_to_each_request() {
        assert_eq!(close_warning(&[], /*other_thread_tabs*/ true), None);
        assert_eq!(
            close_warning(&[OnClose::Move], /*other_thread_tabs*/ true),
            Some(
                "A request in this tab is waiting for your answer. Requests from sub-agents and other threads move to another tab."
                    .to_string()
            )
        );
        assert_eq!(
            close_warning(
                &[OnClose::Cancel, OnClose::Move, OnClose::Decline],
                /*other_thread_tabs*/ false
            ),
            Some(
                "3 requests in this tab are waiting for your answer. Closing the tab cancels the ones from this thread. Requests from sub-agents and other threads are cancelled, because no other thread tab is open. Messages other tabs want to send here are declined."
                    .to_string()
            )
        );
    }

    #[test]
    fn file_change_cache_is_bounded() {
        let mut cache = FileChangeCache::default();
        for index in 0..FILE_CHANGE_CACHE_CAPACITY + 5 {
            cache.insert("t", &index.to_string(), Vec::new());
        }
        assert_eq!(cache.entries.len(), FILE_CHANGE_CACHE_CAPACITY);
        assert!(cache.get("t", "0").is_none());
        assert!(
            cache
                .get("t", &(FILE_CHANGE_CACHE_CAPACITY + 4).to_string())
                .is_some()
        );
        cache.remove("t", &(FILE_CHANGE_CACHE_CAPACITY + 4).to_string());
        assert!(
            cache
                .get("t", &(FILE_CHANGE_CACHE_CAPACITY + 4).to_string())
                .is_none()
        );
    }
}
