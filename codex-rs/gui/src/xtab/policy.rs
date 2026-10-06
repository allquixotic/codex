//! Whether a cross-tab message would reach a tab that may do more than the
//! tab that sent it.
//!
//! A delivered message starts a turn in the target under the target's own
//! approval policy and sandbox, so an agent in a read-only tab could get
//! work done in a Full access tab by messaging it. Such pairs are never
//! allowed for a whole session: every message asks the user (see
//! `AppController::xtab_agent_send`).

use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::NetworkAccess;
use codex_app_server_protocol::SandboxPolicy;

/// The permissions a tab's turns run with, as far as the GUI knows them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TabPermissions<'a> {
    pub(crate) approval: Option<AskForApproval>,
    pub(crate) sandbox: Option<&'a SandboxPolicy>,
    pub(crate) cwd: &'a Path,
}

/// Why `target` may do more than `caller` without asking the user, as a
/// sentence fragment, or `None` when it may not. Unknown permissions count
/// as more: nothing proves the target is not.
pub(crate) fn escalation(
    caller: &TabPermissions<'_>,
    target: &TabPermissions<'_>,
) -> Option<String> {
    let (Some(caller_approval), Some(caller_sandbox), Some(target_approval), Some(target_sandbox)) = (
        caller.approval,
        caller.sandbox,
        target.approval,
        target.sandbox,
    ) else {
        return Some("the permissions of one of the tabs are not known yet".to_string());
    };
    let mut reasons = Vec::new();
    let (caller_rank, target_rank) = (sandbox_rank(caller_sandbox), sandbox_rank(target_sandbox));
    if target_rank > caller_rank {
        reasons.push(format!(
            "it has {} while the sending tab has {}",
            sandbox_label(target_sandbox),
            sandbox_label(caller_sandbox)
        ));
    } else if target_rank == caller_rank
        && let Some(root) = first_root_outside(
            &writable_roots(target_sandbox, target.cwd),
            &writable_roots(caller_sandbox, caller.cwd),
        )
    {
        reasons.push(format!(
            "it can write in {}, which the sending tab cannot",
            root.display()
        ));
    }
    if network_enabled(target_sandbox) && !network_enabled(caller_sandbox) {
        reasons.push("it has network access and the sending tab does not".to_string());
    }
    if approval_rank(target_approval) > approval_rank(caller_approval) {
        reasons.push(format!(
            "it {} while the sending tab {}",
            approval_label(target_approval),
            approval_label(caller_approval)
        ));
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

/// Higher means fewer prompts before the agent acts.
fn approval_rank(policy: AskForApproval) -> u8 {
    match policy {
        AskForApproval::UnlessTrusted => 0,
        // Granular switches only auto-reject categories, never auto-approve.
        AskForApproval::OnRequest | AskForApproval::Granular { .. } => 1,
        AskForApproval::Never => 2,
    }
}

fn approval_label(policy: AskForApproval) -> &'static str {
    match policy {
        AskForApproval::UnlessTrusted => "asks before untrusted commands",
        AskForApproval::OnRequest => "asks when the agent requests it",
        AskForApproval::Granular { .. } => "uses granular approvals",
        AskForApproval::Never => "never asks for approval",
    }
}

/// Higher means more of the machine is reachable.
fn sandbox_rank(policy: &SandboxPolicy) -> u8 {
    match policy {
        SandboxPolicy::ReadOnly { .. } => 0,
        SandboxPolicy::WorkspaceWrite { .. } => 1,
        SandboxPolicy::ExternalSandbox { .. } | SandboxPolicy::DangerFullAccess => 2,
    }
}

fn sandbox_label(policy: &SandboxPolicy) -> &'static str {
    match policy {
        SandboxPolicy::ReadOnly { .. } => "read-only access",
        SandboxPolicy::WorkspaceWrite { .. } => "write access to its workspace",
        SandboxPolicy::ExternalSandbox { .. } => "no Codex sandbox",
        SandboxPolicy::DangerFullAccess => "full access",
    }
}

fn network_enabled(policy: &SandboxPolicy) -> bool {
    match policy {
        SandboxPolicy::DangerFullAccess => true,
        SandboxPolicy::ReadOnly { network_access } => *network_access,
        SandboxPolicy::WorkspaceWrite { network_access, .. } => *network_access,
        SandboxPolicy::ExternalSandbox { network_access } => {
            *network_access == NetworkAccess::Enabled
        }
    }
}

/// Folders a workspace-write turn may change (temp folders aside).
fn writable_roots(policy: &SandboxPolicy, cwd: &Path) -> Vec<PathBuf> {
    match policy {
        SandboxPolicy::WorkspaceWrite { writable_roots, .. } => std::iter::once(cwd.to_path_buf())
            .chain(
                writable_roots
                    .iter()
                    .map(|root| root.as_path().to_path_buf()),
            )
            .collect(),
        _ => Vec::new(),
    }
}

