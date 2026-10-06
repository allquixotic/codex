//! MCP elicitations (`mcpServer/elicitation/request`).
//!
//! Message-only requests become approval choices (with the persist options the
//! request advertises in `_meta`), real forms are rendered from the typed
//! schema and validated before submitting, and URL requests offer to open the
//! link. Modes the TUI cannot show either are declined.

use codex_app_server_protocol::McpElicitationEnumSchema;
use codex_app_server_protocol::McpElicitationMultiSelectEnumSchema;
use codex_app_server_protocol::McpElicitationNumberType;
use codex_app_server_protocol::McpElicitationPrimitiveSchema;
use codex_app_server_protocol::McpElicitationSchema;
use codex_app_server_protocol::McpElicitationSingleSelectEnumSchema;
use codex_app_server_protocol::McpElicitationStringFormat;
use codex_app_server_protocol::McpServerElicitationAction;
use codex_app_server_protocol::McpServerElicitationRequest;
use codex_app_server_protocol::McpServerElicitationRequestParams;
use codex_app_server_protocol::McpServerElicitationRequestResponse;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_KEY;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_MCP_TOOL_CALL;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_TOOL_SUGGESTION;
use codex_protocol::mcp_approval_meta::PERSIST_ALWAYS;
use codex_protocol::mcp_approval_meta::PERSIST_KEY;
use codex_protocol::mcp_approval_meta::PERSIST_SESSION;
use codex_protocol::mcp_approval_meta::TOOL_NAME_KEY;
use codex_protocol::mcp_approval_meta::TOOL_PARAMS_DISPLAY_KEY;
use codex_protocol::mcp_approval_meta::TOOL_PARAMS_KEY;
use codex_protocol::mcp_approval_meta::TOOL_TITLE_KEY;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use super::format;
use super::request::Answer;
use super::request::ApprovalOption;
use super::request::CardView;
use super::request::ChoiceView;
use super::request::Decision;
use super::request::FieldKind;
use super::request::FieldView;
use super::request::FormView;
use super::request::Tone;
use crate::transcript::NoticeKind;

/// Tool parameters listed on a tool-call approval card.
const MAX_DISPLAY_PARAMS: usize = 6;
const MAX_PARAM_VALUE_CHARS: usize = 120;
/// Longest non-openable URL shown as text on a card.
const MAX_SHOWN_URL_CHARS: usize = 300;

#[derive(Clone, Debug)]
pub(crate) struct Elicitation {
    server_name: String,
    message: String,
    mode: Mode,
}

#[derive(Clone, Debug)]
enum Mode {
    /// Empty schema: an approval with the persist choices the server allows.
    Approval {
        tool_call: bool,
        tool_name: Option<String>,
        persist_session: bool,
        persist_always: bool,
        params: Vec<(String, String)>,
    },
    /// Install or enable a suggested connector or plugin in the browser.
    Suggestion {
        tool_name: String,
        install: bool,
        reason: String,
        url: String,
    },
    Form {
        fields: Vec<FormField>,
        error: String,
    },
    Url {
        url: String,
        openable: bool,
    },
}

/// A request mode codex-gui declines automatically (like the TUI).
#[derive(Debug)]
pub(crate) struct Unsupported {
    server_name: String,
    mode: &'static str,
}

impl Unsupported {
    pub(crate) fn decline_response(&self) -> McpServerElicitationRequestResponse {
        McpServerElicitationRequestResponse {
            action: McpServerElicitationAction::Decline,
            content: None,
            meta: None,
        }
    }

    pub(crate) fn notice(&self) -> String {
        format!(
            "Declined a {} request from {} that codex-gui cannot show",
            self.mode, self.server_name
        )
    }
}

impl Elicitation {
    pub(crate) fn from_params(
        params: McpServerElicitationRequestParams,
    ) -> Result<Self, Unsupported> {
        let server_name = params.server_name;
        let unsupported = |mode| Unsupported {
            server_name: server_name.clone(),
            mode,
        };
        let (message, mode) = match params.request {
            McpServerElicitationRequest::Form {
                meta,
                message,
                requested_schema,
            } => {
                let mode = if requested_schema.properties.is_empty() {
                    message_only_mode(meta.as_ref())
                } else {
                    Mode::Form {
                        fields: form_fields(&requested_schema),
                        error: String::new(),
                    }
                };
                (message, mode)
            }
            McpServerElicitationRequest::Url { message, url, .. } => {
                let openable = format::is_openable_url(&url);
                (message, Mode::Url { url, openable })
            }
            McpServerElicitationRequest::UserVerification { .. } => {
                return Err(unsupported("device verification"));
            }
            McpServerElicitationRequest::OpenAiForm { .. }
            | McpServerElicitationRequest::OpenAiElicitationForm { .. } => {
                return Err(unsupported("form"));
            }
        };
        Ok(Self {
            server_name,
            message,
            mode,
        })
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.server_name
    }

    pub(crate) fn is_form(&self) -> bool {
        matches!(self.mode, Mode::Form { .. })
    }

