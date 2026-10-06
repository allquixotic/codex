//! Thread lifecycle for tabs: start, resume, fork, close, and the input path
//! (start a turn, steer a running turn, or queue behind it), plus the tab
//! actions built on top of it: side chats ([`side`]), conversation recaps
//! ([`recap`]), continuing in a managed Git worktree ([`worktree`]) and
//! reviews ([`review`]), and the picker overlay they share ([`picker`]).
//!
//! Stopping a turn needs its id. A thread resumed while it runs reports
//! no turn id (the turn started before this client subscribed), so it is
//! looked up with `thread/turns/list`; a stop requested while `turn/start`
//! is still in flight is sent as soon as the turn's id arrives.

mod picker;
pub(crate) mod recap;
mod review;
mod side;
mod worktree;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadCompactStartParams;
use codex_app_server_protocol::ThreadCompactStartResponse;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadQueueAddParams;
use codex_app_server_protocol::ThreadQueueAddResponse;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSetNameParams;
use codex_app_server_protocol::ThreadSetNameResponse;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnSteerResponse;
use codex_app_server_protocol::UserInput;

use crate::app::AppController;
use crate::app::DialogRequest;
use crate::app::TabId;
use crate::app::TabKind;
use crate::app::ThreadPhase;
use crate::app::ThreadTab;
use crate::backend::Backend;
use crate::backend::BackendError;
use crate::prefs::BusyInput;
use crate::session;
use crate::transcript::HistoryStart;
use crate::transcript::NoticeKind;

pub(crate) use self::picker::PickerButton;
pub(crate) use self::picker::PickerController;
pub(crate) use self::picker::PickerEvent;
pub(crate) use self::picker::PickerFlow;
pub(crate) use self::picker::PickerInputView;
pub(crate) use self::picker::PickerListView;
pub(crate) use self::picker::PickerOutcome;
pub(crate) use self::picker::PickerRowView;
pub(crate) use self::picker::PickerView;
pub(crate) use self::side::SideParent;

const INIT_PROMPT: &str = include_str!("../assets/prompt_for_init_command.md");

/// How long closing or quitting waits for running turns to stop before it
/// detaches from them anyway.
const STOP_WAIT: Duration = Duration::from_secs(3);
/// `turn/interrupt` attempts for one stop request, and the pause before
/// asking again when a just-started turn is not interruptible yet.
const INTERRUPT_ATTEMPTS: u32 = 10;
const INTERRUPT_RETRY_DELAY: Duration = Duration::from_millis(150);

/// Per-tab state of side chats, recaps, worktree creation and the thread's
/// lifecycle requests.
#[derive(Debug, Default)]
pub(crate) struct ThreadExtras {
    /// Set when the tab is a side chat: the thread it branched from.
    pub(crate) side_parent: Option<SideParent>,
    /// Recap being generated for this tab.
    pub(crate) recap: Option<recap::RecapRun>,
    /// A managed worktree is being created from this tab.
    pub(crate) worktree_pending: bool,
    /// Bumped by every `thread/start` and `thread/resume` sent for the tab;
    /// the answer to an older one is dropped.
    pub(crate) open_generation: u64,
    /// The tab's thread comes from `thread/start` (not a resume or fork), so
    /// a start that was lost can simply be sent again.
    pub(crate) started_here: bool,
    /// `turn/start` requests in flight (their turn id is not known yet).
    pub(crate) turn_starts_in_flight: u32,
    /// The user asked to stop before the running turn's id was known; the
    /// interrupt goes out as soon as it is.
    pub(crate) interrupt_pending: bool,
}

/// Id of a piece of background work tracked in a tab (recaps).
fn next_run_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Why a tab action is not available in a side chat, if it is not.
fn side_chat_restriction(action: &str) -> Option<&'static str> {
    match action {
        "rename" => Some("Side chats are temporary and cannot be renamed."),
        "archive" => Some("Side chats are not saved; close the tab to discard it."),
        "fork" | "side" | "worktree" => {
            Some("Side chats are temporary and cannot be forked. Use the main thread.")
        }
        _ => None,
    }
}

/// Input submitted before its thread finished starting.
#[derive(Clone, Debug)]
pub(crate) struct PendingInput {
    pub(crate) input: Vec<UserInput>,
    pub(crate) client_id: String,
}

/// One-line preview of user input (first text item).
pub(crate) fn user_input_preview(content: &[UserInput]) -> String {
    content
        .iter()
        .find_map(|input| match input {
            UserInput::Text { text, .. } if !text.trim().is_empty() => {
                Some(text.trim().to_string())
            }
            _ => None,
        })
        .unwrap_or_default()
}

impl AppController {
    /// Opens a tab with a brand-new thread rooted at `cwd`.
    pub(crate) fn start_thread_in_folder(&mut self, cwd: PathBuf) {
        self.prefs.remember_folder(&cwd);
        self.save_prefs();
        let mut thread = ThreadTab::new(cwd);
        thread.xtab_enabled = self.prefs.cross_tab_tools;
        let index = self.replace_new_tab_page_or_push(TabKind::Thread(Box::new(thread)));
        self.start_thread_in_tab(index);
        self.refresh_tabs();
    }

    /// Starts a new thread in the folder of tab `index`. Input typed
    /// meanwhile waits in the tab and is sent once the thread exists.
    fn start_thread_in_tab(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        thread.thread_id = None;
        thread.phase = ThreadPhase::Starting;
        thread.active_turn_id = None;
        thread.extras.started_here = true;
        thread.extras.interrupt_pending = false;
        thread.extras.turn_starts_in_flight = 0;
        thread.extras.open_generation += 1;
        let generation = thread.extras.open_generation;
        let cwd = thread.cwd.clone();
        let xtab_enabled = thread.xtab_enabled;
        let options = session::NewThreadOptions {
            dynamic_tools: if xtab_enabled {
                self.xtab_dynamic_tools()
            } else {
                None
            },
            ..session::NewThreadOptions::default()
        };
        self.backend.call(
            |id| session::thread_start(id, &cwd, options),
            move |app, result: Result<ThreadStartResponse, BackendError>| {
                let Some(index) = app.tab_for_open_request(tab_id, generation) else {
                    // Tab closed, or a newer start replaced this one: drop
                    // the thread nobody shows.
                    if let Ok(response) = result {
                        app.unsubscribe_thread(response.thread.id);
                    }
                    return;
                };
                match result {
                    Ok(response) => app.on_thread_started(index, response),
                    Err(err) => app.on_thread_open_failed(index, &err),
                }
            },
        );
    }

