//! TOML helpers: syntax validation with line/column positions, JSON <-> TOML
//! value conversion, and the per-key snippets edited inline on the "All
//! settings" page.
//!
//! A snippet is a tiny TOML document holding exactly one key, the setting's
//! last key path segment, so it reads the way it would in `config.toml`:
//! `notify = ["say", "done"]` or `[mcp_servers.docs]` tables.

use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

/// A TOML syntax or shape problem, with a 1-based position when known.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TomlProblem {
    pub(crate) message: String,
    pub(crate) line: Option<usize>,
    pub(crate) column: Option<usize>,
}

impl std::fmt::Display for TomlProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.line, self.column) {
            (Some(line), Some(column)) => {
                write!(f, "Line {line}, column {column}: {}", self.message)
            }
            _ => f.write_str(&self.message),
        }
    }
}

impl TomlProblem {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            line: None,
            column: None,
        }
    }
}

/// 1-based line and column of byte `offset` in `text`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = before
        .get(line_start..)
        .map_or(0, |rest| rest.chars().count())
        + 1;
    (line, column)
}

/// Parses `text` as a TOML document.
pub(crate) fn parse_document(text: &str) -> Result<toml::Table, TomlProblem> {
    text.parse::<toml::Table>().map_err(|err| {
        let message = err.message().trim().to_string();
        match err.span() {
            Some(span) => {
                let (line, column) = line_column(text, span.start);
                TomlProblem {
                    message,
                    line: Some(line),
                    column: Some(column),
                }
            }
            None => TomlProblem::plain(message),
        }
    })
}

/// Converts JSON to TOML. Nulls (unset values) are dropped from tables and
/// arrays; a top-level null has no TOML form.
pub(crate) fn json_to_toml(value: &JsonValue) -> Option<TomlValue> {
    match value {
        JsonValue::Null => None,
        JsonValue::Bool(flag) => Some(TomlValue::Boolean(*flag)),
        JsonValue::Number(number) => {
            if let Some(int) = number.as_i64() {
                Some(TomlValue::Integer(int))
            } else if let Some(uint) = number.as_u64() {
                // TOML integers are i64; larger values degrade to floats.
                Some(TomlValue::Float(uint as f64))
            } else {
                number.as_f64().map(TomlValue::Float)
            }
        }
        JsonValue::String(text) => Some(TomlValue::String(text.clone())),
        JsonValue::Array(items) => Some(TomlValue::Array(
            items.iter().filter_map(json_to_toml).collect(),
        )),
        JsonValue::Object(map) => Some(TomlValue::Table(
            map.iter()
                .filter_map(|(key, value)| json_to_toml(value).map(|value| (key.clone(), value)))
                .collect(),
        )),
    }
}

/// Converts TOML to JSON. Datetimes become their TOML string form.
pub(crate) fn toml_to_json(value: &TomlValue) -> JsonValue {
    match value {
        TomlValue::String(text) => JsonValue::String(text.clone()),
        TomlValue::Integer(int) => JsonValue::from(*int),
        TomlValue::Float(float) => {
            serde_json::Number::from_f64(*float).map_or(JsonValue::Null, JsonValue::Number)
        }
        TomlValue::Boolean(flag) => JsonValue::Bool(*flag),
        TomlValue::Datetime(datetime) => JsonValue::String(datetime.to_string()),
        TomlValue::Array(items) => JsonValue::Array(items.iter().map(toml_to_json).collect()),
        TomlValue::Table(table) => JsonValue::Object(
            table
                .iter()
                .map(|(key, value)| (key.clone(), toml_to_json(value)))
                .collect(),
        ),
    }
}

/// Renders `key = value` (or a `[key]` table) for the snippet editor. An
/// unset value renders as an empty snippet.
pub(crate) fn snippet_for(key: &str, value: Option<&JsonValue>) -> String {
    let Some(value) = value.and_then(json_to_toml) else {
        return String::new();
    };
    let mut table = toml::Table::new();
    table.insert(key.to_string(), value);
    match toml::to_string(&table) {
        Ok(text) => text.trim_end().to_string(),
        Err(err) => format!("# could not render this value: {err}"),
    }
}

/// Parses a snippet back into the JSON value for `key`.
///
/// Returns `Ok(None)` when the snippet is empty (meaning "unset").
pub(crate) fn parse_snippet(key: &str, text: &str) -> Result<Option<JsonValue>, TomlProblem> {
    let mut table = parse_document(text)?;
    if table.is_empty() {
        return Ok(None);
    }
    let value = table
        .remove(key)
        .ok_or_else(|| TomlProblem::plain(format!("The snippet must set `{key}`.")))?;
    if let Some(extra) = table.keys().next() {
        return Err(TomlProblem::plain(format!(
            "Only `{key}` can be set here; remove `{extra}`."
        )));
    }
    Ok(Some(toml_to_json(&value)))
}

