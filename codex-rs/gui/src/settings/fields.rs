//! "Common" and "All settings" pages: config fields with their origin layer,
//! managed locks, and editors. Common is a curated list; All settings is
//! generated from the config schema (see [`super::schema`]). The Memories
//! page reuses the same rows (see [`super::memories`]).
//!
//! While a switch or choice is saving, its row shows the new value; typed
//! text stays in its editor (also next to an error) because rows only
//! resync their controls when the value itself changes.

use std::collections::HashMap;
use std::collections::HashSet;

use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::ConfigLayerMetadata;
use codex_app_server_protocol::ConfigRequirements;
use codex_app_server_protocol::ExperimentalFeature;
use codex_app_server_protocol::Model;
use serde_json::Value;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use super::LoadState;
use super::PendingValues;
use super::RowList;
use super::model;
use super::model::ValueSource;
use super::replace_edit;
use super::schema;
use super::schema::FieldKind;
use super::schema::FieldSpec;
use super::schema::SectionSpec;
use super::toml_value;
use super::words;
use crate::app::AppController;
use crate::ui::FieldData;
use crate::ui::SettingsState;

const NEW_THREAD_NOTE: &str =
    "Applies to new threads. Open threads keep theirs; change it from the composer.";

/// How a field is edited.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Editor {
    Header,
    Bool,
    /// Option `i + 1` writes `values[i]`. Option 0 removes the key when
    /// `zero_unsets`, and is a no-op placeholder otherwise.
    Enum {
        values: Vec<String>,
        zero_unsets: bool,
    },
    Text,
    /// Shell words stored as a string array (`notify`).
    ShellWords,
    Integer {
        min: Option<f64>,
        max: Option<f64>,
    },
    Number {
        min: Option<f64>,
        max: Option<f64>,
    },
    /// TOML snippet for the key's last segment.
    Snippet {
        leaf: String,
    },
}

impl Editor {
    fn code(&self) -> i32 {
        match self {
            Self::Header => 0,
            Self::Bool => 1,
            Self::Enum { .. } => 2,
            Self::Text | Self::ShellWords => 3,
            Self::Integer { .. } => 4,
            Self::Number { .. } => 5,
            Self::Snippet { .. } => 6,
        }
    }
}

/// Plain form of one field row; converted to `FieldData` for Slint.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FieldRow {
    pub(crate) editor: Editor,
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) path: String,
    pub(crate) description: String,
    pub(crate) origin: String,
    pub(crate) origin_detail: String,
    pub(crate) origin_tone: i32,
    pub(crate) locked: bool,
    pub(crate) lock_reason: String,
    pub(crate) can_reset: bool,
    pub(crate) checked: bool,
    pub(crate) options: Vec<String>,
    pub(crate) option_index: i32,
    pub(crate) text: String,
    pub(crate) placeholder: String,
    pub(crate) summary: String,
    pub(crate) note: String,
    pub(crate) error: String,
    pub(crate) expanded: bool,
    pub(crate) busy: bool,
}

impl FieldRow {
    fn new(editor: Editor, key: String, title: String) -> Self {
        Self {
            editor,
            key,
            title,
            path: String::new(),
            description: String::new(),
            origin: String::new(),
            origin_detail: String::new(),
            origin_tone: 0,
            locked: false,
            lock_reason: String::new(),
            can_reset: false,
            checked: false,
            options: Vec::new(),
            option_index: 0,
            text: String::new(),
            placeholder: String::new(),
            summary: String::new(),
            note: String::new(),
            error: String::new(),
            expanded: false,
            busy: false,
        }
    }

    fn header(name: &str, description: &str) -> Self {
        let mut row = Self::new(Editor::Header, format!("§{name}"), name.to_string());
        row.description = description.to_string();
        row
    }

    fn to_slint(&self, revision: i32) -> FieldData {
        let options: Vec<SharedString> = self.options.iter().map(SharedString::from).collect();
        FieldData {
            kind: self.editor.code(),
            key: self.key.as_str().into(),
            title: self.title.as_str().into(),
            path: self.path.as_str().into(),
            description: self.description.as_str().into(),
            origin: self.origin.as_str().into(),
            origin_detail: self.origin_detail.as_str().into(),
            origin_tone: self.origin_tone,
            locked: self.locked,
            lock_reason: self.lock_reason.as_str().into(),
            can_reset: self.can_reset,
            checked: self.checked,
            options: ModelRc::new(VecModel::from(options)),
            option_index: self.option_index,
            text: self.text.as_str().into(),
            placeholder: self.placeholder.as_str().into(),
            summary: self.summary.as_str().into(),
            note: self.note.as_str().into(),
            error: model::display_error(&self.error).into(),
            expanded: self.expanded,
            busy: self.busy,
            revision,
        }
    }
}

fn field_key(row: &FieldRow) -> &str {
    &row.key
}

/// Whether two versions of a row show the same value in their control.
fn same_control(new: &FieldRow, old: &FieldRow) -> bool {
    new.checked == old.checked
        && new.option_index == old.option_index
        && new.options == old.options
        && new.text == old.text
}

/// What applying an edit should write.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FieldValue {
    Set(Value),
    Unset,
    NoChange,
}

/// Config inputs used to build rows.
pub(crate) struct FieldInputs<'a> {
    pub(crate) effective: &'a Value,
    pub(crate) user: &'a Value,
    pub(crate) origins: &'a HashMap<String, ConfigLayerMetadata>,
    pub(crate) requirements: Option<&'a ConfigRequirements>,
    /// Feature flags from `experimentalFeature/list`, for the defaults of
    /// `features.<name>` (the schema has none).
    pub(crate) features: &'a [ExperimentalFeature],
}

/// The feature flag `features.<name>` names, if known.
pub(crate) fn feature_flag<'a>(
    segments: &[String],
    features: &'a [ExperimentalFeature],
) -> Option<&'a ExperimentalFeature> {
    match segments {
        [table, name] if table == "features" => {
            features.iter().find(|feature| feature.name == *name)
        }
        _ => None,
    }
}

/// Fills origin, lock, and reset state for `segments`.
///
/// Snippet fields (tables, arrays) are never locked by layers: other layers
/// may contribute entries, and the user's own entries stay editable.
fn decorate(row: &mut FieldRow, segments: &[String], inputs: &FieldInputs<'_>, snippet: bool) {
    let source = ValueSource::of(inputs.origins, segments);
    let pin = inputs
        .requirements
        .and_then(|requirements| model::requirement_pin(requirements, segments));
    row.origin = source.label();
    row.origin_tone = match &source {
        ValueSource::Default => 0,
        ValueSource::User => 1,
        ValueSource::Lower(_) => 2,
        ValueSource::Higher(_) => 3,
    };
    if let ValueSource::Lower(_) = &source {
        row.origin_detail = source.detail();
    }
    if let Some(field) = pin {
        row.locked = true;
        row.origin = "Managed".to_string();
        row.origin_tone = 3;
        row.lock_reason = format!(
            "Managed by your organization's requirements (`{field}`); it cannot be changed here."
        );
    } else if !snippet && matches!(source, ValueSource::Higher(_)) {
        row.locked = true;
        row.lock_reason = source.detail();
    }
    if snippet {
        let others: Vec<String> = model::origins_for(inputs.origins, segments)
            .into_iter()
            .filter(|metadata| !model::is_base_user_layer(&metadata.name))
            .map(|metadata| model::origin_detail(&metadata.name))
            .collect();
        if !others.is_empty() {
            row.origin_detail = format!(
                "Also set by {}. This editor changes only your config.toml.",
                others.join(", ")
            );
        }
    }
    row.can_reset = !row.locked && model::lookup(inputs.user, segments).is_some();
}

