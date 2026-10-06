//! "Continue in worktree" (`/worktree`, GUI.md §6): creates a managed Git
//! worktree of the thread's repository with `codex-worktree` (the same
//! checkout layout as Codex Desktop, the TUI and `codex exec --worktree`),
//! forks the thread into the checkout, and binds the new thread to it.
//!
//! Git runs on Tokio's blocking pool. A checkout that cannot be used is kept
//! on disk (it may hold the only copy of something) and the error says where
//! it is.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::ExperimentalFeatureListParams;
use codex_app_server_protocol::ExperimentalFeatureListResponse;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadSource;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_worktree::CreateWorktree;
use codex_worktree::ManagedWorktree;
use codex_worktree::WorktreeManager;
use codex_worktree::WorktreeSettings;
use serde_json::Value;

use crate::app::AppController;
use crate::app::TabId;
use crate::app::TabKind;
use crate::app::ThreadPhase;
use crate::app::ThreadTab;
use crate::backend::Backend;
use crate::transcript::HistoryStart;
use crate::transcript::NoticeKind;

/// Feature flag (`[features] worktrees`) that gates managed worktrees.
const WORKTREES_FEATURE: &str = "worktrees";

const NO_HISTORY_MESSAGE: &str = "Send a message first: a thread without messages is not saved yet, so it cannot continue in a worktree.";

/// What decides whether a thread can continue in a worktree right now.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WorktreeContext {
    /// The GUI talks to a daemon or remote server, whose files are elsewhere.
    pub(crate) remote: bool,
    pub(crate) has_thread_id: bool,
    pub(crate) side_chat: bool,
    pub(crate) busy: bool,
    pub(crate) already_creating: bool,
    pub(crate) has_codex_home: bool,
}

/// Why a thread cannot continue in a worktree, checked before any Git work.
pub(crate) fn worktree_blocker(context: WorktreeContext) -> Option<&'static str> {
    if context.remote {
        Some(
            "Worktrees can only be created with the built-in app-server, not over a remote connection.",
        )
    } else if !context.has_codex_home {
        Some("The Codex home folder is unknown, so there is nowhere to create a worktree.")
    } else if context.side_chat {
        Some("Side chats are temporary and cannot continue in a worktree. Use the main thread.")
    } else if !context.has_thread_id {
        Some("Wait for the thread to start before continuing in a worktree.")
    } else if context.already_creating {
        Some("A worktree is already being created for this thread.")
    } else if context.busy {
        Some("Wait for the current turn to finish before continuing in a worktree.")
    } else {
        None
    }
}

/// Why the thread cannot be forked, from a newest-first `thread/turns/list`
/// page: the turn count, or the request's error message. A thread nothing
/// was sent in has no rollout, and `thread/fork` would fail only after the
/// checkout exists. Other errors are left to the fork to report.
pub(crate) fn history_blocker(turns: &Result<usize, String>) -> Option<&'static str> {
    match turns {
        Ok(0) => Some(NO_HISTORY_MESSAGE),
        Err(message) if super::is_unsaved_thread_error(message) => Some(NO_HISTORY_MESSAGE),
        Ok(_) | Err(_) => None,
    }
}

/// Whether the `worktrees` feature is on for the thread. A server that does
/// not list it predates the flag, which is on by default.
pub(crate) fn worktrees_enabled(features: &ExperimentalFeatureListResponse) -> bool {
    features
        .data
        .iter()
        .find(|feature| feature.name == WORKTREES_FEATURE)
        .is_none_or(|feature| feature.enabled)
}

/// User-facing message for a failed checkout creation.
pub(crate) fn creation_error(error: &str, source_cwd: &Path) -> String {
    if error.contains("not a git repository") {
        format!(
            "“{}” is not inside a Git repository, so it cannot continue in a worktree.",
            crate::app::folder_label(source_cwd)
        )
    } else if error.contains("failed to start git") {
        "Git could not be started. Install Git and make sure it is on your PATH.".to_string()
    } else {
        format!("Could not create a worktree: {error}")
    }
}

