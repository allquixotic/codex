//! Pure helpers shared by the settings pages: config key paths, origin
//! labels, JSON lookups, and managed-requirement pins.
//!
//! Key paths follow the app-server's `parse_key_path` rules: segments are
//! split on `.`, and a segment starting with `"` is quoted until the next
//! unescaped `"`, with `\` escaping the next character. The `origins` map of
//! `config/read` joins leaf segments with `.` without quoting, so origin
//! lookups use [`origin_key`] instead.

use std::collections::HashMap;

use codex_app_server_protocol::ConfigLayerMetadata;
use codex_app_server_protocol::ConfigLayerSource;
use codex_app_server_protocol::ConfigRequirements;
use serde_json::Value;

/// Precedence of the base user layer (`$CODEX_HOME/config.toml`). Layers
/// above it override anything the GUI writes.
pub(crate) const USER_LAYER_PRECEDENCE: i16 = 20;

/// Whether `segment` must be quoted to survive `parse_key_path`.
fn needs_quoting(segment: &str) -> bool {
    segment.is_empty()
        || !segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

/// Quotes one key path segment, escaping `"` and `\`.
pub(crate) fn quote_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len() + 2);
    out.push('"');
    for ch in segment.chars() {
        if matches!(ch, '"' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Joins segments into a key path, quoting only segments that need it.
pub(crate) fn key_path<S: AsRef<str>>(segments: &[S]) -> String {
    segments
        .iter()
        .map(|segment| {
            let segment = segment.as_ref();
            if needs_quoting(segment) {
                quote_segment(segment)
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// `features."<name>"`, the form the TUI uses to persist feature toggles.
pub(crate) fn feature_key_path(name: &str) -> String {
    format!("features.{}", quote_segment(name))
}

/// Splits a key path the way the app-server does.
pub(crate) fn parse_key_path(path: &str) -> Result<Vec<String>, String> {
    if path.trim().is_empty() {
        return Err("key path must not be empty".to_string());
    }
    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut chars = path.chars();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' if segment.is_empty() && !quoted => quoted = true,
            '"' if quoted => quoted = false,
            '\\' if quoted => {
                let Some(escaped) = chars.next() else {
                    return Err("unterminated escape in key path".to_string());
                };
                segment.push(escaped);
            }
            '.' if !quoted => {
                if segment.is_empty() {
                    return Err("key path segments must not be empty".to_string());
                }
                segments.push(std::mem::take(&mut segment));
            }
            '"' => return Err("invalid quoted key path segment".to_string()),
            _ => segment.push(ch),
        }
    }
    if quoted {
        return Err("unterminated quoted key path segment".to_string());
    }
    if segment.is_empty() {
        return Err("key path segments must not be empty".to_string());
    }
    segments.push(segment);
    Ok(segments)
}

/// Unquoted dot-join, the form used as keys of the `config/read` origins map.
pub(crate) fn origin_key<S: AsRef<str>>(segments: &[S]) -> String {
    segments
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join(".")
}

/// Value at `segments` inside a JSON object tree.
pub(crate) fn lookup<'a, S: AsRef<str>>(value: &'a Value, segments: &[S]) -> Option<&'a Value> {
    let mut current = value;
    for segment in segments {
        current = current.as_object()?.get(segment.as_ref())?;
    }
    (!current.is_null()).then_some(current)
}

/// Layers that supplied `segments` or any leaf below it, highest precedence
/// first, without duplicates.
pub(crate) fn origins_for<'a, S: AsRef<str>>(
    origins: &'a HashMap<String, ConfigLayerMetadata>,
    segments: &[S],
) -> Vec<&'a ConfigLayerMetadata> {
    let key = origin_key(segments);
    let prefix = format!("{key}.");
    let mut found: Vec<&ConfigLayerMetadata> = origins
        .iter()
        .filter(|(path, _)| **path == key || path.starts_with(&prefix))
        .map(|(_, metadata)| metadata)
        .collect();
    found.sort_by_key(|metadata| std::cmp::Reverse(metadata.name.precedence()));
    found.dedup_by(|a, b| a.name == b.name);
    found
}

