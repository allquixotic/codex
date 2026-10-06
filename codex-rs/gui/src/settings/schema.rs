//! Maps the `config.toml` JSON schema (`codex_config::schema`) to the form
//! model rendered by the "All settings" page.
//!
//! Schema shapes (see the research notes on `config.schema.json`): every key
//! is optional, refs are `#/definitions/<Name>`, and a ref that carries a
//! description or default is wrapped as a single-element `allOf`. Struct-like
//! objects (fixed `properties`) are expanded into one row per property up to
//! [`MAX_DEPTH`] segments; booleans, string enums, strings, and numbers get
//! native controls; everything else (maps, arrays, `anyOf`, mixed `oneOf`)
//! is edited as an inline TOML snippet.

use serde_json::Map;
use serde_json::Value;

/// Top-level keys the GUI never shows: legacy profiles cannot be written over
/// RPC, `tui` is terminal-only, and `notice`/`projects` are managed by
/// dedicated flows (notices, project trust).
pub(crate) const HIDDEN_KEYS: &[&str] = &["profile", "profiles", "tui", "notice", "projects"];

/// Section holding top-level keys that are not struct-like tables.
pub(crate) const GENERAL_SECTION: &str = "General";

/// Deepest key path expanded into individual rows.
const MAX_DEPTH: usize = 3;

/// Control used to edit one setting.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FieldKind {
    Bool,
    /// String enum (plain `enum` or a `oneOf` of single-value enums).
    Enum {
        values: Vec<String>,
        /// Per-value help from `oneOf` variant descriptions (may be empty).
        help: Vec<String>,
    },
    Text,
    Integer {
        min: Option<f64>,
        max: Option<f64>,
    },
    Number {
        min: Option<f64>,
        max: Option<f64>,
    },
    /// Inline TOML snippet editor.
    Snippet,
}

/// One editable setting.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FieldSpec {
    pub(crate) segments: Vec<String>,
    pub(crate) section: String,
    pub(crate) description: String,
    pub(crate) kind: FieldKind,
    pub(crate) default: Option<Value>,
}

/// A section of the "All settings" page: a top-level key (or "General").
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SectionSpec {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) fields: Vec<FieldSpec>,
}

/// The JSON schema for `config.toml`, generated from the running binary's
/// `ConfigToml`.
pub(crate) fn config_schema() -> Result<Value, String> {
    serde_json::to_value(codex_config::schema::config_schema()).map_err(|err| err.to_string())
}

/// Builds the sections of the "All settings" page from the schema.
pub(crate) fn build_sections(schema: &Value) -> Vec<SectionSpec> {
    let empty = Map::new();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut general = SectionSpec {
        name: GENERAL_SECTION.to_string(),
        description: "Top-level settings.".to_string(),
        fields: Vec::new(),
    };
    let mut sections = Vec::new();
    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort();
    for key in keys {
        if HIDDEN_KEYS.contains(&key.as_str()) {
            continue;
        }
        let node = Resolved::new(&properties[key], schema);
        if node.is_struct() {
            let mut section = SectionSpec {
                name: key.clone(),
                description: node.description.clone(),
                fields: Vec::new(),
            };
            collect_struct_fields(
                &node,
                schema,
                std::slice::from_ref(key),
                key,
                &mut section.fields,
            );
            if !section.fields.is_empty() {
                sections.push(section);
            }
        } else {
            general.fields.push(leaf_field(
                &node,
                schema,
                vec![key.clone()],
                GENERAL_SECTION,
            ));
        }
    }
    sections.insert(0, general);
    sections
}

fn collect_struct_fields(
    node: &Resolved<'_>,
    root: &Value,
    prefix: &[String],
    section: &str,
    out: &mut Vec<FieldSpec>,
) {
    let Some(properties) = node.schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort();
    for name in names {
        let child = Resolved::new(&properties[name], root);
        let mut segments = prefix.to_vec();
        segments.push(name.clone());
        if child.is_struct() && segments.len() < MAX_DEPTH {
            collect_struct_fields(&child, root, &segments, section, out);
        } else {
            out.push(leaf_field(&child, root, segments, section));
        }
    }
}