/// Row for a schema field on the All settings page.
pub(crate) fn schema_row(spec: &FieldSpec, inputs: &FieldInputs<'_>) -> FieldRow {
    let segments = &spec.segments;
    let leaf = segments.last().cloned().unwrap_or_default();
    let editor = match &spec.kind {
        FieldKind::Bool => Editor::Bool,
        FieldKind::Enum { values, .. } => Editor::Enum {
            values: values.clone(),
            zero_unsets: true,
        },
        FieldKind::Text => Editor::Text,
        FieldKind::Integer { min, max } => Editor::Integer {
            min: *min,
            max: *max,
        },
        FieldKind::Number { min, max } => Editor::Number {
            min: *min,
            max: *max,
        },
        FieldKind::Snippet => Editor::Snippet { leaf: leaf.clone() },
    };
    let mut row = FieldRow::new(editor, model::key_path(segments), leaf.clone());
    if segments.len() > 1 {
        row.path = segments.join(".");
    }
    row.description = spec.description.clone();
    let snippet = matches!(spec.kind, FieldKind::Snippet);
    decorate(&mut row, segments, inputs, snippet);
    let effective = model::lookup(inputs.effective, segments);
    let default = spec.default.as_ref().filter(|value| !value.is_null());
    match &spec.kind {
        FieldKind::Bool => {
            let feature = feature_flag(segments, inputs.features);
            let default = default
                .and_then(Value::as_bool)
                .or(feature.map(|feature| feature.default_enabled));
            row.checked = effective
                .and_then(Value::as_bool)
                .or(default)
                .unwrap_or(false);
            if let Some(feature) = feature {
                if row.description.is_empty() {
                    row.description = feature
                        .description
                        .clone()
                        .or_else(|| feature.display_name.clone())
                        .unwrap_or_default();
                }
                let default = if feature.default_enabled { "on" } else { "off" };
                row.description = format!("{} Default: {default}.", row.description)
                    .trim()
                    .to_string();
            }
        }
        FieldKind::Enum { values, help } => {
            let default_label = match default.and_then(Value::as_str) {
                Some(value) => format!("Default ({value})"),
                None => "Not set".to_string(),
            };
            let mut labels = values.clone();
            let mut values = values.clone();
            let current = effective.map(model::display_value);
            if let Some(current) = current.as_deref()
                && !values.iter().any(|value| value == current)
            {
                values.push(current.to_string());
                labels.push(format!("{current} (custom)"));
            }
            row.option_index = current
                .as_deref()
                .and_then(|current| values.iter().position(|value| value == current))
                .map_or(0, |index| i32::try_from(index + 1).unwrap_or(0));
            if let Some(index) = usize::try_from(row.option_index)
                .ok()
                .and_then(|i| i.checked_sub(1))
                && let Some(help) = help.get(index).filter(|help| !help.is_empty())
            {
                row.note = help.clone();
            }
            row.options = std::iter::once(default_label).chain(labels).collect();
            row.editor = Editor::Enum {
                values,
                zero_unsets: true,
            };
        }
        FieldKind::Text | FieldKind::Integer { .. } | FieldKind::Number { .. } => {
            row.text = effective.map(model::display_value).unwrap_or_default();
            row.placeholder = match default {
                Some(value) => format!("Default: {}", model::display_value(value)),
                None => "Not set".to_string(),
            };
            if let FieldKind::Integer { min, max } | FieldKind::Number { min, max } = &spec.kind
                && let Some(range) = range_text(*min, *max)
            {
                row.placeholder = format!("{} ({range})", row.placeholder);
            }
        }
        FieldKind::Snippet => {
            let user_value = model::lookup(inputs.user, segments);
            row.text = toml_value::snippet_for(&leaf, user_value);
            row.summary = toml_value::summarize(effective);
            row.placeholder = match default {
                Some(value) if !value.as_object().is_some_and(serde_json::Map::is_empty) => {
                    format!(
                        "# Not set. Default:\n{}",
                        toml_value::snippet_for(&leaf, Some(value))
                    )
                }
                _ => format!("# Not set. Example:\n# {leaf} = …"),
            };
        }
    }
    row
}

fn range_text(min: Option<f64>, max: Option<f64>) -> Option<String> {
    let number = |value: f64| {
        if value.fract() == 0.0 && value.abs() < 1e15 {
            format!("{}", value as i64)
        } else {
            value.to_string()
        }
    };
    match (min, max) {
        (Some(min), Some(max)) => Some(format!("{} to {}", number(min), number(max))),
        (Some(min), None) if min > 0.0 => Some(format!("at least {}", number(min))),
        (None, Some(max)) => Some(format!("at most {}", number(max))),
        _ => None,
    }
}

/// Builds the rows of the All settings page for `query`.
pub(crate) fn all_rows(
    sections: &[SectionSpec],
    query: &str,
    inputs: &FieldInputs<'_>,
) -> Vec<FieldRow> {
    let mut rows = Vec::new();
    for section in sections {
        let fields: Vec<&FieldSpec> = section
            .fields
            .iter()
            .filter(|field| schema::field_matches(field, query))
            .collect();
        if fields.is_empty() {
            continue;
        }
        rows.push(FieldRow::header(&section.name, &section.description));
        rows.extend(fields.into_iter().map(|field| schema_row(field, inputs)));
    }
    rows
}

/// Curated enum on the Common page: (config value, label) pairs.
struct Choices {
    key: &'static str,
    title: &'static str,
    description: &'static str,
    note: &'static str,
    choices: Vec<(String, String)>,
}

fn choices(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(value, label)| ((*value).to_string(), (*label).to_string()))
        .collect()
}

