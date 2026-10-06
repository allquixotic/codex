//! Typed builders for the app-server requests the GUI sends most often.
//!
//! This is the GUI analogue of the TUI's `AppServerSession` param builders.
//! Each function returns a ready [`ClientRequest`] for a given request id so
//! callers can hand it to [`crate::backend::Backend::call`].

use std::path::Path;

use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItemsListCursor;
use codex_app_server_protocol::ThreadItemsListParams;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadSource;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::UserInput;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::ReasoningEffort;

/// Turns fetched when a thread is opened (same as the TUI).
pub(crate) const INITIAL_HISTORY_TURN_LIMIT: u32 = 5;
/// Items per `thread/items/list` page (server maximum).
pub(crate) const HISTORY_ITEM_PAGE_LIMIT: u32 = 100;

/// Settings chosen when opening a new thread.
#[derive(Clone, Debug, Default)]
pub(crate) struct NewThreadOptions {
    pub(crate) model: Option<String>,
    pub(crate) model_provider: Option<String>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) sandbox: Option<SandboxMode>,
    pub(crate) dynamic_tools: Option<Vec<DynamicToolSpec>>,
    pub(crate) ephemeral: bool,
}

pub(crate) fn thread_start(
    request_id: RequestId,
    cwd: &Path,
    options: NewThreadOptions,
) -> ClientRequest {
    ClientRequest::ThreadStart {
        request_id,
        params: ThreadStartParams {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            model: options.model,
            model_provider: options.model_provider,
            approval_policy: options.approval_policy,
            sandbox: options.sandbox,
            ephemeral: Some(options.ephemeral),
            history_mode: (!options.ephemeral).then_some(ThreadHistoryMode::Paginated),
            thread_source: Some(ThreadSource::User),
            dynamic_tools: options.dynamic_tools,
            ..ThreadStartParams::default()
        },
    }
}

/// Re-attaches to a thread without changing its settings; history is paged
/// separately (`exclude_turns`).
pub(crate) fn thread_resume(request_id: RequestId, thread_id: &str) -> ClientRequest {
    ClientRequest::ThreadResume {
        request_id,
        params: ThreadResumeParams {
            thread_id: thread_id.to_string(),
            exclude_turns: true,
            ..ThreadResumeParams::default()
        },
    }
}

/// Forks `thread_id`. `before_turn_id` drops that turn and later ones.
pub(crate) fn thread_fork(
    request_id: RequestId,
    thread_id: &str,
    before_turn_id: Option<String>,
) -> ClientRequest {
    ClientRequest::ThreadFork {
        request_id,
        params: ThreadForkParams {
            thread_id: thread_id.to_string(),
            before_turn_id,
            exclude_turns: true,
            thread_source: Some(ThreadSource::User),
            ..ThreadForkParams::default()
        },
    }
}

pub(crate) fn thread_read(request_id: RequestId, thread_id: &str) -> ClientRequest {
    ClientRequest::ThreadRead {
        request_id,
        params: ThreadReadParams {
            thread_id: thread_id.to_string(),
            include_turns: false,
        },
    }
}

/// Newest-first page of turn metadata (no items).
pub(crate) fn turns_page(
    request_id: RequestId,
    thread_id: &str,
    cursor: Option<String>,
    limit: u32,
) -> ClientRequest {
    ClientRequest::ThreadTurnsList {
        request_id,
        params: ThreadTurnsListParams {
            thread_id: thread_id.to_string(),
            cursor,
            limit: Some(limit),
            sort_direction: Some(SortDirection::Desc),
            items_view: Some(TurnItemsView::NotLoaded),
        },
    }
}

/// Newest-first page of items across the whole thread.
pub(crate) fn items_page(
    request_id: RequestId,
    thread_id: &str,
    cursor: Option<String>,
    limit: u32,
) -> ClientRequest {
    ClientRequest::ThreadItemsList {
        request_id,
        params: ThreadItemsListParams {
            thread_id: thread_id.to_string(),
            turn_id: None,
            cursor: cursor.map(ThreadItemsListCursor::Opaque),
            limit: Some(limit),
            sort_direction: Some(SortDirection::Desc),
        },
    }
}

/// Per-turn overrides; all sticky for later turns on the thread.
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnOverrides {
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) summary: Option<ReasoningSummary>,
    /// `Some(None)` clears the service tier.
    pub(crate) service_tier: Option<Option<String>>,
    /// Plan or Default mode (overrides model and effort when set).
    pub(crate) collaboration_mode: Option<CollaborationMode>,
    pub(crate) output_schema: Option<serde_json::Value>,
}

pub(crate) fn turn_start(
    request_id: RequestId,
    thread_id: &str,
    input: Vec<UserInput>,
    client_user_message_id: String,
    overrides: TurnOverrides,
) -> ClientRequest {
    ClientRequest::TurnStart {
        request_id,
        params: TurnStartParams {
            thread_id: thread_id.to_string(),
            input,
            client_user_message_id: Some(client_user_message_id),
            turn_trigger: Some("user".to_string()),
            model: overrides.model,
            effort: overrides.effort,
            approval_policy: overrides.approval_policy,
            approvals_reviewer: overrides.approvals_reviewer,
            sandbox_policy: overrides.sandbox_policy,
            summary: overrides.summary,
            service_tier: overrides.service_tier,
            collaboration_mode: overrides.collaboration_mode,
            output_schema: overrides.output_schema,
            ..TurnStartParams::default()
        },
    }
}

pub(crate) fn turn_steer(
    request_id: RequestId,
    thread_id: &str,
    expected_turn_id: &str,
    input: Vec<UserInput>,
    client_user_message_id: String,
) -> ClientRequest {
    ClientRequest::TurnSteer {
        request_id,
        params: TurnSteerParams {
            thread_id: thread_id.to_string(),
            expected_turn_id: expected_turn_id.to_string(),
            input,
            client_user_message_id: Some(client_user_message_id),
            ..TurnSteerParams::default()
        },
    }
}

pub(crate) fn turn_interrupt(
    request_id: RequestId,
    thread_id: &str,
    turn_id: &str,
) -> ClientRequest {
    ClientRequest::TurnInterrupt {
        request_id,
        params: TurnInterruptParams {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
        },
    }
}

/// Plain text user input.
pub(crate) fn text_input(text: impl Into<String>) -> UserInput {
    UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }
}

/// New client-side id for optimistic user messages.
pub(crate) fn new_client_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
