//! Messages that arrived from another tab.
//!
//! Cross-tab messages reach a thread as ordinary user input: agent messages
//! as a `<codex_gui_message>` envelope ([`crate::xtab::tools::wrap_agent_message`])
//! and user-forwarded content as a "Forwarded from tab" block
//! ([`crate::xtab::tools::forward_text`]). The transcript shows them as a
//! compact card with the inner message instead of the raw text, and copy,
//! export, and "view as text" use the readable form from this module.

use crate::xtab::tools::parse_agent_message;

const FORWARD_HEADER: &str = "Forwarded from tab \"";
const FORWARD_MARKER: &str = "--- forwarded content";
const FORWARD_END: &str = "\n--- end of forwarded content ---";

/// How the message was sent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CrossTabKind {
    /// Sent by the agent of another tab through the cross-tab tools.
    Agent { reply_expected: bool },
    /// Forwarded by the user with "Send to tab…", with an optional note.
    Forwarded { note: String },
}

/// A cross-tab message recognized in a thread item.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CrossTabMessage {
    pub(crate) kind: CrossTabKind,
    /// Title of the sending tab when the message was sent.
    pub(crate) from_title: String,
    /// Thread of the sending tab, when it had one.
    pub(crate) from_thread_id: Option<String>,
    /// The inner message (agent text or forwarded content).
    pub(crate) message: String,
}

impl CrossTabMessage {
    /// "Message from" / "Forwarded from", shown before the tab title.
    pub(crate) fn verb(&self) -> &'static str {
        match self.kind {
            CrossTabKind::Agent { .. } => "Message from",
            CrossTabKind::Forwarded { .. } => "Forwarded from",
        }
    }

    /// The note the user added when forwarding (empty otherwise).
    pub(crate) fn note(&self) -> &str {
        match &self.kind {
            CrossTabKind::Forwarded { note } => note,
            CrossTabKind::Agent { .. } => "",
        }
    }

    /// What "Copy" puts on the clipboard: the note and the inner message.
    pub(crate) fn copy_text(&self) -> String {
        let note = self.note().trim();
        let message = self.message.trim_end();
        if note.is_empty() {
            message.to_string()
        } else if message.is_empty() {
            note.to_string()
        } else {
            format!("{note}\n\n{message}")
        }
    }

    /// One-line provenance, e.g. `Message from tab "repo-a" (reply expected)`.
    pub(crate) fn provenance(&self) -> String {
        let mut line = format!("{} tab \"{}\"", self.verb(), self.from_title);
        if let Some(thread_id) = &self.from_thread_id {
            line.push_str(&format!(" (thread {thread_id})"));
        }
        if matches!(
            self.kind,
            CrossTabKind::Agent {
                reply_expected: true
            }
        ) {
            line.push_str(", reply expected");
        }
        line
    }

    /// Readable Markdown for export and "view as text": the provenance line
    /// followed by the note and the message. The line stays plain text, since
    /// tab titles often contain Markdown characters.
    pub(crate) fn readable(&self) -> String {
        let body = self.copy_text();
        if body.is_empty() {
            self.provenance()
        } else {
            format!("{}:\n\n{body}", self.provenance())
        }
    }
}

/// Recognizes a cross-tab message in the text of a thread item.
pub(crate) fn parse(text: &str) -> Option<CrossTabMessage> {
    if let Some(message) = parse_agent_message(text) {
        return Some(CrossTabMessage {
            kind: CrossTabKind::Agent {
                reply_expected: message.reply_expected,
            },
            from_title: message.from_title,
            from_thread_id: Some(message.from_thread_id).filter(|id| !id.trim().is_empty()),
            message: message.message,
        });
    }
    parse_forwarded(text)
}

/// Parses the text built by [`crate::xtab::tools::forward_text`]:
///
/// ```text
/// [note]                                   (optional, then a blank line)
/// Forwarded from tab "<title>":
/// --- forwarded content (thread <id>) ---  (or without the thread)
/// <content>
/// --- end of forwarded content ---
/// ```
fn parse_forwarded(text: &str) -> Option<CrossTabMessage> {
    let body = text.trim_end().strip_suffix(FORWARD_END)?;
    // The header is at the start, or after the note and a blank line. The
    // note could itself contain a header-like line, so try each candidate.
    let separated_header = format!("\n\n{FORWARD_HEADER}");
    let candidates = std::iter::once(0)
        .filter(|_| body.starts_with(FORWARD_HEADER))
        .chain(
            body.match_indices(separated_header.as_str())
                .map(|(index, _)| index + 2),
        );
    for start in candidates {
        if let Some(message) = parse_forwarded_at(body, start) {
            return Some(message);
        }
    }
    None
}