/// Enum row for the Common page. Values outside `allowed` are dropped; a
/// single allowed value locks the row.
fn choice_row(spec: Choices, allowed: Option<Vec<String>>, inputs: &FieldInputs<'_>) -> FieldRow {
    let segments = vec![spec.key.to_string()];
    let mut row = FieldRow::new(Editor::Header, spec.key.to_string(), spec.title.to_string());
    row.description = spec.description.to_string();
    row.note = spec.note.to_string();
    decorate(&mut row, &segments, inputs, /*snippet*/ false);
    let mut pairs = spec.choices;
    if let Some(allowed) = allowed.as_ref() {
        pairs.retain(|(value, _)| allowed.contains(value));
        if allowed.len() == 1 && !row.locked {
            row.locked = true;
            row.lock_reason = format!(
                "Your organization allows only {}.",
                pairs
                    .first()
                    .map_or(allowed[0].as_str(), |(_, label)| label.as_str())
            );
        }
    }
    let effective = model::lookup(inputs.effective, &segments);
    let mut zero_unsets = true;
    let mut first = "Default".to_string();
    match effective {
        Some(Value::String(current)) if !pairs.iter().any(|(value, _)| value == current) => {
            pairs.push((current.clone(), format!("{current} (custom)")));
        }
        Some(value) if !value.is_string() => {
            first = "Custom value (see All settings)".to_string();
            zero_unsets = false;
        }
        _ => {}
    }
    row.option_index = effective
        .and_then(Value::as_str)
        .and_then(|current| pairs.iter().position(|(value, _)| value == current))
        .map_or(0, |index| i32::try_from(index + 1).unwrap_or(0));
    row.options = std::iter::once(first)
        .chain(pairs.iter().map(|(_, label)| label.clone()))
        .collect();
    row.editor = Editor::Enum {
        values: pairs.into_iter().map(|(value, _)| value).collect(),
        zero_unsets,
    };
    row
}

fn allowed_names<T: serde::Serialize>(values: Option<&Vec<T>>) -> Option<Vec<String>> {
    values.map(|values| {
        values
            .iter()
            .filter_map(|value| match serde_json::to_value(value) {
                Ok(Value::String(name)) => Some(name),
                _ => None,
            })
            .collect()
    })
}

/// The model whose capabilities drive the effort picker: the configured
/// model, else the catalog default.
fn selected_model<'a>(models: &'a [Model], effective: &Value) -> Option<&'a Model> {
    let configured = model::lookup(effective, &["model"]).and_then(Value::as_str);
    configured
        .and_then(|slug| {
            models
                .iter()
                .find(|model| model.model == slug || model.id == slug)
        })
        .or_else(|| models.iter().find(|model| model.is_default))
}

/// Rows of the Common page.
pub(crate) fn common_rows(inputs: &FieldInputs<'_>, models: &[Model]) -> Vec<FieldRow> {
    let requirements = inputs.requirements;
    let mut rows = Vec::new();

    // Model.
    let model_segments = vec!["model".to_string()];
    let current_model = model::lookup(inputs.effective, &model_segments).and_then(Value::as_str);
    if models.is_empty() {
        let mut row = FieldRow::new(Editor::Text, "model".to_string(), "Model".to_string());
        row.description = "Model used for new threads.".to_string();
        row.note = NEW_THREAD_NOTE.to_string();
        row.text = current_model.unwrap_or_default().to_string();
        row.placeholder = "Provider default".to_string();
        decorate(&mut row, &model_segments, inputs, /*snippet*/ false);
        rows.push(row);
    } else {
        let default_label = models.iter().find(|model| model.is_default).map_or_else(
            || "Default".to_string(),
            |model| format!("Default ({})", model.display_name),
        );
        let pairs: Vec<(String, String)> = models
            .iter()
            .filter(|model| !model.hidden || Some(model.model.as_str()) == current_model)
            .map(|model| (model.model.clone(), model.display_name.clone()))
            .collect();
        let mut row = choice_row(
            Choices {
                key: "model",
                title: "Model",
                description: "Model used for new threads.",
                note: NEW_THREAD_NOTE,
                choices: pairs,
            },
            /*allowed*/ None,
            inputs,
        );
        if let Some(first) = row.options.first_mut()
            && first == "Default"
        {
            *first = default_label;
        }
        rows.push(row);
    }

    // Reasoning effort, from the selected model's capabilities.
    let effort_segments = vec!["model_reasoning_effort".to_string()];
    match selected_model(models, inputs.effective) {
        Some(selected) if !selected.supported_reasoning_efforts.is_empty() => {
            let pairs: Vec<(String, String)> = selected
                .supported_reasoning_efforts
                .iter()
                .map(|option| {
                    let value = option.reasoning_effort.to_string();
                    (value.clone(), capitalize(&value))
                })
                .collect();
            let mut row = choice_row(
                Choices {
                    key: "model_reasoning_effort",
                    title: "Reasoning effort",
                    description: "How much the model reasons before answering. Higher is slower and more thorough.",
                    note: NEW_THREAD_NOTE,
                    choices: pairs,
                },
                /*allowed*/ None,
                inputs,
            );
            if let Some(first) = row.options.first_mut()
                && first == "Default"
            {
                *first = format!("Default ({})", selected.default_reasoning_effort);
            }
            rows.push(row);
        }
        _ => {
            let mut row = FieldRow::new(
                Editor::Text,
                "model_reasoning_effort".to_string(),
                "Reasoning effort".to_string(),
            );
            row.description =
                "How much the model reasons before answering (for example low, medium, high)."
                    .to_string();
            row.note = NEW_THREAD_NOTE.to_string();
            row.text = model::lookup(inputs.effective, &effort_segments)
                .map(model::display_value)
                .unwrap_or_default();
            row.placeholder = "Model default".to_string();
            decorate(&mut row, &effort_segments, inputs, /*snippet*/ false);
            rows.push(row);
        }
    }

    rows.push(choice_row(
        Choices {
            key: "approval_policy",
            title: "Approvals",
            description: "When Codex stops to ask before running commands or editing files. Applies to open threads too.",
            note: "",
            choices: choices(&[
                ("untrusted", "Ask unless trusted"),
                ("on-request", "Ask when needed"),
                ("never", "Never ask"),
            ]),
        },
        allowed_names(requirements.and_then(|r| r.allowed_approval_policies.as_ref())),
        inputs,
    ));
    rows.push(choice_row(
        Choices {
            key: "sandbox_mode",
            title: "Sandbox",
            description: "What commands may touch. Workspace write allows edits inside the thread's folder.",
            note: "",
            choices: choices(&[
                ("read-only", "Read only"),
                ("workspace-write", "Workspace write"),
                ("danger-full-access", "Full access (no sandbox)"),
            ]),
        },
        allowed_names(requirements.and_then(|r| r.allowed_sandbox_modes.as_ref())),
        inputs,
    ));
    rows.push(choice_row(
        Choices {
            key: "web_search",
            title: "Web search",
            description: "Whether the agent can search the web, and how fresh the results are.",
            note: "",
            choices: choices(&[
                ("disabled", "Disabled"),
                ("cached", "Cached"),
                ("indexed", "Indexed"),
                ("live", "Live"),
            ]),
        },
        allowed_names(requirements.and_then(|r| r.allowed_web_search_modes.as_ref())),
        inputs,
    ));
    rows.push(choice_row(
        Choices {
            key: "model_reasoning_summary",
            title: "Reasoning summaries",
            description: "How much of the model's reasoning is summarized in the transcript.",
            note: "",
            choices: choices(&[
                ("auto", "Auto"),
                ("concise", "Concise"),
                ("detailed", "Detailed"),
                ("none", "None"),
            ]),
        },
        /*allowed*/ None,
        inputs,
    ));
    rows.push(choice_row(
        Choices {
            key: "model_verbosity",
            title: "Verbosity",
            description: "Length and detail of answers on models that support it.",
            note: "",
            choices: choices(&[("low", "Low"), ("medium", "Medium"), ("high", "High")]),
        },
        /*allowed*/ None,
        inputs,
    ));

    // Notification command.
    let notify_segments = vec!["notify".to_string()];
    let mut notify = FieldRow::new(
        Editor::ShellWords,
        "notify".to_string(),
        "Notification command".to_string(),
    );
    notify.description = "Program Codex runs when a turn finishes, with a JSON summary as its last argument. Desktop notifications from this app are under Appearance.".to_string();
    notify.placeholder = "Not set, e.g. notify-send Codex".to_string();
    notify.text = model::lookup(inputs.effective, &notify_segments)
        .and_then(Value::as_array)
        .map(|values| {
            let values: Vec<String> = values.iter().map(model::display_value).collect();
            words::join_words(&values)
        })
        .unwrap_or_default();
    decorate(
        &mut notify,
        &notify_segments,
        inputs,
        /*snippet*/ false,
    );
    rows.push(notify);
    rows
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Value written when option `index` is chosen.
pub(crate) fn value_for_option(editor: &Editor, index: i32) -> FieldValue {
    let Editor::Enum {
        values,
        zero_unsets,
    } = editor
    else {
        return FieldValue::NoChange;
    };
    match usize::try_from(index) {
        Ok(0) if *zero_unsets => FieldValue::Unset,
        Ok(0) | Err(_) => FieldValue::NoChange,
        Ok(index) => values.get(index - 1).map_or(FieldValue::NoChange, |value| {
            FieldValue::Set(Value::String(value.clone()))
        }),
    }
}

/// Value written when `text` is applied, or a message for the user.
pub(crate) fn value_for_text(editor: &Editor, text: &str) -> Result<FieldValue, String> {
    match editor {
        Editor::Text => Ok(if text.is_empty() {
            FieldValue::Unset
        } else {
            FieldValue::Set(Value::String(text.to_string()))
        }),
        Editor::ShellWords => {
            let words = words::split_words(text).ok_or("The command has an unbalanced quote.")?;
            Ok(if words.is_empty() {
                FieldValue::Unset
            } else {
                FieldValue::Set(Value::from(words))
            })
        }
        Editor::Integer { min, max } => {
            let text = text.trim().replace('_', "");
            if text.is_empty() {
                return Ok(FieldValue::Unset);
            }
            let value = if let Ok(int) = text.parse::<i64>() {
                Value::from(int)
            } else if let Ok(uint) = text.parse::<u64>() {
                Value::from(uint)
            } else {
                return Err("Enter a whole number.".to_string());
            };
            check_range(value.as_f64().unwrap_or_default(), *min, *max)?;
            Ok(FieldValue::Set(value))
        }
        Editor::Number { min, max } => {
            let text = text.trim();
            if text.is_empty() {
                return Ok(FieldValue::Unset);
            }
            let number: f64 = text.parse().map_err(|_| "Enter a number.".to_string())?;
            if !number.is_finite() {
                return Err("Enter a finite number.".to_string());
            }
            check_range(number, *min, *max)?;
            Ok(
                serde_json::Number::from_f64(number).map_or(FieldValue::NoChange, |number| {
                    FieldValue::Set(Value::Number(number))
                }),
            )
        }
        Editor::Snippet { leaf } => match toml_value::parse_snippet(leaf, text) {
            Ok(Some(value)) => Ok(FieldValue::Set(value)),
            Ok(None) => Ok(FieldValue::Unset),
            Err(problem) => Err(problem.to_string()),
        },
        Editor::Header | Editor::Bool | Editor::Enum { .. } => Ok(FieldValue::NoChange),
    }
}

fn check_range(value: f64, min: Option<f64>, max: Option<f64>) -> Result<(), String> {
    if let Some(min) = min
        && value < min
    {
        return Err(format!("Must be at least {}.", range_number(min)));
    }
    if let Some(max) = max
        && value > max
    {
        return Err(format!("Must be at most {}.", range_number(max)));
    }
    Ok(())
}

fn range_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        value.to_string()
    }
}