    pub(crate) fn options(&self) -> Vec<ApprovalOption> {
        let choice = |label: &str, key: char, tone, action, meta, open_url: Option<&String>| {
            approval_option(label, key, tone, action, meta, open_url.cloned())
        };
        match &self.mode {
            Mode::Approval {
                tool_call,
                persist_session,
                persist_always,
                ..
            } => {
                let mut options = vec![choice(
                    "Allow",
                    'y',
                    Tone::Positive,
                    McpServerElicitationAction::Accept,
                    None,
                    None,
                )];
                if *persist_session {
                    options.push(choice(
                        "Allow for this session",
                        'a',
                        Tone::Positive,
                        McpServerElicitationAction::Accept,
                        Some(json!({ PERSIST_KEY: PERSIST_SESSION })),
                        None,
                    ));
                }
                if *persist_always {
                    options.push(choice(
                        "Always allow",
                        'p',
                        Tone::Positive,
                        McpServerElicitationAction::Accept,
                        Some(json!({ PERSIST_KEY: PERSIST_ALWAYS })),
                        None,
                    ));
                }
                if !*tool_call {
                    options.push(choice(
                        "Deny",
                        'd',
                        Tone::Negative,
                        McpServerElicitationAction::Decline,
                        None,
                        None,
                    ));
                }
                options.push(cancel_option());
                options
            }
            Mode::Suggestion { install, url, .. } => vec![
                choice(
                    if *install {
                        "Open the install page and continue"
                    } else {
                        "Open the page and continue"
                    },
                    'y',
                    Tone::Positive,
                    McpServerElicitationAction::Accept,
                    None,
                    Some(url),
                ),
                approval_option(
                    "Not now",
                    'n',
                    Tone::Negative,
                    McpServerElicitationAction::Decline,
                    None,
                    None,
                )
                .cancel(),
            ],
            Mode::Url { url, openable } => {
                let mut options = Vec::new();
                if *openable {
                    options.push(choice(
                        "Open the link and continue",
                        'y',
                        Tone::Positive,
                        McpServerElicitationAction::Accept,
                        None,
                        Some(url),
                    ));
                }
                options.push(choice(
                    "Decline",
                    'd',
                    Tone::Negative,
                    McpServerElicitationAction::Decline,
                    None,
                    None,
                ));
                options.push(cancel_option());
                options
            }
            Mode::Form { .. } => Vec::new(),
        }
    }

    pub(crate) fn card(&self) -> CardView {
        let server = &self.server_name;
        match &self.mode {
            Mode::Approval {
                tool_call,
                tool_name,
                params,
                ..
            } => {
                let title = match (tool_call, tool_name) {
                    (true, Some(tool)) => format!("Allow {server} to run {tool}?"),
                    (true, None) => format!("Allow this {server} tool call?"),
                    (false, _) => format!("Allow this {server} request?"),
                };
                let mut card = CardView::choices(title, self.options());
                card.reason = self.message.trim().to_string();
                card.details = params
                    .iter()
                    .map(|(name, value)| {
                        format!(
                            "**{}:** {}",
                            format::escape_markdown(name),
                            format::escape_markdown(value)
                        )
                    })
                    .collect();
                card
            }
            Mode::Suggestion {
                tool_name,
                install,
                reason,
                url,
            } => {
                let verb = if *install { "Install" } else { "Enable" };
                let mut card = CardView::choices(format!("{verb} {tool_name}?"), self.options());
                card.reason = [reason.trim(), self.message.trim()]
                    .into_iter()
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                card.link = url.clone();
                card
            }
            Mode::Url { url, openable } => {
                let mut card =
                    CardView::choices(format!("{server} wants you to open a link"), self.options());
                card.reason = self.message.trim().to_string();
                if *openable {
                    card.link = url.clone();
                } else {
                    // Shown as text only: the Open button would hand any
                    // scheme to the OS handler.
                    card.details.push(format!(
                        "**Link:** {}",
                        format::escape_markdown(&crate::app::truncate_chars(
                            url,
                            MAX_SHOWN_URL_CHARS
                        ))
                    ));
                    card.details
                        .push("Only http and https links can be opened.".to_string());
                }
                card
            }
            Mode::Form { fields, error } => {
                let mut card = CardView::form(
                    format!("{server} needs some information"),
                    FormView {
                        fields: fields.iter().map(FormField::view).collect(),
                        submit_label: "Submit".to_string(),
                        secondary_label: "Decline".to_string(),
                        tertiary_label: "Cancel".to_string(),
                        error: error.clone(),
                    },
                );
                card.reason = self.message.trim().to_string();
                card
            }
        }
    }

    pub(crate) fn field(&self, index: usize) -> Option<FieldView> {
        match &self.mode {
            Mode::Form { fields, .. } => fields.get(index).map(FormField::view),
            _ => None,
        }
    }

    /// Toggles choice `choice` of field `field`; returns whether it changed.
    /// Editing a field clears its validation error.
    pub(crate) fn toggle_choice(&mut self, field: usize, choice: usize) -> bool {
        let Mode::Form { fields, error } = &mut self.mode else {
            return false;
        };
        let Some(field) = fields.get_mut(field) else {
            return false;
        };
        if !field.toggle(choice) {
            return false;
        }
        field.error = None;
        if fields.iter().all(|field| field.error.is_none()) {
            error.clear();
        }
        true
    }

    /// Stores typed text; returns true when that cleared a validation error
    /// (so the view needs updating).
    pub(crate) fn set_text(&mut self, field: usize, text: String) -> bool {
        let Mode::Form { fields, error } = &mut self.mode else {
            return false;
        };
        let Some(FormField {
            value: FieldValue::Text(value),
            error: field_error,
            ..
        }) = fields.get_mut(field)
        else {
            return false;
        };
        *value = text;
        if field_error.take().is_none() {
            return false;
        }
        if fields.iter().all(|field| field.error.is_none()) {
            error.clear();
        }
        true
    }

    /// Form-level error shown next to the buttons.
    pub(crate) fn form_error(&self) -> &str {
        match &self.mode {
            Mode::Form { error, .. } => error,
            _ => "",
        }
    }

    /// Validates the form and builds the accept response. On failure the
    /// field errors are stored for display and `None` is returned.
    pub(crate) fn submit(&mut self) -> Option<serde_json::Result<Answer>> {
        let server = self.server_name.clone();
        let Mode::Form { fields, error } = &mut self.mode else {
            return None;
        };
        match form_content(fields) {
            Ok(content) => {
                error.clear();
                for field in fields.iter_mut() {
                    field.error = None;
                }
                Some(answer(
                    McpServerElicitationAction::Accept,
                    Some(Value::Object(content)),
                    None,
                    None,
                    format!("You sent the requested information to {server}"),
                ))
            }
            Err(errors) => {
                for (index, field) in fields.iter_mut().enumerate() {
                    field.error = errors
                        .iter()
                        .find(|(error_index, _)| *error_index == index)
                        .map(|(_, message)| message.clone());
                }
                *error = "Fix the highlighted fields to continue.".to_string();
                None
            }
        }
    }

