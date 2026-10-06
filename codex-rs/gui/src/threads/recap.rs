//! Conversation recap (`/recap`, GUI.md §6), ported from the TUI
//! (`tui/src/app/recap.rs`, `tui/src/temporary_structured_request.rs`).
//!
//! The app-server has no recap RPC. The recap is generated in a temporary,
//! ephemeral, read-only thread with every tool, MCP server and environment
//! disabled: one `turn/start` with an `output_schema` returns a JSON
//! `{summary, next_action}`. The temporary thread never gets a tab. Its
//! notifications reach [`AppController::threads_on_notification`], which
//! forwards them to the task driving the request through the run registered
//! in the source tab. Closing that tab drops the run, which cancels the
//! request (the turn is interrupted). The temporary thread is always
//! unsubscribed, so the server unloads it, even when `thread/start` answers
//! only after the recap gave up waiting.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadSource;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_context_fragments::ContextualUserFragment;
use codex_context_fragments::RecapPrompt;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::app::AppController;
use crate::app::TabId;
use crate::backend::Backend;
use crate::backend::BackendError;
use crate::session;
use crate::transcript::NoticeKind;

/// Title of the transcript card that shows a recap.
pub(crate) const RECAP_CARD_TITLE: &str = "Conversation recap";
/// Bound for starting the temporary thread and, separately, for its turn.
const STRUCTURED_TURN_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest structured answer accepted (the TUI's limit).
const STRUCTURED_RESPONSE_MAX_BYTES: usize = 8 * 1024;
const RECAP_MAX_CHARS: usize = 700;
const RECAP_NEXT_MAX_CHARS: usize = 200;
/// Answered exchanges included in the prompt (the TUI's limit).
const RECAP_HISTORY_MAX_TURNS: usize = 8;
const OMITTED_HISTORY: &str = "[Earlier exchanges omitted]\n\n";
const EXCERPT_MARKER: &str = "\n[... excerpted ...]\n";
/// Section headings of the transcript export (`transcript::export`).
const EXPORT_HEADINGS: &[&str] = &[
    "## User",
    "## Assistant",
    "## Activity",
    "## Plan",
    "## Reasoning",
];

/// A recap being generated for one tab.
#[derive(Debug)]
pub(crate) struct RecapRun {
    id: u64,
    /// The temporary thread, once the task started it.
    thread_id: Option<String>,
    /// Feeds the temporary thread's notifications to the task. Dropping it
    /// cancels the recap.
    events: Option<mpsc::UnboundedSender<ServerNotification>>,
}

/// The structured answer of the recap turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GeneratedRecap {
    pub(crate) summary: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) next_action: Option<String>,
}

impl GeneratedRecap {
    /// Card body: the summary, then the next step when there is one.
    pub(crate) fn markdown(&self) -> String {
        match &self.next_action {
            Some(next) => format!("{}\n\n**Next:** {next}", self.summary),
            None => self.summary.clone(),
        }
    }
}

/// Why a recap could not be shown.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RecapError {
    /// The tab was closed (or the app is shutting down).
    #[error("the recap was cancelled")]
    Cancelled,
    #[error("the request timed out")]
    TimedOut,
    #[error("{0}")]
    Request(#[from] BackendError),
    #[error("the temporary thread did not start read-only")]
    NotReadOnly,
    #[error("the turn ended as {0:?}")]
    TurnEnded(TurnStatus),
    #[error("the model returned no answer")]
    NoAnswer,
    #[error("the answer is larger than {STRUCTURED_RESPONSE_MAX_BYTES} bytes")]
    TooLarge,
    #[error("the answer did not match the expected format")]
    Invalid,
}

/// Settings the temporary thread copies from the visible thread.
#[derive(Clone, Debug, Default)]
pub(crate) struct TemporaryThreadOptions {
    pub(crate) model: Option<String>,
    pub(crate) model_provider: Option<String>,
    pub(crate) cwd: String,
}

/// JSON Schema of the recap answer (the TUI's schema).
pub(crate) fn recap_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "minLength": 1,
                "maxLength": RECAP_MAX_CHARS,
            },
            "next_action": {
                "type": ["string", "null"],
                "maxLength": RECAP_NEXT_MAX_CHARS,
            },
        },
        "required": ["summary", "next_action"],
        "additionalProperties": false,
    })
}

/// The full prompt for `history` (see [`recap_history`]).
pub(crate) fn recap_prompt(history: &str) -> String {
    RecapPrompt::new(history).render()
}

