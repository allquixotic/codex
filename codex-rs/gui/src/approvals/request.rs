//! Pending server requests and how each kind is shown and answered.
//!
//! Everything here is pure: [`intake`] classifies an incoming
//! [`ServerRequest`], [`PendingRequest::card`] describes the card, and
//! [`PendingRequest::answer`] builds the JSON response plus the transcript
//! notice. `mod.rs` wires these to Slint and the backend.

use std::path::Path;

use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionApprovalKind;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::CurrentTimeReadResponse;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::FileChangeRequestApprovalParams;
use codex_app_server_protocol::FileChangeRequestApprovalResponse;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::GrantedPermissionProfile;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::McpServerElicitationAction;
use codex_app_server_protocol::NetworkApprovalContext;
use codex_app_server_protocol::NetworkApprovalProtocol;
use codex_app_server_protocol::NetworkPolicyRuleAction;
use codex_app_server_protocol::PermissionGrantScope;
use codex_app_server_protocol::PermissionsRequestApprovalParams;
use codex_app_server_protocol::PermissionsRequestApprovalResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use serde_json::Value;

use super::delivery::DeliveryChoice;
use super::delivery::DeliveryRequest;
use super::elicitation::Elicitation;
use super::format;
use super::format::DiffLine;
use super::user_input::UserInputForm;
use crate::transcript::NoticeKind;

/// JSON-RPC error code used for requests codex-gui does not support.
pub(crate) const UNSUPPORTED_ERROR_CODE: i64 = -32000;

/// Visual weight of an option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Tone {
    Positive,
    Negative,
}

/// What picking an option means, per request kind.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Decision {
    Command(CommandExecutionApprovalDecision),
    FileChange(FileChangeApprovalDecision),
    Permissions(PermissionsChoice),
    Elicitation {
        action: McpServerElicitationAction,
        meta: Option<Value>,
        /// Opened in the browser when chosen (URL-mode elicitations).
        open_url: Option<String>,
    },
    /// Answered by the cross-tab controller, not with a server response.
    Delivery(DeliveryChoice),
}

/// The four permission-grant choices the TUI offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionsChoice {
    GrantForTurn,
    GrantForTurnWithStrictAutoReview,
    GrantForSession,
    Deny,
}

/// One row of a choices card.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ApprovalOption {
    pub(crate) label: String,
    /// Lowercase key that picks this option while the card has focus.
    pub(crate) key: Option<char>,
    /// Picked by Esc; the key chip shows "Esc".
    pub(crate) cancels: bool,
    pub(crate) tone: Tone,
    pub(crate) decision: Decision,
}

impl ApprovalOption {
    pub(crate) fn new(label: impl Into<String>, key: char, tone: Tone, decision: Decision) -> Self {
        Self {
            label: label.into(),
            key: Some(key),
            cancels: false,
            tone,
            decision,
        }
    }

    /// Marks this option as the one Esc picks.
    pub(crate) fn cancel(mut self) -> Self {
        self.cancels = true;
        self
    }

    /// Text of the key chip.
    pub(crate) fn key_label(&self) -> String {
        if self.cancels {
            "Esc".to_string()
        } else {
            self.key
                .map(|key| key.to_ascii_uppercase().to_string())
                .unwrap_or_default()
        }
    }
}

/// The reply to send and how to record it in the transcript.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Answer {
    pub(crate) result: Value,
    pub(crate) notice: Option<(NoticeKind, String)>,
    pub(crate) open_url: Option<String>,
}

impl Answer {
    fn new<T: serde::Serialize>(
        response: &T,
        kind: NoticeKind,
        text: String,
    ) -> serde_json::Result<Self> {
        Ok(Self {
            result: serde_json::to_value(response)?,
            notice: Some((kind, text)),
            open_url: None,
        })
    }
}

/// A server request waiting for the user, with any in-progress form state.
#[derive(Clone, Debug)]
pub(crate) struct PendingRequest {
    pub(crate) request_id: RequestId,
    /// Thread that sent the request (may differ from the tab's thread for
    /// sub-agents).
    pub(crate) thread_id: String,
    pub(crate) turn_id: Option<String>,
    /// Who asked, when it is not the tab's own thread ("Sub-agent Ada").
    pub(crate) origin: Option<String>,
    pub(crate) kind: RequestKind,
}

#[derive(Clone, Debug)]
pub(crate) enum RequestKind {
    Command(Box<CommandExecutionRequestApprovalParams>),
    FileChange(FileChangeRequestApprovalParams),
    Permissions(PermissionsRequestApprovalParams),
    UserInput(UserInputForm),
    Elicitation(Elicitation),
    /// A cross-tab message waiting for consent in its target tab. The
    /// request id and thread are those of the sender's tool call.
    Delivery(Box<DeliveryRequest>),
}

/// How an incoming server request is handled.
#[derive(Debug)]
pub(crate) enum Intake {
    /// Needs a decision from the user.
    Pending(Box<PendingRequest>),
    /// Answered right away without asking.
    Resolve {
        request_id: RequestId,
        thread_id: Option<String>,
        result: Value,
        notice: Option<String>,
    },
    /// Refused right away.
    Reject {
        request_id: RequestId,
        thread_id: Option<String>,
        error: JSONRPCErrorError,
        notice: Option<String>,
    },
}

fn unsupported(message: impl Into<String>) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: UNSUPPORTED_ERROR_CODE,
        message: message.into(),
        data: None,
    }
}

