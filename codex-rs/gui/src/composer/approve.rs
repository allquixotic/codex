//! Auto-review denials and `/approve` (the TUI's `SlashCommand::AutoReview`).
//!
//! When a thread's approval requests go to the auto-review agent
//! (`ApprovalsReviewer::AutoReview`, the "Auto-review" permission preset),
//! a denied action is declined and the turn continues. `/approve` lets the
//! user authorize one retry of the most recent denial through
//! `thread/approveGuardianDeniedAction`; the model sees the approval and the
//! retry still goes through auto-review.

use std::collections::VecDeque;

use codex_app_server_protocol::AutoReviewDecisionSource;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::GuardianApprovalReviewAction;
use codex_app_server_protocol::GuardianApprovalReviewStatus;
use codex_app_server_protocol::GuardianRiskLevel;
use codex_app_server_protocol::GuardianUserAuthorization;
use codex_app_server_protocol::ItemGuardianApprovalReviewCompletedNotification;
use codex_app_server_protocol::ThreadApproveGuardianDeniedActionParams;
use codex_app_server_protocol::ThreadApproveGuardianDeniedActionResponse;
use codex_protocol::approvals::GuardianAssessmentAction;
use codex_protocol::approvals::GuardianAssessmentDecisionSource;
use codex_protocol::approvals::GuardianAssessmentEvent;
use codex_protocol::approvals::GuardianAssessmentStatus;

use crate::app::AppController;
use crate::app::DialogRequest;
use crate::backend::BackendError;

/// Denials kept per thread (as in the TUI).
const MAX_RECENT_DENIALS: usize = 10;
/// Longest stdin excerpt shown for a denied terminal write.
const MAX_STDIN_PREVIEW_CHARS: usize = 80;

/// One action the auto-review agent denied.
#[derive(Clone, Debug)]
pub(crate) struct Denial {
    /// One-line description of the action ("rm -rf build", "network access
    /// to example.com").
    pub(crate) summary: String,
    pub(crate) rationale: Option<String>,
    /// The assessment `thread/approveGuardianDeniedAction` expects back.
    event: GuardianAssessmentEvent,
}

impl Denial {
    pub(crate) fn id(&self) -> &str {
        &self.event.id
    }
}

/// Builds a [`Denial`] from a completed review; `None` unless it was denied
/// (or its action cannot be expressed in the core protocol).
pub(crate) fn denial_from_review(
    review: &ItemGuardianApprovalReviewCompletedNotification,
) -> Option<Denial> {
    if review.review.status != GuardianApprovalReviewStatus::Denied {
        return None;
    }
    let summary = action_summary(&review.action);
    let action: GuardianAssessmentAction = match review.action.clone().try_into() {
        Ok(action) => action,
        Err(err) => {
            tracing::warn!(%err, "auto-review denial has an unusable action; /approve skips it");
            return None;
        }
    };
    let event = GuardianAssessmentEvent {
        review_reason: None,
        model_context: None,
        id: review.review_id.clone(),
        target_item_id: review.target_item_id.clone(),
        plugin_id: None,
        script_path: None,
        turn_id: review.turn_id.clone(),
        started_at_ms: review.started_at_ms,
        completed_at_ms: Some(review.completed_at_ms),
        status: GuardianAssessmentStatus::Denied,
        risk_level: review.review.risk_level.map(|risk| match risk {
            GuardianRiskLevel::Low => codex_protocol::approvals::GuardianRiskLevel::Low,
            GuardianRiskLevel::Medium => codex_protocol::approvals::GuardianRiskLevel::Medium,
            GuardianRiskLevel::High => codex_protocol::approvals::GuardianRiskLevel::High,
            GuardianRiskLevel::Critical => codex_protocol::approvals::GuardianRiskLevel::Critical,
        }),
        user_authorization: review.review.user_authorization.map(
            |authorization| match authorization {
                GuardianUserAuthorization::Unknown => {
                    codex_protocol::approvals::GuardianUserAuthorization::Unknown
                }
                GuardianUserAuthorization::Low => {
                    codex_protocol::approvals::GuardianUserAuthorization::Low
                }
                GuardianUserAuthorization::Medium => {
                    codex_protocol::approvals::GuardianUserAuthorization::Medium
                }
                GuardianUserAuthorization::High => {
                    codex_protocol::approvals::GuardianUserAuthorization::High
                }
            },
        ),
        rationale: review.review.rationale.clone(),
        decision_source: Some(match review.decision_source {
            AutoReviewDecisionSource::Agent => GuardianAssessmentDecisionSource::Agent,
        }),
        action,
    };
    Some(Denial {
        summary,
        rationale: review
            .review
            .rationale
            .clone()
            .filter(|rationale| !rationale.trim().is_empty()),
        event,
    })
}

