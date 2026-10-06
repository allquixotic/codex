//! Matching a message sent with `wait_for_reply` to the reply it produced.
//!
//! The message is queued in the target with a `client_user_message_id`. When
//! the target starts the turn that consumes it, the server echoes that id as
//! `UserMessage.client_id`; the wait then remembers the turn id and collects
//! the target's agent messages until `turn/completed` for that turn.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::time::Duration;

use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::models::MessagePhase;

pub(crate) type WaitId = u64;

/// A `send_message_to_thread` call that is waiting for the target's reply.
#[derive(Clone, Debug)]
pub(crate) struct Wait {
    /// The pending `item/tool/call` server request to answer.
    pub(crate) request_id: RequestId,
    pub(crate) caller_thread_id: String,
    pub(crate) caller_turn_id: String,
    pub(crate) target_thread_id: String,
    pub(crate) target_title: String,
    /// `client_user_message_id` of the queued message.
    pub(crate) client_id: String,
    /// Mailbox entry of the message.
    pub(crate) message_id: String,
    pub(crate) timeout: Duration,
    /// Turn of the target that consumed the message, once it started.
    pub(crate) target_turn_id: Option<String>,
    final_answer: Option<String>,
    last_message: Option<String>,
}

impl Wait {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request_id: RequestId,
        caller_thread_id: String,
        caller_turn_id: String,
        target_thread_id: String,
        target_title: String,
        client_id: String,
        message_id: String,
        timeout: Duration,
    ) -> Self {
        Self {
            request_id,
            caller_thread_id,
            caller_turn_id,
            target_thread_id,
            target_title,
            client_id,
            message_id,
            timeout,
            target_turn_id: None,
            final_answer: None,
            last_message: None,
        }
    }

    fn note_agent_message(&mut self, text: &str, phase: Option<&MessagePhase>) {
        if text.trim().is_empty() {
            return;
        }
        if !matches!(phase, Some(MessagePhase::Commentary)) {
            self.final_answer = Some(text.to_string());
        }
        self.last_message = Some(text.to_string());
    }

    fn reply(&self) -> Option<String> {
        self.final_answer
            .clone()
            .or_else(|| self.last_message.clone())
    }
}

/// How the target's turn ended.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TurnOutcome {
    pub(crate) turn_id: String,
    pub(crate) status: TurnStatus,
    pub(crate) error: Option<String>,
    /// The final answer, or the last agent message when none was final.
    /// `None` when no agent message was observed (the caller may re-read
    /// the turn from the server).
    pub(crate) reply: Option<String>,
}

#[derive(Debug)]
pub(crate) enum WaitEvent {
    /// The target finished the turn that carried the message.
    Finished {
        id: WaitId,
        wait: Wait,
        outcome: TurnOutcome,
    },
    /// The server stopped waiting for the tool call (the caller's turn ended
    /// or the request was resolved another way): drop it without answering.
    Abandoned { id: WaitId, wait: Wait },
}

#[derive(Debug, Default)]
pub(crate) struct WaitTracker {
    waits: BTreeMap<WaitId, Wait>,
    next_id: WaitId,
}

impl WaitTracker {
    pub(crate) fn insert(&mut self, wait: Wait) -> WaitId {
        self.next_id += 1;
        self.waits.insert(self.next_id, wait);
        self.next_id
    }

    pub(crate) fn remove(&mut self, id: WaitId) -> Option<Wait> {
        self.waits.remove(&id)
    }

    #[cfg(test)]
    fn get(&self, id: WaitId) -> Option<&Wait> {
        self.waits.get(&id)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.waits.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.waits.len()
    }