/// Short label for a layer, shown next to a value.
pub(crate) fn origin_label(source: &ConfigLayerSource) -> String {
    match source {
        ConfigLayerSource::PackagedDefaults { .. } => "Packaged default".to_string(),
        ConfigLayerSource::Mdm { .. } => "Managed (MDM)".to_string(),
        ConfigLayerSource::System { .. } => "System config".to_string(),
        ConfigLayerSource::EnterpriseManaged { name, .. } => format!("Enterprise: {name}"),
        ConfigLayerSource::User { profile: None, .. } => "User config".to_string(),
        ConfigLayerSource::User {
            profile: Some(profile),
            ..
        } => format!("Profile {profile}"),
        ConfigLayerSource::Project { .. } => "Project config".to_string(),
        ConfigLayerSource::SessionFlags => "Command line (-c)".to_string(),
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. } => "Managed config".to_string(),
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => "Managed config (MDM)".to_string(),
    }
}

/// Long description of a layer including its file or source.
pub(crate) fn origin_detail(source: &ConfigLayerSource) -> String {
    let path =
        |path: &codex_utils_absolute_path::AbsolutePathBuf| super::short_path(path.as_path());
    match source {
        ConfigLayerSource::PackagedDefaults { file } => {
            format!("Packaged defaults ({})", path(file))
        }
        ConfigLayerSource::Mdm { domain, key } => format!("MDM profile {domain} ({key})"),
        ConfigLayerSource::System { file } => format!("System config ({})", path(file)),
        ConfigLayerSource::EnterpriseManaged { id, name } => {
            format!("Enterprise-managed layer {name} ({id})")
        }
        ConfigLayerSource::User {
            file,
            profile: None,
        } => format!("User config ({})", path(file)),
        ConfigLayerSource::User {
            file,
            profile: Some(profile),
        } => format!("Profile {profile} ({})", path(file)),
        ConfigLayerSource::Project { dot_codex_folder } => {
            format!("Project config ({})", path(dot_codex_folder))
        }
        ConfigLayerSource::SessionFlags => "Command-line -c overrides".to_string(),
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => {
            format!("managed_config.toml ({})", path(file))
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            "managed_config.toml delivered by MDM".to_string()
        }
    }
}

/// Whether values from `source` win over the user config the GUI writes.
pub(crate) fn overrides_user(source: &ConfigLayerSource) -> bool {
    source.precedence() > USER_LAYER_PRECEDENCE
}

/// Whether `source` is the base user config file.
pub(crate) fn is_base_user_layer(source: &ConfigLayerSource) -> bool {
    matches!(source, ConfigLayerSource::User { profile: None, .. })
}

/// Where the effective value of a setting comes from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ValueSource {
    /// Nothing sets it; the built-in default applies.
    Default,
    /// The user config file sets it.
    User,
    /// A lower-precedence layer (system, MDM) sets it; the user can override.
    Lower(ConfigLayerSource),
    /// A higher-precedence layer (project, profile, -c, managed) sets it.
    Higher(ConfigLayerSource),
}

impl ValueSource {
    /// Classifies the winning origin of `segments`.
    pub(crate) fn of<S: AsRef<str>>(
        origins: &HashMap<String, ConfigLayerMetadata>,
        segments: &[S],
    ) -> Self {
        match origins_for(origins, segments).first() {
            None => Self::Default,
            Some(metadata) if is_base_user_layer(&metadata.name) => Self::User,
            Some(metadata) if overrides_user(&metadata.name) => Self::Higher(metadata.name.clone()),
            Some(metadata) => Self::Lower(metadata.name.clone()),
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Self::Default => "Default".to_string(),
            Self::User => "User config".to_string(),
            Self::Lower(source) | Self::Higher(source) => origin_label(source),
        }
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Default => {
                "Not set in any config file; the built-in default applies.".to_string()
            }
            Self::User => "Set in your config.toml.".to_string(),
            Self::Lower(source) => format!(
                "Set by {}. Changing it here overrides that value in your config.toml.",
                origin_detail(source)
            ),
            Self::Higher(source) => format!(
                "Set by {}, which takes precedence over your config.toml. Edit it there.",
                origin_detail(source)
            ),
        }
    }
}

