//! Cross-tab messaging (GUI.md §5.7, phase 1): agents in different tabs can
//! message each other, and the user can forward text between tabs.
//!
//! - Agents get the `codex_gui` dynamic tool namespace on `thread/start`
//!   ([`tools::tool_specs`]). Every `item/tool/call` for it is answered here
//!   exactly once; a call that waits for a reply stays pending until the
//!   target answers, times out, or the server gives up on it.
//! - An agent's message is delivered only with the user's consent: a card in
//!   the target tab (`crate::approvals::DeliveryRequest`) offers Deliver,
//!   Deliver and allow the pair for this session, or Decline. The sender's
//!   tool call stays pending until then; a decline fails it. A pair is never
//!   allowed when the target may do more than the sender ([`policy`]).
//! - [`limits`] caps sends per turn, messages waiting per tab, and makes
//!   long chains of agent-to-agent messages ask again.
//! - Messages are delivered with `thread/queue/add`: durable, dispatched as
//!   soon as the target is idle, and valid for threads that are not loaded.
//!   Agent messages carry an XML provenance block naming the sender.
//! - Every message is recorded in the mailbox ([`mailbox`]), shown in the
//!   info pane and readable through `read_thread_mailbox`.

mod limits;
mod mailbox;
mod policy;
pub(crate) mod tools;
mod wait;

use std::cell::Cell;
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_app_server_client::legacy_core::config::Config;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemsListParams;
use codex_app_server_protocol::ThreadItemsListResponse;
use codex_app_server_protocol::ThreadQueueAddParams;
use codex_app_server_protocol::ThreadQueueAddResponse;
use codex_app_server_protocol::ThreadQueueStartParams;
use codex_app_server_protocol::ThreadQueueStartResponse;
use codex_app_server_protocol::TurnStatus;
use serde::Serialize;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use crate::app::AppController;
use crate::app::Tab;
use crate::app::TabId;
use crate::app::ThreadPhase;
use crate::app::ThreadTab;
use crate::approvals::DeliveryChoice;
use crate::approvals::DeliveryRequest;
use crate::backend::BackendError;
use crate::session;
use crate::transcript::NoticeKind;
use crate::ui::XtabState;
use crate::ui::XtabTarget;
use mailbox::MailKind;
use mailbox::MailStatus;
use mailbox::MailboxEntry;
use mailbox::StoreOp;
use policy::TabPermissions;
use tools::OpenThread;
use tools::SendArgs;
use tools::ToolCall;
use wait::TurnOutcome;
use wait::Wait;
use wait::WaitEvent;
use wait::WaitId;

/// Items read back when a finished turn's reply was not seen live.
const REPLY_LOOKUP_LIMIT: u32 = 50;

/// Cross-tab state shared by all tabs.
#[derive(Default)]
pub(crate) struct XtabController {
    mailbox: mailbox::Mailbox,
    /// Writer task for the mailbox log, started on first use.
    store: OnceCell<mailbox::MailboxStore>,
    /// The log has been merged into `mailbox` (or there is no log).
    loaded: Cell<bool>,
    /// Lines in the log file, to decide when to compact it.
    file_lines: usize,
    /// `read_thread_mailbox` calls waiting for the log to load.
    pending_reads: Vec<PendingRead>,
    waits: wait::WaitTracker,
    wait_guards: HashMap<WaitId, WaitGuard>,
    /// Queued messages that have not started yet, by client id.
    deliveries: HashMap<String, QueuedDelivery>,
    /// Agent messages waiting for the user's consent in their target tab.
    consents: BTreeMap<u64, PendingDelivery>,
    next_consent_id: u64,
    /// (sender thread, target thread) pairs whose messages the user allowed
    /// for this session.
    allowed_pairs: HashSet<(String, String)>,
    limits: limits::Limits,
    /// Threads whose last turn was interrupted. Their queue does not drain
    /// on its own, so new messages are started explicitly.
    interrupted: HashSet<String>,
    dialog: Option<ForwardDialog>,
    targets: Rc<VecModel<XtabTarget>>,
    /// Tab chosen in the last forward, preselected next time.
    last_forward_target: Option<TabId>,
    /// Server restarts and disconnects seen so far (see [`ServerEpoch`]).
    resets: u64,
}

/// Identifies one run of the app-server. Server request ids restart with
/// the server, so an answer is only sent to the run that asked: the reset
/// counter changes on every restart or disconnect, and an embedded server's
/// config is replaced on every start.
#[derive(Clone, Default)]
struct ServerEpoch {
    resets: u64,
    config: Option<Weak<Config>>,
}

impl ServerEpoch {
    fn is_current(&self, resets: u64, config: Option<&Arc<Config>>) -> bool {
        self.resets == resets && same_allocation(self.config.as_ref(), config)
    }
}

/// Whether `weak` points at `current`. The `Weak` keeps its allocation
/// alive, so the address cannot have been reused by a later value.
fn same_allocation<T>(weak: Option<&Weak<T>>, current: Option<&Arc<T>>) -> bool {
    match (weak, current) {
        (Some(weak), Some(current)) => std::ptr::eq(weak.as_ptr(), Arc::as_ptr(current)),
        (None, None) => true,
        _ => false,
    }
}

struct PendingRead {
    request_id: RequestId,
    thread_id: String,
    limit: usize,
    epoch: ServerEpoch,
}

/// Keeps a wait's timeout alive; dropping it cancels the timer.
struct WaitGuard {
    epoch: ServerEpoch,
    _timer: slint::Timer,
}

/// A queued cross-tab message that has not started a turn yet.
struct QueuedDelivery {
    message_id: String,
    target_thread_id: String,
    /// Agent-to-agent hop of the message; 0 when the user forwarded it.
    hop: u32,
    queued_at: Instant,
}

/// An agent's message waiting for the user's consent. The sender's tool
/// call (`request_id`) is answered once the user decides.
struct PendingDelivery {
    epoch: ServerEpoch,
    request_id: RequestId,
    caller_thread_id: String,
    caller_turn_id: String,
    caller_title: String,
    target_thread_id: String,
    target_title: String,
    args: SendArgs,
    hop: u32,
}