    /// Waits whose caller or target is `thread_id`.
    pub(crate) fn involving(&self, thread_id: &str) -> Vec<WaitId> {
        self.waits
            .iter()
            .filter(|(_, wait)| {
                wait.caller_thread_id == thread_id || wait.target_thread_id == thread_id
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether `target` already waits, directly or through other threads, for
    /// a reply from `caller`. A new wait from `caller` on `target` would then
    /// block both turns until one of them times out.
    pub(crate) fn would_deadlock(&self, caller: &str, target: &str) -> bool {
        let mut stack = vec![target];
        let mut seen = HashSet::new();
        while let Some(thread) = stack.pop() {
            if thread == caller {
                return true;
            }
            if !seen.insert(thread) {
                continue;
            }
            stack.extend(
                self.waits
                    .values()
                    .filter(|wait| wait.caller_thread_id == thread)
                    .map(|wait| wait.target_thread_id.as_str()),
            );
        }
        false
    }

    /// Advances waits on a server notification. Finished and abandoned waits
    /// are removed and returned.
    pub(crate) fn on_notification(&mut self, notification: &ServerNotification) -> Vec<WaitEvent> {
        if self.waits.is_empty() {
            return Vec::new();
        }
        match notification {
            ServerNotification::ItemStarted(started) => {
                self.note_item(&started.thread_id, &started.turn_id, &started.item);
                Vec::new()
            }
            ServerNotification::ItemCompleted(completed) => {
                self.note_item(&completed.thread_id, &completed.turn_id, &completed.item);
                Vec::new()
            }
            ServerNotification::TurnCompleted(completed) => {
                let thread_id = completed.thread_id.as_str();
                let turn = &completed.turn;
                let mut events = Vec::new();
                let ids: Vec<WaitId> = self.waits.keys().copied().collect();
                for id in ids {
                    let Some(wait) = self.waits.get_mut(&id) else {
                        continue;
                    };
                    if wait.caller_thread_id == thread_id && wait.caller_turn_id == turn.id {
                        if let Some(wait) = self.waits.remove(&id) {
                            events.push(WaitEvent::Abandoned { id, wait });
                        }
                        continue;
                    }
                    if wait.target_thread_id != thread_id {
                        continue;
                    }
                    if wait.target_turn_id.is_none()
                        && turn
                            .items
                            .iter()
                            .any(|item| is_user_message_with(item, &wait.client_id))
                    {
                        // The start notification was missed; the turn payload
                        // still proves this turn carried the message.
                        wait.target_turn_id = Some(turn.id.clone());
                    }
                    if wait.target_turn_id.as_deref() != Some(turn.id.as_str()) {
                        continue;
                    }
                    for item in &turn.items {
                        if let ThreadItem::AgentMessage { text, phase, .. } = item {
                            wait.note_agent_message(text, phase.as_ref());
                        }
                    }
                    let outcome = TurnOutcome {
                        turn_id: turn.id.clone(),
                        status: turn.status.clone(),
                        error: turn.error.as_ref().map(|error| error.message.clone()),
                        reply: wait.reply(),
                    };
                    if let Some(wait) = self.waits.remove(&id) {
                        events.push(WaitEvent::Finished { id, wait, outcome });
                    }
                }
                events
            }
            ServerNotification::ServerRequestResolved(resolved) => {
                let (matching, pending): (BTreeMap<WaitId, Wait>, BTreeMap<WaitId, Wait>) =
                    std::mem::take(&mut self.waits)
                        .into_iter()
                        .partition(|(_, wait)| {
                            wait.request_id == resolved.request_id
                                && wait.caller_thread_id == resolved.thread_id
                        });
                self.waits = pending;
                matching
                    .into_iter()
                    .map(|(id, wait)| WaitEvent::Abandoned { id, wait })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn note_item(&mut self, thread_id: &str, turn_id: &str, item: &ThreadItem) {
        for wait in self.waits.values_mut() {
            if wait.target_thread_id != thread_id {
                continue;
            }
            match item {
                ThreadItem::UserMessage { .. }
                    if wait.target_turn_id.is_none()
                        && is_user_message_with(item, &wait.client_id) =>
                {
                    wait.target_turn_id = Some(turn_id.to_string());
                }
                ThreadItem::AgentMessage { text, phase, .. }
                    if wait.target_turn_id.as_deref() == Some(turn_id) =>
                {
                    wait.note_agent_message(text, phase.as_ref());
                }
                _ => {}
            }
        }
    }
}

/// Whether `item` is the user message the GUI submitted as `client_id`.
pub(crate) fn is_user_message_with(item: &ThreadItem, client_id: &str) -> bool {
    matches!(item, ThreadItem::UserMessage { client_id: Some(id), .. } if id == client_id)
}

/// The reply in a list of items: the last final answer, else the last
/// agent message.
pub(crate) fn reply_from_items<'a>(
    items: impl IntoIterator<Item = &'a ThreadItem>,
) -> Option<String> {
    let mut final_answer = None;
    let mut last_message = None;
    for item in items {
        if let ThreadItem::AgentMessage { text, phase, .. } = item
            && !text.trim().is_empty()
        {
            if !matches!(phase, Some(MessagePhase::Commentary)) {
                final_answer = Some(text.clone());
            }
            last_message = Some(text.clone());
        }
    }
    final_answer.or(last_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ItemCompletedNotification;
    use codex_app_server_protocol::ItemStartedNotification;
    use codex_app_server_protocol::ServerRequestResolvedNotification;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnError;
    use codex_app_server_protocol::TurnItemsView;
    use pretty_assertions::assert_eq;

    fn wait(caller: &str, target: &str, client_id: &str) -> Wait {
        Wait::new(
            RequestId::Integer(7),
            caller.to_string(),
            "caller-turn".to_string(),
            target.to_string(),
            "Target".to_string(),
            client_id.to_string(),
            "msg-1".to_string(),
            Duration::from_secs(60),
        )
    }

    fn user_message(client_id: Option<&str>) -> ThreadItem {
        ThreadItem::UserMessage {
            id: "u1".to_string(),
            client_id: client_id.map(str::to_string),
            content: Vec::new(),
        }
    }

    fn agent_message(text: &str, phase: Option<MessagePhase>) -> ThreadItem {
        ThreadItem::AgentMessage {
            id: format!("a-{text}"),
            text: text.to_string(),
            phase,
            memory_citation: None,
            delivery: None,
            questions: None,
        }
    }

    fn started(thread: &str, turn: &str, item: ThreadItem) -> ServerNotification {
        ServerNotification::ItemStarted(ItemStartedNotification {
            item,
            thread_id: thread.to_string(),
            turn_id: turn.to_string(),
            started_at_ms: 0,
        })
    }

    fn completed_item(thread: &str, turn: &str, item: ThreadItem) -> ServerNotification {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            item,
            thread_id: thread.to_string(),
            turn_id: turn.to_string(),
            completed_at_ms: 0,
        })
    }

    fn turn_completed(
        thread: &str,
        turn: &str,
        status: TurnStatus,
        items: Vec<ThreadItem>,
    ) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: thread.to_string(),
            turn: Turn {
                id: turn.to_string(),
                items,
                items_view: TurnItemsView::default(),
                status,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        })
    }

    #[test]
    fn reply_is_the_final_answer_of_the_turn_that_consumed_the_message() {
        let mut tracker = WaitTracker::default();
        let id = tracker.insert(wait("A", "B", "client-1"));

        // An unrelated turn of the target finishes first.
        tracker.on_notification(&started("B", "t0", user_message(Some("other"))));
        tracker.on_notification(&completed_item("B", "t0", agent_message("ignored", None)));
        assert!(
            tracker
                .on_notification(&turn_completed(
                    "B",
                    "t0",
                    TurnStatus::Completed,
                    Vec::new()
                ))
                .is_empty()
        );

        tracker.on_notification(&started("B", "t1", user_message(Some("client-1"))));
        assert_eq!(
            tracker.get(id).and_then(|w| w.target_turn_id.clone()),
            Some("t1".to_string())
        );
        tracker.on_notification(&completed_item(
            "B",
            "t1",
            agent_message("working on it", Some(MessagePhase::Commentary)),
        ));
        tracker.on_notification(&completed_item(
            "B",
            "t1",
            agent_message("Done: 3 tests fixed", Some(MessagePhase::FinalAnswer)),
        ));
        // Agent messages from other threads never leak in.
        tracker.on_notification(&completed_item("C", "t1", agent_message("noise", None)));

        let events = tracker.on_notification(&turn_completed(
            "B",
            "t1",
            TurnStatus::Completed,
            Vec::new(),
        ));
        assert_eq!(events.len(), 1);
        let WaitEvent::Finished {
            id: done, outcome, ..
        } = &events[0]
        else {
            panic!("expected Finished, got {events:?}");
        };
        assert_eq!(*done, id);
        assert_eq!(
            outcome,
            &TurnOutcome {
                turn_id: "t1".to_string(),
                status: TurnStatus::Completed,
                error: None,
                reply: Some("Done: 3 tests fixed".to_string()),
            }
        );
        assert!(tracker.is_empty());
    }

    #[test]
    fn falls_back_to_turn_items_when_notifications_were_missed() {
        let mut tracker = WaitTracker::default();
        tracker.insert(wait("A", "B", "client-1"));
        let mut notification = turn_completed(
            "B",
            "t1",
            TurnStatus::Interrupted,
            vec![
                user_message(Some("client-1")),
                agent_message("partial", Some(MessagePhase::Commentary)),
            ],
        );
        if let ServerNotification::TurnCompleted(completed) = &mut notification {
            completed.turn.error = Some(TurnError {
                message: "stopped".to_string(),
                codex_error_info: None,
                additional_details: None,
                misalignment: None,
            });
        }
        let events = tracker.on_notification(&notification);
        let [WaitEvent::Finished { outcome, .. }] = events.as_slice() else {
            panic!("expected one Finished, got {events:?}");
        };
        assert_eq!(outcome.status, TurnStatus::Interrupted);
        assert_eq!(outcome.reply, Some("partial".to_string()));
        assert_eq!(outcome.error, Some("stopped".to_string()));
    }

    #[test]
    fn caller_turn_end_and_resolution_abandon_the_wait() {
        let mut tracker = WaitTracker::default();
        let first = tracker.insert(wait("A", "B", "c1"));
        let events = tracker.on_notification(&turn_completed(
            "A",
            "caller-turn",
            TurnStatus::Interrupted,
            Vec::new(),
        ));
        assert!(matches!(events.as_slice(), [WaitEvent::Abandoned { id, .. }] if *id == first));

        let second = tracker.insert(wait("A", "B", "c2"));
        let resolved =
            ServerNotification::ServerRequestResolved(ServerRequestResolvedNotification {
                thread_id: "A".to_string(),
                request_id: RequestId::Integer(7),
            });
        let events = tracker.on_notification(&resolved);
        assert!(matches!(events.as_slice(), [WaitEvent::Abandoned { id, .. }] if *id == second));
        assert!(tracker.is_empty());
    }

    #[test]
    fn other_caller_turns_do_not_abandon() {
        let mut tracker = WaitTracker::default();
        tracker.insert(wait("A", "B", "c1"));
        assert!(
            tracker
                .on_notification(&turn_completed(
                    "A",
                    "older-turn",
                    TurnStatus::Completed,
                    Vec::new()
                ))
                .is_empty()
        );
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn detects_wait_cycles() {
        let mut tracker = WaitTracker::default();
        tracker.insert(wait("A", "B", "c1"));
        tracker.insert(wait("B", "C", "c2"));
        assert!(tracker.would_deadlock("C", "A"));
        assert!(tracker.would_deadlock("B", "A"));
        assert!(!tracker.would_deadlock("A", "C"));
        assert!(!tracker.would_deadlock("D", "A"));
        assert_eq!(tracker.involving("B").len(), 2);
    }

    #[test]
    fn reply_from_items_prefers_final_answers() {
        let items = [
            agent_message("first", None),
            agent_message("progress", Some(MessagePhase::Commentary)),
        ];
        assert_eq!(reply_from_items(&items), Some("first".to_string()));
        let commentary_only = [agent_message("only", Some(MessagePhase::Commentary))];
        assert_eq!(reply_from_items(&commentary_only), Some("only".to_string()));
        assert_eq!(reply_from_items(&[user_message(None)]), None);
    }
}
