//! Composer toolbar: model, reasoning effort, permissions and Plan mode for
//! the active thread, plus the catalogs they draw from (`model/list`,
//! `collaborationMode/list`, `skills/list`).
//!
//! Changes to a running thread go through `thread/settings/update` (which
//! also covers messages that steer or queue). Before the thread exists, or
//! when the server does not support that experimental method, they are
//! recorded in `ThreadTab::turn_overrides` and applied by the next
//! `turn/start`.

use std::path::PathBuf;

use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CollaborationModeListParams;
use codex_app_server_protocol::CollaborationModeListResponse;
use codex_app_server_protocol::ConfigRequirements;
use codex_app_server_protocol::ExperimentalFeature;
use codex_app_server_protocol::ExperimentalFeatureListParams;
use codex_app_server_protocol::ExperimentalFeatureListResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::SkillsListParams;
use codex_app_server_protocol::SkillsListResponse;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadSettingsUpdateResponse;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::ReasoningEffort;

use super::input::SkillRef;
use super::presets;
use super::presets::PermissionPreset;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::app::ThreadPhase;
use crate::app::ThreadTab;
use crate::backend::BackendError;
use crate::transcript::NoticeKind;

const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const THREAD_SETTINGS_UPDATE_METHOD: &str = "thread/settings/update";
/// Safety stop for `model/list` and `experimentalFeature/list` paging.
const MAX_MODEL_PAGES: usize = 20;
/// Feature flag (`[features] guardian_approval`) behind auto-review.
const AUTO_REVIEW_FEATURE: &str = "guardian_approval";

/// One requested change to a thread's settings. `None` keeps a field.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SettingsChange {
    pub(crate) model: Option<String>,
    /// `Some(None)` clears the effort (the model's default applies).
    pub(crate) effort: Option<Option<ReasoningEffort>>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) mode: Option<ModeKind>,
}

impl SettingsChange {
    fn touches_collaboration_mode(&self) -> bool {
        self.model.is_some() || self.effort.is_some() || self.mode.is_some()
    }
}

/// Model, effort, approval and sandbox the next turn of `thread` will use:
/// pending overrides first, then what the server reported.
pub(crate) fn effective_model(thread: &ThreadTab) -> Option<String> {
    thread
        .turn_overrides
        .model
        .clone()
        .or_else(|| thread.model.clone())
}

pub(crate) fn effective_effort(thread: &ThreadTab) -> Option<ReasoningEffort> {
    thread
        .turn_overrides
        .effort
        .clone()
        .or_else(|| thread.effort.clone())
}

pub(crate) fn effective_approval(thread: &ThreadTab) -> Option<AskForApproval> {
    thread
        .turn_overrides
        .approval_policy
        .or(thread.approval_policy)
}

pub(crate) fn effective_sandbox(thread: &ThreadTab) -> Option<SandboxPolicy> {
    thread
        .turn_overrides
        .sandbox_policy
        .clone()
        .or_else(|| thread.sandbox.clone())
}

pub(crate) fn effective_reviewer(thread: &ThreadTab) -> Option<ApprovalsReviewer> {
    thread
        .turn_overrides
        .approvals_reviewer
        .or(thread.approvals_reviewer)
}

/// Whether the server lets threads use auto-review: the feature is on and
/// managed requirements allow both the reviewer and on-request approvals.
/// `feature_enabled` is `None` when the server did not list the feature
/// (older servers), which hides the preset.
pub(crate) fn auto_review_allowed(
    feature_enabled: Option<bool>,
    requirements: Option<&ConfigRequirements>,
) -> bool {
    let reviewer_allowed = requirements
        .and_then(|requirements| requirements.allowed_approvals_reviewers.as_ref())
        .is_none_or(|allowed| allowed.contains(&ApprovalsReviewer::AutoReview));
    let on_request_allowed = requirements
        .and_then(|requirements| requirements.allowed_approval_policies.as_ref())
        .is_none_or(|allowed| allowed.contains(&AskForApproval::OnRequest));
    feature_enabled == Some(true) && reviewer_allowed && on_request_allowed
}