    /// Decline or cancel without content (form buttons and Esc).
    pub(crate) fn dismiss_answer(
        &self,
        action: McpServerElicitationAction,
    ) -> serde_json::Result<Answer> {
        self.choice_answer(action, /*meta*/ None, /*open_url*/ None)
    }

    pub(crate) fn choice_answer(
        &self,
        action: McpServerElicitationAction,
        meta: Option<Value>,
        open_url: Option<String>,
    ) -> serde_json::Result<Answer> {
        let server = &self.server_name;
        let persist = meta
            .as_ref()
            .and_then(|meta| meta.get(PERSIST_KEY))
            .and_then(Value::as_str);
        let text = match (action, persist, &open_url) {
            (McpServerElicitationAction::Accept, _, Some(_)) => {
                format!("You opened the link from {server}")
            }
            (McpServerElicitationAction::Accept, Some(PERSIST_SESSION), None) => {
                format!("You allowed the {server} request for this session")
            }
            (McpServerElicitationAction::Accept, Some(PERSIST_ALWAYS), None) => {
                format!("You always allow this {server} request")
            }
            (McpServerElicitationAction::Accept, _, None) => {
                format!("You allowed the {server} request")
            }
            (McpServerElicitationAction::Decline, _, _) => {
                format!("You declined the {server} request")
            }
            (McpServerElicitationAction::Cancel, _, _) => {
                format!("You canceled the {server} request")
            }
        };
        answer(action, /*content*/ None, meta, open_url, text)
    }
}

fn answer(
    action: McpServerElicitationAction,
    content: Option<Value>,
    meta: Option<Value>,
    open_url: Option<String>,
    text: String,
) -> serde_json::Result<Answer> {
    let kind = match action {
        McpServerElicitationAction::Accept => NoticeKind::Info,
        McpServerElicitationAction::Decline | McpServerElicitationAction::Cancel => {
            NoticeKind::Warning
        }
    };
    Ok(Answer {
        result: serde_json::to_value(McpServerElicitationRequestResponse {
            action,
            content,
            meta,
        })?,
        notice: Some((kind, text)),
        open_url,
    })
}

fn approval_option(
    label: &str,
    key: char,
    tone: Tone,
    action: McpServerElicitationAction,
    meta: Option<Value>,
    open_url: Option<String>,
) -> ApprovalOption {
    ApprovalOption::new(
        label,
        key,
        tone,
        Decision::Elicitation {
            action,
            meta,
            open_url,
        },
    )
}

fn cancel_option() -> ApprovalOption {
    approval_option(
        "Cancel",
        'n',
        Tone::Negative,
        McpServerElicitationAction::Cancel,
        None,
        None,
    )
    .cancel()
}

fn meta_str<'a>(meta: Option<&'a Value>, key: &str) -> Option<&'a str> {
    meta?.get(key)?.as_str()
}

fn supports_persist(meta: Option<&Value>, mode: &str) -> bool {
    match meta.and_then(|meta| meta.get(PERSIST_KEY)) {
        Some(Value::String(value)) => value == mode,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(mode)),
        _ => false,
    }
}

fn message_only_mode(meta: Option<&Value>) -> Mode {
    let kind = meta_str(meta, APPROVAL_KIND_KEY);
    if kind == Some(APPROVAL_KIND_TOOL_SUGGESTION)
        && let Some(suggestion) = suggestion_mode(meta)
    {
        return suggestion;
    }
    let tool_call = kind == Some(APPROVAL_KIND_MCP_TOOL_CALL);
    Mode::Approval {
        tool_call,
        tool_name: meta_str(meta, TOOL_TITLE_KEY)
            .or_else(|| meta_str(meta, TOOL_NAME_KEY))
            .filter(|name| !name.trim().is_empty())
            .map(ToString::to_string),
        persist_session: supports_persist(meta, PERSIST_SESSION),
        persist_always: supports_persist(meta, PERSIST_ALWAYS),
        params: if tool_call {
            display_params(meta)
        } else {
            Vec::new()
        },
    }
}

fn suggestion_mode(meta: Option<&Value>) -> Option<Mode> {
    let url = meta_str(meta, "install_url").filter(|url| format::is_openable_url(url))?;
    let install = match meta_str(meta, "suggest_type")? {
        "install" => true,
        "enable" => false,
        _ => return None,
    };
    Some(Mode::Suggestion {
        tool_name: meta_str(meta, TOOL_NAME_KEY)?.to_string(),
        install,
        reason: meta_str(meta, "suggest_reason")
            .unwrap_or_default()
            .to_string(),
        url: url.to_string(),
    })
}