/// Classifies a server request (everything except dynamic tool calls).
pub(crate) fn intake(request: ServerRequest, now_unix_seconds: i64) -> Intake {
    let pending = |request_id, thread_id, turn_id, kind| {
        Intake::Pending(Box::new(PendingRequest {
            request_id,
            thread_id,
            turn_id,
            origin: None,
            kind,
        }))
    };
    match request {
        ServerRequest::CommandExecutionRequestApproval { request_id, params } => pending(
            request_id,
            params.thread_id.clone(),
            Some(params.turn_id.clone()),
            RequestKind::Command(Box::new(params)),
        ),
        ServerRequest::FileChangeRequestApproval { request_id, params } => pending(
            request_id,
            params.thread_id.clone(),
            Some(params.turn_id.clone()),
            RequestKind::FileChange(params),
        ),
        ServerRequest::PermissionsRequestApproval { request_id, params } => pending(
            request_id,
            params.thread_id.clone(),
            Some(params.turn_id.clone()),
            RequestKind::Permissions(params),
        ),
        ServerRequest::ToolRequestUserInput { request_id, params } => pending(
            request_id,
            params.thread_id.clone(),
            Some(params.turn_id.clone()),
            RequestKind::UserInput(UserInputForm::new(&params)),
        ),
        ServerRequest::McpServerElicitationRequest { request_id, params } => {
            let thread_id = params.thread_id.clone();
            let turn_id = params.turn_id.clone();
            match Elicitation::from_params(params) {
                Ok(elicitation) => pending(
                    request_id,
                    thread_id,
                    turn_id,
                    RequestKind::Elicitation(elicitation),
                ),
                Err(unsupported) => {
                    let response = unsupported.decline_response();
                    match serde_json::to_value(response) {
                        Ok(result) => Intake::Resolve {
                            request_id,
                            thread_id: Some(thread_id),
                            result,
                            notice: Some(unsupported.notice()),
                        },
                        Err(err) => Intake::Reject {
                            request_id,
                            thread_id: Some(thread_id),
                            error: unsupported_encoding(&err),
                            notice: Some(unsupported.notice()),
                        },
                    }
                }
            }
        }
        ServerRequest::CurrentTimeRead { request_id, params } => {
            let response = CurrentTimeReadResponse {
                current_time_at: now_unix_seconds,
            };
            match serde_json::to_value(response) {
                Ok(result) => Intake::Resolve {
                    request_id,
                    thread_id: Some(params.thread_id),
                    result,
                    notice: None,
                },
                Err(err) => Intake::Reject {
                    request_id,
                    thread_id: Some(params.thread_id),
                    error: unsupported_encoding(&err),
                    notice: None,
                },
            }
        }
        ServerRequest::ApplyPatchApproval { request_id, params } => Intake::Reject {
            request_id,
            thread_id: Some(params.conversation_id.to_string()),
            error: unsupported(
                "Legacy patch approval requests are not supported by codex-gui; use v2 turns.",
            ),
            notice: Some("Rejected a legacy patch approval request.".to_string()),
        },
        ServerRequest::ExecCommandApproval { request_id, params } => Intake::Reject {
            request_id,
            thread_id: Some(params.conversation_id.to_string()),
            error: unsupported(
                "Legacy command approval requests are not supported by codex-gui; use v2 turns.",
            ),
            notice: Some("Rejected a legacy command approval request.".to_string()),
        },
        ServerRequest::AttestationGenerate { request_id, .. } => Intake::Reject {
            request_id,
            thread_id: None,
            error: unsupported("codex-gui cannot generate attestations."),
            notice: None,
        },
        ServerRequest::ChatgptAuthTokensRefresh { request_id, .. } => Intake::Reject {
            request_id,
            thread_id: None,
            error: unsupported("codex-gui does not manage external ChatGPT auth tokens."),
            notice: None,
        },
        ServerRequest::DynamicToolCall { request_id, params } => Intake::Reject {
            request_id,
            thread_id: Some(params.thread_id),
            error: unsupported(format!(
                "tool {} is not available in codex-gui",
                params.tool
            )),
            notice: None,
        },
    }
}

fn unsupported_encoding(err: &serde_json::Error) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: -32603,
        message: format!("codex-gui failed to encode response: {err}"),
        data: None,
    }
}

/// Body of a card: a list of options, or a form.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CardBody {
    Choices(Vec<ApprovalOption>),
    Form(FormView),
}

/// Field kinds a form can render.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FieldKind {
    SingleChoice,
    MultiChoice,
    Text,
    Number,
    Boolean,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChoiceView {
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) checked: bool,
}

/// One rendered form field.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FieldView {
    pub(crate) kind: FieldKind,
    pub(crate) header: String,
    pub(crate) prompt: String,
    pub(crate) required: bool,
    pub(crate) choices: Vec<ChoiceView>,
    pub(crate) show_text: bool,
    pub(crate) text: String,
    pub(crate) placeholder: String,
    pub(crate) secret: bool,
    pub(crate) error: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FormView {
    pub(crate) fields: Vec<FieldView>,
    pub(crate) submit_label: String,
    /// Empty hides the button.
    pub(crate) secondary_label: String,
    pub(crate) tertiary_label: String,
    pub(crate) error: String,
}

/// Everything the card shows for one request. `details` are inline
/// markdown; untrusted text in them is escaped.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CardView {
    pub(crate) title: String,
    pub(crate) reason: String,
    pub(crate) code: String,
    pub(crate) code_caption: String,
    pub(crate) details: Vec<String>,
    pub(crate) diff: Vec<DiffLine>,
    pub(crate) link: String,
    pub(crate) body: CardBody,
}

impl CardView {
    pub(crate) fn choices(title: impl Into<String>, options: Vec<ApprovalOption>) -> Self {
        Self {
            title: title.into(),
            reason: String::new(),
            code: String::new(),
            code_caption: String::new(),
            details: Vec::new(),
            diff: Vec::new(),
            link: String::new(),
            body: CardBody::Choices(options),
        }
    }