/// Whether `features` (one `experimentalFeature/list` page) says auto-review
/// is on; `None` when the page does not mention it.
fn auto_review_feature(features: &[ExperimentalFeature]) -> Option<bool> {
    features
        .iter()
        .find(|feature| feature.name == AUTO_REVIEW_FEATURE)
        .map(|feature| feature.enabled)
}

/// Builds the collaboration mode for `mode` with the given model and effort.
/// `developer_instructions: None` asks the server for the built-in ones.
pub(crate) fn collaboration_mode(
    mode: ModeKind,
    model: String,
    effort: Option<ReasoningEffort>,
) -> CollaborationMode {
    CollaborationMode {
        mode,
        settings: Settings {
            model,
            reasoning_effort: effort,
            developer_instructions: None,
        },
    }
}

/// Whether a `thread/settings/update` failure means the server does not
/// offer the method (older or remote servers), as opposed to a real error.
pub(crate) fn settings_update_unsupported(error: &JSONRPCErrorError) -> bool {
    error.code == JSONRPC_METHOD_NOT_FOUND
        || (error.code == JSONRPC_INVALID_REQUEST
            && error.message.contains(THREAD_SETTINGS_UPDATE_METHOD))
}

/// Settings of a tab before an optimistic update, restored on failure.
struct SettingsSnapshot {
    model: Option<String>,
    effort: Option<ReasoningEffort>,
    approval_policy: Option<AskForApproval>,
    approvals_reviewer: Option<ApprovalsReviewer>,
    sandbox: Option<SandboxPolicy>,
    mode: ModeKind,
}

