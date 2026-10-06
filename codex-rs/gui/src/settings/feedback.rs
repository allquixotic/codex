//! Settings › Send feedback: `feedback/upload` with a category, a
//! description, an optional thread, and optional logs.
//!
//! The server collects the logs itself: in embedded mode it shares this
//! process's log buffer, so the app's own logs are included without staging
//! files the way the TUI does for remote servers.

use std::collections::BTreeMap;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::FeedbackUploadParams;
use codex_app_server_protocol::FeedbackUploadResponse;
use serde_json::Value;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use super::model;
use crate::app::AppController;
use crate::backend::BackendError;
use crate::ui::SettingsState;

/// Categories in display order: (classification sent to the server, label).
pub(crate) const CATEGORIES: &[(&str, &str)] = &[
    ("bug", "Bug"),
    ("bad_result", "Bad result"),
    ("good_result", "Good result"),
    ("safety_check", "Safety check"),
    ("other", "Other"),
];

/// Index of "Good result", the only category where details are optional.
/// `settings_feedback.slint` uses the same index for its placeholder.
const GOOD_RESULT: usize = 2;

const MAX_DESCRIPTION_CHARS: usize = 10_000;

/// Where non-positive feedback can be followed up, as in the TUI.
const ISSUE_URL: &str = "https://github.com/openai/codex/issues/new?template=3-cli.yml";

/// The feedback form as entered.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FeedbackForm {
    pub(crate) category: usize,
    pub(crate) description: String,
    pub(crate) include_logs: bool,
    pub(crate) thread_id: Option<String>,
}

/// Checks the form and builds the upload request.
pub(crate) fn feedback_params(form: &FeedbackForm) -> Result<FeedbackUploadParams, String> {
    let Some((classification, _)) = CATEGORIES.get(form.category) else {
        return Err("Choose what the feedback is about.".to_string());
    };
    let description = form.description.trim();
    if description.is_empty() && form.category != GOOD_RESULT {
        return Err("Describe what happened so the report can be acted on.".to_string());
    }
    let length = description.chars().count();
    if length > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "The description is too long ({length} characters; the limit is {MAX_DESCRIPTION_CHARS})."
        ));
    }
    Ok(FeedbackUploadParams {
        classification: (*classification).to_string(),
        reason: (!description.is_empty()).then(|| description.to_string()),
        thread_id: form.thread_id.clone(),
        include_logs: form.include_logs,
        extra_log_files: None,
        tags: Some(BTreeMap::from([(
            "client".to_string(),
            crate::startup::GUI_CLIENT_NAME.to_string(),
        )])),
    })
}

/// Follow-up link for a report, or `None` for praise.
pub(crate) fn issue_url(category: usize, feedback_id: &str) -> Option<String> {
    if category == GOOD_RESULT {
        return None;
    }
    Some(format!(
        "{ISSUE_URL}&steps=Uploaded%20thread:%20{feedback_id}"
    ))
}

/// Why feedback cannot be sent, if config or policy turned it off.
pub(crate) fn disabled_reason(
    effective: Option<&Value>,
    requirement: Option<bool>,
) -> Option<String> {
    if requirement == Some(false) {
        return Some("Sending feedback is turned off by your organization.".to_string());
    }
    let configured = effective
        .and_then(|config| model::lookup(config, &["feedback", "enabled"]))
        .and_then(Value::as_bool);
    (configured == Some(false)).then(|| {
        "Sending feedback is turned off in config.toml (feedback.enabled = false).".to_string()
    })
}

#[derive(Default)]
pub(crate) struct FeedbackState {
    /// Thread offered in the thread picker after "No thread".
    threads: Vec<String>,
    /// Thread that most recently started a turn, the default choice.
    last_active_thread: Option<String>,
    busy: bool,
    initialized: bool,
}

impl FeedbackState {
    pub(super) fn note_turn_started(&mut self, thread_id: &str) {
        self.last_active_thread = Some(thread_id.to_string());
    }
}