    pub(crate) fn form(title: impl Into<String>, form: FormView) -> Self {
        Self {
            body: CardBody::Form(form),
            ..Self::choices(title, Vec::new())
        }
    }
}

/// What the card needs besides the request itself.
pub(crate) struct CardContext<'a> {
    /// Folder of the tab showing the card, for relative paths.
    pub(crate) cwd: Option<&'a Path>,
    /// Cached changes of the file-change item, when known.
    pub(crate) file_changes: Option<&'a [FileUpdateChange]>,
}

impl PendingRequest {
    /// Card shown for this request.
    pub(crate) fn card(&self, context: &CardContext<'_>) -> CardView {
        match &self.kind {
            RequestKind::Command(params) => command_card(params),
            RequestKind::FileChange(params) => file_change_card(params, context),
            RequestKind::Permissions(params) => permissions_card(params),
            RequestKind::UserInput(form) => form.card(),
            RequestKind::Elicitation(elicitation) => elicitation.card(),
            RequestKind::Delivery(delivery) => delivery.card(),
        }
    }

    /// Options of a choices card; empty for forms.
    pub(crate) fn options(&self) -> Vec<ApprovalOption> {
        match &self.kind {
            RequestKind::Command(params) => command_options(params),
            RequestKind::FileChange(_) => file_change_options(),
            RequestKind::Permissions(_) => permissions_options(),
            RequestKind::Elicitation(elicitation) => elicitation.options(),
            RequestKind::Delivery(delivery) => delivery.options(),
            RequestKind::UserInput(_) => Vec::new(),
        }
    }

    /// The option Esc picks, if the request has one.
    pub(crate) fn cancel_option(&self) -> Option<ApprovalOption> {
        self.options().into_iter().find(|option| option.cancels)
    }

    /// Response and notice for a picked option.
    pub(crate) fn answer(&self, decision: &Decision) -> serde_json::Result<Answer> {
        match (&self.kind, decision) {
            (RequestKind::Command(params), Decision::Command(decision)) => {
                command_answer(params, decision)
            }
            (RequestKind::FileChange(_), Decision::FileChange(decision)) => {
                file_change_answer(decision)
            }
            (RequestKind::Permissions(params), Decision::Permissions(choice)) => {
                permissions_answer(params, *choice)
            }
            (
                RequestKind::Elicitation(elicitation),
                Decision::Elicitation {
                    action,
                    meta,
                    open_url,
                },
            ) => elicitation.choice_answer(*action, meta.clone(), open_url.clone()),
            (_, decision) => Err(serde::ser::Error::custom(format!(
                "decision {decision:?} does not apply to this request"
            ))),
        }
    }

    /// Short description for desktop notifications.
    pub(crate) fn notification_body(&self) -> String {
        match &self.kind {
            RequestKind::Command(params) => match &params.network_approval_context {
                Some(network) => format!("Allow network access to {}?", network.host),
                None => match params.command.as_deref() {
                    Some(command) => format!(
                        "Run {}?",
                        format::snippet(&format::display_command(command))
                    ),
                    None => "Approve a command?".to_string(),
                },
            },
            RequestKind::FileChange(_) => "Approve file changes?".to_string(),
            RequestKind::Permissions(_) => "Grant additional permissions?".to_string(),
            RequestKind::UserInput(form) => form.notification_body(),
            RequestKind::Elicitation(elicitation) => {
                format!("{} needs your input", elicitation.server_name())
            }
            RequestKind::Delivery(delivery) => delivery.notification_body(),
        }
    }

    /// Item id whose cached file changes this request displays.
    pub(crate) fn file_change_item(&self) -> Option<(&str, &str)> {
        match &self.kind {
            RequestKind::FileChange(params) => Some((&params.thread_id, &params.item_id)),
            _ => None,
        }
    }
}

// ----- command execution ------------------------------------------------------

/// Decisions to offer: the server's list, else the TUI's defaults.
pub(crate) fn command_decisions(
    params: &CommandExecutionRequestApprovalParams,
) -> Vec<CommandExecutionApprovalDecision> {
    if let Some(decisions) = params.available_decisions.as_ref() {
        return decisions.clone();
    }
    if params.network_approval_context.is_some() {
        let mut decisions = vec![
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::AcceptForSession,
        ];
        if let Some(amendment) =
            params
                .proposed_network_policy_amendments
                .as_ref()
                .and_then(|amendments| {
                    amendments
                        .iter()
                        .find(|amendment| amendment.action == NetworkPolicyRuleAction::Allow)
                })
        {
            decisions.push(
                CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
                    network_policy_amendment: amendment.clone(),
                },
            );
        }
        decisions.push(CommandExecutionApprovalDecision::Cancel);
        return decisions;
    }
    if params.additional_permissions.is_some() {
        return vec![
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::Cancel,
        ];
    }
    let mut decisions = vec![CommandExecutionApprovalDecision::Accept];
    if let Some(amendment) = params.proposed_execpolicy_amendment.as_ref() {
        decisions.push(
            CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
                execpolicy_amendment: amendment.clone(),
            },
        );
    }
    decisions.push(CommandExecutionApprovalDecision::Cancel);
    decisions
}