impl AppController {
    /// Fetches the model catalog and collaboration modes, and prunes old
    /// pasted images. Runs on every (re)start of the embedded server.
    pub(super) fn composer_load_catalogs(&mut self) {
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let mut models = Vec::new();
            let mut cursor = None;
            let mut error = None;
            for _ in 0..MAX_MODEL_PAGES {
                let request = ClientRequest::ModelList {
                    request_id: backend.next_request_id(),
                    params: ModelListParams {
                        cursor: cursor.take(),
                        limit: None,
                        include_hidden: Some(true),
                    },
                };
                match backend.request::<ModelListResponse>(request).await {
                    Ok(page) => {
                        models.extend(page.data);
                        cursor = page.next_cursor;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    Err(err) => {
                        error = Some(err.user_message());
                        break;
                    }
                }
            }
            crate::ui_thread::post(move |app| {
                if let Some(error) = error {
                    tracing::warn!(error, "model/list failed; the model picker is limited");
                }
                app.composer_shared.models = models;
                app.composer_refresh();
            });
        });
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let mut enabled = None;
            let mut cursor = None;
            for _ in 0..MAX_MODEL_PAGES {
                let request = ClientRequest::ExperimentalFeatureList {
                    request_id: backend.next_request_id(),
                    params: ExperimentalFeatureListParams {
                        cursor: cursor.take(),
                        ..ExperimentalFeatureListParams::default()
                    },
                };
                match backend
                    .request::<ExperimentalFeatureListResponse>(request)
                    .await
                {
                    Ok(page) => {
                        enabled = auto_review_feature(&page.data);
                        cursor = page.next_cursor;
                        if enabled.is_some() || cursor.is_none() {
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::info!(%err, "experimentalFeature/list failed; hiding auto-review");
                        break;
                    }
                }
            }
            crate::ui_thread::post(move |app| {
                app.composer_shared.auto_review_feature = enabled;
                app.composer_refresh();
            });
        });
        self.backend.call(
            |request_id| ClientRequest::CollaborationModeList {
                request_id,
                params: CollaborationModeListParams {},
            },
            |app, result: Result<CollaborationModeListResponse, BackendError>| {
                match result {
                    Ok(response) => {
                        app.composer_shared.plan_mask = response
                            .data
                            .into_iter()
                            .find(|mask| mask.mode == Some(ModeKind::Plan));
                    }
                    Err(err) => {
                        tracing::info!(%err, "collaboration modes unavailable; hiding Plan mode");
                        app.composer_shared.plan_mask = None;
                    }
                }
                app.composer_refresh();
            },
        );
    }

    /// Loads the skills available in `cwd` once, for `$skill` mentions.
    /// `force_reload` re-scans the disk (after `skills/changed`).
    pub(super) fn composer_ensure_skills(&mut self, cwd: PathBuf, force_reload: bool) {
        if cwd.as_os_str().is_empty()
            || self.composer_shared.skills.contains_key(&cwd)
            || !self.composer_shared.skills_loading.insert(cwd.clone())
        {
            return;
        }
        let requested = cwd.clone();
        self.backend.call(
            move |request_id| ClientRequest::SkillsList {
                request_id,
                params: SkillsListParams {
                    cwds: vec![cwd],
                    force_reload,
                },
            },
            move |app, result: Result<SkillsListResponse, BackendError>| {
                app.composer_shared.skills_loading.remove(&requested);
                let skills = match result {
                    Ok(response) => response
                        .data
                        .into_iter()
                        .flat_map(|entry| entry.skills)
                        .filter(|skill| skill.enabled)
                        .map(|skill| SkillRef {
                            description: skill
                                .interface
                                .as_ref()
                                .and_then(|interface| interface.short_description.clone())
                                .or(skill.short_description)
                                .unwrap_or(skill.description),
                            name: skill.name,
                            path: skill.path.into_path_buf(),
                        })
                        .collect(),
                    Err(err) => {
                        tracing::info!(%err, "skills/list failed; $skill mentions disabled");
                        Vec::new()
                    }
                };
                app.composer_shared.skills.insert(requested, skills);
            },
        );
    }

    /// Applies a toolbar change to tab `index`.
    pub(super) fn composer_apply_settings(&mut self, index: usize, change: SettingsChange) {
        let tab_id = self.tabs[index].id;
        let plan_supported = self.composer_shared.plan_mask.is_some();
        let unsupported = self.composer_shared.settings_update_unsupported;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let snapshot = SettingsSnapshot {
            model: thread.model.clone(),
            effort: thread.effort.clone(),
            approval_policy: thread.approval_policy,
            approvals_reviewer: thread.approvals_reviewer,
            sandbox: thread.sandbox.clone(),
            mode: thread.composer.mode,
        };
        if let Some(mode) = change.mode {
            thread.composer.mode = mode;
        }
        let model = change.model.clone().or_else(|| effective_model(thread));
        let effort = match &change.effort {
            Some(effort) => effort.clone(),
            None => effective_effort(thread),
        };
        let collaboration = (plan_supported && change.touches_collaboration_mode())
            .then(|| model.clone())
            .flatten()
            .map(|model| collaboration_mode(thread.composer.mode, model, effort.clone()));

        let loaded = thread.phase != ThreadPhase::Starting;
        let Some(thread_id) = thread.thread_id.clone().filter(|_| loaded && !unsupported) else {
            // Not started or resumed yet (or no settings RPC): the next
            // turn/start applies it.
            store_turn_overrides(thread, &change, collaboration);
            self.composer_refresh();
            return;
        };

        // Optimistic: show the new values now; thread/settings/updated confirms.
        if let Some(model) = &change.model {
            thread.model = Some(model.clone());
            thread.turn_overrides.model = None;
        }
        if let Some(effort) = &change.effort {
            thread.effort.clone_from(effort);
            thread.turn_overrides.effort = None;
        }
        if let Some(approval) = &change.approval_policy {
            thread.approval_policy = Some(*approval);
            thread.turn_overrides.approval_policy = None;
        }
        if let Some(reviewer) = change.approvals_reviewer {
            thread.approvals_reviewer = Some(reviewer);
            thread.turn_overrides.approvals_reviewer = None;
        }
        if let Some(sandbox) = &change.sandbox_policy {
            thread.sandbox = Some(sandbox.clone());
            thread.turn_overrides.sandbox_policy = None;
        }
        if collaboration.is_some() {
            thread.turn_overrides.collaboration_mode = None;
        }
        thread.composer.settings_epoch = thread.composer.settings_epoch.wrapping_add(1);
        let epoch = thread.composer.settings_epoch;
        self.composer_refresh();

        let params = ThreadSettingsUpdateParams {
            thread_id,
            approval_policy: change.approval_policy,
            approvals_reviewer: change.approvals_reviewer,
            sandbox_policy: change.sandbox_policy.clone(),
            model: change.model.clone(),
            effort: change.effort.clone().flatten(),
            collaboration_mode: collaboration.clone(),
            ..ThreadSettingsUpdateParams::default()
        };
        self.backend.call(
            |request_id| ClientRequest::ThreadSettingsUpdate { request_id, params },
            move |app, result: Result<ThreadSettingsUpdateResponse, BackendError>| {
                let Err(err) = result else {
                    return;
                };
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                if err.server_error().is_some_and(settings_update_unsupported) {
                    app.composer_shared.settings_update_unsupported = true;
                    if let Some(thread) = app.thread_tab_mut(index) {
                        store_turn_overrides(thread, &change, collaboration);
                    }
                    app.composer_refresh();
                    return;
                }
                if let Some(thread) = app.thread_tab_mut(index)
                    && thread.composer.settings_epoch == epoch
                {
                    thread.model = snapshot.model;
                    thread.effort = snapshot.effort;
                    thread.approval_policy = snapshot.approval_policy;
                    thread.approvals_reviewer = snapshot.approvals_reviewer;
                    thread.sandbox = snapshot.sandbox;
                    thread.composer.mode = snapshot.mode;
                }
                app.transcript_push_notice(
                    index,
                    NoticeKind::Error,
                    format!("Could not change thread settings: {}", err.user_message()),
                );
                app.composer_refresh();
            },
        );
    }

    /// A choice from one of the toolbar pickers.
    pub(super) fn composer_choose(&mut self, picker: &str, id: &str) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let change = match picker {
            "model" => {
                if effective_model(thread).as_deref() == Some(id) {
                    return;
                }
                let current_effort = effective_effort(thread);
                let effort = presets::find_model(&self.composer_shared.models, id)
                    .map(|model| presets::effort_for_model(model, current_effort.as_ref()))
                    .filter(|effort| current_effort.as_ref() != Some(effort));
                if !thread.composer.attachments.is_empty()
                    && !presets::model_supports_images(&self.composer_shared.models, Some(id))
                {
                    self.toast(format!(
                        "{} does not accept images; remove the attachments before sending",
                        presets::model_label(&self.composer_shared.models, Some(id))
                    ));
                }
                SettingsChange {
                    model: Some(id.to_string()),
                    effort: effort.map(Some),
                    ..SettingsChange::default()
                }
            }
            "effort" => {
                let Some(effort) = presets::effort_from_id(id) else {
                    return;
                };
                if effective_effort(thread).as_ref() == Some(&effort) {
                    return;
                }
                SettingsChange {
                    effort: Some(Some(effort)),
                    ..SettingsChange::default()
                }
            }
            "permissions" => {
                let Some(preset) = PermissionPreset::from_id(id) else {
                    return;
                };
                let current = PermissionPreset::matching(
                    effective_approval(thread).as_ref(),
                    effective_sandbox(thread).as_ref(),
                    effective_reviewer(thread),
                );
                let change = SettingsChange {
                    approval_policy: Some(preset.approval_policy()),
                    approvals_reviewer: Some(preset.approvals_reviewer()),
                    sandbox_policy: Some(preset.sandbox_policy()),
                    ..SettingsChange::default()
                };
                if preset.needs_confirmation(current) {
                    self.composer_confirm_full_access(index, change);
                    return;
                }
                change
            }
            other => {
                tracing::debug!(picker = other, "unknown composer picker");
                return;
            }
        };
        self.composer_apply_settings(index, change);
    }

    /// Asks before removing the sandbox and every approval prompt of tab
    /// `index` (as the TUI's full-access confirmation does); applies
    /// `change` only when the user accepts.
    fn composer_confirm_full_access(&mut self, index: usize, change: SettingsChange) {
        let tab_id = self.tabs[index].id;
        self.show_dialog(
            DialogRequest::confirm(
                "Enable full access?",
                "Codex will run commands without a sandbox and without asking for approval. \
                 It can edit files outside this folder, delete data and use the network. \
                 Only use this in an environment you trust and can restore.",
            )
            .accept_label("Enable full access")
            .destructive(),
            Box::new(move |app, accepted| {
                if accepted.is_some()
                    && let Some(index) = app.tab_index_by_id(tab_id)
                {
                    app.composer_apply_settings(index, change);
                }
            }),
        );
    }

    /// Toggles Plan mode on the active thread.
    pub(super) fn composer_toggle_plan(&mut self) {
        if let Some(index) = self.active_thread_index() {
            self.composer_switch_plan(index, /*enable*/ None);
        }
    }

    /// Makes the next `turn/start` of tab `index` carry its collaboration
    /// mode itself. `thread/settings/update` and `turn/start` are separate
    /// requests that may reach the server in either order; a message sent
    /// right after switching modes (`/plan <message>`) must not start in the
    /// old mode.
    pub(super) fn composer_carry_mode_on_next_turn(&mut self, index: usize) {
        if self.composer_shared.plan_mask.is_none() {
            return;
        }
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        if let Some(model) = effective_model(thread) {
            let effort = effective_effort(thread);
            thread.turn_overrides.collaboration_mode =
                Some(collaboration_mode(thread.composer.mode, model, effort));
        }
    }

    /// Turns Plan mode of tab `index` on or off (`None` toggles). Returns
    /// whether the thread ends up in the requested mode; false (with a
    /// toast) when Plan mode is unavailable.
    pub(super) fn composer_switch_plan(&mut self, index: usize, enable: Option<bool>) -> bool {
        let Some(mask) = self.composer_shared.plan_mask.clone() else {
            return false;
        };
        let config_plan_effort = self
            .config
            .as_ref()
            .and_then(|config| config.plan_mode_reasoning_effort.clone());
        let Some(thread) = self.thread_tab_mut(index) else {
            return false;
        };
        if effective_model(thread).is_none() {
            // The collaboration mode needs a model; it is known once the
            // thread has started.
            self.toast("Plan mode is available once the thread has started");
            return false;
        }
        let Some(thread) = self.thread_tab_mut(index) else {
            return false;
        };
        let in_plan = thread.composer.mode == ModeKind::Plan;
        if enable.is_some_and(|enable| enable == in_plan) {
            return true;
        }
        let plan_effort = config_plan_effort.or_else(|| mask.reasoning_effort.flatten());
        let (change, pre_plan_effort) = plan_toggle(
            thread.composer.mode,
            effective_effort(thread),
            thread.composer.pre_plan_effort.take(),
            plan_effort,
        );
        thread.composer.pre_plan_effort = pre_plan_effort;
        self.composer_apply_settings(index, change);
        true
    }
}

