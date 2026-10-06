//! Bounds on agent-to-agent messaging, so agents cannot keep each other's
//! tabs busy without the user: a cap on sends per turn, a cap on messages
//! waiting in one tab, and a hop count along chains of messages.
//!
//! A message's hop is one more than the hop of the turn that sent it; turns
//! the user starts have hop 0. Messages up to [`MAX_UNATTENDED_HOPS`] may be
//! delivered under a session allowance; later ones always ask the user.

use std::collections::HashMap;
use std::time::Duration;

/// `send_message_to_thread` calls one turn may make.
pub(crate) const MAX_SENDS_PER_TURN: u32 = 5;
/// Agent messages that may wait for one tab (asking or queued).
pub(crate) const MAX_UNDELIVERED_PER_TARGET: usize = 10;
/// Hops delivered without asking, even for an allowed pair.
pub(crate) const MAX_UNATTENDED_HOPS: u32 = 3;
/// Queued messages older than this stop counting against their target: the
/// user may have removed them from the queue, which the GUI cannot see.
pub(crate) const UNDELIVERED_TTL: Duration = Duration::from_secs(60 * 60);

/// Per-turn send counts and hop numbers.
#[derive(Debug, Default)]
pub(crate) struct Limits {
    /// Sends so far, by (caller thread, caller turn).
    sends: HashMap<(String, String), u32>,
    /// Hop of the message that started a thread's current turn.
    turn_hops: HashMap<String, (String, u32)>,
}

impl Limits {
    /// Hop of a message sent from `turn_id` of `thread_id`.
    pub(crate) fn next_hop(&self, thread_id: &str, turn_id: &str) -> u32 {
        let current = self
            .turn_hops
            .get(thread_id)
            .filter(|(turn, _)| turn == turn_id)
            .map_or(0, |(_, hop)| *hop);
        current.saturating_add(1)
    }

    /// Records that a message with `hop` started `turn_id` in `thread_id`.
    pub(crate) fn turn_started_by_message(&mut self, thread_id: &str, turn_id: &str, hop: u32) {
        self.turn_hops
            .insert(thread_id.to_string(), (turn_id.to_string(), hop));
    }

    /// Counts one send from the caller's turn, or explains why it is refused.
    pub(crate) fn count_send(&mut self, thread_id: &str, turn_id: &str) -> Result<(), String> {
        let sent = self
            .sends
            .entry((thread_id.to_string(), turn_id.to_string()))
            .or_default();
        if *sent >= MAX_SENDS_PER_TURN {
            return Err(format!(
                "This turn already sent {MAX_SENDS_PER_TURN} cross-tab messages, the most codex-gui allows in one turn. Finish the turn; the user can forward more with Send to tab."
            ));
        }
        *sent += 1;
        Ok(())
    }

    pub(crate) fn turn_completed(&mut self, thread_id: &str, turn_id: &str) {
        self.sends
            .remove(&(thread_id.to_string(), turn_id.to_string()));
        if self
            .turn_hops
            .get(thread_id)
            .is_some_and(|(turn, _)| turn == turn_id)
        {
            self.turn_hops.remove(thread_id);
        }
    }

    pub(crate) fn forget_thread(&mut self, thread_id: &str) {
        self.sends.retain(|(thread, _), _| thread != thread_id);
        self.turn_hops.remove(thread_id);
    }
}

/// Refuses a send when `waiting` agent messages already wait for the target.
pub(crate) fn check_undelivered(waiting: usize, target_title: &str) -> Result<(), String> {
    if waiting >= MAX_UNDELIVERED_PER_TARGET {
        return Err(format!(
            "Tab \"{target_title}\" already has {waiting} cross-tab messages waiting to be delivered. Wait until it has worked through them before sending more."
        ));
    }
    Ok(())
}

/// Why a message on `hop` must ask even for an allowed pair, if it must.
pub(crate) fn hop_reason(hop: u32) -> Option<String> {
    (hop > MAX_UNATTENDED_HOPS).then(|| {
        format!(
            "this message is hop {hop} of a chain of agent-to-agent messages, and chains longer than {MAX_UNATTENDED_HOPS} always ask"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn hops_follow_the_turn_a_message_started() {
        let mut limits = Limits::default();
        assert_eq!(limits.next_hop("a", "user-turn"), 1);
        limits.turn_started_by_message("b", "b1", /*hop*/ 1);
        assert_eq!(limits.next_hop("b", "b1"), 2);
        // A later turn of the same thread (the user typed) starts over.
        assert_eq!(limits.next_hop("b", "b2"), 1);
        limits.turn_started_by_message("a", "a2", /*hop*/ 2);
        assert_eq!(limits.next_hop("a", "a2"), 3);
        limits.turn_completed("a", "a2");
        assert_eq!(limits.next_hop("a", "a2"), 1);
        limits.forget_thread("b");
        assert_eq!(limits.next_hop("b", "b1"), 1);
    }

    #[test]
    fn long_chains_always_ask() {
        assert_eq!(hop_reason(1), None);
        assert_eq!(hop_reason(MAX_UNATTENDED_HOPS), None);
        assert!(
            hop_reason(MAX_UNATTENDED_HOPS + 1)
                .is_some_and(|reason| reason.contains("hop 4 of a chain"))
        );
    }

    #[test]
    fn sends_per_turn_are_capped() {
        let mut limits = Limits::default();
        for _ in 0..MAX_SENDS_PER_TURN {
            assert_eq!(limits.count_send("a", "t1"), Ok(()));
        }
        assert!(
            limits
                .count_send("a", "t1")
                .is_err_and(|err| err.contains("already sent 5"))
        );
        // Other turns and threads have their own budget.
        assert_eq!(limits.count_send("a", "t2"), Ok(()));
        assert_eq!(limits.count_send("b", "t1"), Ok(()));
        limits.turn_completed("a", "t1");
        assert_eq!(limits.count_send("a", "t1"), Ok(()));
    }

    #[test]
    fn waiting_messages_per_target_are_capped() {
        assert_eq!(
            check_undelivered(MAX_UNDELIVERED_PER_TARGET - 1, "B"),
            Ok(())
        );
        assert!(
            check_undelivered(MAX_UNDELIVERED_PER_TARGET, "B")
                .is_err_and(|err| err.contains("Tab \"B\" already has 10"))
        );
    }
}