/// State of the Common, All settings, and Memories pages.
#[derive(Default)]
pub(crate) struct FieldsState {
    pub(crate) common: RowList<FieldRow, FieldData>,
    pub(crate) all: RowList<FieldRow, FieldData>,
    pub(crate) memories: RowList<FieldRow, FieldData>,
    sections: Option<Vec<SectionSpec>>,
    schema: LoadState,
    schema_error: Option<String>,
    search: String,
    expanded: HashSet<String>,
    errors: HashMap<String, String>,
    notices: HashMap<String, String>,
    busy: HashSet<String>,
    force: HashSet<String>,
    /// Switch and choice values being saved, by [`pending_key`]. Choices
    /// are option indexes, which differ between pages.
    pending: PendingValues<PendingControl>,
}

impl FieldsState {
    /// Drops saved values that the config read of `generation` shows.
    pub(crate) fn settle_pending(&mut self, generation: u64) {
        self.pending.settle(generation);
    }
}

/// A switch or choice value shown while it is being saved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PendingControl {
    Checked(bool),
    Option(i32),
}

fn pending_key(page: FieldPage, key: &str) -> String {
    format!("{page:?}:{key}")
}

/// Shows a pending switch or choice value in `row`.
fn apply_pending(row: &mut FieldRow, pending: Option<&PendingControl>) {
    match (pending, &row.editor) {
        (Some(PendingControl::Checked(checked)), Editor::Bool) => row.checked = *checked,
        (Some(PendingControl::Option(index)), Editor::Enum { .. }) => row.option_index = *index,
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FieldPage {
    Common,
    All,
    Memories,
}

impl FieldPage {
    fn parse(page: &str) -> Option<Self> {
        match page {
            "common" => Some(Self::Common),
            "all" => Some(Self::All),
            "memories" => Some(Self::Memories),
            _ => None,
        }
    }
}

impl AppController {
    pub(super) fn settings_fields_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.on_all_search_edited(|text| {
            let text = text.to_string();
            crate::ui_thread::with_app(move |app| {
                app.settings.fields.search = text;
                app.settings_fields_refresh();
            });
        });
        state.on_field_toggle(|page, key, value| {
            let (page, key) = (page.to_string(), key.to_string());
            crate::ui_thread::with_app(move |app| {
                app.settings_field_input(&page, &key, FieldInput::Toggle(value));
            });
        });
        state.on_field_select(|page, key, index| {
            let (page, key) = (page.to_string(), key.to_string());
            crate::ui_thread::with_app(move |app| {
                app.settings_field_input(&page, &key, FieldInput::Select(index));
            });
        });
        state.on_field_apply(|page, key, text| {
            let (page, key, text) = (page.to_string(), key.to_string(), text.to_string());
            crate::ui_thread::with_app(move |app| {
                app.settings_field_input(&page, &key, FieldInput::Text(text));
            });
        });
        state.on_field_reset(|page, key| {
            let (page, key) = (page.to_string(), key.to_string());
            crate::ui_thread::with_app(move |app| {
                app.settings_field_input(&page, &key, FieldInput::Reset);
            });
        });
        state.on_field_expand(|_page, key, open| {
            let key = key.to_string();
            crate::ui_thread::with_app(move |app| {
                if open {
                    app.settings.fields.expanded.insert(key.clone());
                } else {
                    app.settings.fields.expanded.remove(&key);
                }
                app.settings.fields.force.insert(key);
                app.settings_fields_refresh();
            });
        });
    }

    pub(super) fn settings_fields_activate(&mut self, page: &str) {
        if page == "all" && self.settings.fields.schema == LoadState::Stale {
            self.settings.fields.schema = LoadState::Loading;
            let backend = self.backend.clone();
            backend.spawn(async move {
                let result = tokio::task::spawn_blocking(|| {
                    schema::config_schema().map(|schema| schema::build_sections(&schema))
                })
                .await
                .unwrap_or_else(|err| Err(format!("schema generation panicked: {err}")));
                crate::ui_thread::post(move |app| {
                    match result {
                        Ok(sections) => {
                            app.settings.fields.sections = Some(sections);
                            app.settings.fields.schema_error = None;
                        }
                        Err(err) => {
                            tracing::warn!(err, "failed to build the config schema");
                            app.settings.fields.schema_error = Some(err);
                        }
                    }
                    app.settings.fields.schema = LoadState::Loaded;
                    app.settings_fields_refresh();
                });
            });
        }
        if page != "common" {
            // Defaults of `features.<name>` come from the feature list.
            self.settings_extensions_ensure_loaded("features");
        }
        if self.settings.snapshot.is_none()
            && !self.settings.snapshot_loading
            && self.settings.server_error.is_none()
        {
            self.settings_reload_config();
        }
        self.settings_fields_refresh();
    }

    /// Rebuilds the field pages from the current snapshot.
    pub(super) fn settings_fields_refresh(&mut self) {
        let empty_object = Value::Null;
        let empty_origins = HashMap::new();
        let snapshot = self.settings.snapshot.as_ref();
        let inputs = FieldInputs {
            effective: snapshot.map_or(&empty_object, |snapshot| &snapshot.effective),
            user: snapshot.map_or(&empty_object, |snapshot| &snapshot.user_config),
            origins: snapshot.map_or(&empty_origins, |snapshot| &snapshot.origins),
            requirements: self.settings.requirements.as_ref(),
            features: self.settings.extensions.feature_flags(),
        };
        let fields = &self.settings.fields;
        let decorate_state = |page: FieldPage| {
            move |mut row: FieldRow| {
                let pending = fields.pending.get(&pending_key(page, &row.key));
                apply_pending(&mut row, pending);
                if let Some(error) = fields.errors.get(&row.key) {
                    row.error = error.clone();
                }
                if let Some(notice) = fields.notices.get(&row.key) {
                    row.note = notice.clone();
                }
                row.busy = fields.busy.contains(&row.key);
                row.expanded = fields.expanded.contains(&row.key);
                row
            }
        };
        let have_config = snapshot.is_some();
        let common: Vec<FieldRow> = if have_config {
            common_rows(&inputs, &self.settings.models)
                .into_iter()
                .map(decorate_state(FieldPage::Common))
                .collect()
        } else {
            Vec::new()
        };
        let all: Vec<FieldRow> = match (&fields.sections, have_config) {
            (Some(sections), true) => all_rows(sections, &fields.search, &inputs)
                .into_iter()
                .map(decorate_state(FieldPage::All))
                .collect(),
            _ => Vec::new(),
        };
        let memories: Vec<FieldRow> = if have_config {
            super::memories::memory_rows(&inputs)
                .into_iter()
                .map(decorate_state(FieldPage::Memories))
                .collect()
        } else {
            Vec::new()
        };
        let status_common = match (&self.settings.snapshot_error, have_config) {
            (Some(error), _) => format!("Could not read the configuration: {error}"),
            (None, false) if self.settings.server_error.is_none() => "Loading…".to_string(),
            _ => String::new(),
        };
        let status_all = if let Some(error) = &fields.schema_error {
            format!("Could not build the settings list: {error}")
        } else if fields.schema != LoadState::Loaded
            || (!have_config && self.settings.server_error.is_none())
        {
            if fields.schema == LoadState::Stale && have_config {
                String::new()
            } else {
                "Loading settings…".to_string()
            }
        } else if all.is_empty() && !fields.search.trim().is_empty() {
            format!("No settings match “{}”.", fields.search.trim())
        } else {
            status_common.clone()
        };
        let memories_feature_on = memories
            .iter()
            .find(|row| row.key == super::memories::FEATURE_KEY)
            .is_none_or(|row| row.checked);
        let force = std::mem::take(&mut self.settings.fields.force);
        let mut revision = self.settings.revision;
        let fields = &mut self.settings.fields;
        for (list, rows) in [
            (&mut fields.common, common),
            (&mut fields.all, all),
            (&mut fields.memories, memories),
        ] {
            list.sync_controls(
                rows,
                field_key,
                same_control,
                &force,
                &mut revision,
                FieldRow::to_slint,
            );
        }
        self.settings.revision = revision;
        let state = self.window.global::<SettingsState>();
        state.set_common_status(status_common.clone().into());
        state.set_all_status(status_all.into());
        state.set_memories_status(status_common.into());
        state.set_memories_feature_on(memories_feature_on);
    }

    fn settings_field_input(&mut self, page: &str, key: &str, input: FieldInput) {
        let Some(page) = FieldPage::parse(page) else {
            return;
        };
        let list = match page {
            FieldPage::Common => &self.settings.fields.common,
            FieldPage::All => &self.settings.fields.all,
            FieldPage::Memories => &self.settings.fields.memories,
        };
        let Some(row) = list.rows.iter().find(|row| row.key == key).cloned() else {
            return;
        };
        if row.locked || row.busy {
            self.settings.fields.force.insert(row.key);
            self.settings_fields_refresh();
            return;
        }
        let value = match &input {
            FieldInput::Toggle(value) => Ok(FieldValue::Set(Value::Bool(*value))),
            FieldInput::Select(index) => Ok(value_for_option(&row.editor, *index)),
            FieldInput::Text(text) => value_for_text(&row.editor, text),
            FieldInput::Reset => Ok(FieldValue::Unset),
        };
        let value = match value {
            Ok(FieldValue::NoChange) => {
                self.settings.fields.force.insert(row.key);
                self.settings_fields_refresh();
                return;
            }
            Ok(FieldValue::Set(value)) => value,
            Ok(FieldValue::Unset) => Value::Null,
            Err(message) => {
                // The typed text stays in the editor next to the message.
                self.settings.fields.errors.insert(row.key, message);
                self.settings_fields_refresh();
                return;
            }
        };
        let pending = match input {
            FieldInput::Toggle(checked) => Some(PendingControl::Checked(checked)),
            FieldInput::Select(index) => Some(PendingControl::Option(index)),
            FieldInput::Text(_) | FieldInput::Reset => None,
        };
        if let Some(pending) = pending {
            self.settings
                .fields
                .pending
                .begin(pending_key(page, &row.key), pending);
        }
        let mut edits = vec![replace_edit(row.key.clone(), value.clone())];
        if row.key == "model" {
            edits.extend(self.settings_effort_fixup(&value));
        }
        self.settings_field_write(page, row.key, edits);
    }

    /// Clears a configured reasoning effort the newly chosen model does not
    /// support, like the TUI's model picker.
    fn settings_effort_fixup(&self, model_value: &Value) -> Option<ConfigEdit> {
        let snapshot = self.settings.snapshot.as_ref()?;
        let effort = model::lookup(&snapshot.user_config, &["model_reasoning_effort"])?.as_str()?;
        let models = &self.settings.models;
        let chosen = match model_value.as_str() {
            Some(slug) => models.iter().find(|model| model.model == slug)?,
            None => models.iter().find(|model| model.is_default)?,
        };
        let supported = chosen
            .supported_reasoning_efforts
            .iter()
            .any(|option| option.reasoning_effort.to_string() == effort);
        (!supported).then(|| replace_edit("model_reasoning_effort", Value::Null))
    }

    fn settings_field_write(&mut self, page: FieldPage, key: String, edits: Vec<ConfigEdit>) {
        let fields = &mut self.settings.fields;
        fields.errors.remove(&key);
        fields.notices.remove(&key);
        fields.busy.insert(key.clone());
        self.settings_fields_refresh();
        let done_key = key;
        self.settings_write(
            edits,
            Box::new(move |app, result| {
                let generation = app.settings.snapshot_generation;
                let fields = &mut app.settings.fields;
                fields.busy.remove(&done_key);
                let pending_key = pending_key(page, &done_key);
                match result {
                    Ok(response) => {
                        // Shown until the follow-up read has the value; an
                        // overridden value then flips back by itself.
                        fields.pending.saved(&pending_key, generation);
                        if let Some(message) = super::overridden_message(&response) {
                            fields.notices.insert(done_key, message);
                        }
                    }
                    Err(message) => {
                        // Switches and choices show the stored value again;
                        // typed text stays next to the error.
                        fields.pending.cancel(&pending_key);
                        fields.errors.insert(done_key, message);
                    }
                }
                app.settings_fields_refresh();
            }),
        );
    }
}

/// A user edit on a field row.
enum FieldInput {
    Toggle(bool),
    Select(i32),
    Text(String),
    Reset,
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ConfigLayerSource;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn abs(path: &str) -> AbsolutePathBuf {
        let path = if cfg!(windows) {
            format!("C:{path}")
        } else {
            path.to_string()
        };
        AbsolutePathBuf::from_absolute_path(path).expect("absolute")
    }

    fn origins(entries: &[(&str, ConfigLayerSource)]) -> HashMap<String, ConfigLayerMetadata> {
        entries
            .iter()
            .map(|(key, source)| {
                (
                    (*key).to_string(),
                    ConfigLayerMetadata {
                        name: source.clone(),
                        version: "v".to_string(),
                    },
                )
            })
            .collect()
    }

    fn user() -> ConfigLayerSource {
        ConfigLayerSource::User {
            file: abs("/h/config.toml"),
            profile: None,
        }
    }

    fn project() -> ConfigLayerSource {
        ConfigLayerSource::Project {
            dot_codex_folder: abs("/r/.codex"),
        }
    }

    fn spec(path: &str, kind: FieldKind, default: Option<Value>) -> FieldSpec {
        FieldSpec {
            segments: path.split('.').map(str::to_string).collect(),
            section: "General".to_string(),
            description: "Help.".to_string(),
            kind,
            default,
        }
    }

    #[test]
    fn schema_rows_show_origins_and_lock_higher_layers() {
        let effective =
            json!({"sandbox_mode": "read-only", "hide_agent_reasoning": true, "model": "x"});
        let user_config = json!({"hide_agent_reasoning": true});
        let origins = origins(&[
            ("sandbox_mode", project()),
            ("hide_agent_reasoning", user()),
        ]);
        let inputs = FieldInputs {
            effective: &effective,
            user: &user_config,
            origins: &origins,
            requirements: None,
            features: &[],
        };
        let sandbox = schema_row(
            &spec(
                "sandbox_mode",
                FieldKind::Enum {
                    values: vec!["read-only".to_string(), "workspace-write".to_string()],
                    help: vec![String::new(), "Edits allowed.".to_string()],
                },
                None,
            ),
            &inputs,
        );
        assert_eq!(sandbox.origin, "Project config");
        assert!(sandbox.locked);
        assert!(sandbox.lock_reason.contains("takes precedence"));
        assert!(!sandbox.can_reset);
        assert_eq!(
            sandbox.options,
            vec!["Not set", "read-only", "workspace-write"]
        );
        assert_eq!(sandbox.option_index, 1);

        let hide = schema_row(
            &spec("hide_agent_reasoning", FieldKind::Bool, None),
            &inputs,
        );
        assert_eq!(hide.origin, "User config");
        assert_eq!(hide.origin_tone, 1);
        assert!(hide.checked);
        assert!(hide.can_reset);
        assert!(!hide.locked);

        let unset = schema_row(
            &spec(
                "check_for_update_on_startup",
                FieldKind::Bool,
                Some(json!(true)),
            ),
            &inputs,
        );
        assert_eq!(unset.origin, "Default");
        assert!(unset.checked, "unset bools show their default");
        assert!(!unset.can_reset);
    }

    #[test]
    fn enum_rows_keep_unknown_values_and_default_labels() {
        let effective = json!({"history": {"persistence": "archive"}});
        let empty = HashMap::new();
        let inputs = FieldInputs {
            effective: &effective,
            user: &effective,
            origins: &empty,
            requirements: None,
            features: &[],
        };
        let row = schema_row(
            &spec(
                "history.persistence",
                FieldKind::Enum {
                    values: vec!["save-all".to_string(), "none".to_string()],
                    help: vec![String::new(), String::new()],
                },
                Some(json!("save-all")),
            ),
            &inputs,
        );
        assert_eq!(row.title, "persistence");
        assert_eq!(row.path, "history.persistence");
        assert_eq!(row.key, "history.persistence");
        assert_eq!(
            row.options,
            vec!["Default (save-all)", "save-all", "none", "archive (custom)"]
        );
        assert_eq!(row.option_index, 3);
        assert_eq!(value_for_option(&row.editor, 0), FieldValue::Unset);
        assert_eq!(
            value_for_option(&row.editor, 2),
            FieldValue::Set(json!("none"))
        );
        assert_eq!(value_for_option(&row.editor, 9), FieldValue::NoChange);
    }

    #[test]
    fn snippet_rows_edit_only_the_user_layer() {
        let effective =
            json!({"mcp_servers": {"mine": {"command": "a"}, "team": {"url": "https://x"}}});
        let user_config = json!({"mcp_servers": {"mine": {"command": "a"}}});
        let origins = origins(&[
            ("mcp_servers.mine.command", user()),
            ("mcp_servers.team.url", project()),
        ]);
        let inputs = FieldInputs {
            effective: &effective,
            user: &user_config,
            origins: &origins,
            requirements: None,
            features: &[],
        };
        let row = schema_row(
            &spec("mcp_servers", FieldKind::Snippet, Some(json!({}))),
            &inputs,
        );
        assert!(
            !row.locked,
            "maps merge across layers; the user part stays editable"
        );
        assert_eq!(row.text, "[mcp_servers.mine]\ncommand = \"a\"");
        assert_eq!(row.summary, "mine, team");
        assert!(row.origin_detail.starts_with("Also set by Project config"));
        assert!(row.can_reset);
        assert_eq!(
            value_for_text(&row.editor, "[mcp_servers.x]\nurl = \"u\""),
            Ok(FieldValue::Set(json!({"x": {"url": "u"}})))
        );
        assert_eq!(value_for_text(&row.editor, ""), Ok(FieldValue::Unset));
        assert!(value_for_text(&row.editor, "[mcp_servers").is_err());
    }

    #[test]
    fn managed_requirements_lock_rows() {
        let effective = json!({"model_provider": "corp"});
        let empty = HashMap::new();
        let requirements: ConfigRequirements =
            serde_json::from_value(json!({"modelProvider": "corp"})).expect("requirements");
        let inputs = FieldInputs {
            effective: &effective,
            user: &Value::Null,
            origins: &empty,
            requirements: Some(&requirements),
            features: &[],
        };
        let row = schema_row(&spec("model_provider", FieldKind::Text, None), &inputs);
        assert!(row.locked);
        assert_eq!(row.origin, "Managed");
        assert!(row.lock_reason.contains("model_provider"));
    }

    #[test]
    fn text_values_parse_and_validate() {
        let int = Editor::Integer {
            min: Some(0.0),
            max: Some(100.0),
        };
        assert_eq!(value_for_text(&int, " 42 "), Ok(FieldValue::Set(json!(42))));
        assert_eq!(value_for_text(&int, "1_0"), Ok(FieldValue::Set(json!(10))));
        assert_eq!(value_for_text(&int, ""), Ok(FieldValue::Unset));
        assert_eq!(
            value_for_text(&int, "4.5"),
            Err("Enter a whole number.".to_string())
        );
        assert_eq!(
            value_for_text(&int, "101"),
            Err("Must be at most 100.".to_string())
        );
        assert_eq!(
            value_for_text(&int, "-1"),
            Err("Must be at least 0.".to_string())
        );
        let big = Editor::Integer {
            min: None,
            max: None,
        };
        assert_eq!(
            value_for_text(&big, "18446744073709551615"),
            Ok(FieldValue::Set(json!(u64::MAX)))
        );
        let number = Editor::Number {
            min: None,
            max: None,
        };
        assert_eq!(
            value_for_text(&number, "2.5"),
            Ok(FieldValue::Set(json!(2.5)))
        );
        assert!(value_for_text(&number, "x").is_err());
        assert!(value_for_text(&number, "inf").is_err());
        assert_eq!(value_for_text(&Editor::Text, ""), Ok(FieldValue::Unset));
        assert_eq!(
            value_for_text(&Editor::Text, "gpt"),
            Ok(FieldValue::Set(json!("gpt")))
        );
        assert_eq!(
            value_for_text(&Editor::ShellWords, r#"say "turn done""#),
            Ok(FieldValue::Set(json!(["say", "turn done"])))
        );
        assert!(value_for_text(&Editor::ShellWords, r#"say "open"#).is_err());
        if cfg!(windows) {
            assert_eq!(
                value_for_text(&Editor::ShellWords, r"C:\tools\notify.exe --flag"),
                Ok(FieldValue::Set(json!([r"C:\tools\notify.exe", "--flag"])))
            );
        }
    }

    #[test]
    fn range_texts() {
        assert_eq!(
            range_text(Some(0.0), Some(10.0)),
            Some("0 to 10".to_string())
        );
        assert_eq!(range_text(Some(0.0), None), None);
        assert_eq!(range_text(Some(1.0), None), Some("at least 1".to_string()));
        assert_eq!(range_text(None, Some(2.5)), Some("at most 2.5".to_string()));
    }

    fn test_model(slug: &str, default: bool, efforts: &[&str]) -> Model {
        serde_json::from_value(json!({
            "id": slug,
            "model": slug,
            "displayName": slug.to_uppercase(),
            "description": "",
            "hidden": false,
            "supportedReasoningEfforts": efforts
                .iter()
                .map(|effort| json!({"reasoningEffort": effort, "description": ""}))
                .collect::<Vec<_>>(),
            "defaultReasoningEffort": "medium",
            "isDefault": default,
        }))
        .expect("model")
    }

    #[test]
    fn common_rows_follow_the_model_catalog_and_requirements() {
        let effective =
            json!({"model": "b", "approval_policy": "on-request", "notify": ["say", "turn done"]});
        let origins = origins(&[
            ("model", user()),
            ("approval_policy", user()),
            ("notify", user()),
        ]);
        let requirements: ConfigRequirements =
            serde_json::from_value(json!({"allowedSandboxModes": ["read-only"]}))
                .expect("requirements");
        let inputs = FieldInputs {
            effective: &effective,
            user: &effective,
            origins: &origins,
            requirements: Some(&requirements),
            features: &[],
        };
        let models = vec![
            test_model("a", true, &["low", "high"]),
            test_model("b", false, &["minimal"]),
        ];
        let rows = common_rows(&inputs, &models);
        let row = |key: &str| rows.iter().find(|row| row.key == key).expect(key);

        let model_row = row("model");
        assert_eq!(model_row.options, vec!["Default (A)", "A", "B"]);
        assert_eq!(model_row.option_index, 2);
        assert_eq!(model_row.note, NEW_THREAD_NOTE);

        let effort = row("model_reasoning_effort");
        assert_eq!(effort.options, vec!["Default (medium)", "Minimal"]);
        assert_eq!(effort.option_index, 0);

        let approvals = row("approval_policy");
        assert_eq!(approvals.option_index, 2);
        assert_eq!(
            value_for_option(&approvals.editor, 3),
            FieldValue::Set(json!("never"))
        );

        let sandbox = row("sandbox_mode");
        assert_eq!(sandbox.options, vec!["Default", "Read only"]);
        assert!(sandbox.locked);
        assert_eq!(
            sandbox.lock_reason,
            "Your organization allows only Read only."
        );

        let notify = row("notify");
        assert_eq!(
            words::split_words(&notify.text),
            Some(vec!["say".to_string(), "turn done".to_string()])
        );
        assert_eq!(notify.editor, Editor::ShellWords);
    }

    #[test]
    fn common_rows_handle_custom_values() {
        let effective = json!({"approval_policy": {"granular": {"rules": true}}});
        let empty = HashMap::new();
        let inputs = FieldInputs {
            effective: &effective,
            user: &effective,
            origins: &empty,
            requirements: None,
            features: &[],
        };
        let rows = common_rows(&inputs, &[]);
        let approvals = rows
            .iter()
            .find(|row| row.key == "approval_policy")
            .expect("row");
        assert_eq!(approvals.options[0], "Custom value (see All settings)");
        assert_eq!(value_for_option(&approvals.editor, 0), FieldValue::NoChange);
        let model_row = rows.iter().find(|row| row.key == "model").expect("row");
        assert_eq!(
            model_row.editor,
            Editor::Text,
            "free text without a model catalog"
        );
    }

    #[test]
    fn all_rows_group_sections_and_filter() {
        let sections = vec![
            SectionSpec {
                name: "General".to_string(),
                description: String::new(),
                fields: vec![spec("model", FieldKind::Text, None)],
            },
            SectionSpec {
                name: "history".to_string(),
                description: "History.".to_string(),
                fields: vec![spec(
                    "history.max_bytes",
                    FieldKind::Integer {
                        min: None,
                        max: None,
                    },
                    None,
                )],
            },
        ];
        let empty = HashMap::new();
        let inputs = FieldInputs {
            effective: &Value::Null,
            user: &Value::Null,
            origins: &empty,
            requirements: None,
            features: &[],
        };
        let keys = |rows: Vec<FieldRow>| rows.into_iter().map(|row| row.key).collect::<Vec<_>>();
        assert_eq!(
            keys(all_rows(&sections, "", &inputs)),
            vec!["§General", "model", "§history", "history.max_bytes"]
        );
        assert_eq!(
            keys(all_rows(&sections, "max_b", &inputs)),
            vec!["§history", "history.max_bytes"]
        );
        assert!(all_rows(&sections, "zzz", &inputs).is_empty());
    }

    fn flag(name: &str, default_enabled: bool) -> ExperimentalFeature {
        serde_json::from_value(json!({
            "name": name,
            "stage": "stable",
            "displayName": null,
            "description": null,
            "announcement": null,
            "enabled": default_enabled,
            "defaultEnabled": default_enabled,
        }))
        .expect("feature")
    }

    #[test]
    fn feature_rows_show_the_feature_default_when_unset() {
        let features = vec![flag("unified_exec", true), flag("memories", false)];
        let effective = json!({"features": {"view_image": false}});
        let empty = HashMap::new();
        let inputs = FieldInputs {
            effective: &effective,
            user: &effective,
            origins: &empty,
            requirements: None,
            features: &features,
        };
        // The schema has no default for features; the flag's own default
        // applies, so default-on features do not show as off.
        let unified = schema_row(
            &spec("features.unified_exec", FieldKind::Bool, None),
            &inputs,
        );
        assert!(unified.checked);
        assert_eq!(unified.description, "Help. Default: on.");
        let memories = schema_row(&spec("features.memories", FieldKind::Bool, None), &inputs);
        assert!(!memories.checked);
        assert!(memories.description.ends_with("Default: off."));
        // An explicit value wins over the default.
        let view_image = schema_row(&spec("features.view_image", FieldKind::Bool, None), &inputs);
        assert!(!view_image.checked);
        // Other booleans are untouched.
        let other = schema_row(&spec("history.save", FieldKind::Bool, None), &inputs);
        assert_eq!(other.description, "Help.");
    }

    fn bool_row(key: &str, checked: bool) -> FieldRow {
        let mut row = FieldRow::new(Editor::Bool, key.to_string(), key.to_string());
        row.checked = checked;
        row
    }

    #[test]
    fn saving_rows_keep_their_controls_until_the_value_changes() {
        let mut list: RowList<FieldRow, FieldData> = RowList::default();
        let mut revision = 0;
        let mut publish =
            |list: &mut RowList<FieldRow, FieldData>, rows: Vec<FieldRow>, force: &[&str]| {
                let force: HashSet<String> = force.iter().map(ToString::to_string).collect();
                list.sync_controls(
                    rows,
                    field_key,
                    same_control,
                    &force,
                    &mut revision,
                    FieldRow::to_slint,
                );
                super::super::rows_of(&list.model)
                    .iter()
                    .map(|row| row.revision)
                    .collect::<Vec<_>>()
            };
        let mut text = FieldRow::new(Editor::Text, "model".to_string(), "Model".to_string());
        text.text = "gpt".to_string();
        let first = publish(&mut list, vec![bool_row("a", false), text.clone()], &[]);
        assert_eq!(first, vec![1, 2]);

        // The user turned `a` on: the pending value is shown while it saves.
        let mut saving = bool_row("a", false);
        apply_pending(&mut saving, Some(&PendingControl::Checked(true)));
        saving.busy = true;
        assert!(saving.checked);
        let mut busy_text = text.clone();
        busy_text.busy = true;
        assert_eq!(
            publish(&mut list, vec![saving, busy_text], &[]),
            vec![3, 2],
            "a busy flag alone does not reset what the user typed"
        );
        // An error next to typed text keeps the text; a failed switch flips
        // back because its value changes.
        let mut failed_text = text;
        failed_text.error = "Must be at most 100.".to_string();
        assert_eq!(
            publish(
                &mut list,
                vec![bool_row("a", false), failed_text.clone()],
                &[]
            ),
            vec![4, 2]
        );
        // A forced row always resyncs.
        assert_eq!(
            publish(
                &mut list,
                vec![bool_row("a", false), failed_text],
                &["model"]
            ),
            vec![4, 5]
        );
        // Pending values only apply to the matching kind of control.
        let mut choice = FieldRow::new(
            Editor::Enum {
                values: vec!["x".to_string()],
                zero_unsets: true,
            },
            "c".to_string(),
            "C".to_string(),
        );
        apply_pending(&mut choice, Some(&PendingControl::Checked(true)));
        assert!(
            !choice.checked,
            "pending switch values only apply to switches"
        );
        apply_pending(&mut choice, Some(&PendingControl::Option(1)));
        assert_eq!(choice.option_index, 1);
    }
}
