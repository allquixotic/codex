//! Copy and export (port of the TUI's `/copy` and `/export` formats).

use std::path::Path;

use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::ThreadItem;

use super::blocks::Block;
use super::blocks::BlockKind;
use super::blocks::Part;
use super::crosstab;
use super::directives::followup_labels;
use super::directives::visible_markdown;
use super::markdown::markdown_to_plain;
use super::output::format_duration_ms;
use super::output::strip_ansi;
use super::render::compact_json;
use super::render::display_command;
use super::render::reasoning_summary_markdown;
use super::render::sanitize_user_text;
use super::render::user_cross_tab_message;
use super::render::user_message_parts;
use super::store::Body;
use super::store::Entry;
use super::store::Live;

pub(crate) const EXPORT_TITLE: &str = "# Codex conversation\n";
/// [`render_transcript`]'s error when nothing is exportable.
pub(crate) const NOTHING_TO_EXPORT: &str = "No conversation content to export.";

/// One exportable element.
pub(crate) enum ExportSource<'a> {
    /// An item; `live_text` overrides the text of a message still streaming.
    Item {
        item: &'a ThreadItem,
        live_text: Option<&'a str>,
    },
    Error(&'a str),
}

/// Exportable elements of the loaded transcript.
pub(crate) fn entry_sources(entries: &[Entry]) -> Vec<ExportSource<'_>> {
    entries
        .iter()
        .filter_map(|entry| match &entry.body {
            // Messages that never reached the server are not part of it.
            Body::Item(item) if item.unsent => None,
            Body::Item(item) => Some(ExportSource::Item {
                item: &item.item,
                live_text: match &item.live {
                    Live::Markdown(stream) => Some(stream.raw()),
                    _ => None,
                },
            }),
            Body::Notice {
                kind: super::NoticeKind::Error,
                text,
                ..
            } => Some(ExportSource::Error(text)),
            Body::TurnEnd(end) if !end.error_reported => {
                end.error.as_deref().map(ExportSource::Error)
            }
            _ => None,
        })
        .collect()
}