/// TUI label for a command decision; `None` hides it (a prefix that spans
/// several lines cannot be shown faithfully).
pub(crate) fn command_option(
    decision: &CommandExecutionApprovalDecision,
    params: &CommandExecutionRequestApprovalParams,
) -> Option<ApprovalOption> {
    let network = params.network_approval_context.is_some();
    let wrap = |decision: CommandExecutionApprovalDecision| Decision::Command(decision);
    let option = match decision {
        CommandExecutionApprovalDecision::Accept => ApprovalOption::new(
            if network {
                "Yes, just this once"
            } else {
                "Yes, proceed"
            },
            'y',
            Tone::Positive,
            wrap(decision.clone()),
        ),
        CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
            execpolicy_amendment,
        } => {
            let prefix = format::display_argv(&execpolicy_amendment.command);
            if prefix.contains('\n') || prefix.contains('\r') {
                return None;
            }
            ApprovalOption::new(
                format!("Yes, and don't ask again for commands that start with `{prefix}`"),
                'p',
                Tone::Positive,
                wrap(decision.clone()),
            )
        }
        CommandExecutionApprovalDecision::AcceptForSession => ApprovalOption::new(
            if network {
                "Yes, and allow this host for this conversation"
            } else if params.additional_permissions.is_some() {
                "Yes, and allow these permissions for this session"
            } else {
                "Yes, and don't ask again for this command in this session"
            },
            'a',
            Tone::Positive,
            wrap(decision.clone()),
        ),
        CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
            network_policy_amendment,
        } => match network_policy_amendment.action {
            NetworkPolicyRuleAction::Allow => ApprovalOption::new(
                "Yes, and allow this host in the future",
                'p',
                Tone::Positive,
                wrap(decision.clone()),
            ),
            NetworkPolicyRuleAction::Deny => ApprovalOption::new(
                "No, and block this host in the future",
                'd',
                Tone::Negative,
                wrap(decision.clone()),
            ),
        },
        CommandExecutionApprovalDecision::Decline => ApprovalOption::new(
            "No, continue without running it",
            'd',
            Tone::Negative,
            wrap(decision.clone()),
        ),
        CommandExecutionApprovalDecision::Cancel => ApprovalOption::new(
            "No, and tell Codex what to do differently",
            'n',
            Tone::Negative,
            wrap(decision.clone()),
        )
        .cancel(),
    };
    Some(option)
}

fn command_options(params: &CommandExecutionRequestApprovalParams) -> Vec<ApprovalOption> {
    command_decisions(params)
        .iter()
        .filter_map(|decision| command_option(decision, params))
        .collect()
}

fn network_scheme(protocol: NetworkApprovalProtocol) -> &'static str {
    match protocol {
        NetworkApprovalProtocol::Http => "http",
        NetworkApprovalProtocol::Https => "https",
        NetworkApprovalProtocol::Socks5Tcp => "socks5-tcp",
        NetworkApprovalProtocol::Socks5Udp => "socks5-udp",
    }
}

/// What a network approval is about, as the TUI names it.
fn network_target(network: &NetworkApprovalContext, command: Option<&str>) -> String {
    let argv = command.map(format::split_command).unwrap_or_default();
    let from_command = match argv.as_slice() {
        [program, target] if program == "network-access" && !target.is_empty() => {
            Some(target.clone())
        }
        [command] => command
            .strip_prefix("network-access ")
            .filter(|target| !target.is_empty())
            .map(ToString::to_string),
        _ => None,
    };
    from_command
        .unwrap_or_else(|| format!("{}://{}", network_scheme(network.protocol), network.host))
}

fn command_card(params: &CommandExecutionRequestApprovalParams) -> CardView {
    let title = if let Some(network) = &params.network_approval_context {
        format!("Allow network access to {}?", network.host)
    } else if params.kind == CommandExecutionApprovalKind::WriteStdin {
        "Send input to the running command?".to_string()
    } else if params.additional_permissions.is_some() {
        "Run this command with additional permissions?".to_string()
    } else {
        "Run this command?".to_string()
    };
    let mut card = CardView::choices(title, command_options(params));
    card.reason = params.reason.clone().unwrap_or_default();
    if let Some(network) = &params.network_approval_context {
        card.code = network_target(network, params.command.as_deref());
    } else if let Some(command) = params.command.as_deref() {
        card.code = if params.kind == CommandExecutionApprovalKind::WriteStdin {
            let input = format::split_command(command).pop().unwrap_or_default();
            format!("{input:?}")
        } else {
            format!("$ {}", format::display_command(command))
        };
    }
    if params.network_approval_context.is_none()
        && let Some(command_cwd) = params.cwd.as_ref()
    {
        card.code_caption = format!(
            "in {}",
            format::display_path(command_cwd.as_str(), /*cwd*/ None)
        );
    }
    if let Some(environment) = non_default_environment(params.environment_id.as_deref()) {
        card.details.push(format!(
            "**Environment:** {}",
            format::escape_markdown(environment)
        ));
    }
    if let Some(permissions) = params.additional_permissions.as_ref()
        && let Some(rule) = format::permissions_rule(
            permissions.network.as_ref(),
            permissions.file_system.as_ref(),
        )
    {
        card.details.push(format!("**Permission rule:** {rule}"));
    }
    card
}

fn command_answer(
    params: &CommandExecutionRequestApprovalParams,
    decision: &CommandExecutionApprovalDecision,
) -> serde_json::Result<Answer> {
    let subject = match &params.network_approval_context {
        Some(network) => Subject::Network(network_target(network, params.command.as_deref())),
        None => Subject::Command(
            params
                .command
                .as_deref()
                .map(|command| format::snippet(&format::display_command(command)))
                .filter(|snippet| !snippet.is_empty()),
        ),
    };
    let (kind, text) = command_notice(&subject, decision);
    Answer::new(
        &CommandExecutionRequestApprovalResponse {
            decision: decision.clone(),
        },
        kind,
        text,
    )
}