/// Error for a checkout that was created but could not be used.
fn retained_error(checkout: &ManagedWorktree, reason: &str) -> String {
    format!(
        "{reason} The new checkout was kept at {}; remove it with `git worktree remove <path>` from the source repository if you do not need it.",
        checkout.root.display()
    )
}

/// Notice shown at the top of the new tab.
fn continued_notice(checkout: &ManagedWorktree) -> String {
    let short_sha: String = checkout.head_sha.chars().take(8).collect();
    format!(
        "Continuing in a new worktree at {} (detached at {short_sha}). Changes made here stay out of {}.",
        crate::newtab::display_path(&checkout.root),
        crate::newtab::display_path(&checkout.source_root)
    )
}

/// `thread/fork` params that continue `thread_id` inside `checkout_cwd`.
pub(crate) fn worktree_fork_params(
    thread_id: String,
    checkout_cwd: &Path,
    model: Option<String>,
    model_provider: Option<String>,
) -> ThreadForkParams {
    ThreadForkParams {
        thread_id,
        cwd: Some(checkout_cwd.to_string_lossy().into_owned()),
        model,
        model_provider,
        thread_source: Some(ThreadSource::User),
        exclude_turns: true,
        // An active goal resumes with the next turn instead of starting
        // work in the new checkout on its own.
        defer_goal_continuation: true,
        ..ThreadForkParams::default()
    }
}

/// Everything the background task needs.
struct WorktreeRequest {
    thread_id: String,
    source_cwd: PathBuf,
    codex_home: PathBuf,
    model: Option<String>,
    model_provider: Option<String>,
}

/// A new thread bound to a new checkout.
struct WorktreeFork {
    response: ThreadForkResponse,
    checkout: ManagedWorktree,
}

async fn create_worktree_fork(
    backend: Backend,
    request: WorktreeRequest,
) -> Result<WorktreeFork, String> {
    // Checked before any Git work, so a refused fork leaves no checkout.
    let turns = backend
        .request::<ThreadTurnsListResponse>(crate::session::turns_page(
            backend.next_request_id(),
            &request.thread_id,
            /*cursor*/ None,
            /*limit*/ 1,
        ))
        .await
        .map(|page| page.data.len())
        .map_err(|err| err.user_message());
    if let Some(reason) = history_blocker(&turns) {
        return Err(reason.to_string());
    }

    let features = backend
        .request::<ExperimentalFeatureListResponse>(ClientRequest::ExperimentalFeatureList {
            request_id: backend.next_request_id(),
            params: ExperimentalFeatureListParams {
                cursor: None,
                limit: None,
                thread_id: Some(request.thread_id.clone()),
            },
        })
        .await;
    match features {
        Ok(features) if !worktrees_enabled(&features) => {
            return Err(
                "Worktrees are turned off. Turn on “worktrees” in Settings › Features to continue in a worktree."
                    .to_string(),
            );
        }
        Ok(_) => {}
        // Older servers cannot list features; the checkout itself is
        // created here, so keep going.
        Err(err) => tracing::debug!(error = %err.user_message(), "feature list failed"),
    }

    // Allocation follows host settings, not the project's (like the TUI).
    let host: ConfigReadResponse = backend
        .request(ClientRequest::ConfigRead {
            request_id: backend.next_request_id(),
            params: ConfigReadParams {
                include_layers: false,
                cwd: None,
            },
        })
        .await
        .map_err(|err| format!("Could not read the configuration: {}", err.user_message()))?;
    let desktop: Option<HashMap<String, Value>> = host.config.desktop;
    let source_cwd = request.source_cwd.clone();
    let codex_home = request.codex_home.clone();
    let (manager, checkout) = tokio::task::spawn_blocking(move || {
        let settings = WorktreeSettings::for_cli(&codex_home, desktop.as_ref())
            .map_err(|err| format!("Invalid worktree settings: {err:#}"))?;
        let manager = WorktreeManager::new(settings);
        manager
            .create(&CreateWorktree {
                source_cwd: source_cwd.clone(),
                base: None,
            })
            .map(|checkout| (manager, checkout))
            .map_err(|err| creation_error(&format!("{err:#}"), &source_cwd))
    })
    .await
    .map_err(|err| format!("Worktree creation stopped unexpectedly: {err}"))??;

    let response: ThreadForkResponse = match backend
        .request(ClientRequest::ThreadFork {
            request_id: backend.next_request_id(),
            params: worktree_fork_params(
                request.thread_id,
                &checkout.cwd,
                request.model,
                request.model_provider,
            ),
        })
        .await
    {
        Ok(response) => response,
        Err(err) => {
            return Err(retained_error(
                &checkout,
                &format!(
                    "Could not fork the thread into the worktree: {}.",
                    err.user_message()
                ),
            ));
        }
    };
    let new_thread_id = response.thread.id.clone();
    if !same_directory(response.cwd.as_path(), &checkout.cwd) {
        discard_thread(&backend, new_thread_id).await;
        return Err(retained_error(
            &checkout,
            "The forked thread did not start in the worktree.",
        ));
    }
    let root = checkout.root.clone();
    let bind_id = new_thread_id.clone();
    let bound = tokio::task::spawn_blocking(move || manager.bind_thread(&root, &bind_id))
        .await
        .map_err(|err| err.to_string())
        .and_then(|result| result.map_err(|err| format!("{err:#}")));
    if let Err(err) = bound {
        discard_thread(&backend, new_thread_id).await;
        return Err(retained_error(
            &checkout,
            &format!("Could not register the worktree for the new thread: {err}."),
        ));
    }
    Ok(WorktreeFork { response, checkout })
}