    /// Index of tab `tab_id` while the open request numbered `generation`
    /// is still its latest one.
    fn tab_for_open_request(&self, tab_id: TabId, generation: u64) -> Option<usize> {
        let index = self.tab_index_by_id(tab_id)?;
        (self.thread_tab(index)?.extras.open_generation == generation).then_some(index)
    }

    fn on_thread_started(&mut self, index: usize, response: ThreadStartResponse) {
        crate::perf::mark("thread-started");
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        thread.thread_id = Some(response.thread.id.clone());
        thread.cwd = response.cwd.into_path_buf();
        thread.name = response.thread.name;
        thread.model = Some(response.model);
        thread.model_provider = Some(response.model_provider);
        thread.effort = response.reasoning_effort;
        thread.approval_policy = Some(response.approval_policy);
        thread.approvals_reviewer = Some(response.approvals_reviewer);
        thread.sandbox = Some(response.sandbox);
        thread.phase = ThreadPhase::Idle;
        self.dispatch_pending_inputs(index);
        self.after_thread_opened(index);
    }

    /// Sends input that was submitted while the thread was starting.
    fn dispatch_pending_inputs(&mut self, index: usize) {
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let pending = std::mem::take(&mut thread.pending_inputs);
        for pending in pending {
            self.dispatch_input(index, pending.input, pending.client_id);
        }
    }

    fn on_thread_open_failed(&mut self, index: usize, err: &BackendError) {
        let message = err.user_message();
        let mut unsent = Vec::new();
        if let Some(thread) = self.thread_tab_mut(index) {
            thread.phase = ThreadPhase::Error;
            thread.last_error = Some(message.clone());
            unsent = std::mem::take(&mut thread.pending_inputs);
        }
        for pending in unsent {
            self.transcript_echo_unsent(index, &pending.client_id);
        }
        self.transcript_push_notice(
            index,
            NoticeKind::Error,
            format!("Could not open thread: {message}"),
        );
        self.refresh_tabs();
    }

    fn after_thread_opened(&mut self, index: usize) {
        if self.active == Some(index) {
            self.show_active();
        }
        self.refresh_tabs();
        self.sidebar_refresh();
    }

    /// Opens an existing thread, or focuses its tab when already open.
    pub(crate) fn open_thread(&mut self, thread_id: String, cwd_hint: Option<PathBuf>) {
        if let Some(index) = self.tab_index_for_thread(&thread_id) {
            self.activate_tab(index);
            return;
        }
        let mut thread = ThreadTab::new(cwd_hint.unwrap_or_default());
        thread.thread_id = Some(thread_id.clone());
        // Threads opened from history may come from the CLI or another
        // client and lack the codex_gui tools (and their "treat messages as
        // untrusted" instructions): they receive cross-tab messages only
        // after the user turns it on in the info pane.
        thread.xtab_enabled = false;
        let index = self.replace_new_tab_page_or_push(TabKind::Thread(Box::new(thread)));
        self.resume_thread_in_tab(index, thread_id, /*restart_unsaved*/ false);
    }

