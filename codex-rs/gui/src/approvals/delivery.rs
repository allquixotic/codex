//! The consent card for a message an agent in another tab wants to deliver
//! to this tab (cross-tab messaging, `crate::xtab`).
//!
//! The card waits in the target tab like any other request. The decision
//! goes back to the cross-tab controller, which delivers the message or
//! fails the sender's tool call.

use super::format;
use super::request::ApprovalOption;
use super::request::CardView;
use super::request::Decision;
use super::request::Tone;
use crate::transcript::NoticeKind;

/// Longest tab title quoted on the card.
const TITLE_CHARS: usize = 40;

/// What the user can do with a pending cross-tab message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryChoice {
    Deliver,
    /// Deliver, and deliver later messages of the same pair without asking
    /// for the rest of the session.
    DeliverAndAllow,
    Decline,
}

/// A message waiting for the user's consent in its target tab.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeliveryRequest {
    /// Key of the delivery in the cross-tab controller.
    pub(crate) id: u64,
    pub(crate) source_title: String,
    pub(crate) target_title: String,
    pub(crate) message: String,
    pub(crate) wait_for_reply: bool,
    /// Why messages of this pair always ask (no session allowance offered).
    pub(crate) always_ask: Option<String>,
}

impl DeliveryRequest {
    fn source(&self) -> String {
        quoted(&self.source_title)
    }

    pub(crate) fn options(&self) -> Vec<ApprovalOption> {
        let mut options = vec![ApprovalOption::new(
            "Deliver",
            'y',
            Tone::Positive,
            Decision::Delivery(DeliveryChoice::Deliver),
        )];
        if self.always_ask.is_none() {
            options.push(ApprovalOption::new(
                format!(
                    "Deliver, and allow {} → {} for this session",
                    self.source(),
                    quoted(&self.target_title)
                ),
                'a',
                Tone::Positive,
                Decision::Delivery(DeliveryChoice::DeliverAndAllow),
            ));
        }
        options.push(
            ApprovalOption::new(
                "Decline",
                'd',
                Tone::Negative,
                Decision::Delivery(DeliveryChoice::Decline),
            )
            .cancel(),
        );
        options
    }

    pub(crate) fn card(&self) -> CardView {
        let mut card = CardView::choices(
            format!("Agent in tab {} wants to send this message", self.source()),
            self.options(),
        );
        card.reason = "Delivering starts a turn in this tab, with this tab's permissions, as soon as it is idle. Deliver only messages you expect."
            .to_string();
        card.code = self.message.clone();
        card.code_caption = if self.wait_for_reply {
            "The sending agent waits for this tab's answer.".to_string()
        } else {
            "The sending agent does not wait for an answer.".to_string()
        };
        if let Some(reason) = &self.always_ask {
            card.details.push(format!(
                "**Always asks:** {}.",
                format::escape_markdown(reason)
            ));
        }
        card
    }

    pub(crate) fn notification_body(&self) -> String {
        format!("Agent in tab {} wants to send a message", self.source())
    }

    /// Transcript notice in the target tab for the user's decision.
    pub(crate) fn notice(&self, choice: DeliveryChoice) -> (NoticeKind, String) {
        let source = self.source();
        match choice {
            DeliveryChoice::Deliver => (
                NoticeKind::Info,
                format!("You delivered a message from tab {source}"),
            ),
            DeliveryChoice::DeliverAndAllow => (
                NoticeKind::Info,
                format!(
                    "You delivered a message from tab {source} and allowed its messages to this tab for this session"
                ),
            ),
            DeliveryChoice::Decline => (
                NoticeKind::Warning,
                format!("You declined a message from tab {source}"),
            ),
        }
    }
}

fn quoted(title: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("“{}”", crate::app::truncate_chars(&title, TITLE_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn request(always_ask: Option<&str>) -> DeliveryRequest {
        DeliveryRequest {
            id: 1,
            source_title: "PR review".to_string(),
            target_title: "Deploy\nscripts".to_string(),
            message: "run `make deploy` and report".to_string(),
            wait_for_reply: true,
            always_ask: always_ask.map(str::to_string),
        }
    }

    fn labels(options: &[ApprovalOption]) -> Vec<(String, String)> {
        options
            .iter()
            .map(|option| (option.key_label(), option.label.clone()))
            .collect()
    }

    #[test]
    fn offers_a_session_allowance_only_for_safe_pairs() {
        assert_eq!(
            labels(&request(None).options()),
            vec![
                ("Y".to_string(), "Deliver".to_string()),
                (
                    "A".to_string(),
                    "Deliver, and allow “PR review” → “Deploy scripts” for this session"
                        .to_string()
                ),
                ("Esc".to_string(), "Decline".to_string()),
            ]
        );
        assert_eq!(
            labels(&request(Some("it has full access")).options()),
            vec![
                ("Y".to_string(), "Deliver".to_string()),
                ("Esc".to_string(), "Decline".to_string()),
            ]
        );
    }

    #[test]
    fn the_card_shows_the_message_and_why_it_always_asks() {
        let card = request(Some(
            "it has full access while the sending tab has read-only access",
        ))
        .card();
        assert_eq!(
            card.title,
            "Agent in tab “PR review” wants to send this message"
        );
        assert_eq!(card.code, "run `make deploy` and report");
        assert_eq!(
            card.code_caption,
            "The sending agent waits for this tab's answer."
        );
        assert_eq!(
            card.details,
            vec![
                "**Always asks:** it has full access while the sending tab has read\\-only access."
                    .to_string()
            ]
        );
        assert!(request(None).card().details.is_empty());
    }

    #[test]
    fn notices_name_the_sending_tab() {
        assert_eq!(
            request(None).notice(DeliveryChoice::Decline),
            (
                NoticeKind::Warning,
                "You declined a message from tab “PR review”".to_string()
            )
        );
    }
}
