//! Pure mappings behind the composer toolbar: permission presets, reasoning
//! effort labels, model lookups and the context-window gauge.
//!
//! The presets mirror `codex-utils-approval-presets` (same ids, wording and
//! approval policies) expressed as app-server v2 sandbox policies, which is
//! what `thread/settings/update` and `turn/start` accept. Auto-review is the
//! TUI's extra `/permissions` entry: the workspace preset with approval
//! requests routed to the auto-review agent instead of the user.

use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::Model as ApiModel;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::TokenUsage;

/// Built-in approval + sandbox combinations offered in the toolbar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionPreset {
    ReadOnly,
    Auto,
    AutoReview,
    FullAccess,
}

impl PermissionPreset {
    pub(crate) const ALL: [Self; 4] = [
        Self::ReadOnly,
        Self::Auto,
        Self::AutoReview,
        Self::FullAccess,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Auto => "auto",
            Self::AutoReview => "auto-review",
            Self::FullAccess => "full-access",
        }
    }

    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == id)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::Auto => "Auto",
            Self::AutoReview => "Auto-review",
            Self::FullAccess => "Full access",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::ReadOnly => {
                "Reads files in the workspace. Asks before editing files or using the network."
            }
            Self::Auto => {
                "Reads and edits files in the workspace and runs commands. Asks before using the network or editing other files."
            }
            Self::AutoReview => {
                "Like Auto, but an auto-review agent approves safe requests for you."
            }
            Self::FullAccess => {
                "Edits files anywhere and uses the network without asking. Use with caution."
            }
        }
    }

    pub(crate) fn approval_policy(self) -> AskForApproval {
        match self {
            Self::ReadOnly | Self::Auto | Self::AutoReview => AskForApproval::OnRequest,
            Self::FullAccess => AskForApproval::Never,
        }
    }

    /// Who answers approval requests under this preset.
    pub(crate) fn approvals_reviewer(self) -> ApprovalsReviewer {
        match self {
            Self::AutoReview => ApprovalsReviewer::AutoReview,
            Self::ReadOnly | Self::Auto | Self::FullAccess => ApprovalsReviewer::User,
        }
    }

    /// Whether choosing this preset while the thread uses `current` must be
    /// confirmed first: it newly removes the sandbox and every approval
    /// prompt (the TUI asks the same before full access).
    pub(crate) fn needs_confirmation(self, current: Option<Self>) -> bool {
        self == Self::FullAccess && current != Some(Self::FullAccess)
    }

    pub(crate) fn sandbox_policy(self) -> SandboxPolicy {
        match self {
            Self::ReadOnly => SandboxPolicy::ReadOnly {
                network_access: false,
            },
            Self::Auto | Self::AutoReview => SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            Self::FullAccess => SandboxPolicy::DangerFullAccess,
        }
    }

    /// The preset a thread's current settings correspond to, if any.
    ///
    /// Extra writable roots or network access on top of a preset still count
    /// as that preset; only the policy kind, approval mode and reviewer are
    /// compared. An unknown reviewer counts as the user.
    pub(crate) fn matching(
        approval: Option<&AskForApproval>,
        sandbox: Option<&SandboxPolicy>,
        reviewer: Option<ApprovalsReviewer>,
    ) -> Option<Self> {
        let auto_review = reviewer == Some(ApprovalsReviewer::AutoReview);
        match (approval?, sandbox?) {
            (AskForApproval::OnRequest, SandboxPolicy::ReadOnly { .. }) => Some(Self::ReadOnly),
            (AskForApproval::OnRequest, SandboxPolicy::WorkspaceWrite { .. }) if auto_review => {
                Some(Self::AutoReview)
            }
            (AskForApproval::OnRequest, SandboxPolicy::WorkspaceWrite { .. }) => Some(Self::Auto),
            (AskForApproval::Never, SandboxPolicy::DangerFullAccess) => Some(Self::FullAccess),
            _ => None,
        }
    }

    /// Presets offered in the picker. Auto-review is listed when the server
    /// allows it, or when the thread already uses it.
    pub(crate) fn offered(auto_review_available: bool, current: Option<Self>) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|preset| {
                *preset != Self::AutoReview
                    || auto_review_available
                    || current == Some(Self::AutoReview)
            })
            .collect()
    }
}

/// Short label for the permission button.
pub(crate) fn permission_label(
    approval: Option<&AskForApproval>,
    sandbox: Option<&SandboxPolicy>,
    reviewer: Option<ApprovalsReviewer>,
) -> String {
    match PermissionPreset::matching(approval, sandbox, reviewer) {
        Some(preset) => preset.label().to_string(),
        None if approval.is_none() || sandbox.is_none() => "Permissions".to_string(),
        None => "Custom".to_string(),
    }
}