struct ForwardDialog {
    source_tab_id: TabId,
    source_title: String,
    source_thread_id: Option<String>,
    /// Tab of each row in `XtabState.targets`.
    targets: Vec<TabId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForwardMode {
    /// `send_user_input`: start a turn, or steer/queue per the user's prefs.
    SendNow,
    /// `thread/queue/add`.
    Queue,
}

impl AppController {
    pub(crate) fn xtab_bind(&mut self) {
        // Without a CODEX_HOME there is nothing to load.
        self.xtab.loaded.set(self.codex_home.is_none());
        let state = self.window.global::<XtabState>();
        state.set_targets(ModelRc::from(self.xtab.targets.clone()));
        state.on_cancel(|| crate::ui_thread::with_app(AppController::xtab_close_dialog));
        state.on_submit(|queue| {
            let mode = if queue {
                ForwardMode::Queue
            } else {
                ForwardMode::SendNow
            };
            crate::ui_thread::with_app(move |app| app.xtab_submit_dialog(mode));
        });
    }

    /// Tools registered on every new thread when cross-tab messaging is on.
    pub(crate) fn xtab_dynamic_tools(&self) -> Option<Vec<DynamicToolSpec>> {
        Some(tools::tool_specs())
    }

    /// Answers (or parks) a dynamic tool call. Never leaves it unanswered.
    pub(crate) fn xtab_on_tool_call(
        &mut self,
        request_id: RequestId,
        params: DynamicToolCallParams,
    ) {
        let epoch = self.xtab_epoch();
        let call = match tools::parse_tool_call(
            params.namespace.as_deref(),
            &params.tool,
            &params.arguments,
        ) {
            Ok(call) => call,
            Err(message) => {
                tracing::debug!(tool = %params.tool, %message, "rejected dynamic tool call");
                self.xtab_fail(&epoch, request_id, &message);
                return;
            }
        };
        let Some(caller) = self
            .tab_index_for_thread(&params.thread_id)
            .and_then(|index| self.thread_tab(index))
        else {
            self.xtab_fail(
                &epoch,
                request_id,
                "This thread is not open in a Codex tab, so it cannot use the codex_gui tools.",
            );
            return;
        };
        if !caller.xtab_enabled {
            self.xtab_fail(
                &epoch,
                request_id,
                "Cross-tab messaging is turned off for this tab.",
            );
            return;
        }
        let caller_title = caller.title();
        match call {
            ToolCall::ListOpenThreads => {
                let threads = self.xtab_open_threads(&params.thread_id);
                self.xtab_succeed(&epoch, request_id, &tools::ThreadList { threads: &threads });
            }
            ToolCall::Send(args) => {
                self.xtab_agent_send(epoch, request_id, &params, caller_title, args);
            }
            ToolCall::ReadMailbox { limit } => {
                let read = PendingRead {
                    request_id,
                    thread_id: params.thread_id,
                    limit,
                    epoch,
                };
                self.xtab_ensure_store();
                if self.xtab.loaded.get() {
                    self.xtab_answer_read(read);
                } else {
                    self.xtab.pending_reads.push(read);
                }
            }
        }
    }

    /// The server stopped or restarted: its pending tool calls died with it,
    /// so waits and parked reads are dropped without an answer. Queued
    /// messages survive in the threads' durable queues.
    pub(crate) fn xtab_on_server_reset(&mut self) {
        self.xtab.resets += 1;
        let dropped = self.xtab.wait_guards.len() + self.xtab.pending_reads.len();
        if dropped > 0 {
            tracing::debug!(
                dropped,
                "server reset; dropping pending cross-tab tool calls"
            );
        }
        self.xtab.waits = wait::WaitTracker::default();
        self.xtab.wait_guards.clear();
        self.xtab.pending_reads.clear();
        self.xtab.interrupted.clear();
        // Their tool calls died too; `approvals_reset` drops the cards.
        self.xtab.consents.clear();
        self.xtab.limits = limits::Limits::default();
    }

    pub(crate) fn xtab_on_notification(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::ItemStarted(started) => {
                self.xtab_note_delivery(&started.thread_id, &started.turn_id, &started.item);
            }
            ServerNotification::ItemCompleted(completed) => {
                self.xtab_note_delivery(&completed.thread_id, &completed.turn_id, &completed.item);
            }
            ServerNotification::TurnCompleted(completed) => {
                if completed.turn.status == TurnStatus::Interrupted {
                    self.xtab.interrupted.insert(completed.thread_id.clone());
                } else {
                    self.xtab.interrupted.remove(&completed.thread_id);
                }
                let (thread_id, turn_id) = (&completed.thread_id, &completed.turn.id);
                self.xtab.limits.turn_completed(thread_id, turn_id);
                // The server gave up on the sender's tool call.
                self.xtab_forget_consents(|delivery| {
                    &delivery.caller_thread_id == thread_id && &delivery.caller_turn_id == turn_id
                });
            }
            ServerNotification::ServerRequestResolved(resolved) => {
                self.xtab_forget_consents(|delivery| {
                    delivery.request_id == resolved.request_id
                        && delivery.caller_thread_id == resolved.thread_id
                });
            }
            ServerNotification::ThreadClosed(closed) => {
                self.xtab_forget_consents(|delivery| delivery.caller_thread_id == closed.thread_id);
            }
            _ => {}
        }
        for event in self.xtab.waits.on_notification(notification) {
            match event {
                WaitEvent::Finished { id, wait, outcome } => {
                    self.xtab_finish_wait(id, wait, outcome)
                }
                WaitEvent::Abandoned { id, wait } => {
                    // The server already failed the tool call (turn ended).
                    self.xtab.wait_guards.remove(&id);
                    tracing::debug!(
                        caller = %wait.caller_thread_id,
                        target = %wait.target_thread_id,
                        "cross-tab wait abandoned"
                    );
                }
            }
        }
    }