/// Validates the model's answer the way the TUI does.
pub(crate) fn parse_recap(response: &str) -> Option<GeneratedRecap> {
    let mut recap = serde_json::from_str::<GeneratedRecap>(response.trim()).ok()?;
    recap.summary = recap.summary.trim().to_string();
    if recap.summary.is_empty() || recap.summary.chars().count() > RECAP_MAX_CHARS {
        return None;
    }
    recap.next_action = recap
        .next_action
        .map(|action| action.trim().to_string())
        .filter(|action| !action.is_empty());
    if recap
        .next_action
        .as_ref()
        .is_some_and(|action| action.chars().count() > RECAP_NEXT_MAX_CHARS)
    {
        return None;
    }
    Some(recap)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Speaker {
    User,
    Assistant,
}

/// User and assistant messages of a transcript export, in order. Tool
/// activity, plans and reasoning are left out, like the TUI's recap.
fn conversation_messages(export: &str) -> Vec<(Speaker, String)> {
    let mut messages = Vec::new();
    let mut current: Option<(Option<Speaker>, Vec<&str>)> = None;
    let mut previous_blank = true;
    let flush = |current: Option<(Option<Speaker>, Vec<&str>)>,
                 messages: &mut Vec<(Speaker, String)>| {
        if let Some((Some(speaker), lines)) = current {
            let text = lines.join("\n").trim().to_string();
            if !text.is_empty() {
                messages.push((speaker, text));
            }
        }
    };
    for line in export.lines() {
        if previous_blank && EXPORT_HEADINGS.contains(&line) {
            flush(current.take(), &mut messages);
            let speaker = match line {
                "## User" => Some(Speaker::User),
                "## Assistant" => Some(Speaker::Assistant),
                _ => None,
            };
            current = Some((speaker, Vec::new()));
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
        previous_blank = line.trim().is_empty();
    }
    flush(current, &mut messages);
    messages
}

#[derive(Debug, Default)]
struct Exchange {
    user: String,
    assistant: String,
}

impl Exchange {
    fn fields(&self) -> impl Iterator<Item = (&'static str, &str)> {
        let user_label = if self.assistant.is_empty() {
            "Pending user request"
        } else {
            "User"
        };
        [
            (user_label, self.user.as_str()),
            ("Assistant", self.assistant.as_str()),
        ]
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
    }
}

/// The newest exchanges, oldest first. Adjacent messages of one speaker are
/// joined (steering belongs to one request).
fn recent_exchanges(messages: &[(Speaker, String)]) -> Vec<Exchange> {
    let mut exchanges = Vec::new();
    let mut current = Exchange::default();
    let mut answered = 0;
    for (speaker, text) in messages.iter().rev() {
        let mut content = text.clone();
        // In reverse order, an answer before a request belongs to the
        // preceding exchange.
        if *speaker == Speaker::Assistant && !current.user.is_empty() {
            answered += usize::from(!current.assistant.is_empty());
            exchanges.push(std::mem::take(&mut current));
            if answered == RECAP_HISTORY_MAX_TURNS {
                break;
            }
        }
        let field = match speaker {
            Speaker::User => &mut current.user,
            Speaker::Assistant => &mut current.assistant,
        };
        if !field.is_empty() {
            content.push_str("\n\n");
            content.push_str(field);
        }
        *field = content;
    }
    if !current.user.is_empty() {
        exchanges.push(current);
    }
    exchanges.reverse();
    exchanges
}

/// Conversation text for the recap prompt, built from a transcript export
/// (`AppController::transcript_export_markdown`).
///
/// Keeps the newest exchanges within [`RecapPrompt::HISTORY_MAX_BYTES`]:
/// older exchanges are dropped first, then the remaining messages are
/// excerpted, keeping both ends of each. Empty when there is nothing to
/// recap.
pub(crate) fn recap_history(export: &str) -> String {
    recap_history_within(export, RecapPrompt::HISTORY_MAX_BYTES)
}

fn recap_history_within(export: &str, max_bytes: usize) -> String {
    let exchanges = recent_exchanges(&conversation_messages(export));
    let Some(latest) = exchanges.last() else {
        return String::new();
    };
    let blocks: Vec<String> = exchanges
        .iter()
        .map(|exchange| {
            exchange
                .fields()
                .map(|(label, text)| format!("{label}: {text}"))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .collect();
    let mut bytes = blocks.iter().map(String::len).sum::<usize>() + 2 * (blocks.len() - 1);
    if bytes <= max_bytes {
        return blocks.join("\n\n");
    }

    // Keep the newest answer and a newer unanswered correction together.
    let retained = if latest.assistant.is_empty() { 2 } else { 1 };
    let oldest_retained = exchanges.len().saturating_sub(retained);
    let mut start = 0;
    while bytes > max_bytes.saturating_sub(OMITTED_HISTORY.len()) && start < oldest_retained {
        bytes -= blocks[start].len() + 2;
        start += 1;
    }
    let omission = if start > 0 { OMITTED_HISTORY } else { "" };
    let budget = max_bytes.saturating_sub(omission.len());
    if bytes <= budget {
        return format!("{omission}{}", blocks[start..].join("\n\n"));
    }

    let fields: Vec<(&str, &str)> = exchanges[start..]
        .iter()
        .flat_map(Exchange::fields)
        .collect();
    let field_count = fields.len();
    let overhead = fields
        .iter()
        .map(|(label, _)| label.len() + 2)
        .sum::<usize>()
        + 2 * field_count.saturating_sub(1);
    let mut remaining = budget.saturating_sub(overhead);
    let excerpts = fields
        .iter()
        .enumerate()
        .map(|(index, (label, text))| {
            let share = remaining / (field_count - index);
            // Reserve a share for later fields without wasting space on
            // short ones.
            let reserved = fields[index + 1..]
                .iter()
                .map(|(_, text)| text.len().min(share))
                .sum::<usize>();
            let excerpt = excerpt(text, remaining.saturating_sub(reserved));
            remaining = remaining.saturating_sub(excerpt.len());
            format!("{label}: {excerpt}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{omission}{excerpts}")
}

/// `text` shortened to `max_bytes`, keeping its start and end.
fn excerpt(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let Some(content_bytes) = max_bytes.checked_sub(EXCERPT_MARKER.len()) else {
        return text[..text.floor_char_boundary(max_bytes)].to_owned();
    };
    let head = text.floor_char_boundary(content_bytes / 2);
    let tail = text.ceil_char_boundary(text.len() - (content_bytes - content_bytes / 2));
    format!("{}{EXCERPT_MARKER}{}", &text[..head], &text[tail..])
}

/// Config overrides that remove every tool from the temporary thread (the
/// TUI's list), plus `mcp_servers` disabling each named server.
pub(crate) fn temporary_thread_config(
    mcp_server_names: &BTreeSet<String>,
) -> HashMap<String, Value> {
    let mut config: HashMap<String, Value> = [
        "features.apps",
        "features.code_mode",
        "features.code_mode_only",
        "features.context_management",
        "features.current_time_reminder",
        "features.deferred_executor",
        "features.enable_fanout",
        "features.goals",
        "features.hooks",
        "features.image_generation",
        "features.memories",
        "features.multi_agent",
        "features.multi_agent_v2",
        "features.plugins",
        "features.request_permissions_tool",
        "features.shell_snapshot",
        "features.shell_tool",
        "features.standalone_web_search",
        "features.token_budget",
        "features.tool_suggest",
        "features.unified_exec",
        "features.view_image",
        "cloud.skills.enabled",
        "skills.include_instructions",
        "tools.experimental_request_user_input.enabled",
        "tools.update_plan.enabled",
    ]
    .into_iter()
    .map(|key| (key.to_string(), Value::Bool(false)))
    .collect();
    config.insert("web_search".to_string(), json!("disabled"));
    config.insert(
        "default_permissions".to_string(),
        json!(BUILT_IN_PERMISSION_PROFILE_READ_ONLY),
    );
    config.insert(
        "mcp_servers".to_string(),
        Value::Object(
            mcp_server_names
                .iter()
                .map(|name| (name.clone(), json!({ "enabled": false })))
                .collect(),
        ),
    );
    config
}

/// `thread/start` params of the temporary thread.
pub(crate) fn temporary_thread_params(
    options: TemporaryThreadOptions,
    config: HashMap<String, Value>,
) -> ThreadStartParams {
    ThreadStartParams {
        model: options.model,
        model_provider: options.model_provider,
        cwd: (!options.cwd.is_empty()).then_some(options.cwd),
        sandbox: Some(SandboxMode::ReadOnly),
        runtime_workspace_roots: Some(Vec::new()),
        ephemeral: Some(true),
        thread_source: Some(ThreadSource::Feature("system".to_string())),
        environments: Some(Vec::new()),
        dynamic_tools: Some(Vec::new()),
        selected_capability_roots: Some(Vec::new()),
        config: Some(config),
        ..ThreadStartParams::default()
    }
}

/// Why [`finish_within`] returned without a result.
#[derive(Debug, Eq, PartialEq)]
enum Unfinished {
    TimedOut,
    /// The task ended without reporting (runtime shutting down).
    Lost,
}

/// Runs `work` on its own task and waits up to `deadline` for its result.
///
/// When the deadline passes first, the task keeps running and `clean_up`
/// receives its successful result, so whatever the work created (a thread
/// the server already subscribed us to) is released instead of leaked.
async fn finish_within<T, E, W, C, F>(
    deadline: Duration,
    work: W,
    clean_up: C,
) -> Result<Result<T, E>, Unfinished>
where
    T: Send + 'static,
    E: Send + 'static,
    W: Future<Output = Result<T, E>> + Send + 'static,
    C: FnOnce(T) -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let (result_tx, mut result_rx) = oneshot::channel();
    tokio::spawn(async move {
        let result = work.await;
        // Nobody waits any more: release what the work created.
        if let Err(Ok(late)) = result_tx.send(result) {
            clean_up(late).await;
        }
    });
    match tokio::time::timeout(deadline, &mut result_rx).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Err(Unfinished::Lost),
        Err(_) => {
            // From here a late result goes to `clean_up`; one that arrived
            // right at the deadline is still used.
            result_rx.close();
            result_rx.try_recv().map_err(|_| Unfinished::TimedOut)
        }
    }
}

/// Starts the temporary thread. Fails closed when the effective MCP server
/// list cannot be read or the thread is not read-only.
///
/// `config/read` and `thread/start` are bounded separately: a `thread/start`
/// that answers after its deadline still gets its thread unsubscribed.
pub(crate) async fn start_temporary_thread(
    backend: &Backend,
    options: TemporaryThreadOptions,
) -> Result<ThreadStartResponse, RecapError> {
    let effective: ConfigReadResponse = tokio::time::timeout(
        STRUCTURED_TURN_TIMEOUT,
        backend.request(ClientRequest::ConfigRead {
            request_id: backend.next_request_id(),
            params: ConfigReadParams {
                include_layers: false,
                cwd: (!options.cwd.is_empty()).then(|| options.cwd.clone()),
            },
        }),
    )
    .await
    .map_err(|_| RecapError::TimedOut)??;
    let mcp_server_names: BTreeSet<String> = effective
        .config
        .additional
        .get("mcp_servers")
        .and_then(Value::as_object)
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default();
    let params = temporary_thread_params(options, temporary_thread_config(&mcp_server_names));
    let start_backend = backend.clone();
    let cleanup_backend = backend.clone();
    let response = finish_within(
        STRUCTURED_TURN_TIMEOUT,
        async move {
            start_backend
                .request::<ThreadStartResponse>(ClientRequest::ThreadStart {
                    request_id: start_backend.next_request_id(),
                    params,
                })
                .await
        },
        move |late: ThreadStartResponse| async move {
            tracing::debug!("the recap thread started after its deadline; unsubscribing");
            unsubscribe_temporary_thread(&cleanup_backend, late.thread.id).await;
        },
    )
    .await
    .map_err(|unfinished| match unfinished {
        Unfinished::TimedOut => RecapError::TimedOut,
        Unfinished::Lost => RecapError::Cancelled,
    })??;
    if !matches!(response.sandbox, SandboxPolicy::ReadOnly { .. }) {
        unsubscribe_temporary_thread(backend, response.thread.id).await;
        return Err(RecapError::NotReadOnly);
    }
    Ok(response)
}

/// The latest assistant message of `turn_id` once that turn completes.
pub(crate) async fn collect_structured_response(
    notifications: &mut mpsc::UnboundedReceiver<ServerNotification>,
    turn_id: &str,
) -> Result<String, RecapError> {
    let mut response = None;
    while let Some(notification) = notifications.recv().await {
        match notification {
            ServerNotification::ItemCompleted(completed) if completed.turn_id == turn_id => {
                if let ThreadItem::AgentMessage { text, .. } = completed.item {
                    if text.len() > STRUCTURED_RESPONSE_MAX_BYTES {
                        return Err(RecapError::TooLarge);
                    }
                    response = Some(text);
                }
            }
            ServerNotification::TurnCompleted(completed) if completed.turn.id == turn_id => {
                if completed.turn.status != TurnStatus::Completed {
                    return Err(RecapError::TurnEnded(completed.turn.status));
                }
                return response.ok_or(RecapError::NoAnswer);
            }
            _ => {}
        }
    }
    Err(RecapError::Cancelled)
}

/// Best-effort, bounded detach of the temporary thread.
pub(crate) async fn unsubscribe_temporary_thread(backend: &Backend, thread_id: String) {
    let request = backend.request::<ThreadUnsubscribeResponse>(ClientRequest::ThreadUnsubscribe {
        request_id: backend.next_request_id(),
        params: ThreadUnsubscribeParams { thread_id },
    });
    match tokio::time::timeout(STRUCTURED_TURN_TIMEOUT, request).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => tracing::debug!(%err, "could not unsubscribe the recap thread"),
        Err(_) => tracing::debug!("unsubscribing the recap thread timed out"),
    }
}

/// Generates a recap: temporary thread, one structured turn, cleanup.
async fn run_recap(
    backend: Backend,
    tab_id: TabId,
    run_id: u64,
    options: TemporaryThreadOptions,
    prompt: String,
) -> Result<GeneratedRecap, RecapError> {
    let thread = start_temporary_thread(&backend, options).await?;
    let thread_id = thread.thread.id;
    let answer = run_structured_turn(&backend, tab_id, run_id, &thread_id, prompt).await;
    unsubscribe_temporary_thread(&backend, thread_id).await;
    parse_recap(&answer?).ok_or(RecapError::Invalid)
}

/// Registers the temporary thread with the source tab, then runs the turn
/// and waits for its answer. The turn is interrupted when the run is
/// cancelled or times out.
async fn run_structured_turn(
    backend: &Backend,
    tab_id: TabId,
    run_id: u64,
    thread_id: &str,
    prompt: String,
) -> Result<String, RecapError> {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (attached_tx, attached) = oneshot::channel();
    let attach_thread_id = thread_id.to_string();
    crate::ui_thread::post(move |app| {
        let attached = app.recap_attach(tab_id, run_id, attach_thread_id, events_tx);
        let _ = attached_tx.send(attached);
    });
    // Registering before `turn/start` guarantees no event of the turn is
    // missed; a refusal means the tab is gone.
    if !matches!(attached.await, Ok(true)) {
        return Err(RecapError::Cancelled);
    }
    let mut turn_id = None;
    let outcome = tokio::time::timeout(STRUCTURED_TURN_TIMEOUT, async {
        let started: TurnStartResponse = backend
            .request(ClientRequest::TurnStart {
                request_id: backend.next_request_id(),
                params: TurnStartParams {
                    thread_id: thread_id.to_string(),
                    input: vec![session::text_input(prompt)],
                    output_schema: Some(recap_output_schema()),
                    ..TurnStartParams::default()
                },
            })
            .await?;
        turn_id = Some(started.turn.id.clone());
        collect_structured_response(&mut events, &started.turn.id).await
    })
    .await
    .unwrap_or(Err(RecapError::TimedOut));
    if matches!(outcome, Err(RecapError::Cancelled | RecapError::TimedOut))
        && let Some(turn_id) = turn_id
    {
        let interrupt = backend.request::<TurnInterruptResponse>(session::turn_interrupt(
            backend.next_request_id(),
            thread_id,
            &turn_id,
        ));
        match tokio::time::timeout(STRUCTURED_TURN_TIMEOUT, interrupt).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => tracing::debug!(%err, "could not interrupt the recap turn"),
            Err(_) => tracing::debug!("interrupting the recap turn timed out"),
        }
    }
    outcome
}

impl AppController {
    /// `/recap`: summarizes the conversation of tab `index` into a card.
    pub(crate) fn recap_start(&mut self, index: usize) {
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        if thread.thread_id.is_none() {
            self.toast("Wait for the thread to start before asking for a recap");
            return;
        }
        if thread.extras.recap.is_some() {
            self.toast("A recap is already being generated");
            return;
        }
        let options = TemporaryThreadOptions {
            model: crate::composer::effective_model(thread),
            model_provider: thread.model_provider.clone(),
            cwd: thread.cwd.to_string_lossy().into_owned(),
        };
        let history = recap_history(&self.transcript_export_markdown(index));
        if history.is_empty() {
            self.transcript_push_notice(
                index,
                NoticeKind::Info,
                "There is no conversation to recap yet.".to_string(),
            );
            return;
        }
        let run_id = super::next_run_id();
        let tab_id = self.tabs[index].id;
        if let Some(thread) = self.thread_tab_mut(index) {
            thread.extras.recap = Some(RecapRun {
                id: run_id,
                thread_id: None,
                events: None,
            });
        }
        self.toast("Generating a recap…");
        let backend = self.backend.clone();
        let prompt = recap_prompt(&history);
        self.backend.spawn(async move {
            let result = run_recap(backend, tab_id, run_id, options, prompt).await;
            crate::ui_thread::post(move |app| app.recap_finished(tab_id, run_id, result));
        });
    }

    /// Connects the temporary thread to the run of tab `tab_id`. False when
    /// the tab or the run is gone (the task then cancels).
    fn recap_attach(
        &mut self,
        tab_id: TabId,
        run_id: u64,
        thread_id: String,
        events: mpsc::UnboundedSender<ServerNotification>,
    ) -> bool {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return false;
        };
        match self
            .thread_tab_mut(index)
            .and_then(|thread| thread.extras.recap.as_mut())
        {
            Some(run) if run.id == run_id => {
                run.thread_id = Some(thread_id);
                run.events = Some(events);
                true
            }
            _ => false,
        }
    }

    /// Forwards notifications of recap threads to their tasks.
    pub(super) fn recap_on_notification(&mut self, notification: &ServerNotification) {
        if !matches!(
            notification,
            ServerNotification::ItemCompleted(_) | ServerNotification::TurnCompleted(_)
        ) {
            return;
        }
        let Some(thread_id) = crate::app::notification_thread_id(notification) else {
            return;
        };
        for tab in &mut self.tabs {
            if let Some(thread) = tab.thread_mut()
                && let Some(run) = thread.extras.recap.as_mut()
                && run.thread_id.as_deref() == Some(thread_id)
                && let Some(events) = run.events.as_ref()
                && events.send(notification.clone()).is_err()
            {
                run.events = None;
            }
        }
    }

    fn recap_finished(
        &mut self,
        tab_id: TabId,
        run_id: u64,
        result: Result<GeneratedRecap, RecapError>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        // Only the run this tab is waiting for.
        if thread.extras.recap.as_ref().map(|run| run.id) != Some(run_id) {
            return;
        }
        thread.extras.recap = None;
        match result {
            Ok(recap) => {
                self.transcript_push_card(index, RECAP_CARD_TITLE.to_string(), recap.markdown());
            }
            Err(RecapError::Cancelled) => {}
            Err(err) => {
                tracing::warn!(%err, "could not generate a recap");
                self.transcript_push_notice(
                    index,
                    NoticeKind::Error,
                    format!("Could not generate a recap: {err}. Please try again."),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ItemCompletedNotification;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use pretty_assertions::assert_eq;

    fn export(sections: &[(&str, &str)]) -> String {
        let mut markdown = String::from("# Codex conversation\n");
        for (heading, body) in sections {
            markdown.push_str(&format!("\n## {heading}\n\n{body}\n"));
        }
        markdown
    }

    #[test]
    fn history_keeps_only_user_and_assistant_messages() {
        let markdown = export(&[
            ("User", "Fix the login bug"),
            ("Reasoning", "thinking about auth"),
            ("Activity", "    $ cargo test\n    ✓"),
            (
                "Assistant",
                "Fixed it in `auth.rs`.\n\n## Details\n\nA heading inside.",
            ),
            ("Plan", "1. done"),
        ]);
        assert_eq!(
            recap_history(&markdown),
            "User: Fix the login bug\n\nAssistant: Fixed it in `auth.rs`.\n\n## Details\n\nA heading inside."
        );
    }

    #[test]
    fn steering_joins_and_unanswered_requests_are_labelled() {
        let markdown = export(&[
            ("User", "Add tests"),
            ("User", "for the parser only"),
            ("Assistant", "Added 3 tests."),
            ("Assistant", "All pass."),
            ("User", "Now update the docs"),
        ]);
        assert_eq!(
            recap_history(&markdown),
            "User: Add tests\n\nfor the parser only\n\nAssistant: Added 3 tests.\n\nAll pass.\n\nPending user request: Now update the docs"
        );
    }

    #[test]
    fn empty_or_activity_only_exports_have_no_history() {
        assert_eq!(recap_history("# Codex conversation\n"), "");
        assert_eq!(recap_history(&export(&[("Activity", "    $ ls")])), "");
        // An assistant message without any request is not a conversation.
        assert_eq!(recap_history(&export(&[("Assistant", "hello")])), "");
    }

    #[test]
    fn only_the_newest_answered_exchanges_are_used() {
        let sections: Vec<(String, String)> = (0..12)
            .flat_map(|turn| {
                [
                    ("User".to_string(), format!("request {turn}")),
                    ("Assistant".to_string(), format!("answer {turn}")),
                ]
            })
            .collect();
        let borrowed: Vec<(&str, &str)> = sections
            .iter()
            .map(|(heading, body)| (heading.as_str(), body.as_str()))
            .collect();
        let history = recap_history(&export(&borrowed));
        assert!(history.starts_with("User: request 4\n"), "{history}");
        assert!(history.ends_with("Assistant: answer 11"), "{history}");
    }

    #[test]
    fn long_history_drops_old_exchanges_and_keeps_the_end() {
        let long = "x".repeat(300);
        let markdown = export(&[
            ("User", "first request"),
            ("Assistant", &long),
            ("User", "second request"),
            ("Assistant", "second answer"),
        ]);
        let history = recap_history_within(&markdown, 120);
        assert_eq!(
            history,
            "[Earlier exchanges omitted]\n\nUser: second request\n\nAssistant: second answer"
        );
    }

    #[test]
    fn oversized_messages_are_excerpted_at_both_ends() {
        let long = format!("{}MIDDLE{}", "a".repeat(200), "z".repeat(200));
        let markdown = export(&[("User", "short"), ("Assistant", &long)]);
        let history = recap_history_within(&markdown, 150);
        assert!(history.len() <= 150, "{} bytes", history.len());
        assert!(
            history.starts_with("User: short\n\nAssistant: aaa"),
            "{history}"
        );
        assert!(history.contains("[... excerpted ...]"));
        assert!(history.ends_with("zzz"));
        assert!(!history.contains("MIDDLE"));
    }

    #[test]
    fn excerpt_respects_char_boundaries() {
        let text = "é".repeat(50);
        let short = excerpt(&text, 40);
        assert!(short.len() <= 40);
        assert!(short.contains("[... excerpted ...]"));
        assert_eq!(excerpt("tiny", 40), "tiny");
        assert_eq!(excerpt(&text, 3), "é");
    }

    #[test]
    fn prompt_wraps_history_in_the_shared_instructions() {
        let prompt = recap_prompt("User: hi\n\nAssistant: hello");
        assert!(prompt.starts_with("Write a brief catch-up"));
        assert!(prompt.ends_with("Conversation:\nUser: hi\n\nAssistant: hello"));
        let huge = "y".repeat(RecapPrompt::MAX_BYTES * 2);
        assert!(recap_prompt(&huge).len() <= RecapPrompt::MAX_BYTES);
    }

    #[test]
    fn schema_requires_both_fields() {
        let schema = recap_output_schema();
        assert_eq!(schema["required"], json!(["summary", "next_action"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["summary"]["maxLength"], json!(700));
    }

    #[test]
    fn parses_valid_recaps() {
        assert_eq!(
            parse_recap(r#"{"summary":"  Fixed auth.  ","next_action":"  Run the suite "}"#),
            Some(GeneratedRecap {
                summary: "Fixed auth.".to_string(),
                next_action: Some("Run the suite".to_string()),
            })
        );
        assert_eq!(
            parse_recap(r#"{"summary":"Done.","next_action":"   "}"#),
            Some(GeneratedRecap {
                summary: "Done.".to_string(),
                next_action: None,
            })
        );
        assert_eq!(
            parse_recap("\n{\"summary\":\"Done.\",\"next_action\":null}\n").map(|r| r.markdown()),
            Some("Done.".to_string())
        );
    }

    #[test]
    fn rejects_invalid_recaps() {
        assert_eq!(parse_recap("You said: hello"), None);
        assert_eq!(parse_recap(r#"{"summary":"x"}"#), None);
        assert_eq!(parse_recap(r#"{"summary":"","next_action":null}"#), None);
        assert_eq!(
            parse_recap(r#"{"summary":"x","next_action":null,"extra":1}"#),
            None
        );
        let long = "s".repeat(RECAP_MAX_CHARS + 1);
        assert_eq!(
            parse_recap(&format!(r#"{{"summary":"{long}","next_action":null}}"#)),
            None
        );
        let next = "n".repeat(RECAP_NEXT_MAX_CHARS + 1);
        assert_eq!(
            parse_recap(&format!(r#"{{"summary":"ok","next_action":"{next}"}}"#)),
            None
        );
    }

    #[test]
    fn recap_markdown_shows_the_next_step() {
        let recap = GeneratedRecap {
            summary: "Fixed the parser.".to_string(),
            next_action: Some("Release 1.2".to_string()),
        };
        assert_eq!(
            recap.markdown(),
            "Fixed the parser.\n\n**Next:** Release 1.2"
        );
    }

    #[test]
    fn temporary_thread_is_ephemeral_read_only_and_tool_less() {
        let names = BTreeSet::from(["docs".to_string(), "github".to_string()]);
        let params = temporary_thread_params(
            TemporaryThreadOptions {
                model: Some("gpt-5".to_string()),
                model_provider: Some("openai".to_string()),
                cwd: "/work".to_string(),
            },
            temporary_thread_config(&names),
        );
        assert_eq!(params.ephemeral, Some(true));
        assert_eq!(params.sandbox, Some(SandboxMode::ReadOnly));
        assert_eq!(params.dynamic_tools, Some(Vec::new()));
        assert_eq!(params.environments, Some(Vec::new()));
        assert_eq!(params.cwd.as_deref(), Some("/work"));
        assert_eq!(params.model.as_deref(), Some("gpt-5"));
        assert_eq!(
            params.thread_source,
            Some(ThreadSource::Feature("system".to_string()))
        );
        let config = params.config.unwrap_or_default();
        assert_eq!(config["features.shell_tool"], json!(false));
        assert_eq!(config["features.unified_exec"], json!(false));
        assert_eq!(config["web_search"], json!("disabled"));
        assert_eq!(config["default_permissions"], json!(":read-only"));
        assert_eq!(
            config["mcp_servers"],
            json!({ "docs": { "enabled": false }, "github": { "enabled": false } })
        );
    }

    fn agent_message(turn_id: &str, text: &str) -> ServerNotification {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: "m".to_string(),
                text: text.to_string(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
            thread_id: "tmp".to_string(),
            turn_id: turn_id.to_string(),
            completed_at_ms: 0,
        })
    }

    fn turn_completed(turn_id: &str, status: TurnStatus) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "tmp".to_string(),
            turn: Turn {
                id: turn_id.to_string(),
                items: Vec::new(),
                items_view: Default::default(),
                status,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        })
    }

    #[tokio::test]
    async fn collects_the_last_answer_of_the_turn() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for notification in [
            agent_message("other", "ignored"),
            agent_message("t1", "first"),
            agent_message("t1", "{\"summary\":\"s\",\"next_action\":null}"),
            turn_completed("other", TurnStatus::Failed),
            turn_completed("t1", TurnStatus::Completed),
        ] {
            assert!(tx.send(notification).is_ok());
        }
        let answer = collect_structured_response(&mut rx, "t1").await;
        assert_eq!(
            answer.ok().as_deref(),
            Some("{\"summary\":\"s\",\"next_action\":null}")
        );
    }

    #[tokio::test]
    async fn failed_turns_and_closed_channels_are_errors() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(
            tx.send(turn_completed("t1", TurnStatus::Interrupted))
                .is_ok()
        );
        assert!(matches!(
            collect_structured_response(&mut rx, "t1").await,
            Err(RecapError::TurnEnded(TurnStatus::Interrupted))
        ));
        assert!(tx.send(turn_completed("t2", TurnStatus::Completed)).is_ok());
        assert!(matches!(
            collect_structured_response(&mut rx, "t2").await,
            Err(RecapError::NoAnswer)
        ));
        assert!(tx.send(agent_message("t3", &"x".repeat(9000))).is_ok());
        assert!(matches!(
            collect_structured_response(&mut rx, "t3").await,
            Err(RecapError::TooLarge)
        ));
        drop(tx);
        assert!(matches!(
            collect_structured_response(&mut rx, "t4").await,
            Err(RecapError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn work_finished_in_time_is_returned_without_clean_up() {
        let (cleaned_tx, mut cleaned_rx) = mpsc::unbounded_channel::<u32>();
        let result = finish_within(
            Duration::from_secs(5),
            async { Ok::<u32, String>(7) },
            move |value| async move {
                let _ = cleaned_tx.send(value);
            },
        )
        .await;
        assert_eq!(result, Ok(Ok(7)));
        // The task has ended, so no clean-up can follow.
        assert_eq!(cleaned_rx.recv().await, None);
    }

    #[tokio::test]
    async fn work_finishing_after_the_deadline_is_cleaned_up() {
        let (cleaned_tx, mut cleaned_rx) = mpsc::unbounded_channel::<u32>();
        let result = finish_within(
            Duration::from_millis(10),
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<u32, String>(7)
            },
            move |value| async move {
                let _ = cleaned_tx.send(value);
            },
        )
        .await;
        assert_eq!(result, Err(Unfinished::TimedOut));
        // The late thread is released instead of leaked.
        assert_eq!(cleaned_rx.recv().await, Some(7));
    }

    #[tokio::test]
    async fn late_failures_need_no_clean_up() {
        let (cleaned_tx, mut cleaned_rx) = mpsc::unbounded_channel::<u32>();
        let result = finish_within(
            Duration::from_millis(10),
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err::<u32, String>("boom".to_string())
            },
            move |value| async move {
                let _ = cleaned_tx.send(value);
            },
        )
        .await;
        assert_eq!(result, Err(Unfinished::TimedOut));
        assert_eq!(cleaned_rx.recv().await, None);
    }
}