enum Subject {
    Command(Option<String>),
    Network(String),
}

fn command_notice(
    subject: &Subject,
    decision: &CommandExecutionApprovalDecision,
) -> (NoticeKind, String) {
    let run = |snippet: &Option<String>| match snippet {
        Some(snippet) => format!("to run {}", format::code_span(snippet)),
        None => "this request".to_string(),
    };
    match (decision, subject) {
        (CommandExecutionApprovalDecision::Accept, Subject::Command(snippet)) => (
            NoticeKind::Info,
            format!("You approved Codex {} this time", run(snippet)),
        ),
        (CommandExecutionApprovalDecision::Accept, Subject::Network(target)) => (
            NoticeKind::Info,
            format!("You approved Codex network access to {target} this time"),
        ),
        (CommandExecutionApprovalDecision::AcceptForSession, Subject::Command(snippet)) => (
            NoticeKind::Info,
            format!(
                "You approved Codex {} every time this session",
                run(snippet)
            ),
        ),
        (CommandExecutionApprovalDecision::AcceptForSession, Subject::Network(target)) => (
            NoticeKind::Info,
            format!("You approved Codex network access to {target} every time this session"),
        ),
        (
            CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
                execpolicy_amendment,
            },
            _,
        ) => (
            NoticeKind::Info,
            format!(
                "You approved Codex to always run commands that start with {}",
                format::code_span(&format::snippet(&format::display_argv(
                    &execpolicy_amendment.command
                )))
            ),
        ),
        (
            CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
                network_policy_amendment,
            },
            subject,
        ) => {
            let target = match subject {
                Subject::Network(target) => target.clone(),
                Subject::Command(_) => network_policy_amendment.host.clone(),
            };
            match network_policy_amendment.action {
                NetworkPolicyRuleAction::Allow => (
                    NoticeKind::Info,
                    format!("You persisted Codex network access to {target}"),
                ),
                NetworkPolicyRuleAction::Deny => (
                    NoticeKind::Warning,
                    format!("You denied Codex network access to {target} and saved that rule"),
                ),
            }
        }
        (CommandExecutionApprovalDecision::Decline, Subject::Command(snippet)) => (
            NoticeKind::Warning,
            format!("You did not approve Codex {}", run(snippet)),
        ),
        (CommandExecutionApprovalDecision::Decline, Subject::Network(target)) => (
            NoticeKind::Warning,
            format!("You did not approve Codex network access to {target}"),
        ),
        (CommandExecutionApprovalDecision::Cancel, Subject::Command(snippet)) => (
            NoticeKind::Warning,
            format!(
                "You canceled the request {} and stopped the turn",
                run(snippet)
            ),
        ),
        (CommandExecutionApprovalDecision::Cancel, Subject::Network(target)) => (
            NoticeKind::Warning,
            format!("You canceled network access to {target} and stopped the turn"),
        ),
    }
}

// ----- file changes -----------------------------------------------------------

fn file_change_options() -> Vec<ApprovalOption> {
    let wrap = Decision::FileChange;
    vec![
        ApprovalOption::new(
            "Yes, proceed",
            'y',
            Tone::Positive,
            wrap(FileChangeApprovalDecision::Accept),
        ),
        ApprovalOption::new(
            "Yes, and don't ask again for these files",
            'a',
            Tone::Positive,
            wrap(FileChangeApprovalDecision::AcceptForSession),
        ),
        ApprovalOption::new(
            "No, continue without these changes",
            'd',
            Tone::Negative,
            wrap(FileChangeApprovalDecision::Decline),
        ),
        ApprovalOption::new(
            "No, and tell Codex what to do differently",
            'n',
            Tone::Negative,
            wrap(FileChangeApprovalDecision::Cancel),
        )
        .cancel(),
    ]
}

fn file_change_card(
    params: &FileChangeRequestApprovalParams,
    context: &CardContext<'_>,
) -> CardView {
    let mut card = CardView::choices("Apply these file changes?", file_change_options());
    card.reason = params.reason.clone().unwrap_or_default();
    match context.file_changes {
        Some(changes) if !changes.is_empty() => {
            card.diff = format::diff_lines(changes, context.cwd);
        }
        _ => card
            .details
            .push("The proposed changes are not available yet.".to_string()),
    }
    if let Some(root) = params.grant_root.as_ref() {
        card.details.push(format!(
            "Codex also asks for write access under {} for the rest of this session.",
            format::code_span(&format::display_path(
                &root.to_string_lossy(),
                /*cwd*/ None
            ))
        ));
    }
    card
}

fn file_change_answer(decision: &FileChangeApprovalDecision) -> serde_json::Result<Answer> {
    let (kind, text) = match decision {
        FileChangeApprovalDecision::Accept => (
            NoticeKind::Info,
            "You approved the file changes".to_string(),
        ),
        FileChangeApprovalDecision::AcceptForSession => (
            NoticeKind::Info,
            "You approved the file changes and future changes to these files this session"
                .to_string(),
        ),
        FileChangeApprovalDecision::Decline => (
            NoticeKind::Warning,
            "You declined the file changes".to_string(),
        ),
        FileChangeApprovalDecision::Cancel => (
            NoticeKind::Warning,
            "You canceled the file changes and stopped the turn".to_string(),
        ),
    };
    Answer::new(
        &FileChangeRequestApprovalResponse {
            decision: decision.clone(),
        },
        kind,
        text,
    )
}

// ----- permissions ------------------------------------------------------------