/// Human label for a reasoning effort.
pub(crate) fn effort_label(effort: &ReasoningEffort) -> String {
    match effort {
        ReasoningEffort::None => "No reasoning".to_string(),
        ReasoningEffort::Minimal => "Minimal".to_string(),
        ReasoningEffort::Low => "Low".to_string(),
        ReasoningEffort::Medium => "Medium".to_string(),
        ReasoningEffort::High => "High".to_string(),
        ReasoningEffort::XHigh => "Extra high".to_string(),
        other => capitalize(other.as_str()),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Parses an effort id produced by `ReasoningEffort::as_str`.
pub(crate) fn effort_from_id(id: &str) -> Option<ReasoningEffort> {
    serde_json::from_value(serde_json::Value::String(id.to_string())).ok()
}

/// Catalog entry for `slug`.
pub(crate) fn find_model<'a>(models: &'a [ApiModel], slug: &str) -> Option<&'a ApiModel> {
    models
        .iter()
        .find(|model| model.model == slug || model.id == slug)
}

/// Whether `slug` accepts image input. Unknown models are assumed to (like
/// the TUI), since the server rejects unsupported input with a clear error.
pub(crate) fn model_supports_images(models: &[ApiModel], slug: Option<&str>) -> bool {
    slug.and_then(|slug| find_model(models, slug))
        .is_none_or(|model| model.input_modalities.contains(&InputModality::Image))
}

/// Effort to use after switching to `model`: keep `current` when the new
/// model supports it, otherwise fall back to the model's default.
pub(crate) fn effort_for_model(
    model: &ApiModel,
    current: Option<&ReasoningEffort>,
) -> ReasoningEffort {
    match current {
        Some(current)
            if model
                .supported_reasoning_efforts
                .iter()
                .any(|option| &option.reasoning_effort == current) =>
        {
            current.clone()
        }
        _ => model.default_reasoning_effort.clone(),
    }
}

/// Display name for the model button.
pub(crate) fn model_label(models: &[ApiModel], slug: Option<&str>) -> String {
    match slug {
        Some(slug) => find_model(models, slug)
            .map(|model| model.display_name.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| slug.to_string()),
        None => "Model".to_string(),
    }
}