fn leaf_field(
    node: &Resolved<'_>,
    root: &Value,
    segments: Vec<String>,
    section: &str,
) -> FieldSpec {
    FieldSpec {
        segments,
        section: section.to_string(),
        description: node.description.clone(),
        kind: classify(node, root),
        default: node.default.clone(),
    }
}

/// Chooses the control for a resolved schema node.
pub(crate) fn classify(node: &Resolved<'_>, root: &Value) -> FieldKind {
    let schema = node.schema;
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && let Some(values) = string_values(values)
    {
        return FieldKind::Enum {
            help: vec![String::new(); values.len()],
            values,
        };
    }
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array)
        && let Some((values, help)) = enum_variants(variants, root)
    {
        return FieldKind::Enum { values, help };
    }
    if schema.get("anyOf").is_some() || schema.get("oneOf").is_some() {
        return FieldKind::Snippet;
    }
    let bounds = |key: &str| schema.get(key).and_then(Value::as_f64);
    match schema.get("type").and_then(Value::as_str) {
        Some("boolean") => FieldKind::Bool,
        Some("string") => FieldKind::Text,
        Some("integer") => FieldKind::Integer {
            min: bounds("minimum"),
            max: bounds("maximum"),
        },
        Some("number") => FieldKind::Number {
            min: bounds("minimum"),
            max: bounds("maximum"),
        },
        _ => FieldKind::Snippet,
    }
}

fn string_values(values: &[Value]) -> Option<Vec<String>> {
    values
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

/// Values and per-value help when every `oneOf` variant is a string enum.
fn enum_variants(variants: &[Value], root: &Value) -> Option<(Vec<String>, Vec<String>)> {
    let mut values = Vec::new();
    let mut help = Vec::new();
    for variant in variants {
        let variant = Resolved::new(variant, root);
        let variant_values = string_values(variant.schema.get("enum")?.as_array()?)?;
        for value in variant_values {
            values.push(value);
            help.push(variant.description.clone());
        }
    }
    (!values.is_empty()).then_some((values, help))
}

/// A schema node with `$ref` and single-element `allOf` wrappers resolved;
/// the outermost description and default win.
pub(crate) struct Resolved<'a> {
    pub(crate) schema: &'a Value,
    pub(crate) description: String,
    pub(crate) default: Option<Value>,
}

impl<'a> Resolved<'a> {
    pub(crate) fn new(node: &'a Value, root: &'a Value) -> Self {
        let mut description = node_description(node);
        let mut default = node
            .get("default")
            .filter(|value| !value.is_null())
            .cloned();
        let mut current = node;
        // Bounded to survive malformed (cyclic) schemas.
        for _ in 0..16 {
            let next = if let Some(target) = current.get("$ref").and_then(Value::as_str) {
                resolve_ref(target, root)
            } else if let Some([only]) = current
                .get("allOf")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
            {
                Some(only)
            } else {
                None
            };
            let Some(next) = next else {
                break;
            };
            current = next;
            if description.is_empty() {
                description = node_description(current);
            }
            if default.is_none() {
                default = current
                    .get("default")
                    .filter(|value| !value.is_null())
                    .cloned();
            }
        }
        Self {
            schema: current,
            description,
            default,
        }
    }

    /// Struct-like table: fixed `properties` and no schema for extra keys.
    fn is_struct(&self) -> bool {
        let has_properties = self
            .schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| !properties.is_empty());
        let open_map = self
            .schema
            .get("additionalProperties")
            .is_some_and(Value::is_object);
        has_properties
            && !open_map
            && self.schema.get("anyOf").is_none()
            && self.schema.get("oneOf").is_none()
    }
}

fn node_description(node: &Value) -> String {
    node.get("description")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn resolve_ref<'a>(target: &str, root: &'a Value) -> Option<&'a Value> {
    let name = target.strip_prefix("#/definitions/")?;
    root.get("definitions")?.get(name)
}