/// Parameters of a tool-call approval: the server's display list, else the
/// raw parameters sorted by name (same precedence as the TUI).
fn display_params(meta: Option<&Value>) -> Vec<(String, String)> {
    let display = meta
        .and_then(|meta| meta.get(TOOL_PARAMS_DISPLAY_KEY))
        .and_then(Value::as_array)
        .map(|params| {
            params
                .iter()
                .filter_map(|param| {
                    let name = param.get("name")?.as_str()?.trim();
                    let display_name = param
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(name)
                        .trim();
                    if name.is_empty() || display_name.is_empty() {
                        return None;
                    }
                    let value = param.get("value")?;
                    Some((display_name.to_string(), param_value(value)))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut params = if display.is_empty() {
        let mut raw = meta
            .and_then(|meta| meta.get(TOOL_PARAMS_KEY))
            .and_then(Value::as_object)
            .map(|params| {
                params
                    .iter()
                    .map(|(name, value)| (name.clone(), param_value(value)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        raw.sort_by(|left, right| left.0.cmp(&right.0));
        raw
    } else {
        display
    };
    params.truncate(MAX_DISPLAY_PARAMS);
    params
}

fn param_value(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.split_whitespace().collect::<Vec<_>>().join(" "),
        other => other.to_string(),
    };
    crate::app::truncate_chars(&text, MAX_PARAM_VALUE_CHARS)
}

// ----- forms --------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct EnumChoice {
    value: String,
    label: String,
}

#[derive(Clone, Debug, PartialEq)]
enum FieldInput {
    Text {
        min_length: Option<u32>,
        max_length: Option<u32>,
        format: Option<McpElicitationStringFormat>,
    },
    Number {
        integer: bool,
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Boolean,
    Single(Vec<EnumChoice>),
    Multi {
        choices: Vec<EnumChoice>,
        min_items: Option<u64>,
        max_items: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum FieldValue {
    Text(String),
    Bool(bool),
    Single(Option<usize>),
    Multi(Vec<bool>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FormField {
    id: String,
    label: String,
    description: Option<String>,
    required: bool,
    input: FieldInput,
    value: FieldValue,
    error: Option<String>,
}

fn form_fields(schema: &McpElicitationSchema) -> Vec<FormField> {
    let required = schema.required.clone().unwrap_or_default();
    schema
        .properties
        .iter()
        .map(|(id, property)| form_field(id, property, required.contains(id)))
        .collect()
}

fn enum_choices(values: &[String], names: Option<&Vec<String>>) -> Vec<EnumChoice> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| EnumChoice {
            value: value.clone(),
            label: names
                .and_then(|names| names.get(index))
                .cloned()
                .unwrap_or_else(|| value.clone()),
        })
        .collect()
}

fn titled_choices(
    options: &[codex_app_server_protocol::McpElicitationConstOption],
) -> Vec<EnumChoice> {
    options
        .iter()
        .map(|option| EnumChoice {
            value: option.const_.clone(),
            label: option.title.clone(),
        })
        .collect()
}

fn single(choices: Vec<EnumChoice>, default: Option<&String>) -> (FieldInput, FieldValue) {
    let selected =
        default.and_then(|default| choices.iter().position(|choice| &choice.value == default));
    (FieldInput::Single(choices), FieldValue::Single(selected))
}

fn format_number(value: f64, integer: bool) -> String {
    if integer && value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn form_field(id: &str, property: &McpElicitationPrimitiveSchema, required: bool) -> FormField {
    let (title, description, input, value) = match property {
        McpElicitationPrimitiveSchema::String(schema) => (
            schema.title.clone(),
            schema.description.clone(),
            FieldInput::Text {
                min_length: schema.min_length,
                max_length: schema.max_length,
                format: schema.format,
            },
            FieldValue::Text(schema.default.clone().unwrap_or_default()),
        ),
        McpElicitationPrimitiveSchema::Number(schema) => {
            let integer = schema.type_ == McpElicitationNumberType::Integer;
            (
                schema.title.clone(),
                schema.description.clone(),
                FieldInput::Number {
                    integer,
                    minimum: schema.minimum,
                    maximum: schema.maximum,
                },
                FieldValue::Text(
                    schema
                        .default
                        .map(|value| format_number(value, integer))
                        .unwrap_or_default(),
                ),
            )
        }
        McpElicitationPrimitiveSchema::Boolean(schema) => (
            schema.title.clone(),
            schema.description.clone(),
            FieldInput::Boolean,
            FieldValue::Bool(schema.default.unwrap_or(false)),
        ),
        McpElicitationPrimitiveSchema::Enum(McpElicitationEnumSchema::Legacy(schema)) => {
            let (input, value) = single(
                enum_choices(&schema.enum_, schema.enum_names.as_ref()),
                schema.default.as_ref(),
            );
            (
                schema.title.clone(),
                schema.description.clone(),
                input,
                value,
            )
        }
        McpElicitationPrimitiveSchema::Enum(McpElicitationEnumSchema::SingleSelect(
            McpElicitationSingleSelectEnumSchema::Untitled(schema),
        )) => {
            let (input, value) = single(
                enum_choices(&schema.enum_, /*names*/ None),
                schema.default.as_ref(),
            );
            (
                schema.title.clone(),
                schema.description.clone(),
                input,
                value,
            )
        }
        McpElicitationPrimitiveSchema::Enum(McpElicitationEnumSchema::SingleSelect(
            McpElicitationSingleSelectEnumSchema::Titled(schema),
        )) => {
            let (input, value) = single(titled_choices(&schema.one_of), schema.default.as_ref());
            (
                schema.title.clone(),
                schema.description.clone(),
                input,
                value,
            )
        }
        McpElicitationPrimitiveSchema::Enum(McpElicitationEnumSchema::MultiSelect(multi)) => {
            let (title, description, choices, min_items, max_items, default) = match multi {
                McpElicitationMultiSelectEnumSchema::Untitled(schema) => (
                    schema.title.clone(),
                    schema.description.clone(),
                    enum_choices(&schema.items.enum_, /*names*/ None),
                    schema.min_items,
                    schema.max_items,
                    schema.default.clone().unwrap_or_default(),
                ),
                McpElicitationMultiSelectEnumSchema::Titled(schema) => (
                    schema.title.clone(),
                    schema.description.clone(),
                    titled_choices(&schema.items.any_of),
                    schema.min_items,
                    schema.max_items,
                    schema.default.clone().unwrap_or_default(),
                ),
            };
            let checked = choices
                .iter()
                .map(|choice| default.contains(&choice.value))
                .collect();
            (
                title,
                description,
                FieldInput::Multi {
                    choices,
                    min_items,
                    max_items,
                },
                FieldValue::Multi(checked),
            )
        }
    };
    FormField {
        id: id.to_string(),
        label: title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| id.to_string()),
        description: description.filter(|description| !description.trim().is_empty()),
        required,
        input,
        value,
        error: None,
    }
}

impl FormField {
    fn toggle(&mut self, choice: usize) -> bool {
        match (&self.input, &mut self.value) {
            (FieldInput::Single(choices), FieldValue::Single(selected)) => {
                if choice >= choices.len() || *selected == Some(choice) {
                    return false;
                }
                *selected = Some(choice);
                true
            }
            (FieldInput::Multi { .. }, FieldValue::Multi(checked)) => match checked.get_mut(choice)
            {
                Some(value) => {
                    *value = !*value;
                    true
                }
                None => false,
            },
            (FieldInput::Boolean, FieldValue::Bool(value)) if choice == 0 => {
                *value = !*value;
                true
            }
            _ => false,
        }
    }

    fn placeholder(&self) -> String {
        match &self.input {
            FieldInput::Text {
                min_length,
                max_length,
                format,
            } => {
                let base = match format {
                    Some(McpElicitationStringFormat::Email) => "name@example.com",
                    Some(McpElicitationStringFormat::Uri) => "https://…",
                    Some(McpElicitationStringFormat::Date) => "YYYY-MM-DD",
                    Some(McpElicitationStringFormat::DateTime) => "YYYY-MM-DDThh:mm:ssZ",
                    None => "",
                };
                let length = match (min_length, max_length) {
                    (Some(min), Some(max)) => format!("{min}–{max} characters"),
                    (Some(min), None) => format!("at least {min} characters"),
                    (None, Some(max)) => format!("at most {max} characters"),
                    (None, None) => String::new(),
                };
                match (base, length.as_str()) {
                    ("", "") => "Type your answer".to_string(),
                    ("", length) => capitalize(length),
                    (base, "") => base.to_string(),
                    (base, length) => format!("{base} ({length})"),
                }
            }
            FieldInput::Number {
                integer,
                minimum,
                maximum,
            } => {
                let noun = if *integer { "Whole number" } else { "Number" };
                let fmt = |value: f64| format_number(value, *integer);
                match (minimum, maximum) {
                    (Some(min), Some(max)) => format!("{noun} from {} to {}", fmt(*min), fmt(*max)),
                    (Some(min), None) => format!("{noun}, at least {}", fmt(*min)),
                    (None, Some(max)) => format!("{noun}, at most {}", fmt(*max)),
                    (None, None) => noun.to_string(),
                }
            }
            FieldInput::Boolean | FieldInput::Single(_) | FieldInput::Multi { .. } => String::new(),
        }
    }

    fn view(&self) -> FieldView {
        let label = if self.required {
            format!("{} *", self.label)
        } else {
            self.label.clone()
        };
        if let (FieldInput::Boolean, FieldValue::Bool(value)) = (&self.input, &self.value) {
            // A labeled checkbox reads better than a header plus "Yes".
            return FieldView {
                kind: FieldKind::Boolean,
                header: String::new(),
                prompt: String::new(),
                required: self.required,
                choices: vec![ChoiceView {
                    label,
                    description: self.description.clone().unwrap_or_default(),
                    checked: *value,
                }],
                show_text: false,
                text: String::new(),
                placeholder: String::new(),
                secret: false,
                error: self.error.clone().unwrap_or_default(),
            };
        }
        let header = label;
        let (kind, choices, show_text, text) = match (&self.input, &self.value) {
            (FieldInput::Text { .. }, FieldValue::Text(text)) => {
                (FieldKind::Text, Vec::new(), true, text.clone())
            }
            (FieldInput::Number { .. }, FieldValue::Text(text)) => {
                (FieldKind::Number, Vec::new(), true, text.clone())
            }
            (FieldInput::Single(choices), FieldValue::Single(selected)) => (
                FieldKind::SingleChoice,
                choices
                    .iter()
                    .enumerate()
                    .map(|(index, choice)| ChoiceView {
                        label: choice.label.clone(),
                        description: String::new(),
                        checked: *selected == Some(index),
                    })
                    .collect(),
                false,
                String::new(),
            ),
            (FieldInput::Multi { choices, .. }, FieldValue::Multi(checked)) => (
                FieldKind::MultiChoice,
                choices
                    .iter()
                    .zip(checked)
                    .map(|(choice, checked)| ChoiceView {
                        label: choice.label.clone(),
                        description: String::new(),
                        checked: *checked,
                    })
                    .collect(),
                false,
                String::new(),
            ),
            _ => (FieldKind::Text, Vec::new(), true, String::new()),
        };
        FieldView {
            kind,
            header,
            prompt: self.description.clone().unwrap_or_default(),
            required: self.required,
            choices,
            show_text,
            text,
            placeholder: self.placeholder(),
            secret: false,
            error: self.error.clone().unwrap_or_default(),
        }
    }

    /// The JSON value to submit, `Ok(None)` when an optional field is empty.
    fn content_value(&self) -> Result<Option<Value>, String> {
        match (&self.input, &self.value) {
            (
                FieldInput::Text {
                    min_length,
                    max_length,
                    format,
                },
                FieldValue::Text(text),
            ) => {
                let text = text.trim();
                if text.is_empty() {
                    return if self.required {
                        Err("This field is required.".to_string())
                    } else {
                        Ok(None)
                    };
                }
                let length = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
                if let Some(min) = min_length
                    && length < *min
                {
                    return Err(format!("Enter at least {min} characters."));
                }
                if let Some(max) = max_length
                    && length > *max
                {
                    return Err(format!("Enter at most {max} characters."));
                }
                if let Some(format) = format {
                    validate_format(text, *format)?;
                }
                Ok(Some(Value::String(text.to_string())))
            }
            (
                FieldInput::Number {
                    integer,
                    minimum,
                    maximum,
                },
                FieldValue::Text(text),
            ) => {
                let text = text.trim();
                if text.is_empty() {
                    return if self.required {
                        Err("This field is required.".to_string())
                    } else {
                        Ok(None)
                    };
                }
                let value: f64 = text
                    .parse()
                    .ok()
                    .filter(|value: &f64| value.is_finite())
                    .ok_or_else(|| "Enter a number.".to_string())?;
                if *integer && value.fract() != 0.0 {
                    return Err("Enter a whole number.".to_string());
                }
                if let Some(min) = minimum
                    && value < *min
                {
                    return Err(format!(
                        "Enter a value of at least {}.",
                        format_number(*min, *integer)
                    ));
                }
                if let Some(max) = maximum
                    && value > *max
                {
                    return Err(format!(
                        "Enter a value of at most {}.",
                        format_number(*max, *integer)
                    ));
                }
                if *integer {
                    let whole = text
                        .parse::<i64>()
                        .map_err(|_| "Enter a whole number.".to_string())?;
                    Ok(Some(Value::from(whole)))
                } else {
                    serde_json::Number::from_f64(value)
                        .map(|number| Some(Value::Number(number)))
                        .ok_or_else(|| "Enter a number.".to_string())
                }
            }
            (FieldInput::Boolean, FieldValue::Bool(value)) => Ok(Some(Value::Bool(*value))),
            (FieldInput::Single(choices), FieldValue::Single(selected)) => {
                match selected.and_then(|index| choices.get(index)) {
                    Some(choice) => Ok(Some(Value::String(choice.value.clone()))),
                    None if self.required => Err("Choose an option.".to_string()),
                    None => Ok(None),
                }
            }
            (
                FieldInput::Multi {
                    choices,
                    min_items,
                    max_items,
                },
                FieldValue::Multi(checked),
            ) => {
                let values: Vec<Value> = choices
                    .iter()
                    .zip(checked)
                    .filter(|(_, checked)| **checked)
                    .map(|(choice, _)| Value::String(choice.value.clone()))
                    .collect();
                let count = values.len() as u64;
                if values.is_empty() && !self.required && min_items.unwrap_or(0) == 0 {
                    return Ok(None);
                }
                let min = min_items.unwrap_or(u64::from(self.required));
                if count < min {
                    return Err(if min == 1 {
                        "Choose at least one option.".to_string()
                    } else {
                        format!("Choose at least {min} options.")
                    });
                }
                if let Some(max) = max_items
                    && count > *max
                {
                    return Err(format!("Choose at most {max} options."));
                }
                Ok(Some(Value::Array(values)))
            }
            _ => Ok(None),
        }
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn validate_format(text: &str, format: McpElicitationStringFormat) -> Result<(), String> {
    let valid = match format {
        McpElicitationStringFormat::Email => text.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !text.contains(char::is_whitespace)
        }),
        McpElicitationStringFormat::Uri => {
            text.split_once(':').is_some_and(|(scheme, rest)| {
                !rest.is_empty()
                    && scheme
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_ascii_alphabetic())
                    && scheme
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
            }) && !text.contains(char::is_whitespace)
        }
        McpElicitationStringFormat::Date => is_date(text),
        McpElicitationStringFormat::DateTime => text
            .split_once(['T', 't', ' '])
            .is_some_and(|(date, time)| is_date(date) && is_time(time)),
    };
    if valid {
        Ok(())
    } else {
        Err(match format {
            McpElicitationStringFormat::Email => "Enter an email address.",
            McpElicitationStringFormat::Uri => "Enter a URL such as https://example.com.",
            McpElicitationStringFormat::Date => "Enter a date as YYYY-MM-DD.",
            McpElicitationStringFormat::DateTime => {
                "Enter a date and time as YYYY-MM-DDThh:mm:ssZ."
            }
        }
        .to_string())
    }
}

fn digits(text: &str, len: usize) -> Option<u32> {
    (text.len() == len && text.chars().all(|ch| ch.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn is_date(text: &str) -> bool {
    let mut parts = text.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    digits(year, /*len*/ 4).is_some()
        && digits(month, 2).is_some_and(|month| (1..=12).contains(&month))
        && digits(day, 2).is_some_and(|day| (1..=31).contains(&day))
}

fn is_time(text: &str) -> bool {
    // hh:mm[:ss[.fraction]] followed by Z or an offset.
    let core_len = text.find(['Z', 'z', '+', '-']).unwrap_or(text.len());
    let (clock, zone) = text.split_at(core_len);
    let mut parts = clock.split(':');
    let hour = parts.next().and_then(|hour| digits(hour, /*len*/ 2));
    let minute = parts.next().and_then(|minute| digits(minute, /*len*/ 2));
    let second_ok = match parts.next() {
        None => true,
        Some(second) => {
            let whole = second.split('.').next().unwrap_or_default();
            digits(whole, 2).is_some_and(|second| second <= 60)
        }
    };
    let zone_ok = zone.is_empty()
        || zone.eq_ignore_ascii_case("z")
        || (zone.len() == 6 && is_time(&zone[1..]));
    hour.is_some_and(|hour| hour < 24)
        && minute.is_some_and(|minute| minute < 60)
        && second_ok
        && parts.next().is_none()
        && zone_ok
}

/// Validates every field and builds the `content` object, or returns the
/// per-field error messages by field index.
fn form_content(fields: &[FormField]) -> Result<Map<String, Value>, Vec<(usize, String)>> {
    let mut content = Map::new();
    let mut errors = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        match field.content_value() {
            Ok(Some(value)) => {
                content.insert(field.id.clone(), value);
            }
            Ok(None) => {}
            Err(message) => errors.push((index, message)),
        }
    }
    if errors.is_empty() {
        Ok(content)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn params(request: Value) -> McpServerElicitationRequestParams {
        let mut base = json!({"threadId": "t", "turnId": "u", "serverName": "docs"});
        if let (Some(base), Some(request)) = (base.as_object_mut(), request.as_object()) {
            for (key, value) in request {
                base.insert(key.clone(), value.clone());
            }
        }
        match serde_json::from_value(base) {
            Ok(params) => params,
            Err(err) => panic!("invalid test params: {err}"),
        }
    }

    fn elicitation(request: Value) -> Elicitation {
        match Elicitation::from_params(params(request)) {
            Ok(elicitation) => elicitation,
            Err(unsupported) => panic!("unexpected unsupported: {unsupported:?}"),
        }
    }

    fn option_summary(elicitation: &Elicitation) -> Vec<(String, String, Value)> {
        elicitation
            .options()
            .into_iter()
            .map(|option| {
                let Decision::Elicitation { action, meta, .. } = option.decision.clone() else {
                    panic!("expected an elicitation decision");
                };
                let answer = match elicitation.choice_answer(action, meta, /*open_url*/ None) {
                    Ok(answer) => answer.result,
                    Err(err) => panic!("encode failed: {err}"),
                };
                (option.key_label(), option.label, answer)
            })
            .collect()
    }

    #[test]
    fn message_only_tool_approval_offers_persist_choices() {
        let request = elicitation(json!({
            "mode": "form",
            "_meta": {
                "codex_approval_kind": "mcp_tool_call",
                "persist": ["session", "always"],
                "tool_title": "Search docs",
                "tool_params": {"query": "slint  layouts", "limit": 5},
            },
            "message": "Allow the docs server to search?",
            "requestedSchema": {"type": "object", "properties": {}},
        }));
        assert_eq!(
            option_summary(&request),
            vec![
                (
                    "Y".to_string(),
                    "Allow".to_string(),
                    json!({"action": "accept", "content": null, "_meta": null})
                ),
                (
                    "A".to_string(),
                    "Allow for this session".to_string(),
                    json!({"action": "accept", "content": null, "_meta": {"persist": "session"}})
                ),
                (
                    "P".to_string(),
                    "Always allow".to_string(),
                    json!({"action": "accept", "content": null, "_meta": {"persist": "always"}})
                ),
                (
                    "Esc".to_string(),
                    "Cancel".to_string(),
                    json!({"action": "cancel", "content": null, "_meta": null})
                ),
            ]
        );
        let card = request.card();
        assert_eq!(card.title, "Allow docs to run Search docs?");
        assert_eq!(
            card.details,
            vec![
                "**limit:** 5".to_string(),
                "**query:** slint layouts".to_string()
            ]
        );
    }

    #[test]
    fn plain_message_only_request_has_deny() {
        let request = elicitation(json!({
            "mode": "form",
            "_meta": {"persist": "session"},
            "message": "Continue?",
            "requestedSchema": {"type": "object", "properties": {}},
        }));
        let labels: Vec<String> = request
            .options()
            .into_iter()
            .map(|option| option.label)
            .collect();
        assert_eq!(
            labels,
            vec!["Allow", "Allow for this session", "Deny", "Cancel"]
        );
    }

    #[test]
    fn url_mode_opens_the_link_on_accept() -> serde_json::Result<()> {
        let request = elicitation(json!({
            "mode": "url",
            "message": "Sign in to continue",
            "url": "https://example.com/login",
            "elicitationId": "e1",
        }));
        let open = request.options().into_iter().next();
        let Some(ApprovalOption {
            decision:
                Decision::Elicitation {
                    action,
                    meta,
                    open_url,
                },
            ..
        }) = open
        else {
            panic!("expected an open option");
        };
        let answer = request.choice_answer(action, meta, open_url)?;
        assert_eq!(
            answer.open_url.as_deref(),
            Some("https://example.com/login")
        );
        assert_eq!(
            answer.result,
            json!({"action": "accept", "content": null, "_meta": null})
        );
        assert_eq!(request.card().link, "https://example.com/login");

        let unsafe_link = elicitation(json!({
            "mode": "url",
            "message": "Run this",
            "url": "file:///etc/passwd",
            "elicitationId": "e2",
        }));
        let labels: Vec<String> = unsafe_link
            .options()
            .into_iter()
            .map(|option| option.label)
            .collect();
        assert_eq!(labels, vec!["Decline", "Cancel"]);
        // The card must not offer an Open button for a link it refuses.
        let card = unsafe_link.card();
        assert_eq!(card.link, "");
        assert_eq!(
            card.details,
            vec![
                "**Link:** file\\:\\/\\/\\/etc\\/passwd".to_string(),
                "Only http and https links can be opened.".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn unsupported_modes_are_declined() -> serde_json::Result<()> {
        let result = Elicitation::from_params(params(json!({
            "mode": "openaiForm",
            "message": "x",
            "requestedSchema": {},
        })));
        let Err(unsupported) = result else {
            panic!("expected unsupported");
        };
        assert_eq!(
            serde_json::to_value(unsupported.decline_response())?,
            json!({"action": "decline", "content": null, "_meta": null})
        );
        Ok(())
    }

    fn form_request() -> Elicitation {
        elicitation(json!({
            "mode": "form",
            "message": "Tell us about the deploy",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "email": {"type": "string", "title": "Email", "format": "email"},
                    "name": {"type": "string", "minLength": 2, "maxLength": 5},
                    "replicas": {"type": "integer", "minimum": 1, "maximum": 10, "default": 3},
                    "ratio": {"type": "number"},
                    "confirm": {"type": "boolean", "title": "Confirm"},
                    "region": {"type": "string", "enum": ["us", "eu"], "enumNames": ["United States", "Europe"]},
                    "tier": {"type": "string", "oneOf": [{"const": "free", "title": "Free"}, {"const": "pro", "title": "Pro"}]},
                    "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b", "c"]}, "maxItems": 2},
                },
                "required": ["email", "region", "tags"],
            },
        }))
    }

    fn field_index(request: &Elicitation, id: &str) -> usize {
        let Mode::Form { fields, .. } = &request.mode else {
            panic!("expected a form");
        };
        match fields.iter().position(|field| field.id == id) {
            Some(index) => index,
            None => panic!("no field {id}"),
        }
    }

    #[test]
    fn form_requires_and_validates_fields() {
        let mut request = form_request();
        assert!(request.submit().is_none(), "missing required fields");
        let errors: Vec<(String, String)> = match &request.mode {
            Mode::Form { fields, .. } => fields
                .iter()
                .filter_map(|field| field.error.clone().map(|error| (field.id.clone(), error)))
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(
            errors,
            vec![
                ("email".to_string(), "This field is required.".to_string()),
                ("region".to_string(), "Choose an option.".to_string()),
                (
                    "tags".to_string(),
                    "Choose at least one option.".to_string()
                ),
            ]
        );

        let email = field_index(&request, "email");
        assert!(
            request.set_text(email, "not-an-email".to_string()),
            "editing clears the field's error"
        );
        assert!(!request.form_error().is_empty(), "other fields still fail");
        let name = field_index(&request, "name");
        assert!(!request.set_text(name, "toolong".to_string()));
        let replicas = field_index(&request, "replicas");
        request.set_text(replicas, "2.5".to_string());
        assert!(request.submit().is_none());
        let views: Vec<String> = [email, name, replicas]
            .iter()
            .filter_map(|index| request.field(*index).map(|view| view.error))
            .collect();
        assert_eq!(
            views,
            vec![
                "Enter an email address.".to_string(),
                "Enter at most 5 characters.".to_string(),
                "Enter a whole number.".to_string(),
            ]
        );
    }

    #[test]
    fn form_content_uses_typed_values() -> serde_json::Result<()> {
        let mut request = form_request();
        let set = |request: &mut Elicitation, id: &str, text: &str| {
            let index = field_index(request, id);
            request.set_text(index, text.to_string());
        };
        set(&mut request, "email", "ada@example.com");
        set(&mut request, "ratio", "0.5");
        let region = field_index(&request, "region");
        assert!(request.toggle_choice(region, /*choice*/ 1));
        let tier = field_index(&request, "tier");
        assert!(request.toggle_choice(tier, /*choice*/ 1));
        let tags = field_index(&request, "tags");
        assert!(request.toggle_choice(tags, /*choice*/ 0));
        assert!(request.toggle_choice(tags, /*choice*/ 2));
        let confirm = field_index(&request, "confirm");
        assert!(request.toggle_choice(confirm, /*choice*/ 0));

        let answer = match request.submit() {
            Some(answer) => answer?,
            None => panic!("form should be valid: {:?}", request.card().body),
        };
        assert_eq!(
            answer.result,
            json!({
                "action": "accept",
                "content": {
                    "email": "ada@example.com",
                    "replicas": 3,
                    "ratio": 0.5,
                    "confirm": true,
                    "region": "eu",
                    "tier": "pro",
                    "tags": ["a", "c"],
                },
                "_meta": null,
            })
        );
        Ok(())
    }

    #[test]
    fn multi_select_respects_max_items() {
        let mut request = form_request();
        let tags = field_index(&request, "tags");
        for choice in 0..3 {
            request.toggle_choice(tags, choice);
        }
        let Mode::Form { fields, .. } = &request.mode else {
            panic!("expected a form");
        };
        assert_eq!(
            fields[tags].content_value(),
            Err("Choose at most 2 options.".to_string())
        );
    }

    #[test]
    fn field_views_show_labels_placeholders_and_markers() {
        let request = form_request();
        let email = request.field(field_index(&request, "email"));
        assert_eq!(
            email.map(|view| (view.header, view.placeholder, view.kind)),
            Some((
                "Email *".to_string(),
                "name@example.com".to_string(),
                FieldKind::Text
            ))
        );
        let replicas = request.field(field_index(&request, "replicas"));
        assert_eq!(
            replicas.map(|view| (view.text, view.placeholder)),
            Some(("3".to_string(), "Whole number from 1 to 10".to_string()))
        );
        let region = request.field(field_index(&request, "region"));
        assert_eq!(
            region.map(|view| view
                .choices
                .into_iter()
                .map(|choice| choice.label)
                .collect::<Vec<_>>()),
            Some(vec!["United States".to_string(), "Europe".to_string()])
        );
    }

    #[test]
    fn formats_are_validated() {
        assert!(validate_format("2026-10-06", McpElicitationStringFormat::Date).is_ok());
        assert!(validate_format("2026-13-06", McpElicitationStringFormat::Date).is_err());
        assert!(
            validate_format("2026-10-06T12:30:00Z", McpElicitationStringFormat::DateTime).is_ok()
        );
        assert!(
            validate_format(
                "2026-10-06T12:30+02:00",
                McpElicitationStringFormat::DateTime
            )
            .is_ok()
        );
        assert!(validate_format("2026-10-06", McpElicitationStringFormat::DateTime).is_err());
        assert!(validate_format("https://x.dev/a", McpElicitationStringFormat::Uri).is_ok());
        assert!(validate_format("no scheme", McpElicitationStringFormat::Uri).is_err());
    }

    #[test]
    fn decline_and_cancel_buttons_send_no_content() -> serde_json::Result<()> {
        let request = form_request();
        assert_eq!(
            request
                .dismiss_answer(McpServerElicitationAction::Decline)?
                .result,
            json!({"action": "decline", "content": null, "_meta": null})
        );
        assert_eq!(
            request
                .dismiss_answer(McpServerElicitationAction::Cancel)?
                .result,
            json!({"action": "cancel", "content": null, "_meta": null})
        );
        Ok(())
    }

    #[test]
    fn tool_suggestion_with_install_url_links_out() {
        let request = elicitation(json!({
            "mode": "form",
            "_meta": {
                "codex_approval_kind": "tool_suggestion",
                "tool_type": "connector",
                "suggest_type": "install",
                "suggest_reason": "Needed to read your calendar",
                "tool_id": "cal",
                "tool_name": "Calendar",
                "install_url": "https://example.com/install",
            },
            "message": "",
            "requestedSchema": {"type": "object", "properties": {}},
        }));
        let card = request.card();
        assert_eq!(card.title, "Install Calendar?");
        assert_eq!(card.link, "https://example.com/install");
        let labels: Vec<String> = request
            .options()
            .into_iter()
            .map(|option| option.key_label())
            .collect();
        assert_eq!(labels, vec!["Y", "Esc"]);
    }
}