/// Short summary of a value for a collapsed snippet row.
pub(crate) fn summarize(value: Option<&JsonValue>) -> String {
    match value {
        None => "Not set".to_string(),
        Some(JsonValue::Array(items)) if items.is_empty() => "Empty list".to_string(),
        Some(JsonValue::Array(items)) => {
            let scalars: Option<Vec<String>> = items
                .iter()
                .map(|item| match item {
                    JsonValue::String(text) => Some(text.clone()),
                    JsonValue::Number(number) => Some(number.to_string()),
                    JsonValue::Bool(flag) => Some(flag.to_string()),
                    _ => None,
                })
                .collect();
            match scalars {
                Some(scalars) => crate::app::truncate_chars(&scalars.join(", "), 80),
                None => plural(items.len(), "item"),
            }
        }
        Some(JsonValue::Object(map)) if map.is_empty() => "Empty table".to_string(),
        Some(JsonValue::Object(map)) => {
            let names: Vec<&str> = map.keys().map(String::as_str).collect();
            crate::app::truncate_chars(&names.join(", "), 80)
        }
        Some(other) => crate::app::truncate_chars(&super::model::display_value(other), 80),
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn syntax_errors_report_line_and_column() {
        let problem = parse_document("model = \"a\"\nbad = \n").expect_err("invalid");
        assert_eq!(problem.line, Some(2));
        assert!(
            problem.to_string().starts_with("Line 2, column"),
            "{problem}"
        );
        let problem = parse_document("a = 1\na = 2\n").expect_err("duplicate");
        assert_eq!(problem.line, Some(2));
        assert!(parse_document("").expect("empty is valid").is_empty());
    }

    #[test]
    fn line_column_counts_characters() {
        assert_eq!(line_column("abc", 0), (1, 1));
        assert_eq!(line_column("ab\ncdé\nx", 7), (2, 4));
        assert_eq!(line_column("ab", 99), (1, 3));
    }

    #[test]
    fn json_and_toml_round_trip() {
        let original = json!({
            "model": "gpt",
            "notify": ["say", "done"],
            "history": {"max_bytes": 1024, "persistence": "none"},
            "ratio": 0.5,
            "enabled": true,
            "unset": null,
        });
        let toml = json_to_toml(&original).expect("object converts");
        let back = toml_to_json(&toml);
        assert_eq!(
            back,
            json!({
                "model": "gpt",
                "notify": ["say", "done"],
                "history": {"max_bytes": 1024, "persistence": "none"},
                "ratio": 0.5,
                "enabled": true,
            })
        );
        assert_eq!(json_to_toml(&JsonValue::Null), None);
        assert_eq!(
            json_to_toml(&json!([1, null, 2])),
            Some(TomlValue::Array(vec![
                TomlValue::Integer(1),
                TomlValue::Integer(2),
            ]))
        );
        assert_eq!(
            json_to_toml(&json!(u64::MAX)),
            Some(TomlValue::Float(u64::MAX as f64))
        );
    }

    #[test]
    fn toml_datetimes_become_strings() {
        let table = parse_document("when = 1979-05-27T07:32:00Z").expect("valid");
        assert_eq!(
            toml_to_json(&TomlValue::Table(table)),
            json!({"when": "1979-05-27T07:32:00Z"})
        );
    }

    #[test]
    fn snippets_render_and_parse_one_key() {
        assert_eq!(
            snippet_for("notify", Some(&json!(["say", "done"]))),
            r#"notify = ["say", "done"]"#
        );
        let servers = json!({"docs": {"command": "npx", "args": ["-y", "docs"]}});
        let snippet = snippet_for("mcp_servers", Some(&servers));
        assert!(snippet.contains("[mcp_servers.docs]"), "{snippet}");
        assert_eq!(parse_snippet("mcp_servers", &snippet), Ok(Some(servers)));
        assert_eq!(snippet_for("notify", None), "");
        assert_eq!(parse_snippet("notify", "  \n# nothing\n"), Ok(None));
        assert_eq!(
            parse_snippet("notify", "other = 1").map_err(|problem| problem.message),
            Err("The snippet must set `notify`.".to_string())
        );
        assert_eq!(
            parse_snippet("notify", "notify = []\nother = 1").map_err(|problem| problem.message),
            Err("Only `notify` can be set here; remove `other`.".to_string())
        );
        assert!(parse_snippet("notify", "notify = [").is_err());
    }

    #[test]
    fn summaries_are_short() {
        assert_eq!(summarize(None), "Not set");
        assert_eq!(summarize(Some(&json!(["a", "b"]))), "a, b");
        assert_eq!(summarize(Some(&json!([{"a": 1}]))), "1 item");
        assert_eq!(summarize(Some(&json!({"x": 1, "y": 2}))), "x, y");
        assert_eq!(summarize(Some(&json!({}))), "Empty table");
        assert_eq!(summarize(Some(&json!(true))), "true");
    }
}