/// Records `denial` as the newest of `denials`, replacing an older copy of
/// the same review and keeping at most [`MAX_RECENT_DENIALS`].
pub(crate) fn push_denial(denials: &mut VecDeque<Denial>, denial: Denial) {
    denials.retain(|existing| existing.id() != denial.id());
    denials.push_front(denial);
    denials.truncate(MAX_RECENT_DENIALS);
}

/// One-line description of a reviewed action, like the TUI's denial list.
pub(crate) fn action_summary(action: &GuardianApprovalReviewAction) -> String {
    match action {
        GuardianApprovalReviewAction::Command { command, .. } => command.clone(),
        GuardianApprovalReviewAction::Execve { program, argv, .. } => {
            let words = if argv.is_empty() {
                vec![program.clone()]
            } else {
                argv.clone()
            };
            shlex::try_join(words.iter().map(String::as_str)).unwrap_or_else(|_| words.join(" "))
        }
        GuardianApprovalReviewAction::WriteStdin {
            process_id, stdin, ..
        } => {
            let stdin = crate::app::truncate_chars(&format!("{stdin:?}"), MAX_STDIN_PREVIEW_CHARS);
            format!("send input to terminal {process_id}: {stdin}")
        }
        GuardianApprovalReviewAction::ApplyPatch { files, .. } => match files.as_slice() {
            [file] => format!("apply_patch touching {file}"),
            files => format!("apply_patch touching {} files", files.len()),
        },
        GuardianApprovalReviewAction::NetworkAccess { target, .. } => {
            format!("network access to {target}")
        }
        GuardianApprovalReviewAction::McpToolCall {
            server,
            tool_name,
            connector_name,
            ..
        } => {
            let label = connector_name.as_deref().unwrap_or(server.as_str());
            format!("MCP {tool_name} on {label}")
        }
        GuardianApprovalReviewAction::RequestPermissions { reason, .. } => reason
            .as_deref()
            .map(|reason| format!("permission request: {reason}"))
            .unwrap_or_else(|| "permission request".to_string()),
    }
}

/// Body of the `/approve` confirmation for `denial`, with `older` more
/// denials still listed.
pub(crate) fn approval_message(denial: &Denial, older: usize) -> String {
    let rationale = denial
        .rationale
        .as_deref()
        .unwrap_or("Auto-review did not include a rationale.");
    let mut message = format!(
        "{}\n\nAuto-review: {rationale}\n\nCodex may retry this action once. The retry still goes through auto-review.",
        denial.summary
    );
    match older {
        0 => {}
        1 => message.push_str("\n\n1 older denial remains; run /approve again to review it."),
        older => message.push_str(&format!(
            "\n\n{older} older denials remain; run /approve again to review them."
        )),
    }
    message
}

impl AppController {
    /// Records auto-review denials for the thread they belong to.
    pub(super) fn composer_on_guardian_review(
        &mut self,
        review: &ItemGuardianApprovalReviewCompletedNotification,
    ) {
        let Some(denial) = denial_from_review(review) else {
            return;
        };
        if let Some(index) = self.tab_index_for_thread(&review.thread_id)
            && let Some(thread) = self.thread_tab_mut(index)
        {
            push_denial(&mut thread.composer.auto_review_denials, denial);
        }
    }

    /// `/approve`: asks to approve one retry of the newest denial of tab
    /// `index`.
    pub(super) fn composer_approve_denial(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let Some(denial) = thread.composer.auto_review_denials.front().cloned() else {
            self.toast("No recent auto-review denials in this thread");
            return;
        };
        let older = thread.composer.auto_review_denials.len() - 1;
        let id = denial.id().to_string();
        self.show_dialog(
            DialogRequest::confirm("Approve a denied action?", approval_message(&denial, older))
                .accept_label("Approve retry")
                .destructive(),
            Box::new(move |app, accepted| {
                if accepted.is_some() {
                    app.composer_send_denial_approval(tab_id, &id);
                }
            }),
        );
    }