    /// Re-attaches tab `index` to `thread_id`. With `restart_unsaved`, a
    /// thread the server cannot resume because nothing was ever sent in it
    /// (it has no rollout yet) is replaced by a new thread in the same
    /// folder, which is what the user had.
    fn resume_thread_in_tab(&mut self, index: usize, thread_id: String, restart_unsaved: bool) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        thread.phase = ThreadPhase::Starting;
        // A turn id from before a restart is stale; `on_thread_resumed`
        // looks up the running turn again.
        thread.active_turn_id = None;
        thread.extras.interrupt_pending = false;
        thread.extras.turn_starts_in_flight = 0;
        thread.extras.open_generation += 1;
        let generation = thread.extras.open_generation;
        let request = session::thread_resume(self.backend.next_request_id(), &thread_id);
        self.backend.call(
            move |_| request,
            move |app, result: Result<ThreadResumeResponse, BackendError>| {
                let Some(index) = app.tab_for_open_request(tab_id, generation) else {
                    // Nothing shows this thread any more (tab closed): the
                    // resume subscribed us again, so detach.
                    if let Ok(response) = result
                        && app.tab_index_for_thread(&response.thread.id).is_none()
                    {
                        app.unsubscribe_thread(response.thread.id);
                    }
                    return;
                };
                match result {
                    Ok(response) => app.on_thread_resumed(index, response),
                    Err(err) if restart_unsaved && is_unsaved_thread_error(&err.user_message()) => {
                        tracing::info!(
                            thread_id,
                            "thread had no messages before the restart; starting a new one"
                        );
                        app.start_thread_in_tab(index);
                        app.refresh_tabs();
                    }
                    Err(err) => app.on_thread_open_failed(index, &err),
                }
            },
        );
        self.refresh_tabs();
    }

    fn on_thread_resumed(&mut self, index: usize, response: ThreadResumeResponse) {
        crate::perf::mark("thread-resumed");
        let thread_id = response.thread.id.clone();
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        thread.thread_id = Some(thread_id.clone());
        thread.cwd = response.cwd.into_path_buf();
        thread.name.clone_from(&response.thread.name);
        thread.preview.clone_from(&response.thread.preview);
        thread.model = Some(response.model);
        thread.model_provider = Some(response.model_provider);
        thread.effort = response.reasoning_effort;
        thread.approval_policy = Some(response.approval_policy);
        thread.approvals_reviewer = Some(response.approvals_reviewer);
        thread.sandbox = Some(response.sandbox);
        thread.phase = match response.thread.status {
            ThreadStatus::Active { .. } => ThreadPhase::Running,
            ThreadStatus::SystemError => ThreadPhase::Error,
            _ => ThreadPhase::Idle,
        };
        if thread.phase == ThreadPhase::Running {
            // No `turn/started` is sent for a turn that was already running.
            self.lookup_running_turn(index);
        }
        self.transcript_load_history(
            index,
            HistoryStart {
                thread_id,
                history_mode: response.thread.history_mode,
                turns: response.thread.turns,
                turns_cursor: response.turns_backwards_cursor,
                items_cursor: response.items_backwards_cursor,
            },
        );
        self.dispatch_pending_inputs(index);
        self.after_thread_opened(index);
    }

    /// Finds the turn a resumed thread is running, so it can be stopped,
    /// steered and queued behind.
    fn lookup_running_turn(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        let generation = thread.extras.open_generation;
        let request = session::turns_page(
            self.backend.next_request_id(),
            &thread_id,
            /*cursor*/ None,
            /*limit*/ 1,
        );
        self.backend.call(
            move |_| request,
            move |app, result: Result<ThreadTurnsListResponse, BackendError>| {
                let Some(index) = app.tab_for_open_request(tab_id, generation) else {
                    return;
                };
                let turn_id = match result {
                    Ok(page) => running_turn_id(&page.data),
                    Err(err) => {
                        tracing::warn!(error = %err.user_message(), "could not find the running turn");
                        None
                    }
                };
                app.on_running_turn_found(index, thread_id, turn_id);
            },
        );
    }

    fn on_running_turn_found(&mut self, index: usize, thread_id: String, turn_id: Option<String>) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        // An idle tab has nothing to adopt or stop.
        if !thread.is_busy() {
            thread.extras.interrupt_pending = false;
            return;
        }
        // `turn/started` is authoritative (and sent a pending stop).
        if thread.active_turn_id.is_some() {
            return;
        }
        match turn_id {
            Some(turn_id) => {
                thread.active_turn_id = Some(turn_id.clone());
                if std::mem::take(&mut thread.extras.interrupt_pending) {
                    self.send_interrupt(tab_id, thread_id, turn_id, /*attempt*/ 0);
                }
            }
            // Busy without a turn (for example still starting up): a stop
            // the user asked for interrupts whatever the thread is doing.
            None if thread.extras.interrupt_pending && thread.extras.turn_starts_in_flight == 0 => {
                thread.extras.interrupt_pending = false;
                self.send_interrupt(tab_id, thread_id, String::new(), /*attempt*/ 0);
            }
            None => {}
        }
    }

    /// Re-attaches every open thread tab after the server restarted.
    ///
    /// Tabs whose thread never started (its `thread/start` failed or was
    /// lost with the old server) get a new thread in their folder.
    pub(crate) fn resume_open_threads(&mut self) {
        enum Reattach {
            /// Resume the thread; `started_here` threads fall back to a new
            /// thread when they never got a message.
            Resume(String, bool),
            Start,
            SideChatLost,
        }
        let targets: Vec<(usize, Reattach)> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                let thread = tab.thread()?;
                let action = match (&thread.thread_id, thread.extras.side_parent.is_some()) {
                    (Some(_), true) => Reattach::SideChatLost,
                    (Some(id), false) => Reattach::Resume(id.clone(), thread.extras.started_here),
                    (None, false)
                        if thread.extras.started_here
                            && matches!(
                                thread.phase,
                                ThreadPhase::Starting | ThreadPhase::Error
                            ) =>
                    {
                        Reattach::Start
                    }
                    (None, _) => return None,
                };
                Some((index, action))
            })
            .collect();
        for (index, action) in targets {
            match action {
                Reattach::SideChatLost => self.side_chat_lost(index),
                Reattach::Resume(thread_id, started_here) => {
                    self.resume_thread_in_tab(index, thread_id, started_here);
                }
                Reattach::Start => {
                    if let Some(thread) = self.thread_tab_mut(index) {
                        thread.last_error = None;
                    }
                    self.start_thread_in_tab(index);
                }
            }
        }
        self.refresh_tabs();
    }

    /// Forks the thread in `index` into a new tab. `before_turn_id` rewinds.
    pub(crate) fn fork_tab(&mut self, index: usize, before_turn_id: Option<String>) {
        let Some(source) = self.thread_tab(index) else {
            return;
        };
        let Some(source_id) = source.thread_id.clone() else {
            return;
        };
        let mut thread = ThreadTab::new(source.cwd.clone());
        thread.name = Some(format!("{} (fork)", source.title()));
        // A fork keeps its source's tools, so it receives messages only when
        // the source does.
        thread.xtab_enabled = self.prefs.cross_tab_tools && source.xtab_enabled;
        let new_index = self.push_tab(TabKind::Thread(Box::new(thread)), /*activate*/ true);
        let tab_id = self.tabs[new_index].id;
        self.backend.call(
            |id| session::thread_fork(id, &source_id, before_turn_id),
            move |app, result: Result<ThreadForkResponse, BackendError>| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    if let Ok(response) = result {
                        app.unsubscribe_thread(response.thread.id);
                    }
                    return;
                };
                match result {
                    Ok(response) => {
                        let thread_id = response.thread.id.clone();
                        if let Some(thread) = app.thread_tab_mut(index) {
                            thread.thread_id = Some(thread_id.clone());
                            thread.cwd = response.cwd.into_path_buf();
                            if response.thread.name.is_some() {
                                thread.name.clone_from(&response.thread.name);
                            }
                            thread.preview.clone_from(&response.thread.preview);
                            thread.model = Some(response.model);
                            thread.model_provider = Some(response.model_provider);
                            thread.effort = response.reasoning_effort;
                            thread.approval_policy = Some(response.approval_policy);
                            thread.approvals_reviewer = Some(response.approvals_reviewer);
                            thread.sandbox = Some(response.sandbox);
                            thread.phase = ThreadPhase::Idle;
                        }
                        app.transcript_load_history(
                            index,
                            HistoryStart {
                                thread_id,
                                history_mode: response.thread.history_mode,
                                turns: response.thread.turns,
                                turns_cursor: None,
                                items_cursor: None,
                            },
                        );
                        app.dispatch_pending_inputs(index);
                        app.after_thread_opened(index);
                    }
                    Err(err) => app.on_thread_open_failed(index, &err),
                }
            },
        );
    }

    /// Called after a thread tab was removed from the strip. A running turn
    /// is stopped first: once nothing is subscribed, nobody could answer
    /// its approvals, and the server keeps a running thread loaded.
    pub(crate) fn on_thread_tab_closed(&mut self, mut thread: ThreadTab) {
        // Requests shown in the tab must still be answered or moved.
        let pending = std::mem::take(&mut thread.approvals);
        self.approvals_on_tab_closed(thread.thread_id.as_deref(), pending);
        self.xtab_on_tab_closed(&thread);
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        if !thread.is_busy() {
            self.unsubscribe_thread(thread_id);
            return;
        }
        let backend = self.backend.clone();
        let turn_id = thread.active_turn_id.clone();
        self.backend.spawn(async move {
            let stopped =
                tokio::time::timeout(STOP_WAIT, stop_turn(&backend, &thread_id, turn_id)).await;
            if stopped.is_err() {
                tracing::warn!(thread_id, "the turn did not stop in time; detaching anyway");
            }
            let unsubscribe = backend
                .request::<ThreadUnsubscribeResponse>(ClientRequest::ThreadUnsubscribe {
                    request_id: backend.next_request_id(),
                    params: ThreadUnsubscribeParams { thread_id },
                })
                .await;
            if let Err(err) = unsubscribe {
                tracing::warn!(%err, "thread/unsubscribe failed");
            }
        });
    }

    /// Stops every running turn, then runs `done` (at most [`STOP_WAIT`]
    /// later). Used when quitting, so turns on a daemon or remote server do
    /// not keep running unattended.
    pub(crate) fn stop_running_turns_then(
        &mut self,
        done: impl FnOnce(&mut AppController) + Send + 'static,
    ) {
        let running: Vec<(String, Option<String>)> = self
            .tabs
            .iter()
            .filter_map(crate::app::Tab::thread)
            .filter(|thread| thread.is_busy())
            .filter_map(|thread| Some((thread.thread_id.clone()?, thread.active_turn_id.clone())))
            .collect();
        if running.is_empty() {
            done(self);
            return;
        }
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let stops = running
                .iter()
                .map(|(thread_id, turn_id)| stop_turn(&backend, thread_id, turn_id.clone()));
            if tokio::time::timeout(STOP_WAIT, futures::future::join_all(stops))
                .await
                .is_err()
            {
                tracing::warn!("running turns did not stop in time");
            }
            crate::ui_thread::post(done);
        });
    }

    fn unsubscribe_thread(&self, thread_id: String) {
        self.backend
            .fire::<ThreadUnsubscribeResponse, _>(|request_id| ClientRequest::ThreadUnsubscribe {
                request_id,
                params: ThreadUnsubscribeParams { thread_id },
            });
    }

    /// Sends user input to the thread in tab `index`.
    ///
    /// Starts a turn when idle. While a turn runs, steers it or queues the
    /// message, depending on the user's preference. Before the thread exists,
    /// the input waits in the tab. Returns false (and says why) when the tab
    /// has no thread that could ever take the input.
    pub(crate) fn send_user_input(&mut self, index: usize, input: Vec<UserInput>) -> bool {
        if input.is_empty() {
            return false;
        }
        let Some(thread) = self.thread_tab(index) else {
            return false;
        };
        if let Some(reason) = input_refusal(thread) {
            self.toast(reason);
            return false;
        }
        let client_id = session::new_client_message_id();
        let preview = user_input_preview(&input);
        self.transcript_push_local_user_message(index, &client_id, &input);
        let Some(thread) = self.thread_tab_mut(index) else {
            return false;
        };
        if thread.preview.is_empty() {
            thread.preview = preview;
        }
        // Until the thread is attached (started, resumed, forked, or a side
        // chat got its boundary) input waits in the tab.
        if thread.thread_id.is_none() || thread.phase == ThreadPhase::Starting {
            thread
                .pending_inputs
                .push(PendingInput { input, client_id });
            self.refresh_tabs();
            return true;
        }
        self.dispatch_input(index, input, client_id);
        self.refresh_tabs();
        true
    }

    fn dispatch_input(&mut self, index: usize, input: Vec<UserInput>, client_id: String) {
        let busy_mode = self.prefs.busy_input;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        match (thread.active_turn_id.clone(), busy_mode) {
            (Some(turn_id), BusyInput::Steer) => {
                self.steer_turn(
                    index, thread_id, turn_id, input, client_id, /*retried*/ false,
                );
            }
            (Some(_), BusyInput::Queue) => self.queue_input(index, thread_id, input, client_id),
            // Running, but the turn id is not known yet (a resumed turn
            // being looked up, or a `turn/start` in flight): the queue does
            // not need it.
            (None, BusyInput::Queue) if thread.is_busy() => {
                self.queue_input(index, thread_id, input, client_id);
            }
            // Idle, or busy in steer mode: the server steers `turn/start`
            // into a running turn.
            (None, _) => {
                if !thread.is_busy() {
                    // A stop asked for an earlier turn does not apply.
                    thread.extras.interrupt_pending = false;
                }
                let overrides = thread.turn_overrides.clone();
                thread.phase = ThreadPhase::Running;
                self.start_turn(index, thread_id, input, client_id, overrides);
            }
        }
    }

    fn start_turn(
        &mut self,
        index: usize,
        thread_id: String,
        input: Vec<UserInput>,
        client_id: String,
        overrides: session::TurnOverrides,
    ) {
        let tab_id = self.tabs[index].id;
        if let Some(thread) = self.thread_tab_mut(index) {
            thread.extras.turn_starts_in_flight += 1;
        }
        let interrupted_thread = thread_id.clone();
        let echo_id = client_id.clone();
        self.backend.call(
            |id| session::turn_start(id, &thread_id, input, client_id, overrides),
            move |app, result: Result<TurnStartResponse, BackendError>| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                if let Some(thread) = app.thread_tab_mut(index) {
                    thread.extras.turn_starts_in_flight =
                        thread.extras.turn_starts_in_flight.saturating_sub(1);
                }
                match result {
                    Ok(response) => {
                        let Some(thread) = app.thread_tab_mut(index) else {
                            return;
                        };
                        // turn/started may already have arrived; never
                        // resurrect a turn that has completed.
                        if thread.is_busy()
                            && thread.active_turn_id.is_none()
                            && response.turn.status == TurnStatus::InProgress
                        {
                            thread.active_turn_id = Some(response.turn.id.clone());
                            // Stop was pressed before the id was known.
                            if std::mem::take(&mut thread.extras.interrupt_pending) {
                                app.send_interrupt(
                                    tab_id,
                                    interrupted_thread,
                                    response.turn.id,
                                    /*attempt*/ 0,
                                );
                            }
                        }
                    }
                    Err(err) => {
                        if let Some(thread) = app.thread_tab_mut(index)
                            && thread.active_turn_id.is_none()
                        {
                            thread.phase = ThreadPhase::Idle;
                            thread.extras.interrupt_pending = false;
                        }
                        app.transcript_echo_unsent(index, &echo_id);
                        app.transcript_push_notice(
                            index,
                            NoticeKind::Error,
                            format!("Could not send message: {}", err.user_message()),
                        );
                    }
                }
                app.refresh_tabs();
                app.composer_show();
            },
        );
    }

    fn steer_turn(
        &mut self,
        index: usize,
        thread_id: String,
        turn_id: String,
        input: Vec<UserInput>,
        client_id: String,
        retried: bool,
    ) {
        let tab_id = self.tabs[index].id;
        let input_for_retry = input.clone();
        let client_for_retry = client_id.clone();
        let steer_request = |id| session::turn_steer(id, &thread_id, &turn_id, input, client_id);
        let request = steer_request(self.backend.next_request_id());
        self.backend.call(
            move |_| request,
            move |app, result: Result<TurnSteerResponse, BackendError>| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                let Err(err) = result else {
                    return;
                };
                let message = err.user_message();
                match classify_steer_error(&message, err.server_error()) {
                    SteerFailure::TurnEnded => {
                        if let Some(thread) = app.thread_tab_mut(index) {
                            thread.active_turn_id = None;
                            let overrides = thread.turn_overrides.clone();
                            thread.phase = ThreadPhase::Running;
                            app.start_turn(
                                index,
                                thread_id,
                                input_for_retry,
                                client_for_retry,
                                overrides,
                            );
                        }
                    }
                    SteerFailure::DifferentTurn(actual) if !retried => {
                        if let Some(thread) = app.thread_tab_mut(index) {
                            thread.active_turn_id = Some(actual.clone());
                        }
                        app.steer_turn(
                            index,
                            thread_id,
                            actual,
                            input_for_retry,
                            client_for_retry,
                            /*retried*/ true,
                        );
                    }
                    SteerFailure::NotSteerable | SteerFailure::DifferentTurn(_) => {
                        app.queue_input(index, thread_id, input_for_retry, client_for_retry);
                    }
                    SteerFailure::Other => {
                        app.transcript_echo_unsent(index, &client_for_retry);
                        app.transcript_push_notice(
                            index,
                            NoticeKind::Error,
                            format!("Could not send message: {message}"),
                        );
                    }
                }
            },
        );
    }

    fn queue_input(
        &mut self,
        index: usize,
        thread_id: String,
        input: Vec<UserInput>,
        client_id: String,
    ) {
        let tab_id = self.tabs[index].id;
        let echo_id = client_id.clone();
        self.backend.call(
            |request_id| ClientRequest::ThreadQueueAdd {
                request_id,
                params: ThreadQueueAddParams {
                    thread_id,
                    input,
                    client_user_message_id: client_id,
                },
            },
            move |app, result: Result<ThreadQueueAddResponse, BackendError>| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                match result {
                    Ok(_) => app.toast("Queued; sends when the current turn ends"),
                    Err(err) => {
                        app.transcript_echo_unsent(index, &echo_id);
                        app.transcript_push_notice(
                            index,
                            NoticeKind::Error,
                            format!("Could not queue message: {}", err.user_message()),
                        );
                    }
                }
            },
        );
    }

    /// Interrupts the running turn of tab `index`, if any.
    ///
    /// When the turn's id is not known yet, the stop is remembered and sent
    /// once it is: from the `turn/start` response or `turn/started` while a
    /// start is in flight, otherwise from a `thread/turns/list` lookup.
    pub(crate) fn interrupt_tab(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        match thread.active_turn_id.clone() {
            Some(turn_id) => {
                self.send_interrupt(tab_id, thread_id, turn_id, /*attempt*/ 0)
            }
            None if thread.is_busy() => {
                thread.extras.interrupt_pending = true;
                if thread.extras.turn_starts_in_flight == 0 {
                    self.lookup_running_turn(index);
                }
            }
            None => {}
        }
    }

    /// Sends `turn/interrupt` (attempt `attempt`, from 0). The response
    /// arrives only once the turn has stopped. A turn that started a moment
    /// ago is not interruptible yet ("no active turn"), so the stop is sent
    /// again while the tab still runs that turn; if another turn runs by
    /// then, that one is stopped instead. Other failures are reported.
    fn send_interrupt(&mut self, tab_id: TabId, thread_id: String, turn_id: String, attempt: u32) {
        let request = session::turn_interrupt(self.backend.next_request_id(), &thread_id, &turn_id);
        self.backend.call(
            move |_| request,
            move |app, result: Result<TurnInterruptResponse, BackendError>| {
                let Err(err) = result else {
                    return;
                };
                let message = err.user_message();
                let index = app.tab_index_by_id(tab_id);
                let still_running =
                    index
                        .and_then(|index| app.thread_tab(index))
                        .is_some_and(|thread| {
                            thread.is_busy()
                                && thread.active_turn_id.as_deref() == Some(turn_id.as_str())
                        });
                match next_interrupt_step(
                    classify_interrupt_error(&message),
                    &turn_id,
                    attempt,
                    still_running,
                ) {
                    InterruptNext::Done => {}
                    InterruptNext::Retry { turn_id, delay } => {
                        if let Some(thread) = index.and_then(|index| app.thread_tab_mut(index))
                            && thread.is_busy()
                        {
                            thread.active_turn_id = Some(turn_id.clone());
                        }
                        slint::Timer::single_shot(delay, move || {
                            crate::ui_thread::with_app(move |app| {
                                app.send_interrupt(tab_id, thread_id, turn_id, attempt + 1);
                            });
                        });
                    }
                    InterruptNext::Report => match index {
                        Some(index) => app.transcript_push_notice(
                            index,
                            NoticeKind::Error,
                            format!("Could not stop the turn: {message}"),
                        ),
                        None => tracing::warn!(%message, "turn/interrupt failed"),
                    },
                }
            },
        );
    }

    /// Thread-specific tab menu actions.
    pub(crate) fn thread_tab_action(&mut self, index: usize, action: &str) {
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let thread_id = thread.thread_id.clone();
        let tab_id = self.tabs[index].id;
        if thread.extras.side_parent.is_some()
            && let Some(reason) = side_chat_restriction(action)
        {
            self.toast(reason);
            return;
        }
        match action {
            "rename" => {
                let current = thread.title();
                self.show_dialog(
                    DialogRequest::prompt("Rename thread", current).accept_label("Rename"),
                    Box::new(move |app, value| {
                        let (Some(name), Some(index)) = (value, app.tab_index_by_id(tab_id)) else {
                            return;
                        };
                        let name = name.trim().to_string();
                        if name.is_empty() {
                            return;
                        }
                        if let Some(thread) = app.thread_tab_mut(index) {
                            thread.name = Some(name.clone());
                        }
                        app.refresh_tabs();
                        if let Some(thread_id) = app
                            .thread_tab(index)
                            .and_then(|thread| thread.thread_id.clone())
                        {
                            app.backend.fire::<ThreadSetNameResponse, _>(|request_id| {
                                ClientRequest::ThreadSetName {
                                    request_id,
                                    params: ThreadSetNameParams { thread_id, name },
                                }
                            });
                        }
                    }),
                );
            }
            "fork" => self.fork_tab(index, None),
            "side" => self.side_chat_start(index, /*first_message*/ None),
            "recap" => self.recap_start(index),
            "worktree" => self.worktree_continue(index),
            "compact" => {
                if let Some(thread_id) = thread_id {
                    self.backend.call(
                        |request_id| ClientRequest::ThreadCompactStart {
                            request_id,
                            params: ThreadCompactStartParams { thread_id },
                        },
                        move |app, result: Result<ThreadCompactStartResponse, BackendError>| {
                            if let Err(err) = result
                                && let Some(index) = app.tab_index_by_id(tab_id)
                            {
                                app.transcript_push_notice(
                                    index,
                                    NoticeKind::Error,
                                    format!("Could not compact: {}", err.user_message()),
                                );
                            }
                        },
                    );
                }
            }
            "review" => self.review_open(index, /*instructions*/ ""),
            "init" => {
                self.send_user_input(index, vec![session::text_input(INIT_PROMPT)]);
            }
            "export" => self.export_thread_markdown(index),
            "view-text" => self.transcript_view_thread_as_text(index),
            "copy-id" => {
                if let Some(thread_id) = thread_id {
                    self.copy_to_clipboard(&thread_id);
                }
            }
            "archive" => {
                let Some(thread_id) = thread_id else {
                    return;
                };
                self.show_dialog(
                    DialogRequest::confirm(
                        "Archive thread?",
                        "The thread is hidden from the thread list. You can restore it from the archived view.",
                    )
                    .accept_label("Archive"),
                    Box::new(move |app, accepted| {
                        if accepted.is_none() {
                            return;
                        }
                        app.backend.call(
                            |request_id| ClientRequest::ThreadArchive {
                                request_id,
                                params: ThreadArchiveParams { thread_id },
                            },
                            move |app, result: Result<ThreadArchiveResponse, BackendError>| {
                                match result {
                                    Ok(_) => {
                                        if let Some(index) = app.tab_index_by_id(tab_id) {
                                            app.close_tab(index);
                                        }
                                        app.sidebar_refresh();
                                    }
                                    Err(err) => app.toast(format!(
                                        "Could not archive: {}",
                                        err.user_message()
                                    )),
                                }
                            },
                        );
                    }),
                );
            }
            other => tracing::debug!(action = other, "unknown tab action"),
        }
    }

    /// Asks for a destination, pages in the whole thread, and writes it as
    /// Markdown in the TUI's export format.
    fn export_thread_markdown(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let file_name = self
            .thread_tab(index)
            .map(|thread| sanitize_file_name(&thread.title()))
            .unwrap_or_else(|| "thread".to_string());
        let directory = self.thread_tab(index).map(|thread| thread.cwd.clone());
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Export thread as Markdown")
            .set_file_name(format!("{file_name}.md"))
            .add_filter("Markdown", &["md"]);
        if let Some(directory) = directory {
            dialog = dialog.set_directory(directory);
        }
        let future = dialog.save_file();
        let spawned = slint::spawn_local(async move {
            let Some(handle) = future.await else {
                return;
            };
            let path = handle.path().to_path_buf();
            crate::ui_thread::with_app(move |app| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                app.toast("Exporting…");
                // Side chats cannot be paged; everything they have is loaded.
                if app
                    .thread_tab(index)
                    .is_some_and(|thread| thread.extras.side_parent.is_some())
                {
                    let markdown = app.transcript_export_markdown(index);
                    app.write_export(path, Ok(markdown));
                } else {
                    app.transcript_export_markdown_complete(index, move |app, markdown| {
                        app.write_export(path, markdown);
                    });
                }
            });
        });
        if let Err(err) = spawned {
            tracing::warn!(%err, "could not open save dialog");
        }
    }

    fn write_export(&mut self, path: PathBuf, markdown: Result<String, String>) {
        let markdown = match markdown {
            Ok(markdown) => markdown,
            Err(err) => {
                self.toast(format!("Export failed: {err}"));
                return;
            }
        };
        self.backend.spawn(async move {
            let write_path = path.clone();
            let result = tokio::task::spawn_blocking(move || std::fs::write(&write_path, markdown))
                .await
                .map_err(std::io::Error::other)
                .and_then(|result| result);
            crate::ui_thread::post(move |app| match result {
                Ok(()) => app.toast(format!("Exported to {}", path.display())),
                Err(err) => app.toast(format!("Export failed: {err}")),
            });
        });
    }

    /// Notifications for thread state kept here (after the generic per-tab
    /// handling in `app.rs`), and for threads that have no tab (recaps).
    pub(crate) fn threads_on_notification(&mut self, notification: &ServerNotification) {
        self.recap_on_notification(notification);
        let index = crate::app::notification_thread_id(notification)
            .and_then(|thread_id| self.tab_index_for_thread(thread_id));
        let Some(index) = index else {
            return;
        };
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        match notification {
            // The id of a turn the user already asked to stop.
            ServerNotification::TurnStarted(started)
                if std::mem::take(&mut thread.extras.interrupt_pending) =>
            {
                let thread_id = started.thread_id.clone();
                let turn_id = started.turn.id.clone();
                self.send_interrupt(tab_id, thread_id, turn_id, /*attempt*/ 0);
            }
            ServerNotification::TurnCompleted(_)
            | ServerNotification::ThreadStatusChanged(_)
            | ServerNotification::ThreadClosed(_)
                if !thread.is_busy() =>
            {
                thread.extras.interrupt_pending = false;
            }
            _ => {}
        }
    }
}

