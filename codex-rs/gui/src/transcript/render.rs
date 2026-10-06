//! Rendering of transcript entries (thread items, notices, turn markers)
//! into rows. Pure functions: the store decides when to call them.

use std::path::Path;

use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::CollabAgentTool;
use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_app_server_protocol::CommandAction;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::HookRunStatus;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::PatchChangeKind;
use codex_app_server_protocol::SubAgentActivityKind;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::TurnPlanStepStatus;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_app_server_protocol::WebSearchAction;

use super::NoticeKind;
use super::blocks::Block;
use super::blocks::BlockKind;
use super::blocks::Gap;
use super::blocks::Line;
use super::blocks::Status;
use super::blocks::Tone;
use super::blocks::frame_blocks;
use super::crosstab;
use super::crosstab::CrossTabMessage;
use super::diff;
use super::directives::visible_markdown;
use super::links::display_path;
use super::links::resolve_path;
use super::markdown;
use super::markdown::BlockStyle;
use super::output::format_duration_ms;
use super::output::format_elapsed_compact;
use super::output::output_tail;
use super::output::strip_ansi;
use super::store::Body;
use super::store::CardEntry;
use super::store::Entry;
use super::store::HookEntry;
use super::store::ItemEntry;
use super::store::Live;
use super::store::PlanUpdateEntry;
use super::store::TurnEndEntry;

/// Output lines shown under a running command or a failed one.
const PREVIEW_OUTPUT_LINES: usize = 5;
/// Output lines shown when a command is expanded.
const EXPANDED_OUTPUT_LINES: usize = 400;
/// Diff lines shown when a file is expanded.
const EXPANDED_DIFF_LINES: usize = 400;
/// Tool result lines shown when a tool card is expanded.
const EXPANDED_RESULT_LINES: usize = 200;
const ARGUMENTS_PREVIEW_CHARS: usize = 160;
const PROMPT_PREVIEW_CHARS: usize = 240;
/// User messages longer than this are cut for display (copy keeps all).
const MAX_USER_TEXT_BYTES: usize = 64 * 1024;

/// Toggle ids used by item entries.
pub(crate) const TOGGLE_MAIN: usize = 0;

/// Inputs every renderer needs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RenderContext<'a> {
    pub(crate) cwd: &'a Path,
    /// `show_raw_agent_reasoning` from config.
    pub(crate) show_raw_reasoning: bool,
    /// `hide_agent_reasoning` from config.
    pub(crate) hide_reasoning: bool,
    /// Threads open in a tab of this window (cross-tab cards link to them).
    pub(crate) open_threads: &'a [String],
}

/// `Block.detail` of a cross-tab card header: the label of its source link.
pub(crate) const GO_TO_TAB: &str = "Go to tab";
pub(crate) const OPEN_THREAD: &str = "Open thread";

/// Renders one entry. The first row gets the entry gap.
pub(crate) fn render_entry(entry: &Entry, ctx: RenderContext<'_>) -> Vec<Block> {
    let mut blocks = match &entry.body {
        Body::Item(item) => render_item(item, ctx),
        Body::Notice { kind, title, text } => vec![notice_block(*kind, title, text)],
        Body::Retry { message, details } => {
            let mut block = notice_block(NoticeKind::Warning, "Reconnecting", message);
            block.status = Status::Running;
            block.detail = details.clone().unwrap_or_default();
            vec![block]
        }
        Body::TurnEnd(end) => render_turn_end(end),
        Body::PlanUpdate(plan) => vec![render_plan_update(plan)],
        Body::Hook(hook) => vec![render_hook(hook)],
        Body::Card(card) => card_blocks(card, ctx),
    };
    if let Some(first) = blocks.first_mut()
        && first.gap != Gap::None
        && first.kind != BlockKind::Explore
    {
        first.gap = Gap::Entry;
    }
    blocks
}

fn notice_block(kind: NoticeKind, title: &str, text: &str) -> Block {
    let mut block = Block::new(BlockKind::Notice);
    block.tone = match kind {
        NoticeKind::Info => Tone::Info,
        NoticeKind::Warning => Tone::Warning,
        NoticeKind::Error => Tone::Error,
    };
    block.title = title.to_string();
    block.text = text.trim().to_string();
    block.copyable = kind == NoticeKind::Error;
    block
}

fn render_turn_end(end: &TurnEndEntry) -> Vec<Block> {
    match end.status {
        TurnStatus::Failed => {
            if end.error_reported {
                return Vec::new();
            }
            let message = end
                .error
                .as_deref()
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("The turn failed.");
            let mut block = notice_block(NoticeKind::Error, "Turn failed", message);
            block.detail = end.error_details.clone().unwrap_or_default();
            vec![block]
        }
        TurnStatus::Interrupted => {
            let mut block = Block::new(BlockKind::Separator);
            block.title = match end.duration_ms {
                Some(ms) => format!("Interrupted after {}", format_elapsed_compact(ms)),
                None => "Interrupted".to_string(),
            };
            block.tone = Tone::Warning;
            vec![block]
        }
        // Turns under a second need no "worked for" marker.
        TurnStatus::Completed => match end.duration_ms.filter(|ms| *ms >= 1000) {
            Some(ms) => {
                let mut block = Block::new(BlockKind::Separator);
                block.title = format!("Worked for {}", format_elapsed_compact(ms));
                vec![block]
            }
            None => Vec::new(),
        },
        TurnStatus::InProgress => Vec::new(),
    }
}

fn render_plan_update(plan: &PlanUpdateEntry) -> Block {
    let mut block = Block::new(BlockKind::PlanUpdate);
    let done = plan
        .steps
        .iter()
        .filter(|(_, status)| *status == TurnPlanStepStatus::Completed)
        .count();
    block.title = "Updated plan".to_string();
    block.detail = format!("{done} of {} done", plan.steps.len());
    block.text = plan.explanation.clone().unwrap_or_default();
    block.lines = plan
        .steps
        .iter()
        .map(|(step, status)| {
            Line::new(
                step.clone(),
                match status {
                    TurnPlanStepStatus::Pending => 0,
                    TurnPlanStepStatus::InProgress => 1,
                    TurnPlanStepStatus::Completed => 2,
                },
            )
        })
        .collect();
    if block.lines.is_empty() {
        block.lines.push(Line::new("(no steps provided)", 0));
    }
    block
}

fn render_hook(hook: &HookEntry) -> Block {
    let run = &hook.run;
    let mut block = Block::new(BlockKind::Notice);
    let event = format!("{:?}", run.event_name);
    let (status, verb) = match run.status {
        HookRunStatus::Running => (Status::Running, "running"),
        HookRunStatus::Completed => (Status::Ok, "completed"),
        HookRunStatus::Failed => (Status::Failed, "failed"),
        HookRunStatus::Blocked => (Status::Failed, "blocked"),
        HookRunStatus::Stopped => (Status::Interrupted, "stopped"),
    };
    block.status = status;
    block.tone = match run.status {
        HookRunStatus::Failed | HookRunStatus::Blocked => Tone::Warning,
        _ => Tone::Muted,
    };
    block.title = format!("{event} hook {verb}");
    block.text = run.status_message.clone().unwrap_or_default();
    block.detail = run.duration_ms.map(format_duration_ms).unwrap_or_default();
    block.lines = run
        .entries
        .iter()
        .map(|entry| Line::new(format!("{:?}: {}", entry.kind, entry.text.trim()), 0))
        .collect();
    block
}

// ----- thread items -------------------------------------------------------