/// Case-insensitive match of `query` against a field's key path and help.
pub(crate) fn field_matches(field: &FieldSpec, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let path = field.segments.join(".").to_lowercase();
    if path.contains(&query) || field.description.to_lowercase().contains(&query) {
        return true;
    }
    match &field.kind {
        FieldKind::Enum { values, .. } => values
            .iter()
            .any(|value| value.to_lowercase().contains(&query)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "hide_agent_reasoning": {"type": "boolean", "description": "Hide reasoning."},
                "sandbox_mode": {
                    "allOf": [{"$ref": "#/definitions/SandboxMode"}],
                    "description": "Sandbox mode to use."
                },
                "model_reasoning_summary": {"$ref": "#/definitions/ReasoningSummary"},
                "model": {"type": "string", "description": "Model."},
                "model_context_window": {"type": "integer", "format": "int64"},
                "tool_output_token_limit": {"type": "integer", "minimum": 0.0},
                "mcp_servers": {
                    "type": "object",
                    "additionalProperties": {"$ref": "#/definitions/RawMcpServerConfig"},
                    "default": {}
                },
                "notify": {"type": "array", "items": {"type": "string"}},
                "approval_policy": {"allOf": [{"$ref": "#/definitions/AskForApproval"}]},
                "history": {
                    "allOf": [{"$ref": "#/definitions/History"}],
                    "description": "History settings."
                },
                "profiles": {"type": "object", "additionalProperties": {"type": "object"}},
                "tui": {"allOf": [{"$ref": "#/definitions/History"}]},
                "notifications": {"anyOf": [{"type": "boolean"}, {"type": "array"}]}
            },
            "definitions": {
                "SandboxMode": {"type": "string", "enum": ["read-only", "workspace-write"]},
                "ReasoningSummary": {
                    "oneOf": [
                        {"type": "string", "enum": ["auto", "concise"]},
                        {"type": "string", "enum": ["none"], "description": "Disable summaries."}
                    ]
                },
                "AskForApproval": {
                    "oneOf": [
                        {"type": "string", "enum": ["on-request"]},
                        {"type": "object", "properties": {"granular": {}}, "required": ["granular"]}
                    ]
                },
                "History": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "max_bytes": {"type": "integer", "minimum": 0.0, "maximum": 100.0},
                        "persistence": {
                            "allOf": [{"$ref": "#/definitions/Persistence"}],
                            "default": "save-all"
                        }
                    }
                },
                "Persistence": {
                    "oneOf": [
                        {"type": "string", "enum": ["save-all"], "description": "Save all."},
                        {"type": "string", "enum": ["none"], "description": "Save nothing."}
                    ]
                },
                "RawMcpServerConfig": {"type": "object", "properties": {"command": {"type": "string"}}}
            }
        })
    }

    fn field<'a>(sections: &'a [SectionSpec], path: &str) -> &'a FieldSpec {
        sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|field| field.segments.join(".") == path)
            .unwrap_or_else(|| panic!("missing field {path}"))
    }

    #[test]
    fn maps_scalars_enums_and_snippets() {
        let schema = schema();
        let sections = build_sections(&schema);
        assert_eq!(
            field(&sections, "hide_agent_reasoning").kind,
            FieldKind::Bool
        );
        assert_eq!(
            field(&sections, "hide_agent_reasoning").description,
            "Hide reasoning."
        );
        assert_eq!(field(&sections, "model").kind, FieldKind::Text);
        assert_eq!(
            field(&sections, "model_context_window").kind,
            FieldKind::Integer {
                min: None,
                max: None
            }
        );
        assert_eq!(
            field(&sections, "tool_output_token_limit").kind,
            FieldKind::Integer {
                min: Some(0.0),
                max: None
            }
        );
        // allOf-wrapped ref keeps the wrapper description.
        let sandbox = field(&sections, "sandbox_mode");
        assert_eq!(
            sandbox.kind,
            FieldKind::Enum {
                values: vec!["read-only".to_string(), "workspace-write".to_string()],
                help: vec![String::new(), String::new()],
            }
        );
        assert_eq!(sandbox.description, "Sandbox mode to use.");
        // oneOf of single-value enums becomes one ComboBox with per-value help.
        assert_eq!(
            field(&sections, "model_reasoning_summary").kind,
            FieldKind::Enum {
                values: vec![
                    "auto".to_string(),
                    "concise".to_string(),
                    "none".to_string()
                ],
                help: vec![
                    String::new(),
                    String::new(),
                    "Disable summaries.".to_string()
                ],
            }
        );
        // Mixed oneOf, maps, arrays, and anyOf use the TOML snippet editor.
        assert_eq!(field(&sections, "approval_policy").kind, FieldKind::Snippet);
        assert_eq!(field(&sections, "mcp_servers").kind, FieldKind::Snippet);
        assert_eq!(field(&sections, "mcp_servers").default, Some(json!({})));
        assert_eq!(field(&sections, "notify").kind, FieldKind::Snippet);
        assert_eq!(field(&sections, "notifications").kind, FieldKind::Snippet);
    }

    #[test]
    fn struct_tables_become_sections_and_hidden_keys_are_skipped() {
        let schema = schema();
        let sections = build_sections(&schema);
        let names: Vec<&str> = sections
            .iter()
            .map(|section| section.name.as_str())
            .collect();
        assert_eq!(names, vec![GENERAL_SECTION, "history"]);
        let history = &sections[1];
        assert_eq!(history.description, "History settings.");
        assert_eq!(
            history
                .fields
                .iter()
                .map(|f| f.segments.join("."))
                .collect::<Vec<_>>(),
            vec![
                "history.max_bytes".to_string(),
                "history.persistence".to_string()
            ]
        );
        assert_eq!(
            field(&sections, "history.max_bytes").kind,
            FieldKind::Integer {
                min: Some(0.0),
                max: Some(100.0)
            }
        );
        let persistence = field(&sections, "history.persistence");
        assert_eq!(persistence.default, Some(json!("save-all")));
        assert!(
            matches!(&persistence.kind, FieldKind::Enum { values, .. } if values == &["save-all", "none"])
        );
        assert!(
            sections
                .iter()
                .flat_map(|section| &section.fields)
                .all(|field| !HIDDEN_KEYS.contains(&field.segments[0].as_str()))
        );
    }

    #[test]
    fn real_schema_builds_and_covers_common_keys() {
        let schema = config_schema().expect("schema generates");
        let sections = build_sections(&schema);
        assert!(sections.len() > 5);
        assert!(matches!(
            field(&sections, "sandbox_mode").kind,
            FieldKind::Enum { .. }
        ));
        assert!(matches!(
            field(&sections, "web_search").kind,
            FieldKind::Enum { .. }
        ));
        assert_eq!(field(&sections, "model").kind, FieldKind::Text);
        assert_eq!(field(&sections, "mcp_servers").kind, FieldKind::Snippet);
        assert!(matches!(
            field(&sections, "history.persistence").kind,
            FieldKind::Enum { .. }
        ));
        assert!(
            sections
                .iter()
                .flat_map(|section| &section.fields)
                .all(|field| !HIDDEN_KEYS.contains(&field.segments[0].as_str())
                    && field.segments.len() <= MAX_DEPTH)
        );
    }

    #[test]
    fn search_matches_path_description_and_values() {
        let spec = FieldSpec {
            segments: vec!["history".to_string(), "persistence".to_string()],
            section: "history".to_string(),
            description: "Whether to save history.".to_string(),
            kind: FieldKind::Enum {
                values: vec!["save-all".to_string()],
                help: vec![String::new()],
            },
            default: None,
        };
        assert!(field_matches(&spec, ""));
        assert!(field_matches(&spec, "HISTORY.pers"));
        assert!(field_matches(&spec, "save history"));
        assert!(field_matches(&spec, "save-all"));
        assert!(!field_matches(&spec, "sandbox"));
    }
}