    fn composer_send_denial_approval(&mut self, tab_id: crate::app::TabId, id: &str) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        let denials = &mut thread.composer.auto_review_denials;
        let Some(position) = denials.iter().position(|denial| denial.id() == id) else {
            self.toast("That auto-review denial is no longer available");
            return;
        };
        let Some(denial) = denials.remove(position) else {
            return;
        };
        let event = match serde_json::to_value(&denial.event) {
            Ok(event) => event,
            Err(err) => {
                self.toast(format!("Could not approve the action: {err}"));
                return;
            }
        };
        self.backend.call(
            |request_id| ClientRequest::ThreadApproveGuardianDeniedAction {
                request_id,
                params: ThreadApproveGuardianDeniedActionParams { thread_id, event },
            },
            move |app, result: Result<ThreadApproveGuardianDeniedActionResponse, BackendError>| {
                match result {
                    Ok(_) => {
                        app.toast("Approved one retry; the retry still goes through auto-review")
                    }
                    Err(err) => {
                        if let Some(index) = app.tab_index_by_id(tab_id)
                            && let Some(thread) = app.thread_tab_mut(index)
                        {
                            push_denial(&mut thread.composer.auto_review_denials, denial);
                        }
                        app.toast(format!(
                            "Could not approve the action: {}",
                            err.user_message()
                        ));
                    }
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn review(
        id: &str,
        status: &str,
        action: serde_json::Value,
    ) -> ItemGuardianApprovalReviewCompletedNotification {
        serde_json::from_value(serde_json::json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "startedAtMs": 1,
            "completedAtMs": 2,
            "reviewId": id,
            "targetItemId": null,
            "decisionSource": "agent",
            "review": {
                "status": status,
                "riskLevel": "high",
                "userAuthorization": "low",
                "rationale": "Deletes files outside the workspace.",
            },
            "action": action,
        }))
        .expect("valid review json")
    }

    /// An absolute folder on every platform (actions carry native paths).
    fn cwd() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    fn command(command: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "command",
            "source": "shell",
            "command": command,
            "cwd": cwd(),
        })
    }

    #[test]
    fn only_denied_reviews_become_denials() {
        let denied = denial_from_review(&review("r1", "denied", command("rm -rf /tmp/x")))
            .expect("denied review is recorded");
        assert_eq!(denied.summary, "rm -rf /tmp/x");
        assert_eq!(denied.id(), "r1");
        assert_eq!(
            denied.rationale.as_deref(),
            Some("Deletes files outside the workspace.")
        );
        assert_eq!(denied.event.status, GuardianAssessmentStatus::Denied);
        assert_eq!(denied.event.turn_id, "turn-1");
        assert_eq!(denied.event.completed_at_ms, Some(2));
        // The approval RPC gets the core event back, with the action.
        let event = serde_json::to_value(&denied.event).expect("event serializes");
        assert_eq!(event["id"], "r1");
        assert_eq!(event["status"], "denied");
        assert!(denial_from_review(&review("r2", "approved", command("ls"))).is_none());
    }

    #[test]
    fn keeps_the_ten_newest_denials_newest_first() {
        let mut denials = VecDeque::new();
        for id in 0..12 {
            let denial = denial_from_review(&review(&format!("r{id}"), "denied", command("x")))
                .expect("denied");
            push_denial(&mut denials, denial);
        }
        // A repeated review replaces its older copy.
        let again = denial_from_review(&review("r5", "denied", command("x"))).expect("denied");
        push_denial(&mut denials, again);
        let ids: Vec<&str> = denials.iter().map(Denial::id).collect();
        assert_eq!(
            ids,
            vec!["r5", "r11", "r10", "r9", "r8", "r7", "r6", "r4", "r3", "r2"]
        );
    }

    #[test]
    fn summaries_describe_each_action() {
        let summary = |action: serde_json::Value| {
            let action: GuardianApprovalReviewAction =
                serde_json::from_value(action).expect("valid action json");
            action_summary(&action)
        };
        assert_eq!(
            summary(serde_json::json!({
                "type": "networkAccess",
                "target": "example.com:443",
                "host": "example.com",
                "protocol": "https",
                "port": 443,
            })),
            "network access to example.com:443"
        );
        assert_eq!(
            summary(serde_json::json!({
                "type": "applyPatch",
                "cwd": "/repo",
                "files": ["/repo/a.rs", "/repo/b.rs"],
            })),
            "apply_patch touching 2 files"
        );
        assert_eq!(
            summary(serde_json::json!({
                "type": "execve",
                "source": "shell",
                "program": "/bin/rm",
                "argv": ["rm", "-rf", "my dir"],
                "cwd": cwd(),
            })),
            "rm -rf 'my dir'"
        );
        assert_eq!(
            summary(serde_json::json!({
                "type": "mcpToolCall",
                "server": "docs",
                "toolName": "search",
                "connectorId": null,
                "connectorName": null,
                "toolTitle": null,
            })),
            "MCP search on docs"
        );
    }

    #[test]
    fn approval_message_mentions_older_denials() {
        let denial =
            denial_from_review(&review("r1", "denied", command("rm -rf build"))).expect("denied");
        let message = approval_message(&denial, 0);
        assert!(message.starts_with("rm -rf build\n\nAuto-review: Deletes files"));
        assert!(!message.contains("older"));
        assert!(approval_message(&denial, 1).ends_with("run /approve again to review it."));
        assert!(approval_message(&denial, 3).contains("3 older denials remain"));
    }
}