fn render_item(entry: &ItemEntry, ctx: RenderContext<'_>) -> Vec<Block> {
    let expanded = |toggle: usize| entry.expanded.contains(&toggle);
    match &entry.item {
        ThreadItem::UserMessage { content, .. } => match user_cross_tab_message(content) {
            Some(message) => crosstab_blocks(&message, delivery_status(entry), ctx),
            None => vec![user_block(content, delivery_status(entry))],
        },
        ThreadItem::AgentMessage { text, .. }
            if !matches!(entry.live, Live::Markdown(_))
                && let Some(message) = crosstab::parse(text) =>
        {
            crosstab_blocks(&message, Status::None, ctx)
        }
        ThreadItem::AgentMessage {
            text,
            memory_citation,
            questions,
            ..
        } => {
            let style = BlockStyle {
                tone: Tone::Normal,
                message: true,
            };
            let mut blocks = match &entry.live {
                Live::Markdown(stream) => stream.blocks(),
                _ => markdown::render(&visible_markdown(text, ctx.cwd), style),
            };
            if let Some(citation) = memory_citation
                && !citation.entries.is_empty()
            {
                let mut block = Block::new(BlockKind::Paragraph);
                block.tone = Tone::Muted;
                let sources: Vec<String> = citation
                    .entries
                    .iter()
                    .map(|entry| {
                        format!(
                            "[{}]({})",
                            markdown::escape_text(&format!(
                                "{}:{}",
                                display_path(&entry.path, ctx.cwd),
                                entry.line_start
                            )),
                            markdown::link_destination(&format!(
                                "{}:{}",
                                entry.path, entry.line_start
                            ))
                        )
                    })
                    .collect();
                block.rich = Some(format!("Memory: {}", sources.join(", ")));
                block.message = true;
                blocks.push(block);
            }
            if let Some(questions) = questions.as_ref().filter(|questions| !questions.is_empty()) {
                let mut block = Block::new(BlockKind::Paragraph);
                block.tone = Tone::Muted;
                block.rich = Some(
                    questions
                        .iter()
                        .map(|question| {
                            let options = question
                                .options
                                .as_ref()
                                .map(|options| format!(" ({})", options.join(" / ")))
                                .unwrap_or_default();
                            format!(
                                "- {}",
                                markdown::escape_text(&format!("{}{options}", question.title))
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                block.message = true;
                blocks.push(block);
            }
            mark_footer(&mut blocks, entry);
            blocks
        }
        ThreadItem::Plan { text, .. } => {
            let style = BlockStyle {
                tone: Tone::Plan,
                message: true,
            };
            let mut body = match &entry.live {
                Live::Markdown(stream) => stream.blocks(),
                _ => markdown::render(&visible_markdown(text, ctx.cwd), style),
            };
            let streaming = matches!(entry.live, Live::Markdown(_));
            if body.is_empty() && !streaming {
                let mut empty = Block::new(BlockKind::Paragraph);
                empty.tone = Tone::Plan;
                empty.rich = Some("\\(empty\\)".to_string());
                body.push(empty);
            }
            let mut header = Block::new(BlockKind::Section);
            header.title = "Proposed plan".to_string();
            header.tone = Tone::Plan;
            header.status = if streaming {
                Status::Running
            } else {
                Status::None
            };
            header.message = true;
            let mut blocks = vec![header];
            for mut block in body {
                if block.gap == Gap::Block && blocks.len() == 1 {
                    block.gap = Gap::None;
                }
                blocks.push(block);
            }
            mark_footer(&mut blocks, entry);
            blocks
        }
        ThreadItem::Reasoning { .. } if ctx.hide_reasoning => Vec::new(),
        ThreadItem::Reasoning {
            summary, content, ..
        } => reasoning_blocks(summary, content, entry, ctx, expanded(TOGGLE_MAIN)),
        ThreadItem::CommandExecution {
            command,
            source,
            status,
            command_actions,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } => exec_blocks(
            ExecView {
                command,
                source: *source,
                status: status.clone(),
                actions: command_actions,
                output: match &entry.live {
                    Live::Output(output) => Some(output.text()),
                    _ => aggregated_output.as_deref(),
                },
                dropped_lines: match &entry.live {
                    Live::Output(output) => output.dropped_lines(),
                    _ => 0,
                },
                exit_code: *exit_code,
                duration_ms: *duration_ms,
                interaction: entry.progress.as_deref(),
            },
            expanded(TOGGLE_MAIN),
        ),
        ThreadItem::FileChange {
            changes, status, ..
        } => patch_blocks(changes, status.clone(), &entry.expanded, ctx.cwd),
        ThreadItem::McpToolCall {
            server,
            tool,
            status,
            arguments,
            result,
            error,
            duration_ms,
            ..
        } => {
            let mut result_lines = Vec::new();
            if let Some(result) = result {
                for content in &result.content {
                    result_lines.push(mcp_content_text(content));
                }
                if let Some(structured) = &result.structured_content
                    && result.content.is_empty()
                {
                    result_lines.push(format!("structured result: {structured}"));
                }
            }
            let failed = *status == McpToolCallStatus::Failed || error.is_some();
            if let Some(error) = error {
                result_lines.push(format!("Error: {}", error.message));
            }
            vec![tool_block(ToolView {
                title: format!("{server}.{tool}"),
                verb: if *status == McpToolCallStatus::InProgress {
                    "Calling"
                } else {
                    "Called"
                },
                arguments: compact_json(arguments),
                status: match status {
                    McpToolCallStatus::InProgress => Status::Running,
                    _ if failed => Status::Failed,
                    _ => Status::Ok,
                },
                duration_ms: *duration_ms,
                progress: entry.progress.clone(),
                result: result_lines.join("\n"),
                expanded: expanded(TOGGLE_MAIN),
            })]
        }
        ThreadItem::DynamicToolCall {
            namespace,
            tool,
            arguments,
            status,
            content_items,
            success,
            duration_ms,
            ..
        } => {
            let result = content_items
                .iter()
                .flatten()
                .map(|item| match item {
                    DynamicToolCallOutputContentItem::InputText { text } => text.clone(),
                    DynamicToolCallOutputContentItem::InputImage { .. } => {
                        "Returned image".to_string()
                    }
                    DynamicToolCallOutputContentItem::InputAudio { .. } => {
                        "Returned audio".to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            vec![tool_block(ToolView {
                title: match namespace {
                    Some(namespace) => format!("{namespace}.{tool}"),
                    None => tool.clone(),
                },
                verb: if *status == DynamicToolCallStatus::InProgress {
                    "Calling"
                } else {
                    "Called"
                },
                arguments: compact_json(arguments),
                status: match status {
                    DynamicToolCallStatus::InProgress => Status::Running,
                    DynamicToolCallStatus::Failed => Status::Failed,
                    DynamicToolCallStatus::Completed if *success == Some(false) => Status::Failed,
                    DynamicToolCallStatus::Completed => Status::Ok,
                },
                duration_ms: *duration_ms,
                progress: None,
                result,
                expanded: expanded(TOGGLE_MAIN),
            })]
        }
        ThreadItem::WebSearch(search) => {
            let detail = match &search.action {
                Some(WebSearchAction::Search { query, queries }) => query
                    .clone()
                    .filter(|query| !query.is_empty())
                    .or_else(|| queries.as_ref().map(|queries| queries.join(", ")))
                    .unwrap_or_else(|| search.query.clone()),
                Some(WebSearchAction::OpenPage { url }) => url.clone().unwrap_or_default(),
                Some(WebSearchAction::FindInPage { url, pattern }) => match (pattern, url) {
                    (Some(pattern), Some(url)) => format!("'{pattern}' in {url}"),
                    (Some(pattern), None) => format!("'{pattern}'"),
                    (None, Some(url)) => url.clone(),
                    (None, None) => String::new(),
                },
                Some(WebSearchAction::Other) | None => search.query.clone(),
            };
            let detail = detail.replace(['\n', '\t'], " ");
            let done = entry.completed;
            let title = match &search.action {
                Some(WebSearchAction::OpenPage { .. }) => {
                    if done {
                        "Opened page"
                    } else {
                        "Opening page"
                    }
                }
                Some(WebSearchAction::FindInPage { .. }) => {
                    if done {
                        "Searched in page"
                    } else {
                        "Searching in page"
                    }
                }
                _ if !done && detail.is_empty() => "Browsing the web",
                _ => {
                    if done {
                        "Searched the web"
                    } else {
                        "Searching the web"
                    }
                }
            };
            let mut block = Block::new(BlockKind::Tool);
            block.title = title.to_string();
            block.text = detail;
            block.status = if done { Status::Ok } else { Status::Running };
            block.meta = "web".to_string();
            vec![block]
        }
        ThreadItem::ImageView { path, .. } => {
            let mut block = Block::new(BlockKind::Tool);
            block.title = "Viewed image".to_string();
            block.text = display_path(path.as_str(), ctx.cwd);
            block.status = Status::Ok;
            block.meta = "image".to_string();
            vec![block]
        }
        ThreadItem::ImageGeneration(generation) => {
            let failed =
                generation.status.eq_ignore_ascii_case("failed") || generation.failure.is_some();
            let mut block = Block::new(BlockKind::Tool);
            block.meta = "image".to_string();
            block.title = if failed {
                "Image generation failed"
            } else if entry.completed {
                "Generated image"
            } else {
                "Generating image"
            }
            .to_string();
            block.status = if failed {
                Status::Failed
            } else if entry.completed {
                Status::Ok
            } else {
                Status::Running
            };
            block.text = generation.revised_prompt.clone().unwrap_or_default();
            if let Some(saved) = &generation.saved_path {
                let saved = saved.as_path().display().to_string();
                block.detail = display_path(&saved, ctx.cwd);
            }
            vec![block]
        }
        ThreadItem::CollabAgentToolCall {
            tool,
            status,
            receiver_thread_ids,
            prompt,
            model,
            reasoning_effort,
            agents_states,
            ..
        } => {
            let agent_label = |id: &str| format!("Agent {}", short_id(id));
            let target = match receiver_thread_ids.as_slice() {
                [only] => agent_label(only),
                [] => "agent".to_string(),
                many => format!("{} agents", many.len()),
            };
            let running = *status == CollabAgentToolCallStatus::InProgress;
            let title = match tool {
                CollabAgentTool::SpawnAgent => match status {
                    CollabAgentToolCallStatus::Failed => "Agent spawn failed".to_string(),
                    _ if running => "Spawning agent".to_string(),
                    _ => format!("Spawned {target}"),
                },
                CollabAgentTool::SendInput => format!("Sent input to {target}"),
                CollabAgentTool::ResumeAgent => {
                    if running {
                        format!("Resuming {target}")
                    } else {
                        format!("Resumed {target}")
                    }
                }
                CollabAgentTool::Wait => {
                    if running {
                        format!("Waiting for {target}")
                    } else {
                        "Finished waiting".to_string()
                    }
                }
                CollabAgentTool::CloseAgent => format!("Closed {target}"),
                // V2 tools are reported through `SubAgentActivity` items.
                CollabAgentTool::SendMessage
                | CollabAgentTool::FollowupTask
                | CollabAgentTool::InterruptAgent
                | CollabAgentTool::ListAgents => return Vec::new(),
            };
            let mut block = Block::new(BlockKind::Agent);
            block.title = title;
            block.status = match status {
                CollabAgentToolCallStatus::InProgress => Status::Running,
                CollabAgentToolCallStatus::Completed => Status::Ok,
                CollabAgentToolCallStatus::Failed => Status::Failed,
                CollabAgentToolCallStatus::Interrupted => Status::Interrupted,
            };
            block.text = prompt
                .as_deref()
                .map(|prompt| truncate_chars(prompt.trim(), PROMPT_PREVIEW_CHARS))
                .unwrap_or_default();
            block.detail = [
                model.clone(),
                reasoning_effort
                    .as_ref()
                    .map(|effort| format!("{effort:?}").to_lowercase()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            block.lines = receiver_thread_ids
                .iter()
                .map(|id| {
                    let state = agents_states.get(id);
                    let status_text = state
                        .map(|state| agent_status_text(&state.status, state.message.as_deref()))
                        .unwrap_or_default();
                    let text = if status_text.is_empty() {
                        agent_label(id)
                    } else {
                        format!("{} — {status_text}", agent_label(id))
                    };
                    Line {
                        text,
                        kind: state.map_or(Status::None, |state| agent_status(&state.status))
                            as i32,
                        gutter: String::new(),
                        target: id.clone(),
                    }
                })
                .collect();
            vec![block]
        }
        ThreadItem::SubAgentActivity {
            kind,
            agent_thread_id,
            agent_path,
            ..
        } => {
            let mut block = Block::new(BlockKind::Agent);
            let (verb, status) = match kind {
                SubAgentActivityKind::Started => ("Started", Status::Running),
                SubAgentActivityKind::Interacted => ("Interacted with", Status::Ok),
                SubAgentActivityKind::Interrupted => ("Interrupted", Status::Interrupted),
                SubAgentActivityKind::Completed => ("Completed", Status::Ok),
            };
            block.title = format!("{verb} {agent_path}");
            block.status = status;
            block.target = agent_thread_id.clone();
            vec![block]
        }
        ThreadItem::EnteredReviewMode { review, .. } => {
            let mut block = Block::new(BlockKind::Section);
            block.title = "Code review started".to_string();
            block.text = review.clone();
            block.tone = Tone::Info;
            vec![block]
        }
        ThreadItem::ExitedReviewMode { review, .. } => {
            let mut header = Block::new(BlockKind::Section);
            header.title = "Code review finished".to_string();
            header.tone = Tone::Info;
            header.message = true;
            let mut blocks = vec![header];
            let style = BlockStyle {
                tone: Tone::Normal,
                message: true,
            };
            blocks.extend(markdown::render(&visible_markdown(review, ctx.cwd), style));
            blocks
        }
        ThreadItem::ContextCompaction { .. } => {
            let mut block = notice_block(
                NoticeKind::Info,
                "",
                if entry.completed {
                    "Context compacted"
                } else {
                    "Compacting context…"
                },
            );
            block.status = if entry.completed {
                Status::Ok
            } else {
                Status::Running
            };
            vec![block]
        }
        ThreadItem::HookPrompt { .. }
        | ThreadItem::Sleep(_)
        | ThreadItem::FunctionCallOutput { .. } => Vec::new(),
    }
}

/// Shows the message actions under the last row of a completed message.
fn mark_footer(blocks: &mut [Block], entry: &ItemEntry) {
    if entry.completed
        && let Some(last) = blocks.last_mut()
    {
        last.footer = true;
    }
}

/// The cross-tab message a user message carries, if it is one (text only;
/// messages with attachments are shown as typed).
pub(crate) fn user_cross_tab_message(content: &[UserInput]) -> Option<CrossTabMessage> {
    match content {
        [UserInput::Text { text, .. }] => crosstab::parse(text),
        _ => None,
    }
}

/// A message from another tab: a card with the sender and the inner
/// message, linking back to the sending tab.
/// Status glyph of a user message: sending (a local echo), not sent (the
/// request failed), or none once the server has it.
fn delivery_status(entry: &ItemEntry) -> Status {
    match (entry.local_echo, entry.unsent) {
        (true, true) => Status::Failed,
        (true, false) => Status::Running,
        (false, _) => Status::None,
    }
}

/// Caption under a user message that was not sent.
pub(crate) const NOT_SENT: &str = "Not sent";

fn crosstab_blocks(
    message: &CrossTabMessage,
    status: Status,
    ctx: RenderContext<'_>,
) -> Vec<Block> {
    let mut header = Block::new(BlockKind::CardHeader);
    header.meta = message.verb().to_string();
    header.title = format!("tab \"{}\"", message.from_title);
    if matches!(
        message.kind,
        crosstab::CrossTabKind::Agent {
            reply_expected: true
        }
    ) {
        header.text = "reply expected".to_string();
    }
    if let Some(thread_id) = &message.from_thread_id {
        header.target = thread_id.clone();
        header.detail = if ctx.open_threads.contains(thread_id) {
            GO_TO_TAB
        } else {
            OPEN_THREAD
        }
        .to_string();
    }
    header.status = status;
    if status == Status::Failed {
        header.text = NOT_SENT.to_string();
    }
    header.tone = Tone::Info;
    header.message = true;
    header.copyable = true;
    let mut blocks = vec![header];
    let style = BlockStyle {
        tone: Tone::Normal,
        message: true,
    };
    let note = message.note().trim();
    if !note.is_empty() {
        let mut block = Block::new(BlockKind::Paragraph);
        block.rich = Some(
            note.lines()
                .map(markdown::escape_text)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        block.message = true;
        blocks.push(block);
    }
    // Forwarded content under a note reads as a quotation of the source.
    let quote = i32::from(!note.is_empty());
    let mut body = Vec::new();
    for (_, node) in markdown::parse(&visible_markdown(&message.message, ctx.cwd)).nodes {
        markdown::node_blocks(&node, style, quote, /*level*/ 0, &mut body);
    }
    if body.is_empty() && note.is_empty() {
        let mut empty = Block::new(BlockKind::Paragraph);
        empty.tone = Tone::Muted;
        empty.rich = Some("\\(empty message\\)".to_string());
        empty.message = true;
        body.push(empty);
    }
    blocks.extend(body);
    finish_card(&mut blocks);
    blocks
}

/// A titled Markdown card added by the GUI (for example a recap).
fn card_blocks(card: &CardEntry, ctx: RenderContext<'_>) -> Vec<Block> {
    let mut header = Block::new(BlockKind::CardHeader);
    header.title = card.title.clone();
    header.tone = Tone::Info;
    header.message = true;
    header.copyable = true;
    let mut blocks = vec![header];
    let style = BlockStyle {
        tone: Tone::Normal,
        message: true,
    };
    blocks.extend(markdown::render(
        &visible_markdown(&card.markdown, ctx.cwd),
        style,
    ));
    finish_card(&mut blocks);
    blocks
}

/// Rows of a card sit close under its header and share one frame.
fn finish_card(blocks: &mut [Block]) {
    if let Some(first_body) = blocks.get_mut(1) {
        first_body.gap = Gap::Item;
    }
    frame_blocks(blocks);
}

/// Text and attachment chips of a user message.
pub(crate) fn user_message_parts(content: &[UserInput]) -> (String, Vec<Line>) {
    let mut texts = Vec::new();
    let mut chips = Vec::new();
    for input in content {
        match input {
            UserInput::Text { text, .. } => texts.push(sanitize_user_text(text)),
            UserInput::Image { .. } => chips.push(Line::new("Image", 0)),
            UserInput::LocalImage { path, .. } => chips.push(Line {
                text: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Image".to_string()),
                kind: 0,
                gutter: String::new(),
                target: path.display().to_string(),
            }),
            UserInput::Audio { .. } | UserInput::LocalAudio { .. } => {
                chips.push(Line::new("Audio", 3));
            }
            UserInput::Skill { name, .. } => chips.push(Line::new(format!("${name}"), 2)),
            UserInput::Mention { name, .. } => chips.push(Line::new(format!("@{name}"), 1)),
        }
    }
    (texts.join("\n"), chips)
}

fn user_block(content: &[UserInput], status: Status) -> Block {
    let (text, chips) = user_message_parts(content);
    let mut block = Block::new(BlockKind::User);
    block.text = if text.len() > MAX_USER_TEXT_BYTES {
        let mut end = MAX_USER_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}\n… {} more characters (use Copy for the full message)",
            &text[..end],
            text[end..].chars().count()
        )
    } else {
        text
    };
    block.lines = chips;
    block.message = true;
    block.copyable = true;
    block.status = status;
    if status == Status::Failed {
        block.detail = NOT_SENT.to_string();
    }
    block
}

/// Strips escape sequences and control characters except newline and tab.
pub(crate) fn sanitize_user_text(text: &str) -> String {
    strip_ansi(text).replace('\r', "")
}

fn reasoning_blocks(
    summary: &[String],
    content: &[String],
    entry: &ItemEntry,
    ctx: RenderContext<'_>,
    expanded: bool,
) -> Vec<Block> {
    let header_title = reasoning_header(summary);
    let full = reasoning_summary_markdown(summary);
    // The headline is already shown in the row; do not repeat it as the
    // first line of the expanded body.
    let body = match &header_title {
        Some(title) => full
            .strip_prefix(&format!("**{title}**"))
            .map(|rest| rest.trim_start().to_string())
            .unwrap_or_else(|| full.clone()),
        None => full.clone(),
    };
    let raw = if ctx.show_raw_reasoning {
        content
            .iter()
            .map(|part| part.trim())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        String::new()
    };
    if full.is_empty() && raw.is_empty() {
        return Vec::new();
    }
    let mut header = Block::new(BlockKind::Reasoning);
    header.title = header_title.unwrap_or_else(|| "Thinking".to_string());
    header.status = if entry.completed {
        Status::None
    } else {
        Status::Running
    };
    let expandable = !body.is_empty() || !raw.is_empty();
    header.toggle = expandable.then_some(TOGGLE_MAIN);
    header.expanded = expanded && expandable;
    let mut blocks = vec![header];
    if expanded && expandable {
        let style = BlockStyle {
            tone: Tone::Muted,
            message: false,
        };
        let mut body_blocks = markdown::render(&body, style);
        if !raw.is_empty() {
            let mut label = Block::new(BlockKind::Paragraph);
            label.tone = Tone::Muted;
            label.rich = Some("*Raw reasoning*".to_string());
            body_blocks.push(label);
            let mut text = Block::new(BlockKind::Paragraph);
            text.tone = Tone::Muted;
            text.rich = Some(
                raw.lines()
                    .map(markdown::escape_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            body_blocks.push(text);
        }
        for block in &mut body_blocks {
            block.level += 1;
        }
        if let Some(first) = body_blocks.first_mut() {
            first.gap = Gap::None;
        }
        blocks.extend(body_blocks);
    }
    blocks
}

/// Summary parts joined for display (empty and placeholder parts dropped).
pub(crate) fn reasoning_summary_markdown(summary: &[String]) -> String {
    summary
        .iter()
        .map(|part| part.trim())
        .filter(|part| {
            if part.is_empty() {
                return false;
            }
            // Drop parts whose body after a `**Header**` is an empty comment.
            let body = part
                .strip_prefix("**")
                .and_then(|rest| rest.split_once("**"))
                .map_or(*part, |(_, body)| body.trim());
            body != "<!-- -->"
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Headline of the latest summary part: its leading `**bold**` text, or
/// its last meaningful line.
pub(crate) fn reasoning_header(summary: &[String]) -> Option<String> {
    let part = summary.iter().rev().find(|part| !part.trim().is_empty())?;
    let part = part.trim();
    if let Some(rest) = part.strip_prefix("**")
        && let Some((bold, _)) = rest.split_once("**")
        && !bold.trim().is_empty()
    {
        return Some(bold.trim().to_string());
    }
    part.lines().rev().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with("<!--") {
            return None;
        }
        let line = line.trim_start_matches('#').trim().replace("**", "");
        (!line.is_empty()).then(|| truncate_chars(&line, 120))
    })
}

struct ExecView<'a> {
    command: &'a str,
    source: CommandExecutionSource,
    status: CommandExecutionStatus,
    actions: &'a [CommandAction],
    output: Option<&'a str>,
    dropped_lines: usize,
    exit_code: Option<i32>,
    duration_ms: Option<i64>,
    interaction: Option<&'a str>,
}

fn exec_blocks(view: ExecView<'_>, expanded: bool) -> Vec<Block> {
    let display = display_command(view.command);
    let status = match view.status {
        CommandExecutionStatus::InProgress => Status::Running,
        CommandExecutionStatus::Completed if view.exit_code.unwrap_or(0) == 0 => Status::Ok,
        CommandExecutionStatus::Completed | CommandExecutionStatus::Failed => Status::Failed,
        CommandExecutionStatus::Declined => Status::Declined,
    };
    let running = status == Status::Running;
    let output = view.output.unwrap_or_default();
    let has_output = !output.trim().is_empty();
    let multi_line = display.contains('\n');

    let mut detail = Vec::new();
    match view.status {
        CommandExecutionStatus::Declined => detail.push("declined".to_string()),
        CommandExecutionStatus::Failed | CommandExecutionStatus::Completed if !running => {
            let code = if view.status == CommandExecutionStatus::Completed {
                view.exit_code.unwrap_or(0)
            } else {
                view.exit_code.filter(|code| *code != 0).unwrap_or(1)
            };
            if code != 0 {
                detail.push(format!("exit {code}"));
            }
        }
        _ => {}
    }
    if let Some(ms) = view.duration_ms {
        detail.push(format_duration_ms(ms));
    }

    if is_exploring(view.source, view.actions) && !expanded {
        let mut block = Block::new(BlockKind::Explore);
        block.title = exploring_summary(view.actions);
        block.meta = exploring_verb(view.actions).to_string();
        block.status = status;
        block.detail = if status == Status::Failed {
            detail.join(" · ")
        } else {
            String::new()
        };
        block.toggle = Some(TOGGLE_MAIN);
        return vec![block];
    }

    let mut block = Block::new(BlockKind::Exec);
    let first_line = display.lines().next().unwrap_or_default().to_string();
    block.title = if multi_line {
        format!("{first_line} …")
    } else {
        first_line
    };
    block.meta =
        match view.source {
            CommandExecutionSource::UserShell => {
                if running {
                    "You are running"
                } else {
                    "You ran"
                }
            }
            CommandExecutionSource::UnifiedExecInteraction => {
                if view.interaction.is_some_and(|stdin| !stdin.is_empty()) {
                    "Sent input to"
                } else {
                    "Waited for"
                }
            }
            CommandExecutionSource::Agent | CommandExecutionSource::UnifiedExecStartup => {
                if running { "Running" } else { "Ran" }
            }
        }
        .to_string();
    block.status = status;
    block.detail = detail.join(" · ");
    block.expanded = expanded;
    if has_output || multi_line || is_exploring(view.source, view.actions) {
        block.toggle = Some(TOGGLE_MAIN);
    }
    let lines = if expanded {
        EXPANDED_OUTPUT_LINES
    } else if running || status == Status::Failed {
        PREVIEW_OUTPUT_LINES
    } else {
        0
    };
    let mut text = String::new();
    if expanded && multi_line {
        text.push_str(
            &display
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    if index == 0 {
                        format!("$ {line}")
                    } else {
                        format!("  {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        if has_output {
            text.push_str("\n\n");
        }
    }
    if lines > 0 && has_output {
        let (tail, omitted) = output_tail(output, lines);
        let omitted = omitted + view.dropped_lines;
        if omitted > 0 {
            text.push_str(&format!("… {omitted} earlier lines\n"));
        }
        text.push_str(&tail);
    } else if expanded && !has_output && !running {
        text.push_str("(no output)");
    }
    block.text = text;
    if has_output {
        block.copyable = true;
        block.copy_text = Some(strip_ansi(output).trim_end().to_string());
    }
    vec![block]
}

fn is_exploring(source: CommandExecutionSource, actions: &[CommandAction]) -> bool {
    source != CommandExecutionSource::UserShell
        && !actions.is_empty()
        && actions.iter().all(|action| {
            matches!(
                action,
                CommandAction::Read { .. }
                    | CommandAction::ListFiles { .. }
                    | CommandAction::Search { .. }
            )
        })
}

fn exploring_verb(actions: &[CommandAction]) -> &'static str {
    match actions.first() {
        Some(CommandAction::Read { .. }) => "Read",
        Some(CommandAction::ListFiles { .. }) => "List",
        Some(CommandAction::Search { .. }) => "Search",
        _ => "Run",
    }
}

/// `Read a.rs, b.rs` / `List src` / `Search foo in src`, joined with ` · `.
pub(crate) fn exploring_summary(actions: &[CommandAction]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut reads: Vec<String> = Vec::new();
    let flush_reads = |parts: &mut Vec<String>, reads: &mut Vec<String>| {
        if !reads.is_empty() {
            let text = reads.join(", ");
            if parts.is_empty() {
                parts.push(text);
            } else {
                parts.push(format!("Read {text}"));
            }
            reads.clear();
        }
    };
    for action in actions {
        match action {
            CommandAction::Read { name, .. } => {
                if !reads.contains(name) {
                    reads.push(name.clone());
                }
            }
            CommandAction::ListFiles { command, path } => {
                flush_reads(&mut parts, &mut reads);
                let target = path.clone().unwrap_or_else(|| command.clone());
                parts.push(if parts.is_empty() {
                    target
                } else {
                    format!("List {target}")
                });
            }
            CommandAction::Search {
                command,
                query,
                path,
            } => {
                flush_reads(&mut parts, &mut reads);
                let text = match (query, path) {
                    (Some(query), Some(path)) => format!("{query} in {path}"),
                    (Some(query), None) => query.clone(),
                    (None, Some(path)) => path.clone(),
                    (None, None) => command.clone(),
                };
                parts.push(if parts.is_empty() {
                    text
                } else {
                    format!("Search {text}")
                });
            }
            CommandAction::Unknown { command } => {
                flush_reads(&mut parts, &mut reads);
                parts.push(format!("Run {command}"));
            }
        }
    }
    flush_reads(&mut parts, &mut reads);
    parts.join(" · ")
}

/// The command as the user should read it: shell wrappers removed.
pub(crate) fn display_command(command: &str) -> String {
    strip_shell_wrapper(&split_command_string(command)).unwrap_or_else(|| command.to_string())
}

/// `ThreadItem.command` is a shlex-joined argv; split it back when that
/// round-trips, otherwise keep the string as one argument.
fn split_command_string(command: &str) -> Vec<String> {
    let Some(parts) = shlex::split(command) else {
        return vec![command.to_string()];
    };
    match shlex::try_join(parts.iter().map(String::as_str)) {
        Ok(round_trip)
            if round_trip == command
                || (!command.contains(":\\")
                    && shlex::split(&round_trip).as_ref() == Some(&parts)) =>
        {
            parts
        }
        _ => vec![command.to_string()],
    }
}

/// The script of `bash -lc "<script>"`, `zsh -c`, `sh -c`, or
/// `powershell -Command`.
fn strip_shell_wrapper(parts: &[String]) -> Option<String> {
    let program = parts.first()?;
    let name = Path::new(program)
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    match name {
        "bash" | "zsh" | "sh" => match parts {
            [_, flag, script] if flag == "-lc" || flag == "-c" => Some(script.clone()),
            _ => None,
        },
        "powershell" | "pwsh" => {
            let index = parts
                .iter()
                .position(|part| part.eq_ignore_ascii_case("-command") || part == "-c")?;
            (index + 2 == parts.len()).then(|| parts[index + 1].clone())
        }
        _ => None,
    }
}

fn patch_blocks(
    changes: &[FileUpdateChange],
    status: PatchApplyStatus,
    expanded: &std::collections::BTreeSet<usize>,
    cwd: &Path,
) -> Vec<Block> {
    let status_value = match status {
        PatchApplyStatus::InProgress => Status::Running,
        PatchApplyStatus::Completed => Status::Ok,
        PatchApplyStatus::Failed => Status::Failed,
        PatchApplyStatus::Declined => Status::Declined,
    };
    let status_text = match status {
        PatchApplyStatus::InProgress => "",
        PatchApplyStatus::Completed => "",
        PatchApplyStatus::Failed => "Failed to apply",
        PatchApplyStatus::Declined => "Declined",
    };
    let mut indexed: Vec<(usize, &FileUpdateChange)> = changes.iter().enumerate().collect();
    indexed.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    let mut blocks = Vec::new();
    let mut total_added = 0;
    let mut total_removed = 0;
    let mut files = Vec::new();
    for (index, change) in indexed {
        let (added, removed) = diff::line_counts(change);
        total_added += added;
        total_removed += removed;
        let mut block = Block::new(BlockKind::PatchFile);
        block.meta = match &change.kind {
            PatchChangeKind::Add => "Added",
            PatchChangeKind::Delete => "Deleted",
            PatchChangeKind::Update { move_path: Some(_) } => "Moved",
            PatchChangeKind::Update { move_path: None } => "Edited",
        }
        .to_string();
        let shown = display_path(&change.path, cwd);
        block.title = match &change.kind {
            PatchChangeKind::Update {
                move_path: Some(dest),
            } => format!(
                "{shown} → {}",
                display_path(&dest.display().to_string(), cwd)
            ),
            _ => shown,
        };
        if !matches!(change.kind, PatchChangeKind::Delete) {
            let path = match &change.kind {
                PatchChangeKind::Update {
                    move_path: Some(dest),
                } => dest.display().to_string(),
                _ => change.path.clone(),
            };
            block.target = resolve_path(&path, cwd).display().to_string();
        }
        block.added = i32::try_from(added).unwrap_or(i32::MAX);
        block.removed = i32::try_from(removed).unwrap_or(i32::MAX);
        block.toggle = Some(index);
        block.expanded = expanded.contains(&index);
        if block.expanded {
            block.lines = diff::file_diff(change, EXPANDED_DIFF_LINES).lines;
        }
        block.gap = Gap::None;
        files.push(block);
    }
    if files.len() == 1 {
        if let Some(mut only) = files.pop() {
            only.status = status_value;
            only.detail = status_text.to_string();
            only.gap = Gap::Block;
            blocks.push(only);
        }
        return blocks;
    }
    let mut summary = Block::new(BlockKind::PatchSummary);
    let verb = match status {
        PatchApplyStatus::InProgress => "Editing",
        _ => "Edited",
    };
    summary.title = format!("{verb} {} files", files.len());
    summary.added = i32::try_from(total_added).unwrap_or(i32::MAX);
    summary.removed = i32::try_from(total_removed).unwrap_or(i32::MAX);
    summary.status = status_value;
    summary.detail = status_text.to_string();
    blocks.push(summary);
    blocks.extend(files);
    blocks
}

struct ToolView {
    title: String,
    verb: &'static str,
    arguments: String,
    status: Status,
    duration_ms: Option<i64>,
    progress: Option<String>,
    result: String,
    expanded: bool,
}

fn tool_block(view: ToolView) -> Block {
    let mut block = Block::new(BlockKind::Tool);
    block.title = view.title;
    block.meta = view.verb.to_string();
    block.status = view.status;
    block.detail = match (&view.progress, view.duration_ms) {
        (Some(progress), _) if view.status == Status::Running => progress.clone(),
        (_, Some(ms)) => format_duration_ms(ms),
        _ => String::new(),
    };
    block.text = truncate_chars(&view.arguments, ARGUMENTS_PREVIEW_CHARS);
    let result = view.result.trim_end();
    if !result.is_empty() {
        block.toggle = Some(TOGGLE_MAIN);
        block.expanded = view.expanded;
        block.copyable = true;
        block.copy_text = Some(result.to_string());
        let shown_lines = if view.expanded {
            EXPANDED_RESULT_LINES
        } else if view.status == Status::Failed {
            3
        } else {
            0
        };
        if shown_lines > 0 {
            let (tail, omitted) = head_lines(result, shown_lines);
            block.lines = tail
                .lines()
                .map(|line| Line::new(line.to_string(), 0))
                .collect();
            if omitted > 0 {
                block
                    .lines
                    .push(Line::new(format!("… {omitted} more lines"), 4));
            }
        }
    }
    block
}

fn head_lines(text: &str, max: usize) -> (String, usize) {
    let total = text.lines().count();
    let shown: Vec<&str> = text.lines().take(max).collect();
    (shown.join("\n"), total.saturating_sub(max))
}

/// Text of one MCP content block.
fn mcp_content_text(content: &serde_json::Value) -> String {
    match content.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some("image") => "Returned image".to_string(),
        Some("audio") => "<audio content>".to_string(),
        Some("resource") => format!(
            "embedded resource: {}",
            content
                .pointer("/resource/uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unknown>")
        ),
        Some("resource_link") => format!(
            "link: {}",
            content
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unknown>")
        ),
        _ => content.to_string(),
    }
}

/// One-line JSON for argument previews (`{}` and `null` show nothing).
pub(crate) fn compact_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(map) if map.is_empty() => String::new(),
        other => other.to_string(),
    }
}

fn agent_status(status: &CollabAgentStatus) -> Status {
    match status {
        CollabAgentStatus::PendingInit | CollabAgentStatus::Running => Status::Running,
        CollabAgentStatus::Completed => Status::Ok,
        CollabAgentStatus::Errored | CollabAgentStatus::NotFound => Status::Failed,
        CollabAgentStatus::Interrupted | CollabAgentStatus::Shutdown => Status::Interrupted,
    }
}

fn agent_status_text(status: &CollabAgentStatus, message: Option<&str>) -> String {
    let message = message
        .map(|message| truncate_chars(message.trim(), PROMPT_PREVIEW_CHARS))
        .filter(|message| !message.is_empty());
    match status {
        CollabAgentStatus::PendingInit => "pending".to_string(),
        CollabAgentStatus::Running => "running".to_string(),
        CollabAgentStatus::Interrupted => "interrupted".to_string(),
        CollabAgentStatus::Completed => match message {
            Some(message) => format!("completed: {message}"),
            None => "completed".to_string(),
        },
        CollabAgentStatus::Errored => match message {
            Some(message) => format!("error: {message}"),
            None => "error".to_string(),
        },
        CollabAgentStatus::Shutdown => "shut down".to_string(),
        CollabAgentStatus::NotFound => "not found".to_string(),
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    crate::app::truncate_chars(&text.replace('\n', " "), max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::blocks::Frame;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn strips_shell_wrappers() {
        assert_eq!(display_command("bash -lc 'ls -la'"), "ls -la");
        assert_eq!(display_command("/bin/zsh -c 'echo hi'"), "echo hi");
        assert_eq!(display_command("git status"), "git status");
        assert_eq!(
            display_command("powershell.exe -NoProfile -Command 'Get-ChildItem'"),
            "Get-ChildItem"
        );
        assert_eq!(
            display_command("C:\\tools\\x.exe /q"),
            "C:\\tools\\x.exe /q"
        );
    }

    #[test]
    fn summarizes_exploring_commands() -> serde_json::Result<()> {
        let actions: Vec<CommandAction> = serde_json::from_value(serde_json::json!([
            {"type": "read", "command": "cat a.rs", "name": "a.rs", "path": "a.rs"},
            {"type": "read", "command": "cat b.rs", "name": "b.rs", "path": "b.rs"},
            {"type": "search", "command": "rg foo src", "query": "foo", "path": "src"},
        ]))?;
        assert_eq!(
            exploring_summary(&actions),
            "a.rs, b.rs · Search foo in src"
        );
        assert!(is_exploring(CommandExecutionSource::Agent, &actions));
        assert!(!is_exploring(CommandExecutionSource::UserShell, &actions));
        Ok(())
    }

    #[test]
    fn reasoning_header_prefers_bold_title() {
        let summary = vec![
            "**First**\n\nbody".to_string(),
            "**Planning the reply**\n\nI will show".to_string(),
        ];
        assert_eq!(
            reasoning_header(&summary),
            Some("Planning the reply".to_string())
        );
        assert_eq!(
            reasoning_header(&["## Checking files\n".to_string()]),
            Some("Checking files".to_string())
        );
        assert_eq!(reasoning_header(&[]), None);
        assert_eq!(
            reasoning_summary_markdown(&["**H**\n<!-- -->".to_string(), " a ".to_string()]),
            "a"
        );
    }

    #[test]
    fn exec_rows_report_status_and_tail() {
        let blocks = exec_blocks(
            ExecView {
                command: "bash -lc 'cargo test'",
                source: CommandExecutionSource::Agent,
                status: CommandExecutionStatus::Completed,
                actions: &[],
                output: Some("1\n2\n3\n4\n5\n6\n7\n"),
                dropped_lines: 0,
                exit_code: Some(101),
                duration_ms: Some(1500),
                interaction: None,
            },
            /*expanded*/ false,
        );
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert_eq!(block.title, "cargo test");
        assert_eq!(block.meta, "Ran");
        assert_eq!(block.status, Status::Failed);
        assert_eq!(block.detail, "exit 101 · 1.50s");
        assert_eq!(block.text, "… 2 earlier lines\n3\n4\n5\n6\n7");
        assert_eq!(block.copy_text.as_deref(), Some("1\n2\n3\n4\n5\n6\n7"));
    }

    #[test]
    fn successful_exec_output_is_collapsed() {
        let blocks = exec_blocks(
            ExecView {
                command: "ls",
                source: CommandExecutionSource::Agent,
                status: CommandExecutionStatus::Completed,
                actions: &[],
                output: Some("a\nb\n"),
                dropped_lines: 0,
                exit_code: Some(0),
                duration_ms: Some(20),
                interaction: None,
            },
            /*expanded*/ false,
        );
        assert_eq!(blocks[0].text, "");
        assert_eq!(blocks[0].status, Status::Ok);
        assert_eq!(blocks[0].detail, "20ms");
        assert_eq!(blocks[0].toggle, Some(TOGGLE_MAIN));
    }

    #[test]
    fn patch_rows_have_counts_and_targets() {
        let changes = vec![
            FileUpdateChange {
                path: "/repo/b.txt".to_string(),
                kind: PatchChangeKind::Add,
                diff: "x\ny\n".to_string(),
            },
            FileUpdateChange {
                path: "/repo/a.txt".to_string(),
                kind: PatchChangeKind::Delete,
                diff: "z\n".to_string(),
            },
        ];
        let blocks = patch_blocks(
            &changes,
            PatchApplyStatus::Completed,
            &std::collections::BTreeSet::from([0]),
            Path::new("/repo"),
        );
        let summary: Vec<(BlockKind, String, i32, i32)> = blocks
            .iter()
            .map(|block| (block.kind, block.title.clone(), block.added, block.removed))
            .collect();
        assert_eq!(
            summary,
            vec![
                (BlockKind::PatchSummary, "Edited 2 files".to_string(), 2, 1),
                (BlockKind::PatchFile, "a.txt".to_string(), 0, 1),
                (BlockKind::PatchFile, "b.txt".to_string(), 2, 0),
            ]
        );
        assert_eq!(blocks[1].target, "");
        assert_eq!(
            blocks[2].target,
            PathBuf::from("/repo/b.txt").display().to_string()
        );
        assert!(blocks[2].expanded);
        assert_eq!(blocks[2].lines.len(), 2);
    }

    #[test]
    fn user_message_chips() {
        let (text, chips) = user_message_parts(&[
            UserInput::Text {
                text: "hi\u{1b}[31m there".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::LocalImage {
                detail: None,
                path: PathBuf::from("/tmp/shot.png"),
            },
            UserInput::Mention {
                name: "repo".to_string(),
                path: "app://x".to_string(),
            },
        ]);
        assert_eq!(text, "hi there");
        let labels: Vec<String> = chips.into_iter().map(|chip| chip.text).collect();
        assert_eq!(labels, vec!["shot.png".to_string(), "@repo".to_string()]);
    }

    fn item_entry(item: ThreadItem, completed: bool) -> ItemEntry {
        ItemEntry {
            item,
            live: Live::None,
            expanded: std::collections::BTreeSet::new(),
            local_echo: false,
            unsent: false,
            completed,
            progress: None,
            from_event: true,
        }
    }

    fn ctx() -> RenderContext<'static> {
        RenderContext {
            cwd: Path::new("/repo"),
            show_raw_reasoning: false,
            hide_reasoning: false,
            open_threads: &[],
        }
    }

    #[test]
    fn mcp_tool_cards_show_arguments_and_errors() -> serde_json::Result<()> {
        let item: ThreadItem = serde_json::from_value(serde_json::json!({
            "type": "mcpToolCall",
            "id": "m",
            "server": "docs",
            "tool": "search",
            "status": "failed",
            "arguments": {"q": "slint"},
            "appContext": null,
            "mcpAppUi": null,
            "pluginId": null,
            "readOnlyHint": null,
            "result": null,
            "error": {"message": "timeout"},
            "durationMs": 1500
        }))?;
        let blocks = render_item(&item_entry(item, /*completed*/ true), ctx());
        let block = &blocks[0];
        assert_eq!(block.kind, BlockKind::Tool);
        assert_eq!(block.title, "docs.search");
        assert_eq!(block.meta, "Called");
        assert_eq!(block.text, r#"{"q":"slint"}"#);
        assert_eq!(block.status, Status::Failed);
        assert_eq!(block.detail, "1.50s");
        // Failures show their error without expanding.
        assert_eq!(
            block.lines.first().map(|line| line.text.as_str()),
            Some("Error: timeout")
        );
        Ok(())
    }

    #[test]
    fn web_search_titles_follow_the_action() -> serde_json::Result<()> {
        let item: ThreadItem = serde_json::from_value(serde_json::json!({
            "type": "webSearch",
            "id": "w",
            "query": "slint styled text",
            "action": {"type": "search", "query": "slint styled text", "queries": null}
        }))?;
        let running = render_item(&item_entry(item.clone(), /*completed*/ false), ctx());
        assert_eq!(running[0].title, "Searching the web");
        assert_eq!(running[0].status, Status::Running);
        let done = render_item(&item_entry(item, /*completed*/ true), ctx());
        assert_eq!(done[0].title, "Searched the web");
        assert_eq!(done[0].text, "slint styled text");
        Ok(())
    }

    #[test]
    fn agent_cards_link_to_threads() -> serde_json::Result<()> {
        let item: ThreadItem = serde_json::from_value(serde_json::json!({
            "type": "collabAgentToolCall",
            "id": "c",
            "tool": "spawnAgent",
            "status": "completed",
            "senderThreadId": "parent",
            "receiverThreadIds": ["0198aaaa-bbbb-cccc"],
            "prompt": "Investigate the flaky test",
            "model": "gpt-x",
            "reasoningEffort": null,
            "agentsStates": {"0198aaaa-bbbb-cccc": {"status": "running", "message": null}}
        }))?;
        let blocks = render_item(&item_entry(item, /*completed*/ true), ctx());
        let block = &blocks[0];
        assert_eq!(block.kind, BlockKind::Agent);
        assert_eq!(block.title, "Spawned Agent 0198aaaa");
        assert_eq!(block.text, "Investigate the flaky test");
        assert_eq!(block.detail, "gpt-x");
        assert_eq!(block.lines.len(), 1);
        assert_eq!(block.lines[0].target, "0198aaaa-bbbb-cccc");
        assert_eq!(block.lines[0].kind, Status::Running as i32);
        Ok(())
    }

    #[test]
    fn proposed_plans_have_a_header_and_actions() {
        let item = ThreadItem::Plan {
            id: "p".to_string(),
            text: "1. Do it\n2. Test it".to_string(),
        };
        let blocks = render_item(&item_entry(item, /*completed*/ true), ctx());
        let kinds: Vec<BlockKind> = blocks.iter().map(|block| block.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Section,
                BlockKind::Paragraph,
                BlockKind::Paragraph
            ]
        );
        assert_eq!(blocks[0].title, "Proposed plan");
        assert_eq!(blocks[1].tone, Tone::Plan);
        assert!(blocks[2].footer);
        assert!(!blocks[0].footer);
        assert!(!blocks[1].footer);
    }

    fn user_text(id: &str, text: &str) -> ThreadItem {
        ThreadItem::UserMessage {
            id: id.to_string(),
            client_id: None,
            content: vec![crate::session::text_input(text)],
        }
    }

    #[test]
    fn cross_tab_messages_render_as_cards() {
        let text = crate::xtab::tools::wrap_agent_message(
            "source-thread",
            "repo-a: tests",
            "Run the **tests**.\n\n- one\n- two",
            /*reply_expected*/ true,
        );
        let entry = item_entry(user_text("u", &text), /*completed*/ true);
        let open = ["source-thread".to_string()];
        let blocks = render_item(
            &entry,
            RenderContext {
                open_threads: &open,
                ..ctx()
            },
        );
        let summary: Vec<(BlockKind, Frame)> = blocks
            .iter()
            .map(|block| (block.kind, block.frame))
            .collect();
        assert_eq!(
            summary,
            vec![
                (BlockKind::CardHeader, Frame::Top),
                (BlockKind::Paragraph, Frame::Middle),
                (BlockKind::Paragraph, Frame::Middle),
                (BlockKind::Paragraph, Frame::Bottom),
            ]
        );
        let header = &blocks[0];
        assert_eq!(header.meta, "Message from");
        assert_eq!(header.title, "tab \"repo-a: tests\"");
        assert_eq!(header.text, "reply expected");
        assert_eq!(header.target, "source-thread");
        assert_eq!(header.detail, GO_TO_TAB);
        assert_eq!(blocks[1].rich.as_deref(), Some("Run the **tests**\\."));
        assert_eq!(blocks[1].gap, Gap::Item);
        assert!(blocks.iter().all(|block| block.message));
        // The source tab is closed: the link reopens the thread.
        let closed = render_item(&entry, ctx());
        assert_eq!(closed[0].detail, OPEN_THREAD);
    }

    #[test]
    fn forwarded_messages_show_the_note_and_quote_the_content() {
        let text = crate::xtab::tools::forward_text(
            "repo-b",
            /*source_thread_id*/ None,
            "Please look",
            "Found a bug.",
        );
        let mut entry = item_entry(user_text("c1", &text), /*completed*/ false);
        entry.local_echo = true;
        let blocks = render_item(&entry, ctx());
        let summary: Vec<(BlockKind, Option<&str>, i32)> = blocks
            .iter()
            .map(|block| (block.kind, block.rich.as_deref(), block.quote))
            .collect();
        assert_eq!(
            summary,
            vec![
                (BlockKind::CardHeader, None, 0),
                (BlockKind::Paragraph, Some("Please look"), 0),
                (BlockKind::Quote, Some("Found a bug\\."), 1),
            ]
        );
        assert_eq!(blocks[0].meta, "Forwarded from");
        // No source thread: nothing to link to. Pending echo: spinner.
        assert_eq!(blocks[0].detail, "");
        assert_eq!(blocks[0].status, Status::Running);
        // Ordinary messages are still bubbles.
        let plain = render_item(&item_entry(user_text("u", "hi"), /*completed*/ true), ctx());
        assert_eq!(plain[0].kind, BlockKind::User);
    }

    #[test]
    fn cards_have_a_header_and_framed_markdown() {
        let card = CardEntry {
            title: "Recap".to_string(),
            markdown: "Did **this**.\n\n```sh\nls\n```".to_string(),
        };
        let blocks = card_blocks(&card, ctx());
        let summary: Vec<(BlockKind, Frame)> = blocks
            .iter()
            .map(|block| (block.kind, block.frame))
            .collect();
        assert_eq!(
            summary,
            vec![
                (BlockKind::CardHeader, Frame::Top),
                (BlockKind::Paragraph, Frame::Middle),
                (BlockKind::Code, Frame::Bottom),
            ]
        );
        assert_eq!(blocks[0].title, "Recap");
        assert!(blocks[0].copyable);
        assert_eq!(blocks[0].target, "");
        let empty = card_blocks(
            &CardEntry {
                title: "Empty".to_string(),
                markdown: String::new(),
            },
            ctx(),
        );
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].frame, Frame::Single);
    }

    #[test]
    fn turn_end_markers() {
        let end = |status, duration_ms, error_reported| TurnEndEntry {
            status,
            duration_ms,
            error: Some("rate limited".to_string()),
            error_details: None,
            error_reported,
        };
        let titles = |end: TurnEndEntry| -> Vec<String> {
            render_turn_end(&end)
                .into_iter()
                .map(|block| format!("{}|{}", block.title, block.text))
                .collect()
        };
        assert_eq!(
            titles(end(TurnStatus::Completed, Some(65_000), false)),
            vec!["Worked for 1m 05s|".to_string()]
        );
        assert!(titles(end(TurnStatus::Completed, Some(400), false)).is_empty());
        assert_eq!(
            titles(end(TurnStatus::Failed, None, false)),
            vec!["Turn failed|rate limited".to_string()]
        );
        assert!(titles(end(TurnStatus::Failed, None, true)).is_empty());
        assert_eq!(
            titles(end(TurnStatus::Interrupted, None, false)),
            vec!["Interrupted|".to_string()]
        );
    }

    #[test]
    fn hidden_reasoning_renders_nothing() {
        let item = ThreadItem::Reasoning {
            id: "r".to_string(),
            summary: vec!["**Look**".to_string()],
            content: Vec::new(),
        };
        let entry = item_entry(item, /*completed*/ true);
        assert_eq!(render_item(&entry, ctx()).len(), 1);
        let hidden = RenderContext {
            hide_reasoning: true,
            ..ctx()
        };
        assert!(render_item(&entry, hidden).is_empty());
    }
}