/// Percent of the context window still available, like the TUI footer.
pub(crate) fn context_left_percent(usage: &ThreadTokenUsage) -> Option<i64> {
    let window = usage.model_context_window?;
    let last = TokenUsage {
        input_tokens: usage.last.input_tokens,
        cached_input_tokens: usage.last.cached_input_tokens,
        cache_write_input_tokens: usage.last.cache_write_input_tokens,
        output_tokens: usage.last.output_tokens,
        reasoning_output_tokens: usage.last.reasoning_output_tokens,
        total_tokens: usage.last.total_tokens,
        codex_rollout_budget_units: None,
    };
    Some(last.percent_of_context_window_remaining(window))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::TokenUsageBreakdown;
    use pretty_assertions::assert_eq;

    fn model(slug: &str, efforts: &[ReasoningEffort], images: bool) -> ApiModel {
        serde_json::from_value(serde_json::json!({
            "id": slug,
            "model": slug,
            "displayName": slug.to_uppercase(),
            "description": "test model",
            "hidden": false,
            "supportedReasoningEfforts": efforts
                .iter()
                .map(|effort| serde_json::json!({"reasoningEffort": effort, "description": ""}))
                .collect::<Vec<_>>(),
            "defaultReasoningEffort": efforts.first().cloned().unwrap_or(ReasoningEffort::Medium),
            "inputModalities": if images { vec!["text", "image"] } else { vec!["text"] },
            "isDefault": false,
        }))
        .expect("valid model json")
    }

    #[test]
    fn presets_map_to_policies_and_back() {
        for preset in PermissionPreset::ALL {
            assert_eq!(PermissionPreset::from_id(preset.id()), Some(preset));
            assert_eq!(
                PermissionPreset::matching(
                    Some(&preset.approval_policy()),
                    Some(&preset.sandbox_policy()),
                    Some(preset.approvals_reviewer()),
                ),
                Some(preset)
            );
        }
        assert_eq!(
            PermissionPreset::matching(
                Some(&AskForApproval::UnlessTrusted),
                Some(&SandboxPolicy::DangerFullAccess),
                /*reviewer*/ None,
            ),
            None
        );
        assert_eq!(PermissionPreset::from_id("bogus"), None);
    }

    #[test]
    fn only_newly_enabling_full_access_needs_confirmation() {
        let full = PermissionPreset::FullAccess;
        assert!(full.needs_confirmation(Some(PermissionPreset::Auto)));
        assert!(full.needs_confirmation(Some(PermissionPreset::ReadOnly)));
        // Custom or unknown settings.
        assert!(full.needs_confirmation(/*current*/ None));
        // Already full access: nothing new is given away.
        assert!(!full.needs_confirmation(Some(full)));
        for safer in [
            PermissionPreset::ReadOnly,
            PermissionPreset::Auto,
            PermissionPreset::AutoReview,
        ] {
            assert!(!safer.needs_confirmation(Some(full)));
            assert!(!safer.needs_confirmation(/*current*/ None));
        }
    }

    #[test]
    fn auto_review_is_the_workspace_preset_with_the_review_agent() {
        let workspace = PermissionPreset::Auto.sandbox_policy();
        assert_eq!(PermissionPreset::AutoReview.sandbox_policy(), workspace);
        assert_eq!(
            PermissionPreset::AutoReview.approval_policy(),
            AskForApproval::OnRequest
        );
        assert_eq!(
            PermissionPreset::matching(
                Some(&AskForApproval::OnRequest),
                Some(&workspace),
                /*reviewer*/ None,
            ),
            Some(PermissionPreset::Auto)
        );
        assert_eq!(
            permission_label(
                Some(&AskForApproval::OnRequest),
                Some(&workspace),
                Some(ApprovalsReviewer::AutoReview),
            ),
            "Auto-review"
        );
        // Offered only when available, unless the thread already uses it.
        assert_eq!(
            PermissionPreset::offered(/*auto_review_available*/ false, /*current*/ None),
            vec![
                PermissionPreset::ReadOnly,
                PermissionPreset::Auto,
                PermissionPreset::FullAccess,
            ]
        );
        assert!(
            PermissionPreset::offered(
                /*auto_review_available*/ false,
                Some(PermissionPreset::AutoReview)
            )
            .contains(&PermissionPreset::AutoReview)
        );
        assert_eq!(
            PermissionPreset::offered(/*auto_review_available*/ true, /*current*/ None).len(),
            4
        );
    }

    #[test]
    fn workspace_write_with_extras_is_still_auto() {
        let sandbox = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: false,
        };
        assert_eq!(
            permission_label(Some(&AskForApproval::OnRequest), Some(&sandbox), None),
            "Auto"
        );
        assert_eq!(
            permission_label(Some(&AskForApproval::Never), Some(&sandbox), None),
            "Custom"
        );
        assert_eq!(permission_label(None, Some(&sandbox), None), "Permissions");
    }

    #[test]
    fn effort_ids_round_trip() {
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ] {
            assert_eq!(effort_from_id(effort.as_str()), Some(effort));
        }
        assert_eq!(effort_label(&ReasoningEffort::XHigh), "Extra high");
        assert_eq!(effort_label(&ReasoningEffort::Medium), "Medium");
    }

    #[test]
    fn switching_models_keeps_supported_effort() {
        let fast = model(
            "fast",
            &[ReasoningEffort::Low, ReasoningEffort::Medium],
            true,
        );
        assert_eq!(
            effort_for_model(&fast, Some(&ReasoningEffort::Medium)),
            ReasoningEffort::Medium
        );
        assert_eq!(
            effort_for_model(&fast, Some(&ReasoningEffort::XHigh)),
            ReasoningEffort::Low
        );
        assert_eq!(effort_for_model(&fast, None), ReasoningEffort::Low);
    }

    #[test]
    fn image_support_follows_catalog() {
        let models = vec![
            model("vision", &[ReasoningEffort::Medium], true),
            model("text-only", &[ReasoningEffort::Medium], false),
        ];
        assert!(model_supports_images(&models, Some("vision")));
        assert!(!model_supports_images(&models, Some("text-only")));
        assert!(model_supports_images(&models, Some("unknown")));
        assert!(model_supports_images(&models, None));
        assert_eq!(model_label(&models, Some("vision")), "VISION");
        assert_eq!(model_label(&models, Some("custom")), "custom");
    }

    #[test]
    fn context_gauge_matches_core_formula() {
        let breakdown = |total| TokenUsageBreakdown {
            total_tokens: total,
            input_tokens: total,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
        };
        let usage = ThreadTokenUsage {
            total: breakdown(500_000),
            last: breakdown(12_000 + 94_000),
            model_context_window: Some(12_000 + 188_000),
        };
        assert_eq!(context_left_percent(&usage), Some(50));
        let unknown = ThreadTokenUsage {
            model_context_window: None,
            ..usage
        };
        assert_eq!(context_left_percent(&unknown), None);
    }
}