/// Requirement field that pins `segments` to a managed value, mirroring
/// `ConfigRequirementsToml::exact_requirement_for_config_path`, plus pinned
/// feature flags.
pub(crate) fn requirement_pin<S: AsRef<str>>(
    requirements: &ConfigRequirements,
    segments: &[S],
) -> Option<String> {
    let segments: Vec<&str> = segments.iter().map(AsRef::as_ref).collect();
    let overlaps = |managed: &[&str]| {
        segments
            .iter()
            .zip(managed)
            .all(|(segment, managed)| segment == managed)
    };
    if let Some(providers) = requirements.model_providers.as_ref()
        && let Some(id) = providers
            .keys()
            .find(|id| overlaps(&["model_providers", id.as_str()]))
    {
        return Some(format!("model_providers.{id}"));
    }
    let managed: [(bool, &[&str], &str); 9] = [
        (
            requirements.model_provider.is_some(),
            &["model_provider"],
            "model_provider",
        ),
        (
            requirements.sqlite_home.is_some(),
            &["sqlite_home"],
            "sqlite_home",
        ),
        (requirements.log_dir.is_some(), &["log_dir"], "log_dir"),
        (
            requirements.model_catalog_json.is_some(),
            &["model_catalog_json"],
            "model_catalog_json",
        ),
        (
            requirements.check_for_update_on_startup.is_some(),
            &["check_for_update_on_startup"],
            "check_for_update_on_startup",
        ),
        (
            requirements.allow_login_shell.is_some(),
            &["allow_login_shell"],
            "allow_login_shell",
        ),
        (
            requirements
                .feedback
                .as_ref()
                .and_then(|feedback| feedback.enabled)
                .is_some(),
            &["feedback", "enabled"],
            "feedback.enabled",
        ),
        (
            requirements.cli_auth_credentials_store.is_some(),
            &["cli_auth_credentials_store"],
            "cli_auth_credentials_store",
        ),
        (
            requirements.chatgpt_base_url.is_some(),
            &["chatgpt_base_url"],
            "chatgpt_base_url",
        ),
    ];
    if let Some((_, _, field)) = managed
        .iter()
        .find(|(is_managed, path, _)| *is_managed && overlaps(path))
    {
        return Some((*field).to_string());
    }
    if let ["features", name, ..] = segments.as_slice()
        && requirements
            .feature_requirements
            .as_ref()
            .is_some_and(|features| features.contains_key(*name))
    {
        return Some(format!("features.{name}"));
    }
    None
}

/// One-line display of a JSON value (strings unquoted).
pub(crate) fn display_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

/// Longest run without a break opportunity before one is inserted.
const MAX_UNBROKEN: usize = 32;

/// Error text for an inline label: whitespace collapsed, capped in length,
/// and long unbreakable runs (type paths, URLs) given zero-width break
/// opportunities so they wrap instead of widening the row.
pub(crate) fn display_error(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped = crate::app::truncate_chars(&collapsed, 320);
    let mut out = String::with_capacity(capped.len());
    let mut run = 0;
    let mut chars = capped.chars().peekable();
    while let Some(ch) = chars.next() {
        out.push(ch);
        if ch.is_whitespace() {
            run = 0;
            continue;
        }
        run += 1;
        // Break after a separator, but never inside `::`.
        let separator = matches!(ch, '/' | ':' | '<' | ',' | '=') && chars.peek() != Some(&':');
        if run >= MAX_UNBROKEN || (run > 8 && separator) {
            out.push('\u{200B}');
            run = 0;
        }
    }
    out
}

/// Wire string of a serde enum (`"on-request"`, `"workspace-write"`...).
pub(crate) fn wire_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