fn same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (std::fs::canonicalize(left), std::fs::canonicalize(right)),
            (Ok(left), Ok(right)) if left == right
        )
}

/// Removes a thread that was forked but cannot be used.
async fn discard_thread(backend: &Backend, thread_id: String) {
    let unsubscribe = backend
        .request::<ThreadUnsubscribeResponse>(ClientRequest::ThreadUnsubscribe {
            request_id: backend.next_request_id(),
            params: ThreadUnsubscribeParams {
                thread_id: thread_id.clone(),
            },
        })
        .await;
    if let Err(err) = unsubscribe {
        tracing::warn!(%err, "could not unsubscribe a discarded worktree thread");
    }
    let archive = backend
        .request::<ThreadArchiveResponse>(ClientRequest::ThreadArchive {
            request_id: backend.next_request_id(),
            params: ThreadArchiveParams { thread_id },
        })
        .await;
    if let Err(err) = archive {
        tracing::warn!(%err, "could not archive a discarded worktree thread");
    }
}

impl AppController {
    /// Tab menu › Continue in worktree.
    pub(crate) fn worktree_continue(&mut self, index: usize) {
        let codex_home = self
            .config
            .as_ref()
            .map(|config| config.codex_home.to_path_buf())
            .or_else(|| self.codex_home.clone());
        // From the connection target, not readiness, so the guard also holds
        // while a remote connection is (re)connecting.
        let remote = !self.backend.connection().is_embedded();
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let context = WorktreeContext {
            remote,
            has_thread_id: thread.thread_id.is_some(),
            side_chat: thread.extras.side_parent.is_some(),
            busy: thread.is_busy() || thread.phase == ThreadPhase::Starting,
            already_creating: thread.extras.worktree_pending,
            has_codex_home: codex_home.is_some(),
        };
        if let Some(reason) = worktree_blocker(context) {
            self.transcript_push_notice(index, NoticeKind::Warning, reason.to_string());
            return;
        }
        let (Some(thread_id), Some(codex_home)) = (thread.thread_id.clone(), codex_home) else {
            return;
        };
        let request = WorktreeRequest {
            thread_id,
            source_cwd: thread.cwd.clone(),
            codex_home,
            model: crate::composer::effective_model(thread),
            model_provider: thread.model_provider.clone(),
        };
        let source_title = thread.title();
        let tab_id = self.tabs[index].id;
        if let Some(thread) = self.thread_tab_mut(index) {
            thread.extras.worktree_pending = true;
        }
        self.toast("Creating a worktree…");
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let result = create_worktree_fork(backend, request).await;
            crate::ui_thread::post(move |app| app.worktree_finished(tab_id, source_title, result));
        });
    }

    fn worktree_finished(
        &mut self,
        source_tab: TabId,
        source_title: String,
        result: Result<WorktreeFork, String>,
    ) {
        let source_index = self.tab_index_by_id(source_tab);
        if let Some(thread) = source_index.and_then(|index| self.thread_tab_mut(index)) {
            thread.extras.worktree_pending = false;
        }
        let fork = match result {
            Ok(fork) => fork,
            Err(message) => {
                match source_index {
                    Some(index) => self.transcript_push_notice(index, NoticeKind::Error, message),
                    None => self.toast(message),
                }
                return;
            }
        };
        let WorktreeFork { response, checkout } = fork;
        let thread_id = response.thread.id.clone();
        let mut thread = ThreadTab::new(checkout.cwd.clone());
        thread.thread_id = Some(thread_id.clone());
        thread.name = Some(format!("{source_title} (worktree)"));
        thread.preview.clone_from(&response.thread.preview);
        thread.model = Some(response.model);
        thread.model_provider = Some(response.model_provider);
        thread.effort = response.reasoning_effort;
        thread.approval_policy = Some(response.approval_policy);
        thread.approvals_reviewer = Some(response.approvals_reviewer);
        thread.sandbox = Some(response.sandbox);
        thread.phase = ThreadPhase::Idle;
        // A fork keeps its source's tools, so it receives cross-tab
        // messages only when the source does.
        thread.xtab_enabled = self.prefs.cross_tab_tools
            && source_index
                .and_then(|index| self.thread_tab(index))
                .is_some_and(|source| source.xtab_enabled);
        let index = self.push_tab(TabKind::Thread(Box::new(thread)), /*activate*/ true);
        self.transcript_load_history(
            index,
            HistoryStart {
                thread_id,
                history_mode: response.thread.history_mode,
                turns: response.thread.turns,
                turns_cursor: None,
                items_cursor: None,
            },
        );
        self.transcript_push_notice(index, NoticeKind::Info, continued_notice(&checkout));
        self.after_thread_opened(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ExperimentalFeature;
    use codex_app_server_protocol::ExperimentalFeatureStage;
    use pretty_assertions::assert_eq;

    fn ready() -> WorktreeContext {
        WorktreeContext {
            has_thread_id: true,
            has_codex_home: true,
            ..WorktreeContext::default()
        }
    }

    #[test]
    fn preconditions_are_checked_in_order() {
        assert_eq!(worktree_blocker(ready()), None);
        assert!(
            worktree_blocker(WorktreeContext {
                remote: true,
                busy: true,
                ..ready()
            })
            .is_some_and(|reason| reason.contains("remote connection"))
        );
        assert!(
            worktree_blocker(WorktreeContext {
                side_chat: true,
                ..ready()
            })
            .is_some_and(|reason| reason.contains("Side chats"))
        );
        assert!(
            worktree_blocker(WorktreeContext {
                has_thread_id: false,
                ..ready()
            })
            .is_some_and(|reason| reason.contains("start"))
        );
        assert!(
            worktree_blocker(WorktreeContext {
                already_creating: true,
                busy: true,
                ..ready()
            })
            .is_some_and(|reason| reason.contains("already"))
        );
        assert!(
            worktree_blocker(WorktreeContext {
                busy: true,
                ..ready()
            })
            .is_some_and(|reason| reason.contains("current turn"))
        );
        assert!(
            worktree_blocker(WorktreeContext {
                has_codex_home: false,
                ..ready()
            })
            .is_some()
        );
    }

    #[test]
    fn threads_without_messages_are_refused_before_any_git_work() {
        assert_eq!(history_blocker(&Ok(0)), Some(NO_HISTORY_MESSAGE));
        assert_eq!(
            history_blocker(&Err("no rollout found for thread id 123".to_string())),
            Some(NO_HISTORY_MESSAGE)
        );
        // What the server says for a new paginated thread (the GUI's kind).
        assert_eq!(
            history_blocker(&Err("thread 01a1 is not materialized yet; thread/turns/list is unavailable before first user message".to_string())),
            Some(NO_HISTORY_MESSAGE)
        );
        assert_eq!(history_blocker(&Ok(1)), None);
        // Other failures are left to the fork to report.
        assert_eq!(history_blocker(&Err("method not found".to_string())), None);
    }

    fn feature(name: &str, enabled: bool) -> ExperimentalFeature {
        ExperimentalFeature {
            name: name.to_string(),
            stage: ExperimentalFeatureStage::Stable,
            display_name: None,
            description: None,
            announcement: None,
            enabled,
            default_enabled: true,
        }
    }

    #[test]
    fn feature_flag_is_read_from_the_feature_list() {
        let list = |data| ExperimentalFeatureListResponse {
            data,
            next_cursor: None,
        };
        assert!(worktrees_enabled(&list(vec![feature("worktrees", true)])));
        assert!(!worktrees_enabled(&list(vec![
            feature("memories", true),
            feature("worktrees", false),
        ])));
        assert!(worktrees_enabled(&list(vec![feature("memories", false)])));
    }

    #[test]
    fn git_errors_become_readable() {
        let cwd = Path::new("/work/notes");
        assert_eq!(
            creation_error(
                "cannot resolve repository root: git command failed: fatal: not a git repository (or any of the parent directories): .git",
                cwd
            ),
            "“notes” is not inside a Git repository, so it cannot continue in a worktree."
        );
        assert!(creation_error("failed to start git: No such file", cwd).contains("Install Git"));
        assert_eq!(
            creation_error("disk full", cwd),
            "Could not create a worktree: disk full"
        );
    }

    #[test]
    fn fork_targets_the_checkout() {
        let params = worktree_fork_params(
            "t1".to_string(),
            Path::new("/home/me/.codex/worktrees/ab12/repo/sub"),
            Some("gpt-5".to_string()),
            None,
        );
        assert_eq!(params.thread_id, "t1");
        assert_eq!(
            params.cwd.as_deref(),
            Some("/home/me/.codex/worktrees/ab12/repo/sub")
        );
        assert!(params.exclude_turns);
        assert!(params.defer_goal_continuation);
        assert!(!params.ephemeral);
        assert_eq!(params.model.as_deref(), Some("gpt-5"));
        assert_eq!(params.thread_source, Some(ThreadSource::User));
    }

    #[test]
    fn creates_a_checkout_in_a_git_repository() -> anyhow::Result<()> {
        let git = |dir: &Path, args: &[&str]| -> anyhow::Result<()> {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Codex GUI Test",
                    "-c",
                    "user.email=codex-gui-test@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                ])
                .args(args)
                .current_dir(dir)
                .output()?
                .status;
            anyhow::ensure!(status.success(), "git {args:?} failed");
            Ok(())
        };
        let home = tempfile::tempdir()?;
        let repo = tempfile::tempdir()?;
        if git(repo.path(), &["init", "-q"]).is_err() {
            // No git on this machine; nothing to test.
            return Ok(());
        }
        std::fs::write(repo.path().join("README.md"), "hello\n")?;
        git(repo.path(), &["add", "."])?;
        git(repo.path(), &["commit", "-q", "-m", "init"])?;
        let manager = WorktreeManager::new(WorktreeSettings::for_cli(home.path(), None)?);
        let checkout = manager.create(&CreateWorktree {
            source_cwd: repo.path().to_path_buf(),
            base: None,
        })?;
        assert!(checkout.cwd.join("README.md").is_file());
        assert!(same_directory(&checkout.cwd, &checkout.cwd));
        manager.bind_thread(&checkout.root, "thread-1")?;
        assert_eq!(manager.owner(&checkout.root)?.as_deref(), Some("thread-1"));

        let plain = tempfile::tempdir()?;
        let error = manager
            .create(&CreateWorktree {
                source_cwd: plain.path().to_path_buf(),
                base: None,
            })
            .err()
            .map(|err| creation_error(&format!("{err:#}"), plain.path()))
            .unwrap_or_default();
        assert!(error.contains("is not inside a Git repository"), "{error}");
        Ok(())
    }
}