fn permissions_options() -> Vec<ApprovalOption> {
    let wrap = Decision::Permissions;
    vec![
        ApprovalOption::new(
            "Yes, grant these permissions for this turn",
            'y',
            Tone::Positive,
            wrap(PermissionsChoice::GrantForTurn),
        ),
        ApprovalOption::new(
            "Yes, grant for this turn with strict auto review",
            'r',
            Tone::Positive,
            wrap(PermissionsChoice::GrantForTurnWithStrictAutoReview),
        ),
        ApprovalOption::new(
            "Yes, grant these permissions for this session",
            'a',
            Tone::Positive,
            wrap(PermissionsChoice::GrantForSession),
        ),
        ApprovalOption::new(
            "No, continue without permissions",
            'd',
            Tone::Negative,
            wrap(PermissionsChoice::Deny),
        )
        .cancel(),
    ]
}

fn permissions_card(params: &PermissionsRequestApprovalParams) -> CardView {
    let mut card = CardView::choices("Grant additional permissions?", permissions_options());
    card.reason = params.reason.clone().unwrap_or_default();
    match format::permissions_rule(
        params.permissions.network.as_ref(),
        params.permissions.file_system.as_ref(),
    ) {
        Some(rule) => card.details.push(format!("**Permission rule:** {rule}")),
        None => card
            .details
            .push("No specific permissions were listed.".to_string()),
    }
    card.code_caption = format!(
        "in {}",
        format::display_path(params.cwd.as_str(), /*cwd*/ None)
    );
    if let Some(environment) = non_default_environment(params.environment_id.as_deref()) {
        card.details.push(format!(
            "**Environment:** {}",
            format::escape_markdown(environment)
        ));
    }
    card
}

/// Environment worth naming on a card; the default local one is implied.
fn non_default_environment(environment: Option<&str>) -> Option<&str> {
    environment.filter(|environment| !environment.is_empty() && *environment != "local")
}