fn parse_forwarded_at(body: &str, start: usize) -> Option<CrossTabMessage> {
    let rest = body.get(start..)?.strip_prefix(FORWARD_HEADER)?;
    let (header, rest) = rest.split_once('\n')?;
    let title = header.strip_suffix("\":")?;
    let (marker, content) = match rest.split_once('\n') {
        Some((marker, content)) => (marker, content),
        // Forwarded empty content: the marker ends the body.
        None => (rest, ""),
    };
    let marker = marker.strip_prefix(FORWARD_MARKER)?.strip_suffix(" ---")?;
    let from_thread_id = if marker.is_empty() {
        None
    } else {
        let id = marker.strip_prefix(" (thread ")?.strip_suffix(')')?;
        Some(id.to_string()).filter(|id| !id.trim().is_empty())
    };
    let note = body[..start].trim_end().to_string();
    Some(CrossTabMessage {
        kind: CrossTabKind::Forwarded { note },
        from_title: title.to_string(),
        from_thread_id,
        message: content.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xtab::tools::forward_text;
    use crate::xtab::tools::wrap_agent_message;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_agent_envelopes() {
        let text = wrap_agent_message(
            "thread-a",
            "repo-a: <refactor> & tests",
            "Please run `cargo test` & report.",
            /*reply_expected*/ true,
        );
        let message = parse(&text);
        assert_eq!(
            message,
            Some(CrossTabMessage {
                kind: CrossTabKind::Agent {
                    reply_expected: true
                },
                from_title: "repo-a: <refactor> & tests".to_string(),
                from_thread_id: Some("thread-a".to_string()),
                message: "Please run `cargo test` & report.".to_string(),
            })
        );
        let message = message.unwrap_or_else(|| panic!("parsed"));
        assert_eq!(message.verb(), "Message from");
        assert_eq!(message.copy_text(), "Please run `cargo test` & report.");
        assert_eq!(
            message.readable(),
            "Message from tab \"repo-a: <refactor> & tests\" (thread thread-a), reply expected:\n\nPlease run `cargo test` & report."
        );
    }

    #[test]
    fn parses_forwarded_content_with_and_without_note() {
        let text = forward_text(
            "repo-b",
            Some("thread-b"),
            "Can you check this?",
            "# Findings\n\n- one\n- two\n",
        );
        assert_eq!(
            parse(&text),
            Some(CrossTabMessage {
                kind: CrossTabKind::Forwarded {
                    note: "Can you check this?".to_string()
                },
                from_title: "repo-b".to_string(),
                from_thread_id: Some("thread-b".to_string()),
                message: "# Findings\n\n- one\n- two".to_string(),
            })
        );
        let bare = forward_text("repo-b", /*source_thread_id*/ None, "  ", "hello");
        let message = parse(&bare).unwrap_or_else(|| panic!("parsed"));
        assert_eq!(message.note(), "");
        assert_eq!(message.from_thread_id, None);
        assert_eq!(message.message, "hello");
        assert_eq!(message.copy_text(), "hello");
        assert_eq!(
            message.readable(),
            "Forwarded from tab \"repo-b\":\n\nhello"
        );
    }

    #[test]
    fn forwarded_content_may_contain_marker_lines() {
        let content = "Forwarded from tab \"x\":\n--- end of forwarded content ---\nstill content";
        let text = forward_text("source", Some("t"), "note\n\nwith blank line", content);
        let message = parse(&text).unwrap_or_else(|| panic!("parsed"));
        assert_eq!(message.note(), "note\n\nwith blank line");
        assert_eq!(message.from_title, "source");
        assert_eq!(message.message, content);
    }

    #[test]
    fn forwarded_empty_content() {
        let text = forward_text("source", Some("t"), "only a note", "");
        let message = parse(&text).unwrap_or_else(|| panic!("parsed"));
        assert_eq!(message.message, "");
        assert_eq!(message.copy_text(), "only a note");
    }

    #[test]
    fn ordinary_text_is_not_a_cross_tab_message() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("Forwarded from tab \"x\": nothing else"), None);
        assert_eq!(parse("Talk about <codex_gui_message> tags in docs"), None);
        assert_eq!(
            parse("Forwarded from tab \"x\":\nno marker\n--- end of forwarded content ---"),
            None
        );
    }
}