    /// Fails waits and pending messages that involve a closed tab and
    /// refreshes the dialog.
    pub(crate) fn xtab_on_tab_closed(&mut self, thread: &ThreadTab) {
        if let Some(thread_id) = thread.thread_id.as_deref() {
            self.xtab.interrupted.remove(thread_id);
            self.xtab.limits.forget_thread(thread_id);
            self.xtab_fail_consents(
                |delivery| delivery.caller_thread_id == thread_id,
                |_| "The calling tab was closed.".to_string(),
            );
            // Normally declined with the closed tab's cards already.
            self.xtab_fail_consents(
                |delivery| delivery.target_thread_id == thread_id,
                |delivery| {
                    format!(
                        "Tab \"{}\" was closed before the user decided.",
                        delivery.target_title
                    )
                },
            );
            for id in self.xtab.waits.involving(thread_id) {
                let Some((wait, epoch)) = self.xtab_take_wait(id) else {
                    continue;
                };
                let message = if wait.caller_thread_id == thread_id {
                    "The calling tab was closed.".to_string()
                } else if wait.target_turn_id.is_some() {
                    format!(
                        "Tab \"{}\" was closed while it was working on the message; its reply will not be returned.",
                        wait.target_title
                    )
                } else {
                    format!(
                        "Tab \"{}\" was closed before it replied. The message stays queued in that thread and runs when the thread is opened again.",
                        wait.target_title
                    )
                };
                self.xtab_fail(&epoch, wait.request_id, &message);
            }
        }
        if self.xtab.dialog.is_some() {
            self.xtab_refresh_dialog_targets();
        }
    }

    /// Opens the "Send to tab…" dialog to forward `text` from tab `from_index`.
    pub(crate) fn xtab_open_forward_dialog(&mut self, from_index: usize, text: String) {
        let Some(source) = self.thread_tab(from_index) else {
            self.toast("Only thread tabs can forward messages");
            return;
        };
        let source_title = source.title();
        let source_thread_id = source.thread_id.clone();
        // Forwarding a message another tab sent: forward its content.
        let text = match tools::parse_agent_message(&text) {
            Some(message) => message.message,
            None => text,
        };
        self.xtab.dialog = Some(ForwardDialog {
            source_tab_id: self.tabs[from_index].id,
            source_title: source_title.clone(),
            source_thread_id,
            targets: Vec::new(),
        });
        let state = self.window.global::<XtabState>();
        state.set_source_title(source_title.into());
        state.set_message(text.into());
        state.set_note(SharedString::new());
        state.set_error(SharedString::new());
        state.set_selected_target(-1);
        self.xtab_refresh_dialog_targets();
        self.window.global::<XtabState>().set_dialog_open(true);
    }

    /// Enables or disables cross-tab messaging for tab `index`. A disabled
    /// tab neither receives agent messages nor may use the tools.
    pub(crate) fn xtab_set_enabled(&mut self, index: usize, enabled: bool) {
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        thread.xtab_enabled = enabled;
        let (Some(thread_id), false) = (thread.thread_id.clone(), enabled) else {
            return;
        };
        // Turning messages off also withdraws consent given for this tab and
        // declines messages still asking.
        self.xtab
            .allowed_pairs
            .retain(|(_, target)| *target != thread_id);
        self.xtab_fail_consents(
            |delivery| delivery.target_thread_id == thread_id,
            |delivery| {
                format!(
                    "Tab \"{}\" turned off cross-tab messages.",
                    delivery.target_title
                )
            },
        );
    }

    /// The user decided on an agent's message (consent card in the target).
    pub(crate) fn xtab_delivery_decided(&mut self, id: u64, choice: DeliveryChoice) {
        let Some(delivery) = self.xtab.consents.remove(&id) else {
            return;
        };
        match choice {
            DeliveryChoice::Decline => {
                let message = format!(
                    "The user declined to deliver this message to tab \"{}\".",
                    delivery.target_title
                );
                self.xtab_fail(&delivery.epoch, delivery.request_id, &message);
            }
            DeliveryChoice::Deliver | DeliveryChoice::DeliverAndAllow => {
                // The option is only offered for safe pairs; permissions may
                // have changed while the card was waiting.
                if choice == DeliveryChoice::DeliverAndAllow
                    && self
                        .xtab_escalation(&delivery.caller_thread_id, &delivery.target_thread_id)
                        .is_none()
                {
                    self.xtab.allowed_pairs.insert((
                        delivery.caller_thread_id.clone(),
                        delivery.target_thread_id.clone(),
                    ));
                }
                self.xtab_deliver(delivery);
            }
        }
    }

    /// An agent's message was declined without the user deciding (its
    /// card could not be shown, or the target tab closed).
    pub(crate) fn xtab_delivery_dropped(&mut self, id: u64, reason: &str) {
        if let Some(delivery) = self.xtab.consents.remove(&id) {
            let message = format!(
                "The message was not delivered to tab \"{}\": {reason}",
                delivery.target_title
            );
            self.xtab_fail(&delivery.epoch, delivery.request_id, &message);
        }
    }