fn permissions_answer(
    params: &PermissionsRequestApprovalParams,
    choice: PermissionsChoice,
) -> serde_json::Result<Answer> {
    let granted = match choice {
        PermissionsChoice::Deny => GrantedPermissionProfile::default(),
        PermissionsChoice::GrantForTurn
        | PermissionsChoice::GrantForTurnWithStrictAutoReview
        | PermissionsChoice::GrantForSession => GrantedPermissionProfile {
            network: params.permissions.network.clone(),
            file_system: params.permissions.file_system.clone(),
        },
    };
    let scope = if choice == PermissionsChoice::GrantForSession {
        PermissionGrantScope::Session
    } else {
        PermissionGrantScope::Turn
    };
    let strict = choice == PermissionsChoice::GrantForTurnWithStrictAutoReview;
    let (kind, text) = match choice {
        PermissionsChoice::Deny => (
            NoticeKind::Warning,
            "You did not grant additional permissions",
        ),
        PermissionsChoice::GrantForTurnWithStrictAutoReview => (
            NoticeKind::Info,
            "You granted additional permissions with strict auto review",
        ),
        PermissionsChoice::GrantForSession => (
            NoticeKind::Info,
            "You granted additional permissions for this session",
        ),
        PermissionsChoice::GrantForTurn => (NoticeKind::Info, "You granted additional permissions"),
    };
    Answer::new(
        &PermissionsRequestApprovalResponse {
            permissions: granted,
            scope,
            strict_auto_review: strict.then_some(true),
        },
        kind,
        text.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn command_params(extra: Value) -> CommandExecutionRequestApprovalParams {
        let mut base = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "call-1",
            "startedAtMs": 1,
            "command": "/bin/zsh -lc 'git status'",
            "cwd": "/repo",
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        match serde_json::from_value(base) {
            Ok(params) => params,
            Err(err) => panic!("invalid test params: {err}"),
        }
    }

    fn labels(options: &[ApprovalOption]) -> Vec<(String, String)> {
        options
            .iter()
            .map(|option| (option.key_label(), option.label.clone()))
            .collect()
    }

    fn pending(kind: RequestKind) -> PendingRequest {
        PendingRequest {
            request_id: RequestId::Integer(7),
            thread_id: "thread-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            origin: None,
            kind,
        }
    }

    #[test]
    fn command_decisions_prefer_server_list() {
        let params = command_params(json!({"availableDecisions": ["accept", "decline"]}));
        assert_eq!(
            command_decisions(&params),
            vec![
                CommandExecutionApprovalDecision::Accept,
                CommandExecutionApprovalDecision::Decline
            ]
        );
    }

    #[test]
    fn command_decisions_fall_back_like_the_tui() {
        let plain = command_params(json!({"proposedExecpolicyAmendment": ["git", "status"]}));
        assert_eq!(
            command_decisions(&plain),
            vec![
                CommandExecutionApprovalDecision::Accept,
                CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
                    execpolicy_amendment: codex_app_server_protocol::ExecPolicyAmendment {
                        command: vec!["git".to_string(), "status".to_string()],
                    },
                },
                CommandExecutionApprovalDecision::Cancel,
            ]
        );

        let network = command_params(json!({
            "command": null,
            "networkApprovalContext": {"host": "example.com", "protocol": "https"},
            "proposedNetworkPolicyAmendments": [
                {"host": "example.com", "action": "deny"},
                {"host": "example.com", "action": "allow"},
            ],
        }));
        assert_eq!(
            command_decisions(&network),
            vec![
                CommandExecutionApprovalDecision::Accept,
                CommandExecutionApprovalDecision::AcceptForSession,
                CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
                    network_policy_amendment: codex_app_server_protocol::NetworkPolicyAmendment {
                        host: "example.com".to_string(),
                        action: NetworkPolicyRuleAction::Allow,
                    },
                },
                CommandExecutionApprovalDecision::Cancel,
            ]
        );

        let permissions = command_params(json!({
            "additionalPermissions": {"network": {"enabled": true}, "fileSystem": null},
        }));
        assert_eq!(
            command_decisions(&permissions),
            vec![
                CommandExecutionApprovalDecision::Accept,
                CommandExecutionApprovalDecision::Cancel
            ]
        );
    }

    #[test]
    fn command_labels_match_the_tui() {
        let params = command_params(json!({
            "availableDecisions": [
                "accept",
                "acceptForSession",
                {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["git", "status"]}},
                "decline",
                "cancel",
            ],
        }));
        assert_eq!(
            labels(&command_options(&params)),
            vec![
                ("Y".to_string(), "Yes, proceed".to_string()),
                (
                    "A".to_string(),
                    "Yes, and don't ask again for this command in this session".to_string()
                ),
                (
                    "P".to_string(),
                    "Yes, and don't ask again for commands that start with `git status`"
                        .to_string()
                ),
                (
                    "D".to_string(),
                    "No, continue without running it".to_string()
                ),
                (
                    "Esc".to_string(),
                    "No, and tell Codex what to do differently".to_string()
                ),
            ]
        );
    }

    #[test]
    fn network_labels_and_hidden_multiline_prefix() {
        let network = command_params(json!({
            "command": null,
            "networkApprovalContext": {"host": "example.com", "protocol": "https"},
            "availableDecisions": [
                "accept",
                "acceptForSession",
                {"applyNetworkPolicyAmendment": {"network_policy_amendment": {"host": "example.com", "action": "allow"}}},
                {"applyNetworkPolicyAmendment": {"network_policy_amendment": {"host": "example.com", "action": "deny"}}},
            ],
        }));
        assert_eq!(
            labels(&command_options(&network)),
            vec![
                ("Y".to_string(), "Yes, just this once".to_string()),
                (
                    "A".to_string(),
                    "Yes, and allow this host for this conversation".to_string()
                ),
                (
                    "P".to_string(),
                    "Yes, and allow this host in the future".to_string()
                ),
                (
                    "D".to_string(),
                    "No, and block this host in the future".to_string()
                ),
            ]
        );

        let multiline = command_params(json!({
            "availableDecisions": [
                {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["bash", "-lc", "echo a\necho b"]}},
                "cancel",
            ],
        }));
        assert_eq!(
            labels(&command_options(&multiline)),
            vec![(
                "Esc".to_string(),
                "No, and tell Codex what to do differently".to_string()
            )]
        );

        let permissions = command_params(json!({
            "additionalPermissions": {"network": {"enabled": true}, "fileSystem": null},
            "availableDecisions": ["acceptForSession"],
        }));
        assert_eq!(
            labels(&command_options(&permissions)),
            vec![(
                "A".to_string(),
                "Yes, and allow these permissions for this session".to_string()
            )]
        );
    }

    #[test]
    fn command_responses_keep_snake_case_payloads() -> serde_json::Result<()> {
        let request = pending(RequestKind::Command(Box::new(command_params(json!({})))));
        let cases = vec![
            (
                CommandExecutionApprovalDecision::Accept,
                json!({"decision": "accept"}),
            ),
            (
                CommandExecutionApprovalDecision::AcceptForSession,
                json!({"decision": "acceptForSession"}),
            ),
            (
                CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
                    execpolicy_amendment: codex_app_server_protocol::ExecPolicyAmendment {
                        command: vec!["git".to_string(), "status".to_string()],
                    },
                },
                json!({"decision": {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["git", "status"]}}}),
            ),
            (
                CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
                    network_policy_amendment: codex_app_server_protocol::NetworkPolicyAmendment {
                        host: "example.com".to_string(),
                        action: NetworkPolicyRuleAction::Deny,
                    },
                },
                json!({"decision": {"applyNetworkPolicyAmendment": {"network_policy_amendment": {"host": "example.com", "action": "deny"}}}}),
            ),
            (
                CommandExecutionApprovalDecision::Decline,
                json!({"decision": "decline"}),
            ),
            (
                CommandExecutionApprovalDecision::Cancel,
                json!({"decision": "cancel"}),
            ),
        ];
        for (decision, expected) in cases {
            let answer = request.answer(&Decision::Command(decision))?;
            assert_eq!(answer.result, expected);
        }
        Ok(())
    }

    #[test]
    fn command_notices_describe_the_decision() -> serde_json::Result<()> {
        let request = pending(RequestKind::Command(Box::new(command_params(json!({})))));
        let accept =
            request.answer(&Decision::Command(CommandExecutionApprovalDecision::Accept))?;
        assert_eq!(
            accept.notice,
            Some((
                NoticeKind::Info,
                "You approved Codex to run `git status` this time".to_string()
            ))
        );
        let decline = request.answer(&Decision::Command(
            CommandExecutionApprovalDecision::Decline,
        ))?;
        assert_eq!(
            decline.notice,
            Some((
                NoticeKind::Warning,
                "You did not approve Codex to run `git status`".to_string()
            ))
        );
        Ok(())
    }

    #[test]
    fn command_card_shows_script_cwd_and_reason() {
        let params = command_params(json!({
            "reason": "Needs to inspect the repo",
            "environmentId": "remote-1",
        }));
        assert_eq!(non_default_environment(Some("local")), None);
        let card = command_card(&params);
        assert_eq!(card.title, "Run this command?");
        assert_eq!(card.code, "$ git status");
        assert_eq!(card.code_caption, "in /repo");
        assert_eq!(card.reason, "Needs to inspect the repo");
        assert_eq!(
            card.details,
            vec!["**Environment:** remote\\-1".to_string()]
        );

        let stdin = command_params(json!({"kind": "writeStdin", "command": "y"}));
        let card = command_card(&stdin);
        assert_eq!(card.title, "Send input to the running command?");
        assert_eq!(card.code, "\"y\"");
    }

    #[test]
    fn file_change_responses_and_card() -> serde_json::Result<()> {
        let params: FileChangeRequestApprovalParams = serde_json::from_value(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "patch-1",
            "startedAtMs": 1,
            "reason": null,
            "grantRoot": "/repo",
        }))?;
        let request = pending(RequestKind::FileChange(params));
        assert_eq!(
            labels(&request.options()),
            vec![
                ("Y".to_string(), "Yes, proceed".to_string()),
                (
                    "A".to_string(),
                    "Yes, and don't ask again for these files".to_string()
                ),
                (
                    "D".to_string(),
                    "No, continue without these changes".to_string()
                ),
                (
                    "Esc".to_string(),
                    "No, and tell Codex what to do differently".to_string()
                ),
            ]
        );
        for (decision, wire) in [
            (FileChangeApprovalDecision::Accept, "accept"),
            (
                FileChangeApprovalDecision::AcceptForSession,
                "acceptForSession",
            ),
            (FileChangeApprovalDecision::Decline, "decline"),
            (FileChangeApprovalDecision::Cancel, "cancel"),
        ] {
            let answer = request.answer(&Decision::FileChange(decision))?;
            assert_eq!(answer.result, json!({"decision": wire}));
        }

        let changes = vec![FileUpdateChange {
            path: "/repo/a.txt".to_string(),
            kind: codex_app_server_protocol::PatchChangeKind::Add,
            diff: "hello\n".to_string(),
        }];
        let card = request.card(&CardContext {
            cwd: Some(Path::new("/repo")),
            file_changes: Some(&changes),
        });
        assert_eq!(
            card.diff
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Added a.txt (+1 -0)", "+hello"]
        );
        assert_eq!(
            card.details,
            vec![
                "Codex also asks for write access under `/repo` for the rest of this session."
                    .to_string()
            ]
        );
        Ok(())
    }

    #[test]
    fn permission_responses_echo_or_clear_the_grant() -> serde_json::Result<()> {
        let params: PermissionsRequestApprovalParams = serde_json::from_value(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "perm-1",
            "startedAtMs": 1,
            "cwd": "/repo",
            "reason": "Needs network",
            "permissions": {"network": {"enabled": true}, "fileSystem": null},
        }))?;
        let request = pending(RequestKind::Permissions(params));
        let turn = request.answer(&Decision::Permissions(PermissionsChoice::GrantForTurn))?;
        assert_eq!(
            turn.result,
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"})
        );
        let strict = request.answer(&Decision::Permissions(
            PermissionsChoice::GrantForTurnWithStrictAutoReview,
        ))?;
        assert_eq!(
            strict.result,
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn", "strictAutoReview": true})
        );
        let session = request.answer(&Decision::Permissions(PermissionsChoice::GrantForSession))?;
        assert_eq!(
            session.result,
            json!({"permissions": {"network": {"enabled": true}}, "scope": "session"})
        );
        let deny = request.answer(&Decision::Permissions(PermissionsChoice::Deny))?;
        assert_eq!(deny.result, json!({"permissions": {}, "scope": "turn"}));
        assert_eq!(
            deny.notice,
            Some((
                NoticeKind::Warning,
                "You did not grant additional permissions".to_string()
            ))
        );
        assert_eq!(
            request.cancel_option().map(|option| option.decision),
            Some(Decision::Permissions(PermissionsChoice::Deny))
        );
        let card = request.card(&CardContext {
            cwd: None,
            file_changes: None,
        });
        assert_eq!(
            card.details,
            vec!["**Permission rule:** network".to_string()]
        );
        Ok(())
    }

    #[test]
    fn mismatched_decisions_are_errors() {
        let request = pending(RequestKind::Command(Box::new(command_params(json!({})))));
        assert!(
            request
                .answer(&Decision::FileChange(FileChangeApprovalDecision::Accept))
                .is_err()
        );
    }

    #[test]
    fn intake_answers_time_and_rejects_legacy_requests() -> serde_json::Result<()> {
        let time: ServerRequest = serde_json::from_value(json!({
            "method": "currentTime/read",
            "id": 3,
            "params": {"threadId": "thread-1"},
        }))?;
        match intake(time, 1_700_000_000) {
            Intake::Resolve {
                result, request_id, ..
            } => {
                assert_eq!(request_id, RequestId::Integer(3));
                assert_eq!(result, json!({"currentTimeAt": 1_700_000_000}));
            }
            other => panic!("unexpected intake: {other:?}"),
        }

        let legacy: ServerRequest = serde_json::from_value(json!({
            "method": "execCommandApproval",
            "id": 4,
            "params": {
                "conversationId": "67e55044-10b1-426f-9247-bb680e5fe0c8",
                "callId": "c",
                "command": ["ls"],
                "cwd": "/repo",
                "reason": null,
                "parsedCmd": [],
            },
        }))?;
        match intake(legacy, 0) {
            Intake::Reject { error, .. } => {
                assert_eq!(error.code, UNSUPPORTED_ERROR_CODE);
                assert!(error.message.contains("Legacy command approval"));
            }
            other => panic!("unexpected intake: {other:?}"),
        }

        let attestation: ServerRequest = serde_json::from_value(json!({
            "method": "attestation/generate",
            "id": 5,
            "params": {},
        }))?;
        assert!(matches!(intake(attestation, 0), Intake::Reject { .. }));
        Ok(())
    }
}