fn first_root_outside(roots: &[PathBuf], allowed: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .find(|root| !allowed.iter().any(|allowed| root.starts_with(allowed)))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn workspace(roots: &[&str], network_access: bool) -> SandboxPolicy {
        match serde_json::from_value(serde_json::json!({
            "type": "workspaceWrite",
            "writableRoots": roots,
            "networkAccess": network_access,
        })) {
            Ok(policy) => policy,
            Err(err) => panic!("invalid policy: {err}"),
        }
    }

    fn read_only() -> SandboxPolicy {
        SandboxPolicy::ReadOnly {
            network_access: false,
        }
    }

    fn tab<'a>(
        approval: AskForApproval,
        sandbox: &'a SandboxPolicy,
        cwd: &'a str,
    ) -> TabPermissions<'a> {
        TabPermissions {
            approval: Some(approval),
            sandbox: Some(sandbox),
            cwd: Path::new(cwd),
        }
    }

    #[test]
    fn equal_or_narrower_targets_do_not_escalate() {
        let on_request = AskForApproval::OnRequest;
        let untrusted = AskForApproval::UnlessTrusted;
        let write = workspace(&[], /*network_access*/ false);
        let ro = read_only();
        let caller = tab(on_request, &write, "/repo");
        assert_eq!(escalation(&caller, &tab(on_request, &write, "/repo")), None);
        assert_eq!(
            escalation(&caller, &tab(on_request, &write, "/repo/sub")),
            None,
            "a sub-folder of the caller's workspace"
        );
        assert_eq!(
            escalation(&caller, &tab(untrusted, &ro, "/elsewhere")),
            None
        );
        let full = SandboxPolicy::DangerFullAccess;
        let never = AskForApproval::Never;
        assert_eq!(
            escalation(&tab(never, &full, "/a"), &tab(never, &full, "/b")),
            None
        );
    }

    #[test]
    fn a_read_only_tab_messaging_a_full_access_tab_escalates() {
        let on_request = AskForApproval::OnRequest;
        let never = AskForApproval::Never;
        let ro = read_only();
        let full = SandboxPolicy::DangerFullAccess;
        assert_eq!(
            escalation(&tab(on_request, &ro, "/pr"), &tab(never, &full, "/repo")),
            Some(
                "it has full access while the sending tab has read-only access; \
                 it has network access and the sending tab does not; \
                 it never asks for approval while the sending tab asks when the agent requests it"
                    .to_string()
            )
        );
    }

    #[test]
    fn writes_outside_the_callers_folders_escalate() {
        let on_request = AskForApproval::OnRequest;
        let caller_write = workspace(&["/shared"], /*network_access*/ false);
        let target_write = workspace(&["/shared/cache"], /*network_access*/ false);
        let caller = tab(on_request, &caller_write, "/repo-a");
        assert_eq!(
            escalation(&caller, &tab(on_request, &target_write, "/repo-b")),
            Some("it can write in /repo-b, which the sending tab cannot".to_string())
        );
        assert_eq!(
            escalation(&caller, &tab(on_request, &target_write, "/repo-a/sub")),
            None,
            "every target root is inside a caller root"
        );
    }

    #[test]
    fn network_and_approval_differences_escalate() {
        let on_request = AskForApproval::OnRequest;
        let untrusted = AskForApproval::UnlessTrusted;
        let offline = read_only();
        let online = SandboxPolicy::ReadOnly {
            network_access: true,
        };
        assert_eq!(
            escalation(
                &tab(on_request, &offline, "/r"),
                &tab(on_request, &online, "/r")
            ),
            Some("it has network access and the sending tab does not".to_string())
        );
        assert_eq!(
            escalation(
                &tab(untrusted, &offline, "/r"),
                &tab(on_request, &offline, "/r")
            ),
            Some(
                "it asks when the agent requests it while the sending tab asks before untrusted commands"
                    .to_string()
            )
        );
        let granular: AskForApproval = match serde_json::from_value(serde_json::json!({
            "granular": {"sandbox_approval": false, "rules": false, "mcp_elicitations": false}
        })) {
            Ok(policy) => policy,
            Err(err) => panic!("invalid policy: {err}"),
        };
        assert_eq!(
            escalation(
                &tab(on_request, &offline, "/r"),
                &tab(granular, &offline, "/r")
            ),
            None,
            "granular approvals never auto-approve"
        );
    }

    #[test]
    fn unknown_permissions_always_escalate() {
        let on_request = AskForApproval::OnRequest;
        let ro = read_only();
        let known = tab(on_request, &ro, "/r");
        let unknown = TabPermissions {
            approval: None,
            sandbox: Some(&ro),
            cwd: Path::new("/r"),
        };
        assert!(escalation(&known, &unknown).is_some());
        assert!(escalation(&unknown, &known).is_some());
    }
}