enum SteerFailure {
    /// No turn is running any more; start a new one instead.
    TurnEnded,
    /// The running turn has a different id than we thought.
    DifferentTurn(String),
    /// Review and compaction turns cannot be steered.
    NotSteerable,
    Other,
}

fn classify_steer_error(
    message: &str,
    error: Option<&codex_app_server_protocol::JSONRPCErrorError>,
) -> SteerFailure {
    if message.contains("no active turn") {
        return SteerFailure::TurnEnded;
    }
    if let Some(rest) = message.split("but found `").nth(1)
        && let Some(actual) = rest.split('`').next()
        && !actual.is_empty()
    {
        return SteerFailure::DifferentTurn(actual.to_string());
    }
    let not_steerable = error
        .and_then(|error| error.data.as_ref())
        .and_then(|data| {
            serde_json::from_value::<codex_app_server_protocol::TurnError>(data.clone()).ok()
        })
        .is_some_and(|turn_error| {
            matches!(
                turn_error.codex_error_info,
                Some(codex_app_server_protocol::CodexErrorInfo::ActiveTurnNotSteerable { .. })
            )
        });
    if not_steerable || message.contains("cannot steer") {
        return SteerFailure::NotSteerable;
    }
    SteerFailure::Other
}

/// Why a tab cannot take input, if it cannot: its thread never started, or
/// it was lost (a side chat after a restart). Such input would wait forever.
fn input_refusal(thread: &ThreadTab) -> Option<&'static str> {
    if thread.thread_id.is_some() {
        return None;
    }
    match thread.phase {
        ThreadPhase::Error => Some(
            "This thread could not start, so it cannot take messages. Close the tab and open the folder again.",
        ),
        ThreadPhase::Closed => Some("This thread has ended. Close the tab and start a new one."),
        ThreadPhase::Starting
        | ThreadPhase::Idle
        | ThreadPhase::Running
        | ThreadPhase::WaitingOnUser => None,
    }
}