    /// Fails the tool calls of pending messages matching `predicate` and
    /// removes their cards.
    fn xtab_fail_consents(
        &mut self,
        predicate: impl Fn(&PendingDelivery) -> bool,
        message: impl Fn(&PendingDelivery) -> String,
    ) {
        let ids: Vec<u64> = self
            .xtab
            .consents
            .iter()
            .filter(|(_, delivery)| predicate(delivery))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(delivery) = self.xtab.consents.remove(id) {
                let text = message(&delivery);
                self.xtab_fail(&delivery.epoch, delivery.request_id, &text);
            }
        }
        self.approvals_remove_deliveries(&ids);
    }

    /// Drops pending messages whose tool call the server already gave up on.
    fn xtab_forget_consents(&mut self, predicate: impl Fn(&PendingDelivery) -> bool) {
        let ids: Vec<u64> = self
            .xtab
            .consents
            .iter()
            .filter(|(_, delivery)| predicate(delivery))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            self.xtab.consents.remove(id);
        }
        self.approvals_remove_deliveries(&ids);
    }

    /// Cross-tab messages sent or received by `thread_id`, oldest first.
    /// Replies to messages the thread sent are listed as incoming.
    pub(crate) fn xtab_mailbox_for_thread(&self, thread_id: &str) -> Vec<MailboxEntryView> {
        self.xtab_ensure_store();
        let mut views = Vec::new();
        for entry in self.xtab.mailbox.iter() {
            if entry.from_thread_id == thread_id {
                views.push(MailboxEntryView {
                    direction: MailDirection::Outgoing,
                    peer_thread_id: entry.to_thread_id.clone(),
                    peer_title: entry.to_title.clone(),
                    text: entry.text.clone(),
                    timestamp: entry.timestamp,
                });
                if let Some(reply) = &entry.reply {
                    views.push(MailboxEntryView {
                        direction: MailDirection::Incoming,
                        peer_thread_id: entry.to_thread_id.clone(),
                        peer_title: entry.to_title.clone(),
                        text: reply.clone(),
                        timestamp: entry.replied_at.unwrap_or(entry.timestamp),
                    });
                }
            } else if entry.to_thread_id == thread_id {
                views.push(MailboxEntryView {
                    direction: MailDirection::Incoming,
                    peer_thread_id: entry.from_thread_id.clone(),
                    peer_title: entry.from_title.clone(),
                    text: entry.text.clone(),
                    timestamp: entry.timestamp,
                });
            }
        }
        views.sort_by_key(|view| view.timestamp);
        views
    }

    /// Drives the forward dialog from automation scripts: `send`, `queue`,
    /// or `cancel`.
    pub(crate) fn xtab_automation_submit(&mut self, mode: &str) {
        match mode {
            "send" => self.xtab_submit_dialog(ForwardMode::SendNow),
            "queue" => self.xtab_submit_dialog(ForwardMode::Queue),
            _ => self.xtab_close_dialog(),
        }
    }

    // ----- tools --------------------------------------------------------

    fn xtab_open_threads(&self, caller_thread_id: &str) -> Vec<OpenThread> {
        self.tabs
            .iter()
            .filter_map(Tab::thread)
            .filter_map(|thread| {
                let thread_id = thread.thread_id.clone()?;
                Some(OpenThread {
                    is_self: thread_id == caller_thread_id,
                    title: thread.title(),
                    cwd: thread.cwd.display().to_string(),
                    status: phase_name(thread.phase),
                    accepts_messages: thread.xtab_enabled && thread.phase != ThreadPhase::Closed,
                    thread_id,
                })
            })
            .collect()
    }

    fn xtab_agent_send(
        &mut self,
        epoch: ServerEpoch,
        request_id: RequestId,
        params: &DynamicToolCallParams,
        caller_title: String,
        args: SendArgs,
    ) {
        let threads = self.xtab_open_threads(&params.thread_id);
        let target = match tools::resolve_target(&threads, &args.target)
            .and_then(|target| tools::check_target(target).map(|()| target.clone()))
        {
            Ok(target) => target,
            Err(message) => {
                self.xtab_fail(&epoch, request_id, &message);
                return;
            }
        };
        if let Some(message) = self.xtab_deadlock(&params.thread_id, &target, args.wait_for_reply) {
            self.xtab_fail(&epoch, request_id, &message);
            return;
        }
        let waiting = self.xtab_waiting_for(&target.thread_id);
        if let Err(message) = limits::check_undelivered(waiting, &target.title).and_then(|()| {
            self.xtab
                .limits
                .count_send(&params.thread_id, &params.turn_id)
        }) {
            self.xtab_fail(&epoch, request_id, &message);
            return;
        }
        let hop = self
            .xtab
            .limits
            .next_hop(&params.thread_id, &params.turn_id);
        let always_ask = self
            .xtab_escalation(&params.thread_id, &target.thread_id)
            .map(|reason| format!("this tab can do more than the sending tab: {reason}"))
            .or_else(|| limits::hop_reason(hop));
        let allowed = self
            .xtab
            .allowed_pairs
            .contains(&(params.thread_id.clone(), target.thread_id.clone()));
        let delivery = PendingDelivery {
            epoch,
            request_id,
            caller_thread_id: params.thread_id.clone(),
            caller_turn_id: params.turn_id.clone(),
            caller_title,
            target_thread_id: target.thread_id,
            target_title: target.title,
            args,
            hop,
        };
        if allowed && always_ask.is_none() {
            self.xtab_deliver(delivery);
        } else {
            self.xtab_ask_consent(delivery, always_ask);
        }
    }

    /// Why waiting on `target` from `caller` would deadlock, if it would.
    fn xtab_deadlock(&self, caller: &str, target: &OpenThread, wait: bool) -> Option<String> {
        (wait && self.xtab.waits.would_deadlock(caller, &target.thread_id))
            .then(|| deadlock_message(&target.title))
    }

    /// Agent messages waiting for `target_thread_id`: asking the user, or
    /// queued and not started yet.
    fn xtab_waiting_for(&self, target_thread_id: &str) -> usize {
        let asking = self
            .xtab
            .consents
            .values()
            .filter(|delivery| delivery.target_thread_id == target_thread_id)
            .count();
        let queued = self
            .xtab
            .deliveries
            .values()
            .filter(|delivery| {
                delivery.hop > 0
                    && delivery.target_thread_id == target_thread_id
                    && delivery.queued_at.elapsed() < limits::UNDELIVERED_TTL
            })
            .count();
        asking + queued
    }

    /// Why `target_thread_id` may do more than `caller_thread_id`, so that
    /// their messages always ask. The target's pending composer overrides
    /// count too: they apply to its next turn.
    fn xtab_escalation(&self, caller_thread_id: &str, target_thread_id: &str) -> Option<String> {
        let thread = |thread_id: &str| {
            self.tab_index_for_thread(thread_id)
                .and_then(|index| self.thread_tab(index))
        };
        let (Some(caller), Some(target)) = (thread(caller_thread_id), thread(target_thread_id))
        else {
            return Some("one of the tabs is not open".to_string());
        };
        let caller_permissions = TabPermissions {
            approval: caller
                .approval_policy
                .or(caller.turn_overrides.approval_policy),
            sandbox: caller
                .sandbox
                .as_ref()
                .or(caller.turn_overrides.sandbox_policy.as_ref()),
            cwd: &caller.cwd,
        };
        let target_now = TabPermissions {
            approval: target.approval_policy,
            sandbox: target.sandbox.as_ref(),
            cwd: &target.cwd,
        };
        let target_next = TabPermissions {
            approval: target
                .turn_overrides
                .approval_policy
                .or(target.approval_policy),
            sandbox: target
                .turn_overrides
                .sandbox_policy
                .as_ref()
                .or(target.sandbox.as_ref()),
            cwd: &target.cwd,
        };
        policy::escalation(&caller_permissions, &target_now)
            .or_else(|| policy::escalation(&caller_permissions, &target_next))
    }

    /// Shows the consent card for `delivery` in its target tab.
    fn xtab_ask_consent(&mut self, delivery: PendingDelivery, always_ask: Option<String>) {
        let Some(target_index) = self.tab_index_for_thread(&delivery.target_thread_id) else {
            let message = format!("Tab \"{}\" is no longer open.", delivery.target_title);
            self.xtab_fail(&delivery.epoch, delivery.request_id, &message);
            return;
        };
        self.xtab.next_consent_id += 1;
        let id = self.xtab.next_consent_id;
        let card = DeliveryRequest {
            id,
            source_title: delivery.caller_title.clone(),
            target_title: delivery.target_title.clone(),
            message: delivery.args.message.clone(),
            wait_for_reply: delivery.args.wait_for_reply,
            always_ask,
        };
        let request_id = delivery.request_id.clone();
        let caller_thread_id = delivery.caller_thread_id.clone();
        let caller_turn_id = delivery.caller_turn_id.clone();
        if let Some(caller_index) = self.tab_index_for_thread(&caller_thread_id) {
            self.transcript_push_notice(
                caller_index,
                NoticeKind::Info,
                format!(
                    "Waiting for you to allow this agent's message in tab “{}”",
                    delivery.target_title
                ),
            );
        }
        self.xtab.consents.insert(id, delivery);
        self.approvals_push_delivery(
            target_index,
            request_id,
            caller_thread_id,
            caller_turn_id,
            card,
        );
    }

    /// Queues an agent's message in its target and answers the sender (now,
    /// or with the reply when it waits for one).
    fn xtab_deliver(&mut self, delivery: PendingDelivery) {
        let PendingDelivery {
            epoch,
            request_id,
            caller_thread_id,
            caller_turn_id,
            caller_title,
            target_thread_id,
            target_title,
            args,
            hop,
        } = delivery;
        if !epoch.is_current(self.xtab.resets, self.config.as_ref()) {
            // The server that asked is gone, and the call with it.
            return;
        }
        // Another wait may have formed a cycle while the user decided.
        if args.wait_for_reply
            && self
                .xtab
                .waits
                .would_deadlock(&caller_thread_id, &target_thread_id)
        {
            self.xtab_fail(&epoch, request_id, &deadlock_message(&target_title));
            return;
        }
        let text = tools::wrap_agent_message(
            &caller_thread_id,
            &caller_title,
            &args.message,
            args.wait_for_reply,
        );
        let client_id = session::new_client_message_id();
        let message_id = new_message_id();
        self.xtab_record(MailboxEntry {
            id: message_id.clone(),
            from_thread_id: caller_thread_id.clone(),
            from_title: caller_title,
            to_thread_id: target_thread_id.clone(),
            to_title: target_title.clone(),
            text: mailbox::truncate_bytes(&args.message, mailbox::MAX_STORED_TEXT_BYTES),
            timestamp: now_secs(),
            kind: MailKind::Agent,
            status: MailStatus::Queued,
            reply: None,
            replied_at: None,
            error: None,
        });
        // Registered before the request so a fast dispatch is not missed.
        let wait_id = args.wait_for_reply.then(|| {
            self.xtab_start_wait(
                Wait::new(
                    request_id.clone(),
                    caller_thread_id,
                    caller_turn_id,
                    target_thread_id.clone(),
                    target_title.clone(),
                    client_id.clone(),
                    message_id.clone(),
                    args.timeout,
                ),
                epoch.clone(),
            )
        });
        let receipt = tools::SendReceipt {
            status: "queued",
            message_id: message_id.clone(),
            target_thread_id: target_thread_id.clone(),
            target_title: target_title.clone(),
            reply: None,
            note: Some(tools::QUEUED_NOTE),
        };
        self.xtab_enqueue(
            target_thread_id,
            text,
            client_id,
            message_id,
            hop,
            move |app, result| match (result, wait_id) {
                (Ok(()), None) => app.xtab_succeed(&epoch, request_id, &receipt),
                // Answered when the reply arrives, the wait times out, or a
                // tab closes.
                (Ok(()), Some(_)) => {}
                (Err(error), None) => app.xtab_fail(
                    &epoch,
                    request_id,
                    &format!("Could not deliver the message to tab \"{target_title}\": {error}"),
                ),
                (Err(error), Some(wait_id)) => {
                    if let Some((wait, epoch)) = app.xtab_take_wait(wait_id) {
                        app.xtab_fail(
                            &epoch,
                            wait.request_id,
                            &format!(
                                "Could not deliver the message to tab \"{target_title}\": {error}"
                            ),
                        );
                    }
                }
            },
        );
    }

    /// Adds `text` to the target's queue and reports the outcome to `on_done`.
    /// `hop` is the message's agent-to-agent hop (0 when the user sent it).
    fn xtab_enqueue(
        &mut self,
        target_thread_id: String,
        text: String,
        client_id: String,
        message_id: String,
        hop: u32,
        on_done: impl FnOnce(&mut AppController, Result<(), String>) + Send + 'static,
    ) {
        self.xtab.deliveries.insert(
            client_id.clone(),
            QueuedDelivery {
                message_id: message_id.clone(),
                target_thread_id: target_thread_id.clone(),
                hop,
                queued_at: Instant::now(),
            },
        );
        let thread_id = target_thread_id.clone();
        let client_user_message_id = client_id.clone();
        self.backend.call(
            |request_id| ClientRequest::ThreadQueueAdd {
                request_id,
                params: ThreadQueueAddParams {
                    thread_id,
                    input: vec![session::text_input(text)],
                    client_user_message_id,
                },
            },
            move |app, result: Result<ThreadQueueAddResponse, BackendError>| {
                let result = match result {
                    Ok(response) => {
                        app.xtab_after_enqueued(&target_thread_id, response.queued_submission.id);
                        Ok(())
                    }
                    Err(err) => {
                        let message = err.user_message();
                        app.xtab.deliveries.remove(&client_id);
                        app.xtab_update(&message_id, |entry| {
                            entry.status = MailStatus::Failed;
                            entry.error = Some(message.clone());
                        });
                        Err(message)
                    }
                };
                on_done(app, result);
            },
        );
    }

    /// The queue drains by itself when a turn completes or an idle thread
    /// gets a new item, but not after an interrupt; start it then.
    fn xtab_after_enqueued(&mut self, target_thread_id: &str, submission_id: String) {
        let idle = self
            .tab_index_for_thread(target_thread_id)
            .and_then(|index| self.thread_tab(index))
            .is_some_and(|thread| {
                thread.phase == ThreadPhase::Idle && thread.active_turn_id.is_none()
            });
        if !idle || !self.xtab.interrupted.contains(target_thread_id) {
            return;
        }
        let thread_id = target_thread_id.to_string();
        self.backend.call(
            |request_id| ClientRequest::ThreadQueueStart {
                request_id,
                params: ThreadQueueStartParams {
                    thread_id,
                    queued_submission_id: Some(submission_id),
                },
            },
            |_app, result: Result<ThreadQueueStartResponse, BackendError>| {
                // Losing a race with another dispatch is harmless.
                if let Err(err) = result {
                    tracing::debug!(%err, "could not start the queued cross-tab message");
                }
            },
        );
    }

    fn xtab_start_wait(&mut self, wait: Wait, epoch: ServerEpoch) -> WaitId {
        let timeout = wait.timeout;
        let id = self.xtab.waits.insert(wait);
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::SingleShot, timeout, move || {
            crate::ui_thread::with_app(move |app| app.xtab_wait_timed_out(id));
        });
        self.xtab.wait_guards.insert(
            id,
            WaitGuard {
                epoch,
                _timer: timer,
            },
        );
        id
    }

    fn xtab_take_wait(&mut self, id: WaitId) -> Option<(Wait, ServerEpoch)> {
        let wait = self.xtab.waits.remove(id)?;
        let guard = self.xtab.wait_guards.remove(&id)?;
        Some((wait, guard.epoch))
    }

    fn xtab_wait_timed_out(&mut self, id: WaitId) {
        let Some((wait, epoch)) = self.xtab_take_wait(id) else {
            return;
        };
        let state = if wait.target_turn_id.is_some() {
            "was delivered and that agent may still be working on it"
        } else {
            "is still queued in that thread"
        };
        self.xtab_fail(
            &epoch,
            wait.request_id,
            &format!(
                "No reply from tab \"{}\" within {} seconds. The message {state}; its reply will not be returned to you. Ask it to message you back if you still need an answer.",
                wait.target_title,
                wait.timeout.as_secs()
            ),
        );
    }

    fn xtab_finish_wait(&mut self, id: WaitId, wait: Wait, outcome: TurnOutcome) {
        let Some(guard) = self.xtab.wait_guards.remove(&id) else {
            return;
        };
        let epoch = guard.epoch;
        if outcome.reply.is_some() {
            self.xtab_answer_reply(&epoch, wait, outcome);
            return;
        }
        // Live agent-message notifications can be dropped under load; read
        // the turn back from the server before reporting "no answer".
        let thread_id = wait.target_thread_id.clone();
        let turn_id = outcome.turn_id.clone();
        self.backend.call(
            |request_id| ClientRequest::ThreadItemsList {
                request_id,
                params: ThreadItemsListParams {
                    thread_id,
                    turn_id: Some(turn_id),
                    cursor: None,
                    limit: Some(REPLY_LOOKUP_LIMIT),
                    sort_direction: Some(SortDirection::Desc),
                },
            },
            move |app, result: Result<ThreadItemsListResponse, BackendError>| {
                let mut outcome = outcome;
                match result {
                    Ok(response) => {
                        outcome.reply = wait::reply_from_items(
                            response.data.iter().rev().map(|entry| &entry.item),
                        );
                    }
                    Err(err) => tracing::debug!(%err, "could not read the replying turn"),
                }
                app.xtab_answer_reply(&epoch, wait, outcome);
            },
        );
    }

    fn xtab_answer_reply(&mut self, epoch: &ServerEpoch, wait: Wait, outcome: TurnOutcome) {
        match outcome.status {
            TurnStatus::Completed | TurnStatus::InProgress => {
                let reply = outcome
                    .reply
                    .as_deref()
                    .map(|reply| tools::truncate_chars(reply, tools::MAX_REPLY_CHARS));
                let stored = reply
                    .as_deref()
                    .map(|reply| mailbox::truncate_bytes(reply, mailbox::MAX_STORED_TEXT_BYTES));
                self.xtab_update(&wait.message_id, |entry| {
                    entry.status = MailStatus::Replied;
                    entry.reply = stored;
                    entry.replied_at = Some(now_secs());
                });
                let note = reply.is_none().then_some(tools::NO_TEXT_REPLY_NOTE);
                let receipt = tools::SendReceipt {
                    status: "replied",
                    message_id: wait.message_id,
                    target_thread_id: wait.target_thread_id,
                    target_title: wait.target_title,
                    reply,
                    note,
                };
                self.xtab_succeed(epoch, wait.request_id, &receipt);
            }
            TurnStatus::Interrupted | TurnStatus::Failed => {
                let what = if outcome.status == TurnStatus::Interrupted {
                    "was interrupted"
                } else {
                    "failed"
                };
                let mut text = format!(
                    "Tab \"{}\" received the message, but its turn {what} before it finished",
                    wait.target_title
                );
                if let Some(error) = &outcome.error {
                    text.push_str(&format!(": {error}"));
                }
                text.push('.');
                self.xtab_update(&wait.message_id, |entry| entry.error = Some(text.clone()));
                if let Some(reply) = &outcome.reply {
                    text.push_str(&format!(
                        " Its last message was:\n{}",
                        tools::truncate_chars(reply, 1_500)
                    ));
                }
                self.xtab_fail(epoch, wait.request_id, &text);
            }
        }
    }

    fn xtab_answer_read(&mut self, read: PendingRead) {
        let messages = self
            .xtab
            .mailbox
            .received_by(&read.thread_id, read.limit)
            .into_iter()
            .map(|entry| tools::MailboxMessage {
                message_id: &entry.id,
                from_thread_id: &entry.from_thread_id,
                from_title: &entry.from_title,
                kind: entry.kind.as_str(),
                status: entry.status.as_str(),
                timestamp: entry.timestamp,
                text: &entry.text,
            })
            .collect();
        let response = tools::success_response(&tools::MailboxList { messages });
        self.xtab_resolve(&read.epoch, read.request_id, &response);
    }

    fn xtab_succeed<T: Serialize>(&self, epoch: &ServerEpoch, request_id: RequestId, value: &T) {
        self.xtab_resolve(epoch, request_id, &tools::success_response(value));
    }

    fn xtab_fail(&self, epoch: &ServerEpoch, request_id: RequestId, message: &str) {
        self.xtab_resolve(epoch, request_id, &tools::failure_response(message));
    }

    fn xtab_epoch(&self) -> ServerEpoch {
        ServerEpoch {
            resets: self.xtab.resets,
            config: self.config.as_ref().map(Arc::downgrade),
        }
    }

    fn xtab_resolve(
        &self,
        epoch: &ServerEpoch,
        request_id: RequestId,
        response: &DynamicToolCallResponse,
    ) {
        if epoch.is_current(self.xtab.resets, self.config.as_ref()) {
            self.backend.resolve_typed(request_id, response);
        } else {
            tracing::debug!(
                ?request_id,
                "server restarted; dropping a stale tool answer"
            );
        }
    }

    // ----- mailbox ------------------------------------------------------

    /// The mailbox writer, started (and the log load queued) on first use.
    fn xtab_ensure_store(&self) -> Option<&mailbox::MailboxStore> {
        let codex_home = self.codex_home.as_deref()?;
        Some(self.xtab.store.get_or_init(|| {
            mailbox::MailboxStore::start(
                self.backend.runtime(),
                mailbox::mailbox_path(codex_home),
                |loaded| {
                    crate::ui_thread::post(move |app| app.xtab_mailbox_loaded(loaded));
                },
            )
        }))
    }

    fn xtab_mailbox_loaded(&mut self, loaded: std::io::Result<mailbox::LoadedLog>) {
        match loaded {
            Ok(log) => {
                if log.skipped > 0 {
                    tracing::warn!(
                        skipped = log.skipped,
                        "ignored unreadable cross-tab mailbox lines"
                    );
                }
                self.xtab.file_lines += log.lines;
                self.xtab.mailbox.merge_loaded(log.entries);
            }
            Err(err) => tracing::warn!(%err, "could not read the cross-tab mailbox"),
        }
        self.xtab.loaded.set(true);
        self.xtab_maybe_compact();
        for read in std::mem::take(&mut self.xtab.pending_reads) {
            self.xtab_answer_read(read);
        }
        self.xtab_refresh_info(&[]);
    }

    /// Records a new entry or a new version of an existing one.
    fn xtab_record(&mut self, entry: MailboxEntry) {
        let threads = [entry.from_thread_id.clone(), entry.to_thread_id.clone()];
        self.xtab.mailbox.upsert(entry.clone());
        let persisted = match self.xtab_ensure_store() {
            Some(store) => {
                store.send(StoreOp::Append(entry));
                true
            }
            None => false,
        };
        if persisted {
            self.xtab.file_lines += 1;
            self.xtab_maybe_compact();
        }
        self.xtab_refresh_info(&threads);
    }

    fn xtab_update(&mut self, message_id: &str, update: impl FnOnce(&mut MailboxEntry)) {
        let Some(entry) = self.xtab.mailbox.get_mut(message_id) else {
            return;
        };
        update(entry);
        let entry = entry.clone();
        self.xtab_record(entry);
    }

    fn xtab_maybe_compact(&mut self) {
        if !self.xtab.loaded.get() || self.xtab.file_lines < mailbox::COMPACT_AT_LINES {
            return;
        }
        let snapshot = self.xtab.mailbox.snapshot();
        let lines = snapshot.len();
        if let Some(store) = self.xtab_ensure_store() {
            store.send(StoreOp::Rewrite(snapshot));
        }
        self.xtab.file_lines = lines;
    }

    /// Marks a queued message delivered when its turn starts, and remembers
    /// the turn's hop for messages that turn sends.
    fn xtab_note_delivery(&mut self, thread_id: &str, turn_id: &str, item: &ThreadItem) {
        if self.xtab.deliveries.is_empty() {
            return;
        }
        let ThreadItem::UserMessage {
            client_id: Some(client_id),
            ..
        } = item
        else {
            return;
        };
        let Some(delivery) = self.xtab.deliveries.remove(client_id) else {
            return;
        };
        if delivery.hop > 0 {
            self.xtab
                .limits
                .turn_started_by_message(thread_id, turn_id, delivery.hop);
        }
        self.xtab_update(&delivery.message_id, |entry| {
            if entry.status == MailStatus::Queued {
                entry.status = MailStatus::Delivered;
            }
        });
    }

    /// Re-renders the info pane when it shows one of `thread_ids` (or any
    /// thread, when empty).
    fn xtab_refresh_info(&mut self, thread_ids: &[String]) {
        let active = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
            .and_then(|thread| thread.thread_id.clone());
        if active.is_some_and(|active| thread_ids.is_empty() || thread_ids.contains(&active)) {
            self.info_show();
        }
    }

    // ----- forward dialog -----------------------------------------------

    fn xtab_refresh_dialog_targets(&mut self) {
        let Some(dialog) = self.xtab.dialog.as_ref() else {
            return;
        };
        let state = self.window.global::<XtabState>();
        let previous = usize::try_from(state.get_selected_target())
            .ok()
            .and_then(|index| dialog.targets.get(index).copied());
        let source = dialog.source_tab_id;
        let home = dirs::home_dir();
        let mut ids = Vec::new();
        let mut rows = Vec::new();
        for tab in &self.tabs {
            let Some(thread) = tab.thread().filter(|_| tab.id != source) else {
                continue;
            };
            ids.push(tab.id);
            rows.push(target_row(thread, home.as_deref()));
        }
        let selected = previous
            .or(self.xtab.last_forward_target)
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
            .or_else(|| (!ids.is_empty()).then_some(0));
        self.xtab.targets.set_vec(rows);
        state.set_selected_target(
            selected
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
        if let Some(dialog) = self.xtab.dialog.as_mut() {
            dialog.targets = ids;
        }
    }

    fn xtab_submit_dialog(&mut self, mode: ForwardMode) {
        let Some(dialog) = self.xtab.dialog.as_ref() else {
            return;
        };
        let state = self.window.global::<XtabState>();
        let message = state.get_message().to_string();
        let note = state.get_note().to_string();
        let target_tab = usize::try_from(state.get_selected_target())
            .ok()
            .and_then(|index| dialog.targets.get(index).copied());
        let source_title = dialog.source_title.clone();
        let source_thread_id = dialog.source_thread_id.clone();
        let Some(target_tab) = target_tab else {
            self.xtab_dialog_error("Choose a tab to send to.");
            return;
        };
        let Some(target_index) = self.tab_index_by_id(target_tab) else {
            self.xtab_refresh_dialog_targets();
            self.xtab_dialog_error("That tab was closed. Choose another one.");
            return;
        };
        if message.trim().is_empty() {
            self.xtab_dialog_error("The message is empty.");
            return;
        }
        let Some(target) = self.thread_tab(target_index) else {
            return;
        };
        let target_title = target.title();
        let target_thread_id = target.thread_id.clone();
        if mode == ForwardMode::Queue && target_thread_id.is_none() {
            self.xtab_dialog_error(
                "That tab is still starting. Use Send now, or try again in a moment.",
            );
            return;
        }
        let text = tools::forward_text(&source_title, source_thread_id.as_deref(), &note, &message);
        let stored = match note.trim() {
            "" => message.trim_end().to_string(),
            note => format!("{note}\n\n{}", message.trim_end()),
        };
        let message_id = new_message_id();
        self.xtab_record(MailboxEntry {
            id: message_id.clone(),
            from_thread_id: source_thread_id.unwrap_or_default(),
            from_title: source_title,
            to_thread_id: target_thread_id.clone().unwrap_or_default(),
            to_title: target_title.clone(),
            text: mailbox::truncate_bytes(&stored, mailbox::MAX_STORED_TEXT_BYTES),
            timestamp: now_secs(),
            kind: MailKind::User,
            status: match mode {
                ForwardMode::SendNow => MailStatus::Sent,
                ForwardMode::Queue => MailStatus::Queued,
            },
            reply: None,
            replied_at: None,
            error: None,
        });
        match (mode, target_thread_id) {
            (ForwardMode::Queue, Some(target_thread_id)) => {
                let title = target_title;
                self.xtab_enqueue(
                    target_thread_id,
                    text,
                    session::new_client_message_id(),
                    message_id,
                    /*hop*/ 0,
                    move |app, result| match result {
                        Ok(()) => app.toast(format!("Queued for “{title}”")),
                        Err(error) => app.toast(format!("Could not queue for “{title}”: {error}")),
                    },
                );
            }
            _ => {
                // A tab that cannot take input says so itself.
                if self.send_user_input(target_index, vec![session::text_input(text)]) {
                    self.toast(format!("Sent to “{target_title}”"));
                }
            }
        }
        self.xtab.last_forward_target = Some(target_tab);
        self.xtab_close_dialog();
    }

    fn xtab_dialog_error(&self, message: &str) {
        self.window.global::<XtabState>().set_error(message.into());
    }

    fn xtab_close_dialog(&mut self) {
        self.xtab.dialog = None;
        self.xtab.targets.set_vec(Vec::new());
        let state = self.window.global::<XtabState>();
        state.set_dialog_open(false);
        state.set_message(SharedString::new());
        state.set_note(SharedString::new());
        state.set_error(SharedString::new());
        state.set_selected_target(-1);
    }
}