impl AppController {
    pub(super) fn settings_feedback_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        let labels: Vec<SharedString> = CATEGORIES
            .iter()
            .map(|(_, label)| SharedString::from(*label))
            .collect();
        state.set_feedback_categories(ModelRc::new(VecModel::from(labels)));
        state.on_feedback_submit(|| {
            crate::ui_thread::with_app(AppController::settings_feedback_submit);
        });
        state.on_feedback_again(|| {
            crate::ui_thread::with_app(|app| {
                let state = app.window.global::<SettingsState>();
                state.set_feedback_result("".into());
                state.set_feedback_result_id("".into());
                state.set_feedback_issue_url("".into());
                state.set_feedback_text("".into());
                state.set_feedback_error("".into());
            });
        });
    }

    pub(super) fn settings_feedback_activate(&mut self) {
        let state = self.window.global::<SettingsState>();
        // Keep the thread the user picked when it is still open.
        let picked = usize::try_from(state.get_feedback_thread_index() - 1)
            .ok()
            .and_then(|index| self.settings.feedback.threads.get(index).cloned());
        let mut labels = vec![SharedString::from("No thread")];
        let mut threads = Vec::new();
        for tab in &self.tabs {
            if let Some(thread) = tab.thread()
                && let Some(thread_id) = thread.thread_id.clone()
            {
                let title = thread.title();
                let folder = crate::app::folder_label(&thread.cwd);
                labels.push(SharedString::from(if title == folder {
                    title
                } else {
                    format!("{title} — {folder}")
                }));
                threads.push(thread_id);
            }
        }
        let feedback = &mut self.settings.feedback;
        let preferred = if feedback.initialized {
            picked
        } else {
            feedback.initialized = true;
            feedback
                .last_active_thread
                .clone()
                .filter(|id| threads.contains(id))
                .or_else(|| (threads.len() == 1).then(|| threads[0].clone()))
        };
        let index = preferred
            .and_then(|id| threads.iter().position(|thread| *thread == id))
            .map_or(0, |index| index + 1);
        feedback.threads = threads;
        state.set_feedback_threads(ModelRc::new(VecModel::from(labels)));
        state.set_feedback_thread_index(i32::try_from(index).unwrap_or(0));
        state.set_feedback_busy(feedback.busy);
        self.settings_feedback_refresh_disabled();
    }

    pub(super) fn settings_feedback_refresh_disabled(&mut self) {
        let requirement = self
            .settings
            .requirements
            .as_ref()
            .and_then(|requirements| requirements.feedback.as_ref())
            .and_then(|feedback| feedback.enabled);
        let effective = self
            .settings
            .snapshot
            .as_ref()
            .map(|snapshot| &snapshot.effective);
        let mut reason = disabled_reason(effective, requirement);
        if reason.is_none()
            && effective.is_none()
            && self
                .config
                .as_ref()
                .is_some_and(|config| !config.feedback_enabled)
        {
            reason = disabled_reason(
                Some(&serde_json::json!({"feedback": {"enabled": false}})),
                /*requirement*/ None,
            );
        }
        self.window
            .global::<SettingsState>()
            .set_feedback_disabled(reason.unwrap_or_default().into());
    }

    fn settings_feedback_form(&self) -> FeedbackForm {
        let state = self.window.global::<SettingsState>();
        let thread_id = usize::try_from(state.get_feedback_thread_index() - 1)
            .ok()
            .and_then(|index| self.settings.feedback.threads.get(index).cloned());
        FeedbackForm {
            category: usize::try_from(state.get_feedback_category()).unwrap_or(usize::MAX),
            description: state.get_feedback_text().to_string(),
            include_logs: state.get_feedback_include_logs(),
            thread_id,
        }
    }

    fn settings_feedback_submit(&mut self) {
        if self.settings.feedback.busy {
            return;
        }
        let form = self.settings_feedback_form();
        let state = self.window.global::<SettingsState>();
        if !state.get_feedback_disabled().is_empty() {
            return;
        }
        let params = match feedback_params(&form) {
            Ok(params) => params,
            Err(err) => {
                state.set_feedback_error(err.into());
                return;
            }
        };
        state.set_feedback_error("".into());
        state.set_feedback_busy(true);
        self.settings.feedback.busy = true;
        let include_logs = form.include_logs;
        let category = form.category;
        self.backend.call(
            move |request_id| ClientRequest::FeedbackUpload { request_id, params },
            move |app, result: Result<FeedbackUploadResponse, BackendError>| {
                app.settings.feedback.busy = false;
                let state = app.window.global::<SettingsState>();
                state.set_feedback_busy(false);
                match result {
                    Ok(response) => {
                        app.settings_feedback_show_result(
                            category,
                            include_logs,
                            &response.thread_id,
                        );
                    }
                    Err(err) => {
                        tracing::warn!(%err, "feedback/upload failed");
                        state.set_feedback_error(
                            format!("Feedback was not sent: {}", err.user_message()).into(),
                        );
                    }
                }
            },
        );
    }

    /// Shows the "sent" card with the feedback id the server returned.
    fn settings_feedback_show_result(&mut self, category: usize, include_logs: bool, id: &str) {
        let state = self.window.global::<SettingsState>();
        let message = if include_logs {
            "Thanks! Your feedback and logs were sent."
        } else {
            "Thanks! Your feedback was sent (without logs)."
        };
        state.set_feedback_result(message.into());
        state.set_feedback_issue_url(issue_url(category, id).unwrap_or_default().into());
        state.set_feedback_result_id(id.into());
    }

    /// Scripted input for UI automation: `["category", index]`,
    /// `["text", text]`, `["logs", "true"]`, `["thread", index]`, `["submit"]`,
    /// and `["sent", id]`, which shows the result card without uploading.
    pub(super) fn settings_feedback_automation(&mut self, args: &[String]) {
        let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
        let state = self.window.global::<SettingsState>();
        match arg(0) {
            "category" => state.set_feedback_category(arg(1).parse().unwrap_or(0)),
            "text" => state.set_feedback_text(arg(1).into()),
            "logs" => state.set_feedback_include_logs(arg(1) == "true"),
            "thread" => state.set_feedback_thread_index(arg(1).parse().unwrap_or(0)),
            "submit" => state.invoke_feedback_submit(),
            "sent" => {
                let category = usize::try_from(state.get_feedback_category()).unwrap_or(0);
                let include_logs = state.get_feedback_include_logs();
                self.settings_feedback_show_result(category, include_logs, arg(1));
            }
            other => tracing::warn!(action = other, "unknown feedback automation action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn form(category: usize, description: &str) -> FeedbackForm {
        FeedbackForm {
            category,
            description: description.to_string(),
            include_logs: false,
            thread_id: None,
        }
    }

    #[test]
    fn reports_need_a_description_except_praise() {
        assert!(feedback_params(&form(0, "  ")).is_err());
        assert!(feedback_params(&form(1, "")).is_err());
        let praise = feedback_params(&form(GOOD_RESULT, " "));
        assert_eq!(
            praise.map(|params| (params.classification, params.reason)),
            Ok(("good_result".to_string(), None))
        );
        assert!(feedback_params(&form(CATEGORIES.len(), "x")).is_err());
    }

    #[test]
    fn params_carry_the_form() -> Result<(), String> {
        let params = feedback_params(&FeedbackForm {
            category: 3,
            description: "  Refused a safe command \n".to_string(),
            include_logs: true,
            thread_id: Some("thread-1".to_string()),
        })?;
        assert_eq!(params.classification, "safety_check");
        assert_eq!(params.reason.as_deref(), Some("Refused a safe command"));
        assert!(params.include_logs);
        assert_eq!(params.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(params.extra_log_files, None);
        assert_eq!(
            params.tags,
            Some(BTreeMap::from([(
                "client".to_string(),
                "codex-gui".to_string()
            )]))
        );
        Ok(())
    }

    #[test]
    fn long_descriptions_are_rejected() {
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        assert!(feedback_params(&form(0, &long)).is_err());
        let fits = "é".repeat(MAX_DESCRIPTION_CHARS);
        assert!(feedback_params(&form(0, &fits)).is_ok());
    }

    #[test]
    fn issue_links_only_for_problems() {
        assert_eq!(issue_url(GOOD_RESULT, "abc"), None);
        assert_eq!(
            issue_url(0, "abc").as_deref(),
            Some(
                "https://github.com/openai/codex/issues/new?template=3-cli.yml&steps=Uploaded%20thread:%20abc"
            )
        );
    }

    #[test]
    fn disabled_by_config_or_policy() {
        assert_eq!(disabled_reason(None, None), None);
        assert_eq!(
            disabled_reason(Some(&json!({"feedback": {"enabled": true}})), None),
            None
        );
        assert!(disabled_reason(Some(&json!({"feedback": {"enabled": false}})), None).is_some());
        assert!(disabled_reason(None, Some(false)).is_some());
    }
}