/// A request failed because the thread was never saved: nothing was sent
/// in it yet, so the server has no rollout for it (legacy history) or did
/// not materialize it (paginated history).
fn is_unsaved_thread_error(message: &str) -> bool {
    message.contains("no rollout found for thread id")
        || message.contains("is not materialized yet")
}

/// Id of the in-progress turn on a newest-first page of turns.
fn running_turn_id(turns: &[Turn]) -> Option<String> {
    turns
        .iter()
        .find(|turn| turn.status == TurnStatus::InProgress)
        .map(|turn| turn.id.clone())
}

#[derive(Debug, PartialEq)]
enum InterruptFailure {
    /// The turn already ended.
    NoActiveTurn,
    /// Another turn is running (the server names it).
    DifferentTurn(String),
    Other,
}

/// What to do after a failed `turn/interrupt`.
#[derive(Debug, PartialEq)]
enum InterruptNext {
    /// Nothing is left to stop.
    Done,
    /// Send it again for `turn_id` after `delay`.
    Retry { turn_id: String, delay: Duration },
    /// Give up and tell the user.
    Report,
}

/// Decides the next step after attempt `attempt` (from 0) to stop
/// `turn_id` failed. `still_running` says whether that turn still runs as
/// far as the client knows: the server answers "no active turn" for a turn
/// that started a moment ago, before it can be interrupted.
fn next_interrupt_step(
    failure: InterruptFailure,
    turn_id: &str,
    attempt: u32,
    still_running: bool,
) -> InterruptNext {
    let attempts_left = attempt + 1 < INTERRUPT_ATTEMPTS;
    match failure {
        InterruptFailure::NoActiveTurn if !still_running => InterruptNext::Done,
        InterruptFailure::NoActiveTurn if attempts_left => InterruptNext::Retry {
            turn_id: turn_id.to_string(),
            delay: INTERRUPT_RETRY_DELAY,
        },
        InterruptFailure::DifferentTurn(actual) if attempts_left => InterruptNext::Retry {
            turn_id: actual,
            delay: Duration::ZERO,
        },
        InterruptFailure::NoActiveTurn
        | InterruptFailure::DifferentTurn(_)
        | InterruptFailure::Other => InterruptNext::Report,
    }
}