/// Direction of a cross-tab message relative to the thread being viewed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MailDirection {
    Incoming,
    Outgoing,
}

/// One cross-tab message for display in the info pane.
#[derive(Clone, Debug)]
pub(crate) struct MailboxEntryView {
    pub(crate) direction: MailDirection,
    pub(crate) peer_thread_id: String,
    pub(crate) peer_title: String,
    pub(crate) text: String,
    /// Unix seconds.
    pub(crate) timestamp: i64,
}

fn target_row(thread: &ThreadTab, home: Option<&Path>) -> XtabTarget {
    XtabTarget {
        title: thread.title().into(),
        folder: abbreviate_home(&thread.cwd, home).into(),
        status: phase_label(thread.phase).into(),
        busy: thread.is_busy(),
        can_queue: thread.thread_id.is_some() && thread.phase != ThreadPhase::Closed,
    }
}

/// Status reported by `list_open_threads`.
fn phase_name(phase: ThreadPhase) -> &'static str {
    match phase {
        ThreadPhase::Starting => "starting",
        ThreadPhase::Idle => "idle",
        ThreadPhase::Running => "running",
        ThreadPhase::WaitingOnUser => "waiting_on_user",
        ThreadPhase::Error => "error",
        ThreadPhase::Closed => "closed",
    }
}