/// Error code of a failed config write (`configVersionConflict`, ...).
pub(crate) fn config_write_error_code(
    error: &codex_app_server_protocol::JSONRPCErrorError,
) -> Option<&str> {
    error
        .data
        .as_ref()?
        .get("config_write_error_code")?
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn abs(path: &str) -> AbsolutePathBuf {
        let path = if cfg!(windows) {
            format!("C:{path}")
        } else {
            path.to_string()
        };
        AbsolutePathBuf::from_absolute_path(path).unwrap_or_else(|err| panic!("{err}"))
    }

    fn user() -> ConfigLayerSource {
        ConfigLayerSource::User {
            file: abs("/home/me/.codex/config.toml"),
            profile: None,
        }
    }

    fn project() -> ConfigLayerSource {
        ConfigLayerSource::Project {
            dot_codex_folder: abs("/work/repo/.codex"),
        }
    }

    fn metadata(name: ConfigLayerSource) -> ConfigLayerMetadata {
        ConfigLayerMetadata {
            name,
            version: "sha256:0".to_string(),
        }
    }

    #[test]
    fn key_paths_quote_only_when_needed() {
        assert_eq!(key_path(&["model"]), "model");
        assert_eq!(
            key_path(&["mcp_servers", "docs-server", "enabled"]),
            "mcp_servers.docs-server.enabled"
        );
        assert_eq!(
            key_path(&["projects", "/abs/repo", "trust_level"]),
            r#"projects."/abs/repo".trust_level"#
        );
        assert_eq!(
            key_path(&["plugins", "sample@catalog"]),
            r#"plugins."sample@catalog""#
        );
        assert_eq!(key_path(&["a", r#"we"ird\"#]), r#"a."we\"ird\\""#);
        assert_eq!(feature_key_path("x"), r#"features."x""#);
    }

    #[test]
    fn key_paths_round_trip_through_the_server_parser() {
        for segments in [
            vec!["projects", "/abs/a.b", "trust_level"],
            vec!["features", "x"],
            vec!["hooks", "state"],
            vec!["a", r#"q"uo\te"#, "b.c"],
        ] {
            let path = key_path(&segments);
            assert_eq!(
                parse_key_path(&path),
                Ok(segments.iter().map(ToString::to_string).collect())
            );
        }
        assert_eq!(
            parse_key_path(&feature_key_path("x")),
            Ok(vec!["features".to_string(), "x".to_string()])
        );
        assert!(parse_key_path("a..b").is_err());
        assert!(parse_key_path(r#"a."open"#).is_err());
        assert!(parse_key_path("").is_err());
    }

    #[test]
    fn origin_labels_name_the_layer() {
        assert_eq!(origin_label(&user()), "User config");
        assert_eq!(origin_label(&project()), "Project config");
        assert_eq!(
            origin_label(&ConfigLayerSource::User {
                file: abs("/home/me/.codex/work.config.toml"),
                profile: Some("work".to_string()),
            }),
            "Profile work"
        );
        assert_eq!(
            origin_label(&ConfigLayerSource::EnterpriseManaged {
                id: "l1".to_string(),
                name: "Acme".to_string(),
            }),
            "Enterprise: Acme"
        );
        assert_eq!(
            origin_label(&ConfigLayerSource::SessionFlags),
            "Command line (-c)"
        );
        assert!(origin_detail(&project()).contains("repo"));
        assert!(origin_detail(&project()).starts_with("Project config ("));
    }

    #[test]
    fn value_source_picks_the_highest_layer_of_any_leaf() {
        let mut origins = HashMap::new();
        origins.insert("model".to_string(), metadata(user()));
        origins.insert("history.persistence".to_string(), metadata(user()));
        origins.insert("history.max_bytes".to_string(), metadata(project()));
        origins.insert(
            "sandbox_mode".to_string(),
            metadata(ConfigLayerSource::System {
                file: abs("/etc/codex/config.toml"),
            }),
        );
        assert_eq!(ValueSource::of(&origins, &["model"]), ValueSource::User);
        assert_eq!(
            ValueSource::of(&origins, &["history"]),
            ValueSource::Higher(project())
        );
        assert_eq!(
            ValueSource::of(&origins, &["history", "persistence"]),
            ValueSource::User
        );
        assert!(matches!(
            ValueSource::of(&origins, &["sandbox_mode"]),
            ValueSource::Lower(ConfigLayerSource::System { .. })
        ));
        assert_eq!(
            ValueSource::of(&origins, &["web_search"]),
            ValueSource::Default
        );
        // A sibling key sharing a prefix is not a match.
        origins.insert("model_provider".to_string(), metadata(project()));
        assert_eq!(ValueSource::of(&origins, &["model"]), ValueSource::User);
        assert_eq!(ValueSource::Default.label(), "Default");
        assert_eq!(ValueSource::Higher(project()).label(), "Project config");
    }

    #[test]
    fn lookup_walks_objects_and_treats_null_as_unset() {
        let config = json!({"history": {"persistence": "none", "max_bytes": null}, "model": "m"});
        assert_eq!(lookup(&config, &["model"]), Some(&json!("m")));
        assert_eq!(
            lookup(&config, &["history", "persistence"]),
            Some(&json!("none"))
        );
        assert_eq!(lookup(&config, &["history", "max_bytes"]), None);
        assert_eq!(lookup(&config, &["model", "x"]), None);
        assert_eq!(lookup(&config, &["missing"]), None);
    }

    #[test]
    fn requirement_pins_match_managed_paths_and_features() {
        let requirements: ConfigRequirements = serde_json::from_value(json!({
            "modelProvider": "corp",
            "modelProviders": {"corp": {"base_url": "https://x"}},
            "feedback": {"enabled": false},
            "featureRequirements": {"memories": false},
        }))
        .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(
            requirement_pin(&requirements, &["model_provider"]),
            Some("model_provider".to_string())
        );
        assert_eq!(
            requirement_pin(&requirements, &["model_providers", "corp", "base_url"]),
            Some("model_providers.corp".to_string())
        );
        assert_eq!(
            requirement_pin(&requirements, &["model_providers"]),
            Some("model_providers.corp".to_string())
        );
        assert_eq!(
            requirement_pin(&requirements, &["model_providers", "other"]),
            None
        );
        assert_eq!(
            requirement_pin(&requirements, &["feedback", "enabled"]),
            Some("feedback.enabled".to_string())
        );
        assert_eq!(
            requirement_pin(&requirements, &["features", "memories"]),
            Some("features.memories".to_string())
        );
        assert_eq!(requirement_pin(&requirements, &["features", "apps"]), None);
        assert_eq!(requirement_pin(&requirements, &["model"]), None);
    }

    #[test]
    fn errors_are_capped_and_breakable() {
        assert_eq!(display_error("  a\n  b  "), "a b");
        let long = format!("x{}", "y".repeat(80));
        let shown = display_error(&long);
        assert!(shown.contains('\u{200B}'));
        assert_eq!(shown.replace('\u{200B}', ""), long);
        let path = display_error("failed: codex_rmcp_client::transport::Worker<x>");
        assert!(path.contains("::\u{200B}"), "{path:?}");
        assert!(display_error(&"word ".repeat(200)).chars().count() <= 321);
    }

    #[test]
    fn display_and_wire_names() {
        assert_eq!(display_value(&json!("x")), "x");
        assert_eq!(display_value(&json!(3)), "3");
        assert_eq!(display_value(&json!(["a"])), r#"["a"]"#);
        assert_eq!(
            wire_name(&codex_app_server_protocol::SandboxMode::WorkspaceWrite),
            "workspace-write"
        );
        let error = codex_app_server_protocol::JSONRPCErrorError {
            code: -32600,
            data: Some(json!({"config_write_error_code": "configVersionConflict"})),
            message: "conflict".to_string(),
        };
        assert_eq!(
            config_write_error_code(&error),
            Some("configVersionConflict")
        );
    }
}