/// Renders the TUI's markdown transcript format. `Err` when nothing is
/// exportable.
pub(crate) fn render_transcript(
    sources: &[ExportSource<'_>],
    cwd: &Path,
) -> Result<String, String> {
    let mut markdown = String::from(EXPORT_TITLE);
    let mut in_review = false;
    for source in sources {
        let (heading, lines) = match source {
            ExportSource::Error(text) => ("Activity", vec![format!("error: {text}")]),
            ExportSource::Item { item, live_text } => {
                match item {
                    ThreadItem::EnteredReviewMode { .. } => in_review = true,
                    ThreadItem::ExitedReviewMode { .. } => in_review = false,
                    ThreadItem::UserMessage { .. } if in_review => continue,
                    _ => {}
                }
                match export_item(item, *live_text, cwd) {
                    Some(cell) => cell,
                    None => continue,
                }
            }
        };
        if lines.iter().all(|line| line.trim().is_empty()) {
            continue;
        }
        let indent = heading == "Activity";
        markdown.push_str(&format!("\n## {heading}\n\n"));
        for line in lines {
            for line in line.split('\n') {
                if indent {
                    markdown.push_str("    ");
                }
                markdown.push_str(&sanitize_user_text(line));
                markdown.push('\n');
            }
        }
    }
    if markdown == EXPORT_TITLE {
        Err(NOTHING_TO_EXPORT.to_string())
    } else {
        Ok(markdown)
    }
}

fn export_item(
    item: &ThreadItem,
    live_text: Option<&str>,
    cwd: &Path,
) -> Option<(&'static str, Vec<String>)> {
    let cell = match item {
        ThreadItem::UserMessage { content, .. } => {
            if let Some(message) = user_cross_tab_message(content) {
                return Some(("User", vec![message.readable()]));
            }
            let (text, chips) = user_message_parts(content);
            let mut lines = vec![text];
            let mut image = 0;
            let labels: Vec<String> = chips
                .iter()
                .map(|chip| {
                    if chip.kind == 0 {
                        image += 1;
                        format!("[Image #{image}]")
                    } else {
                        chip.text.clone()
                    }
                })
                .collect();
            if !labels.is_empty() {
                lines.push(String::new());
                lines.extend(labels);
            }
            ("User", lines)
        }
        ThreadItem::AgentMessage { text, .. } => {
            let text = live_text.filter(|_| text.is_empty()).unwrap_or(text);
            match crosstab::parse(text) {
                Some(message) => ("Assistant", vec![message.readable()]),
                None => ("Assistant", vec![message_markdown(text, cwd)]),
            }
        }
        ThreadItem::Plan { text, .. } => {
            let text = live_text.filter(|_| text.is_empty()).unwrap_or(text);
            ("Plan", vec![message_markdown(text, cwd)])
        }
        ThreadItem::Reasoning { summary, .. } => {
            ("Reasoning", vec![reasoning_summary_markdown(summary)])
        }
        ThreadItem::CommandExecution {
            command,
            status,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } => {
            let mut lines = vec![format!("$ {}", display_command(command))];
            if let Some(output) = aggregated_output {
                let output = strip_ansi(output);
                let output = output.trim_end();
                if !output.is_empty() {
                    lines.push(output.to_string());
                }
            }
            let duration = duration_ms
                .map(|ms| format!(" • {}", format_duration_ms(ms)))
                .unwrap_or_default();
            let outcome = match status {
                CommandExecutionStatus::InProgress => "In progress".to_string(),
                CommandExecutionStatus::Declined => "Declined".to_string(),
                CommandExecutionStatus::Completed if exit_code.unwrap_or(0) == 0 => {
                    format!("✓{duration}")
                }
                _ => format!(
                    "✗ ({}){duration}",
                    exit_code.filter(|code| *code != 0).unwrap_or(1)
                ),
            };
            lines.push(outcome);
            ("Activity", lines)
        }
        ThreadItem::FileChange {
            changes, status, ..
        } => {
            let mut lines = vec![format!(
                "file changes: {status:?} · {} changes",
                changes.len()
            )];
            for change in changes {
                lines.push(format!("{:?}: {}", change.kind, change.path));
                lines.extend(change.diff.lines().map(str::to_string));
            }
            ("Activity", lines)
        }
        ThreadItem::McpToolCall {
            server,
            tool,
            status,
            arguments,
            result,
            error,
            ..
        } => {
            let mut lines = vec![format!(
                "mcp tool: {server}/{tool}({arguments}) · {status:?}"
            )];
            if let Some(result) = result {
                for content in &result.content {
                    match content.get("type").and_then(serde_json::Value::as_str) {
                        Some("text") => lines.push(
                            content
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        ),
                        Some("image") => lines.push("Returned image".to_string()),
                        Some("audio") => lines.push("<audio content>".to_string()),
                        Some("resource") => lines.push(format!(
                            "embedded resource: {}",
                            content
                                .pointer("/resource/uri")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("<unknown embedded resource>")
                        )),
                        Some("resource_link") => lines.push(format!(
                            "link: {}",
                            content
                                .get("uri")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                        )),
                        _ => lines.push(content.to_string()),
                    }
                }
                if let Some(structured) = &result.structured_content {
                    lines.push(format!("structured result: {structured}"));
                }
            }
            if let Some(error) = error {
                lines.push(format!("error: {}", error.message));
            }
            ("Activity", lines)
        }
        ThreadItem::DynamicToolCall {
            namespace,
            tool,
            arguments,
            status,
            content_items,
            ..
        } => {
            let name = match namespace {
                Some(namespace) => format!("{namespace}.{tool}"),
                None => tool.clone(),
            };
            let mut lines = vec![format!(
                "tool: {name}({}) · {status:?}",
                compact_json(arguments)
            )];
            for item in content_items.iter().flatten() {
                lines.push(match item {
                    DynamicToolCallOutputContentItem::InputText { text } => text.clone(),
                    DynamicToolCallOutputContentItem::InputImage { .. } => {
                        "Returned image".to_string()
                    }
                    DynamicToolCallOutputContentItem::InputAudio { .. } => {
                        "Returned audio".to_string()
                    }
                });
            }
            ("Activity", lines)
        }
        ThreadItem::WebSearch(search) => (
            "Activity",
            vec![format!("Searched the web: {}", search.query)],
        ),
        ThreadItem::ExitedReviewMode { review, .. } => {
            ("Assistant", vec![message_markdown(review, cwd)])
        }
        ThreadItem::EnteredReviewMode { review, .. } => {
            ("Activity", vec![format!("Code review started: {review}")])
        }
        ThreadItem::ContextCompaction { .. } => ("Activity", vec!["Context compacted".to_string()]),
        ThreadItem::ImageView { path, .. } => {
            ("Activity", vec![format!("Viewed image {}", path.as_str())])
        }
        ThreadItem::ImageGeneration(generation) => (
            "Activity",
            vec![format!(
                "Generated image: {}",
                generation.revised_prompt.clone().unwrap_or_default()
            )],
        ),
        ThreadItem::CollabAgentToolCall { tool, prompt, .. } => (
            "Activity",
            vec![format!(
                "agent {tool:?}: {}",
                prompt.clone().unwrap_or_default()
            )],
        ),
        ThreadItem::SubAgentActivity {
            kind, agent_path, ..
        } => (
            "Activity",
            vec![format!("sub-agent {kind:?}: {agent_path}")],
        ),
        ThreadItem::HookPrompt { .. }
        | ThreadItem::Sleep(_)
        | ThreadItem::FunctionCallOutput { .. } => return None,
    };
    Some(cell)
}

/// Agent markdown as the user should copy it.
pub(crate) fn message_markdown(text: &str, cwd: &Path) -> String {
    followup_labels(&visible_markdown(text, cwd))
}

/// "Copy as Markdown" for one entry. Cross-tab messages copy their inner
/// message.
pub(crate) fn entry_markdown(entry: &Entry, cwd: &Path) -> Option<String> {
    let Body::Item(item) = &entry.body else {
        return match &entry.body {
            Body::Notice { text, .. } => Some(text.clone()),
            Body::Card(card) => {
                Some(message_markdown(&card.markdown, cwd)).filter(|text| !text.trim().is_empty())
            }
            _ => None,
        };
    };
    if let Some(message) = entry_cross_tab_message(entry) {
        let text = message.copy_text();
        return (!text.trim().is_empty()).then_some(text);
    }
    let live = match &item.live {
        Live::Markdown(stream) => Some(stream.raw()),
        _ => None,
    };
    let text = match &item.item {
        ThreadItem::UserMessage { content, .. } => user_message_parts(content).0,
        ThreadItem::AgentMessage { text, .. } | ThreadItem::Plan { text, .. } => {
            message_markdown(live.filter(|_| text.is_empty()).unwrap_or(text), cwd)
        }
        ThreadItem::ExitedReviewMode { review, .. } => message_markdown(review, cwd),
        ThreadItem::Reasoning { summary, .. } => reasoning_summary_markdown(summary),
        other => {
            let (_, lines) = export_item(other, live, cwd)?;
            lines.join("\n")
        }
    };
    (!text.trim().is_empty()).then_some(text)
}

/// The cross-tab message shown by `entry`, if any.
pub(crate) fn entry_cross_tab_message(entry: &Entry) -> Option<crosstab::CrossTabMessage> {
    match &entry.item()?.item {
        ThreadItem::UserMessage { content, .. } => user_cross_tab_message(content),
        ThreadItem::AgentMessage { text, .. } => crosstab::parse(text),
        _ => None,
    }
}

/// A short name for `entry` and its text, for "View as text": what kind of
/// message it is, and its Markdown (cross-tab messages with their sender).
pub(crate) fn entry_text_view(entry: &Entry, cwd: &Path) -> Option<(String, String)> {
    if let Some(message) = entry_cross_tab_message(entry) {
        let title = format!("{} {}", message.verb(), message.from_title);
        return Some((title, message.readable()));
    }
    let text = entry_markdown(entry, cwd)?;
    let title = match &entry.body {
        Body::Card(card) => card.title.clone(),
        Body::Notice { title, .. } if !title.is_empty() => title.clone(),
        Body::Notice { .. } => "Notice".to_string(),
        Body::Item(item) => match &item.item {
            ThreadItem::UserMessage { .. } => "Your message".to_string(),
            ThreadItem::AgentMessage { .. } => "Reply".to_string(),
            ThreadItem::Plan { .. } => "Plan".to_string(),
            ThreadItem::Reasoning { .. } => "Reasoning".to_string(),
            ThreadItem::ExitedReviewMode { .. } => "Code review".to_string(),
            ThreadItem::CommandExecution { .. } => "Command".to_string(),
            _ => "Activity".to_string(),
        },
        _ => "Message".to_string(),
    };
    Some((title, text))
}

/// Text copied by a row's Copy action.
pub(crate) fn block_copy_text(block: &Block) -> String {
    if let Some(text) = &block.copy_text {
        return text.clone();
    }
    match block.kind {
        BlockKind::Table => block
            .table
            .as_ref()
            .map(|table| {
                table
                    .rows
                    .iter()
                    .map(|row| {
                        let cells: Vec<String> =
                            row.iter().map(|cell| markdown_to_plain(cell)).collect();
                        format!("| {} |", cells.join(" | "))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        _ => match &block.rich {
            Some(rich) => markdown_to_plain(rich),
            None if block.text.is_empty() => block.title.clone(),
            None => block.text.clone(),
        },
    }
}

/// The whole code block that row `local` of `blocks` belongs to: the text
/// of its rows, and the lines its note row left out. `None` when the row is
/// not code.
pub(crate) fn code_block_text(blocks: &[Block], local: usize) -> Option<String> {
    if blocks.get(local)?.kind != BlockKind::Code {
        return None;
    }
    let continues = |block: &Block| matches!(block.part, Part::Middle | Part::Last);
    let mut start = local;
    while start > 0 && continues(&blocks[start]) {
        start -= 1;
    }
    let mut end = local;
    while end + 1 < blocks.len() && continues(&blocks[end + 1]) {
        end += 1;
    }
    let mut text = String::new();
    for (offset, block) in blocks[start..=end].iter().enumerate() {
        if offset > 0 {
            text.push('\n');
        }
        // The note row of a cut block carries the lines it does not show.
        text.push_str(block.copy_text.as_deref().unwrap_or(&block.text));
    }
    Some(text)
}

/// `text` as a markdown block quote followed by a blank line.
pub(crate) fn quote(text: &str) -> String {
    let mut out: String = text
        .trim_end()
        .lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str("\n\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn code_rows_copy_the_whole_block() {
        use crate::transcript::markdown::BlockStyle;
        use crate::transcript::markdown::MAX_CODE_LINES;
        let code: Vec<String> = (0..MAX_CODE_LINES + 3)
            .map(|index| format!("line {index}"))
            .collect();
        let source = format!("Before.\n\n```\n{}\n```\n\nAfter.", code.join("\n"));
        let blocks = crate::transcript::markdown::render(&source, BlockStyle::default());
        assert!(blocks.len() > 4);
        let whole = code.join("\n");
        // Any row of the block, the note row included, copies all of it.
        for local in [1, 5, blocks.len() - 2] {
            assert_eq!(
                code_block_text(&blocks, local).as_deref(),
                Some(whole.as_str())
            );
        }
        assert_eq!(code_block_text(&blocks, 0), None);
    }

    #[test]
    fn exports_the_tui_format() {
        let user = ThreadItem::UserMessage {
            id: "u".to_string(),
            client_id: None,
            content: vec![crate::session::text_input("Run the tests")],
        };
        let exec = ThreadItem::CommandExecution {
            sandbox_type: None,
            model_context: None,
            id: "e".to_string(),
            plugin_id: None,
            script_path: None,
            command: "bash -lc 'cargo test'".to_string(),
            cwd: serde_json::from_value(serde_json::Value::String("/repo".to_string()))
                .unwrap_or_else(|_| panic!("path")),
            process_id: None,
            source: Default::default(),
            status: CommandExecutionStatus::Completed,
            command_actions: Vec::new(),
            aggregated_output: Some("ok\n".to_string()),
            exit_code: Some(0),
            duration_ms: Some(1200),
        };
        let agent = ThreadItem::AgentMessage {
            id: "a".to_string(),
            text: "All **green**. :codex-followup[Ship it]{id=\"x\"}".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        };
        let sources = vec![
            ExportSource::Item {
                item: &user,
                live_text: None,
            },
            ExportSource::Item {
                item: &exec,
                live_text: None,
            },
            ExportSource::Item {
                item: &agent,
                live_text: None,
            },
        ];
        assert_eq!(
            render_transcript(&sources, Path::new("/repo")),
            Ok("# Codex conversation\n\n## User\n\nRun the tests\n\n## Activity\n\n    $ cargo test\n    ok\n    ✓ • 1.20s\n\n## Assistant\n\nAll **green**. Ship it\n".to_string())
        );
        assert_eq!(
            render_transcript(&[], Path::new("/")),
            Err("No conversation content to export.".to_string())
        );
    }

    #[test]
    fn exports_cross_tab_messages_readably() {
        let envelope = crate::xtab::tools::wrap_agent_message(
            "thread-a",
            "repo-a",
            "Tests pass on **main**.",
            /*reply_expected*/ false,
        );
        let user = ThreadItem::UserMessage {
            id: "u".to_string(),
            client_id: None,
            content: vec![crate::session::text_input(&envelope)],
        };
        let forwarded = ThreadItem::UserMessage {
            id: "f".to_string(),
            client_id: None,
            content: vec![crate::session::text_input(
                crate::xtab::tools::forward_text(
                    "repo-b",
                    /*source_thread_id*/ None,
                    "FYI",
                    "Line one\nLine two",
                ),
            )],
        };
        let sources = vec![
            ExportSource::Item {
                item: &user,
                live_text: None,
            },
            ExportSource::Item {
                item: &forwarded,
                live_text: None,
            },
        ];
        assert_eq!(
            render_transcript(&sources, Path::new("/repo")),
            Ok("# Codex conversation\n\n## User\n\nMessage from tab \"repo-a\" (thread thread-a):\n\nTests pass on **main**.\n\n## User\n\nForwarded from tab \"repo-b\":\n\nFYI\n\nLine one\nLine two\n".to_string())
        );
    }

    #[test]
    fn text_views_name_the_message() {
        let agent = ThreadItem::AgentMessage {
            id: "a".to_string(),
            text: "Done :codex-followup[Next]{id=\"x\"}".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        };
        let entry = Entry::from_history("turn".to_string(), agent);
        assert_eq!(
            entry_text_view(&entry, Path::new("/repo")),
            Some(("Reply".to_string(), "Done Next".to_string()))
        );
        let envelope = crate::xtab::tools::wrap_agent_message(
            "t", "repo-a", "Hi", /*reply_expected*/ false,
        );
        let user = ThreadItem::UserMessage {
            id: "u".to_string(),
            client_id: None,
            content: vec![crate::session::text_input(&envelope)],
        };
        let entry = Entry::from_history("turn".to_string(), user);
        assert_eq!(
            entry_markdown(&entry, Path::new("/repo")),
            Some("Hi".to_string())
        );
        assert_eq!(
            entry_text_view(&entry, Path::new("/repo")),
            Some((
                "Message from repo-a".to_string(),
                "Message from tab \"repo-a\" (thread t):\n\nHi".to_string()
            ))
        );
    }

    #[test]
    fn quotes_text() {
        assert_eq!(quote("a\n\nb\n"), "> a\n>\n> b\n\n");
    }
}