/// Status shown in the forward dialog.
fn phase_label(phase: ThreadPhase) -> &'static str {
    match phase {
        ThreadPhase::Starting => "Starting",
        ThreadPhase::Idle => "Idle",
        ThreadPhase::Running => "Working",
        ThreadPhase::WaitingOnUser => "Needs you",
        ThreadPhase::Error => "Error",
        ThreadPhase::Closed => "Closed",
    }
}

fn abbreviate_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

fn deadlock_message(target_title: &str) -> String {
    format!(
        "Tab \"{target_title}\" is itself waiting for a reply from this thread, so waiting here would deadlock. Send without wait_for_reply instead."
    )
}

fn new_message_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn home_is_abbreviated_in_folder_labels() {
        let home = PathBuf::from("/Users/me");
        assert_eq!(
            abbreviate_home(Path::new("/Users/me/dev/repo"), Some(&home)),
            "~/dev/repo"
        );
        assert_eq!(abbreviate_home(Path::new("/Users/me"), Some(&home)), "~");
        assert_eq!(
            abbreviate_home(Path::new("/opt/repo"), Some(&home)),
            "/opt/repo"
        );
        assert_eq!(abbreviate_home(Path::new("/opt/repo"), None), "/opt/repo");
    }

    #[test]
    fn epochs_change_on_reset_and_on_a_new_config() {
        let epoch = ServerEpoch::default();
        assert!(epoch.is_current(/*resets*/ 0, None));
        assert!(!epoch.is_current(/*resets*/ 1, None));
    }

    #[test]
    fn epochs_match_only_the_same_server_run() {
        let first = Arc::new("config".to_string());
        let epoch = Arc::downgrade(&first);
        assert!(same_allocation(Some(&epoch), Some(&first)));
        let restarted = Arc::new("config".to_string());
        assert!(!same_allocation(Some(&epoch), Some(&restarted)));
        assert!(!same_allocation(Some(&epoch), None));
        assert!(!same_allocation(None, Some(&restarted)));
        assert!(same_allocation::<String>(None, None));
    }

    #[test]
    fn phase_names_are_stable() {
        assert_eq!(phase_name(ThreadPhase::WaitingOnUser), "waiting_on_user");
        assert_eq!(phase_label(ThreadPhase::Running), "Working");
    }
}