/// The change that toggles Plan mode, and the effort to remember for leaving
/// it again.
///
/// Entering Plan mode switches to `plan_effort` (the configured or preset
/// Plan effort) and remembers the current effort; leaving restores the
/// remembered effort, which may be "none".
pub(crate) fn plan_toggle(
    mode: ModeKind,
    current_effort: Option<ReasoningEffort>,
    pre_plan_effort: Option<Option<ReasoningEffort>>,
    plan_effort: Option<ReasoningEffort>,
) -> (SettingsChange, Option<Option<ReasoningEffort>>) {
    if mode == ModeKind::Plan {
        let change = SettingsChange {
            mode: Some(ModeKind::Default),
            effort: pre_plan_effort.filter(|effort| *effort != current_effort),
            ..SettingsChange::default()
        };
        (change, None)
    } else {
        let change = SettingsChange {
            mode: Some(ModeKind::Plan),
            effort: plan_effort
                .filter(|effort| current_effort.as_ref() != Some(effort))
                .map(Some),
            ..SettingsChange::default()
        };
        (change, Some(current_effort))
    }
}

/// Records `change` for the next `turn/start` of `thread`.
fn store_turn_overrides(
    thread: &mut ThreadTab,
    change: &SettingsChange,
    collaboration: Option<CollaborationMode>,
) {
    match &change.effort {
        Some(Some(effort)) => thread.turn_overrides.effort = Some(effort.clone()),
        Some(None) => {
            // The collaboration mode below carries "no effort"; drop both the
            // pending override and the stale server value from the label.
            thread.turn_overrides.effort = None;
            thread.effort = None;
        }
        None => {}
    }
    let overrides = &mut thread.turn_overrides;
    if let Some(model) = &change.model {
        overrides.model = Some(model.clone());
    }
    if let Some(approval) = &change.approval_policy {
        overrides.approval_policy = Some(*approval);
    }
    if let Some(reviewer) = change.approvals_reviewer {
        overrides.approvals_reviewer = Some(reviewer);
    }
    if let Some(sandbox) = &change.sandbox_policy {
        overrides.sandbox_policy = Some(sandbox.clone());
    }
    if collaboration.is_some() {
        overrides.collaboration_mode = collaboration;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn detects_unsupported_settings_rpc() {
        let error = |code, message: &str| JSONRPCErrorError {
            code,
            message: message.to_string(),
            data: None,
        };
        assert!(settings_update_unsupported(&error(
            -32601,
            "method not found"
        )));
        assert!(settings_update_unsupported(&error(
            -32600,
            "thread/settings/update requires experimentalApi capability"
        )));
        assert!(!settings_update_unsupported(&error(
            -32600,
            "invalid thread id"
        )));
        assert!(!settings_update_unsupported(&error(
            -32603,
            "thread/settings/update failed"
        )));
    }

    #[test]
    fn overrides_take_precedence_over_server_values() {
        let mut thread = ThreadTab::new(PathBuf::from("/repo"));
        thread.model = Some("server".to_string());
        thread.effort = Some(ReasoningEffort::Low);
        assert_eq!(effective_model(&thread), Some("server".to_string()));
        store_turn_overrides(
            &mut thread,
            &SettingsChange {
                model: Some("picked".to_string()),
                sandbox_policy: Some(PermissionPreset::FullAccess.sandbox_policy()),
                ..SettingsChange::default()
            },
            Some(collaboration_mode(
                ModeKind::Plan,
                "picked".to_string(),
                None,
            )),
        );
        assert_eq!(effective_model(&thread), Some("picked".to_string()));
        assert_eq!(effective_effort(&thread), Some(ReasoningEffort::Low));
        assert_eq!(
            effective_sandbox(&thread),
            Some(SandboxPolicy::DangerFullAccess)
        );
        assert_eq!(
            thread
                .turn_overrides
                .collaboration_mode
                .as_ref()
                .map(|mode| mode.mode),
            Some(ModeKind::Plan)
        );
    }

    #[test]
    fn plan_mode_switches_effort_and_restores_it() {
        let (enter, remembered) = plan_toggle(
            ModeKind::Default,
            /*current_effort*/ None,
            /*pre_plan_effort*/ None,
            Some(ReasoningEffort::Medium),
        );
        assert_eq!(
            enter,
            SettingsChange {
                mode: Some(ModeKind::Plan),
                effort: Some(Some(ReasoningEffort::Medium)),
                ..SettingsChange::default()
            }
        );
        assert_eq!(remembered, Some(None));

        let (leave, remembered) = plan_toggle(
            ModeKind::Plan,
            Some(ReasoningEffort::Medium),
            remembered,
            Some(ReasoningEffort::Medium),
        );
        assert_eq!(
            leave,
            SettingsChange {
                mode: Some(ModeKind::Default),
                effort: Some(None),
                ..SettingsChange::default()
            }
        );
        assert_eq!(remembered, None);

        // Same effort before and in Plan mode: only the mode changes.
        let (enter, _) = plan_toggle(
            ModeKind::Default,
            Some(ReasoningEffort::Medium),
            None,
            Some(ReasoningEffort::Medium),
        );
        assert_eq!(enter.effort, None);
    }

    #[test]
    fn clearing_effort_without_settings_rpc_clears_the_label() {
        let mut thread = ThreadTab::new(PathBuf::from("/repo"));
        thread.effort = Some(ReasoningEffort::Medium);
        store_turn_overrides(
            &mut thread,
            &SettingsChange {
                mode: Some(ModeKind::Default),
                effort: Some(None),
                ..SettingsChange::default()
            },
            Some(collaboration_mode(ModeKind::Default, "m".to_string(), None)),
        );
        assert_eq!(effective_effort(&thread), None);
        assert!(thread.turn_overrides.collaboration_mode.is_some());
    }

    #[test]
    fn reviewer_overrides_apply_to_the_next_turn() {
        let mut thread = ThreadTab::new(PathBuf::from("/repo"));
        thread.approvals_reviewer = Some(ApprovalsReviewer::User);
        let preset = PermissionPreset::AutoReview;
        store_turn_overrides(
            &mut thread,
            &SettingsChange {
                approval_policy: Some(preset.approval_policy()),
                approvals_reviewer: Some(preset.approvals_reviewer()),
                sandbox_policy: Some(preset.sandbox_policy()),
                ..SettingsChange::default()
            },
            /*collaboration*/ None,
        );
        assert_eq!(
            effective_reviewer(&thread),
            Some(ApprovalsReviewer::AutoReview)
        );
        assert_eq!(
            thread.turn_overrides.approvals_reviewer,
            Some(ApprovalsReviewer::AutoReview)
        );
    }

    #[test]
    fn auto_review_needs_the_feature_and_allowing_requirements() {
        assert!(auto_review_allowed(Some(true), /*requirements*/ None));
        assert!(!auto_review_allowed(
            Some(false),
            /*requirements*/ None
        ));
        assert!(!auto_review_allowed(
            /*feature_enabled*/ None, /*requirements*/ None
        ));
        let requirements = |json: serde_json::Value| -> ConfigRequirements {
            serde_json::from_value(json).expect("valid requirements json")
        };
        let only_user = requirements(serde_json::json!({"allowedApprovalsReviewers": ["user"]}));
        assert!(!auto_review_allowed(Some(true), Some(&only_user)));
        let never_only = requirements(serde_json::json!({"allowedApprovalPolicies": ["never"]}));
        assert!(!auto_review_allowed(Some(true), Some(&never_only)));
        let both = requirements(serde_json::json!({
            "allowedApprovalsReviewers": ["user", "auto_review"],
        }));
        assert!(auto_review_allowed(Some(true), Some(&both)));
    }

    #[test]
    fn auto_review_feature_is_read_from_the_feature_list() {
        let feature = |name: &str, enabled: bool| -> ExperimentalFeature {
            serde_json::from_value(serde_json::json!({
                "name": name,
                "stage": "stable",
                "displayName": null,
                "description": null,
                "announcement": null,
                "enabled": enabled,
                "defaultEnabled": true,
            }))
            .expect("valid feature json")
        };
        assert_eq!(
            auto_review_feature(&[
                feature("worktrees", true),
                feature("guardian_approval", false)
            ]),
            Some(false)
        );
        assert_eq!(auto_review_feature(&[feature("worktrees", true)]), None);
    }

    #[test]
    fn collaboration_mode_uses_builtin_instructions() {
        let mode = collaboration_mode(ModeKind::Plan, "m".to_string(), Some(ReasoningEffort::High));
        assert_eq!(mode.settings.developer_instructions, None);
        assert_eq!(mode.settings.reasoning_effort, Some(ReasoningEffort::High));
        assert!(
            SettingsChange {
                mode: Some(ModeKind::Default),
                ..SettingsChange::default()
            }
            .touches_collaboration_mode()
        );
        assert!(
            !SettingsChange {
                approval_policy: Some(AskForApproval::Never),
                ..SettingsChange::default()
            }
            .touches_collaboration_mode()
        );
    }
}