/// Classifies a `turn/interrupt` error, like the TUI's
/// `active_turn_interrupt_race`.
fn classify_interrupt_error(message: &str) -> InterruptFailure {
    if message.contains("no active turn") {
        return InterruptFailure::NoActiveTurn;
    }
    if let Some((_, actual)) = message
        .strip_prefix("expected active turn id ")
        .and_then(|rest| rest.split_once(" but found "))
    {
        let actual = actual.trim().trim_matches('`');
        if !actual.is_empty() {
            return InterruptFailure::DifferentTurn(actual.to_string());
        }
    }
    InterruptFailure::Other
}

/// Stops the running turn of `thread_id` and waits until it has stopped.
/// Without a known turn id, the in-progress turn is looked up; when there
/// is none, an interrupt without a turn id stops whatever the thread is
/// doing (the TUI's "startup interrupt").
async fn stop_turn(backend: &Backend, thread_id: &str, turn_id: Option<String>) {
    let running_turn = || async {
        backend
            .request::<ThreadTurnsListResponse>(session::turns_page(
                backend.next_request_id(),
                thread_id,
                /*cursor*/ None,
                /*limit*/ 1,
            ))
            .await
            .ok()
            .and_then(|page| running_turn_id(&page.data))
    };
    let mut turn_id = match turn_id {
        Some(turn_id) => turn_id,
        None => running_turn().await.unwrap_or_default(),
    };
    for attempt in 0..INTERRUPT_ATTEMPTS {
        let result = backend
            .request::<TurnInterruptResponse>(session::turn_interrupt(
                backend.next_request_id(),
                thread_id,
                &turn_id,
            ))
            .await;
        let Err(err) = result else {
            return;
        };
        let failure = classify_interrupt_error(&err.user_message());
        let still_running = failure == InterruptFailure::NoActiveTurn
            && running_turn().await.as_deref() == Some(turn_id.as_str());
        match next_interrupt_step(failure, &turn_id, attempt, still_running) {
            InterruptNext::Done => return,
            InterruptNext::Retry {
                turn_id: next,
                delay,
            } => {
                tokio::time::sleep(delay).await;
                turn_id = next;
            }
            InterruptNext::Report => {
                tracing::warn!(%err, thread_id, "could not stop the turn");
                return;
            }
        }
    }
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "thread".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn steer_errors_are_classified() {
        assert!(matches!(
            classify_steer_error("no active turn to steer", None),
            SteerFailure::TurnEnded
        ));
        match classify_steer_error("expected active turn id `a` but found `b`", None) {
            SteerFailure::DifferentTurn(actual) => assert_eq!(actual, "b"),
            _ => panic!("expected DifferentTurn"),
        }
        assert!(matches!(
            classify_steer_error("cannot steer a review turn", None),
            SteerFailure::NotSteerable
        ));
        assert!(matches!(
            classify_steer_error("boom", None),
            SteerFailure::Other
        ));
    }

    #[test]
    fn preview_uses_first_non_empty_text() {
        let input = vec![
            UserInput::LocalImage {
                detail: None,
                path: PathBuf::from("/x.png"),
            },
            session::text_input("  "),
            session::text_input(" hello "),
        ];
        assert_eq!(user_input_preview(&input), "hello");
    }

    #[test]
    fn interrupt_errors_are_classified() {
        // The server's format has no backticks; tolerate them anyway.
        assert_eq!(
            classify_interrupt_error("expected active turn id turn-a but found turn-b"),
            InterruptFailure::DifferentTurn("turn-b".to_string())
        );
        assert_eq!(
            classify_interrupt_error("expected active turn id `a` but found `b`"),
            InterruptFailure::DifferentTurn("b".to_string())
        );
        assert_eq!(
            classify_interrupt_error("no active turn to interrupt"),
            InterruptFailure::NoActiveTurn
        );
        assert_eq!(
            classify_interrupt_error("thread not found: x"),
            InterruptFailure::Other
        );
    }

    #[test]
    fn interrupts_of_just_started_turns_are_retried() {
        let retry = |turn_id: &str| InterruptNext::Retry {
            turn_id: turn_id.to_string(),
            delay: INTERRUPT_RETRY_DELAY,
        };
        // Not interruptible yet while the turn still runs: ask again.
        assert_eq!(
            next_interrupt_step(InterruptFailure::NoActiveTurn, "t1", 0, true),
            retry("t1")
        );
        // The turn ended meanwhile: nothing to stop.
        assert_eq!(
            next_interrupt_step(InterruptFailure::NoActiveTurn, "t1", 0, false),
            InterruptNext::Done
        );
        // Another turn runs: stop that one right away.
        assert_eq!(
            next_interrupt_step(
                InterruptFailure::DifferentTurn("t2".to_string()),
                "t1",
                0,
                true
            ),
            InterruptNext::Retry {
                turn_id: "t2".to_string(),
                delay: Duration::ZERO,
            }
        );
        // Out of attempts, or a real error: tell the user.
        assert_eq!(
            next_interrupt_step(
                InterruptFailure::NoActiveTurn,
                "t1",
                INTERRUPT_ATTEMPTS - 1,
                true
            ),
            InterruptNext::Report
        );
        assert_eq!(
            next_interrupt_step(InterruptFailure::Other, "t1", 0, true),
            InterruptNext::Report
        );
    }

    fn turn(id: &str, status: TurnStatus) -> Turn {
        Turn {
            id: id.to_string(),
            items: Vec::new(),
            items_view: Default::default(),
            status,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    #[test]
    fn running_turn_is_the_in_progress_one() {
        assert_eq!(
            running_turn_id(&[
                turn("t2", TurnStatus::InProgress),
                turn("t1", TurnStatus::Completed)
            ]),
            Some("t2".to_string())
        );
        assert_eq!(running_turn_id(&[turn("t1", TurnStatus::Completed)]), None);
        assert_eq!(running_turn_id(&[]), None);
    }

    #[test]
    fn unsaved_threads_are_recognized_by_the_resume_error() {
        assert!(is_unsaved_thread_error(
            "no rollout found for thread id 0199a8b0-0000-7000-8000-000000000000"
        ));
        assert!(is_unsaved_thread_error(
            "thread 01a1 is not materialized yet; thread/turns/list is unavailable before first user message"
        ));
        assert!(!is_unsaved_thread_error("thread not found"));
    }

    #[test]
    fn dead_tabs_refuse_input_instead_of_parking_it() {
        let mut thread = ThreadTab::new(PathBuf::from("/repo"));
        // Still starting: input waits for the thread.
        assert_eq!(input_refusal(&thread), None);
        thread.phase = ThreadPhase::Error;
        assert!(input_refusal(&thread).is_some());
        thread.phase = ThreadPhase::Closed;
        assert!(input_refusal(&thread).is_some());
        // A thread that exists reports its own errors when input fails.
        thread.thread_id = Some("t".to_string());
        thread.phase = ThreadPhase::Error;
        assert_eq!(input_refusal(&thread), None);
    }

    #[test]
    fn side_chats_refuse_actions_that_need_a_saved_thread() {
        for action in ["rename", "archive", "fork", "side", "worktree"] {
            assert!(side_chat_restriction(action).is_some(), "{action}");
        }
        for action in ["recap", "compact", "review", "export", "copy-id", "init"] {
            assert_eq!(side_chat_restriction(action), None, "{action}");
        }
    }

    #[test]
    fn run_ids_are_unique() {
        let first = next_run_id();
        assert!(next_run_id() > first);
    }

    #[test]
    fn file_names_are_sanitized() {
        assert_eq!(sanitize_file_name("a/b: c?"), "a-b- c-");
        assert_eq!(sanitize_file_name("///"), "---");
        assert_eq!(sanitize_file_name(""), "thread");
    }
}
