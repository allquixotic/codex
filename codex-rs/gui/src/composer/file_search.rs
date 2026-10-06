//! `@` mention file search: one session per search root, the query updated
//! on every keystroke, results posted back to the UI thread.
//!
//! Local servers (embedded, `unix://` daemon) share this machine's files, so
//! the search runs here with `codex-file-search`, like the TUI. A remote
//! (WebSocket) server's folders exist only there, so the search runs on the
//! server through `fuzzyFileSearch/sessionStart|sessionUpdate|sessionStop`
//! and its `sessionUpdated` / `sessionCompleted` notifications.
//!
//! "Searching…" ends with a snapshot from a finished walk or with the
//! session's completion signal: the matcher does not send a final snapshot
//! when nothing changed after the walk finished.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::FuzzyFileSearchMatchType;
use codex_app_server_protocol::FuzzyFileSearchResult;
use codex_app_server_protocol::FuzzyFileSearchSessionStartParams;
use codex_app_server_protocol::FuzzyFileSearchSessionStartResponse;
use codex_app_server_protocol::FuzzyFileSearchSessionStopParams;
use codex_app_server_protocol::FuzzyFileSearchSessionStopResponse;
use codex_app_server_protocol::FuzzyFileSearchSessionUpdateParams;
use codex_app_server_protocol::FuzzyFileSearchSessionUpdateResponse;
use codex_file_search::FileMatch;
use codex_file_search::FileSearchOptions;
use codex_file_search::FileSearchSession;
use codex_file_search::FileSearchSnapshot;
use codex_file_search::SessionReporter;

use crate::app::AppController;
use crate::backend::Backend;
use crate::backend::BackendError;

/// Owns the live search session for the active composer.
#[derive(Default)]
pub(crate) struct FileSearch {
    root: Option<PathBuf>,
    session: Option<SearchSession>,
    progress: Progress,
}

/// Which results and completion signals belong to the current query.
#[derive(Debug, Default)]
struct Progress {
    /// Bumped whenever the session is replaced so stale results are ignored.
    generation: u64,
    query: String,
    /// Query of the newest results the session delivered.
    reported_query: Option<String>,
    /// Query the session reported complete for.
    done_query: Option<String>,
}

impl Progress {
    /// A new session starts.
    fn restart(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.reported_query = None;
        self.done_query = None;
    }

    fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.done_query = None;
    }

    fn accept_results(&mut self, generation: u64, query: &str) -> bool {
        if self.generation != generation {
            return false;
        }
        self.reported_query = Some(query.to_string());
        self.query == query
    }

    fn finish(&mut self, generation: u64) -> Option<String> {
        if self.generation != generation || self.query.is_empty() {
            return None;
        }
        if self
            .reported_query
            .as_ref()
            .is_some_and(|reported| *reported != self.query)
        {
            return None;
        }
        self.done_query = Some(self.query.clone());
        Some(self.query.clone())
    }
}

enum SearchSession {
    Local(FileSearchSession),
    Remote(RemoteSession),
}

/// Orders a remote session's requests. Each `Backend::call` travels on its
/// own task, so two calls can reach the server in either order: the query
/// is only sent after `sessionStart` was answered, one update at a time,
/// and only the newest waiting query is kept.
#[derive(Debug, Default)]
struct RequestQueue {
    started: bool,
    in_flight: bool,
    pending: Option<String>,
}

impl RequestQueue {
    /// A new query; returns it when it can be sent now.
    fn push(&mut self, query: &str) -> Option<String> {
        if self.started && !self.in_flight {
            self.in_flight = true;
            return Some(query.to_string());
        }
        self.pending = Some(query.to_string());
        None
    }

    /// `sessionStart` was answered; returns the query to send next.
    fn on_started(&mut self) -> Option<String> {
        self.started = true;
        self.next()
    }

    /// An update was answered; returns the query to send next.
    fn on_updated(&mut self) -> Option<String> {
        self.in_flight = false;
        self.next()
    }

    fn next(&mut self) -> Option<String> {
        if self.in_flight {
            return None;
        }
        let query = self.pending.take()?;
        self.in_flight = true;
        Some(query)
    }
}

/// A search session on the app-server; stopped when dropped.
struct RemoteSession {
    id: String,
    backend: Backend,
    queue: RequestQueue,
}

impl RemoteSession {
    fn start(backend: &Backend, root: &Path, generation: u64) -> Self {
        let id = format!("codex-gui-{}", uuid::Uuid::new_v4().simple());
        let params = FuzzyFileSearchSessionStartParams {
            session_id: id.clone(),
            roots: vec![root.to_string_lossy().into_owned()],
        };
        backend.call(
            |request_id| ClientRequest::FuzzyFileSearchSessionStart { request_id, params },
            move |app, result: Result<FuzzyFileSearchSessionStartResponse, BackendError>| {
                match result {
                    Ok(_) => app.composer_on_remote_search_reply(generation, /*started*/ true),
                    Err(err) => {
                        tracing::warn!(%err, "remote file search failed to start");
                        app.composer_on_file_search_done(generation);
                    }
                }
            },
        );
        Self {
            id,
            backend: backend.clone(),
            queue: RequestQueue::default(),
        }
    }

    fn send_query(&self, query: String, generation: u64) {
        let params = FuzzyFileSearchSessionUpdateParams {
            session_id: self.id.clone(),
            query,
        };
        self.backend.call(
            |request_id| ClientRequest::FuzzyFileSearchSessionUpdate { request_id, params },
            move |app, result: Result<FuzzyFileSearchSessionUpdateResponse, BackendError>| {
                if let Err(err) = result {
                    tracing::warn!(%err, "remote file search update failed");
                    app.composer_on_file_search_done(generation);
                }
                app.composer_on_remote_search_reply(generation, /*started*/ false);
            },
        );
    }
}

impl AppController {
    /// The server answered the remote session's `sessionStart` (`started`)
    /// or a `sessionUpdate`: sends the query that waited for it.
    fn composer_on_remote_search_reply(&mut self, generation: u64, started: bool) {
        let search = &mut self.composer_shared.file_search;
        if search.progress.generation != generation {
            return;
        }
        let Some(SearchSession::Remote(session)) = search.session.as_mut() else {
            return;
        };
        let next = if started {
            session.queue.on_started()
        } else {
            session.queue.on_updated()
        };
        if let Some(query) = next {
            session.send_query(query, generation);
        }
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        let session_id = self.id.clone();
        self.backend
            .fire::<FuzzyFileSearchSessionStopResponse, _>(|request_id| {
                ClientRequest::FuzzyFileSearchSessionStop {
                    request_id,
                    params: FuzzyFileSearchSessionStopParams { session_id },
                }
            });
    }
}

impl FileSearch {
    /// Updates the query for `root`, starting a session when needed. With
    /// `remote`, the server searches its own files.
    ///
    /// An empty query drops the session (the walk restarts with the next
    /// query, as in the TUI).
    pub(crate) fn update(&mut self, root: &Path, query: &str, remote: Option<&Backend>) {
        let remote_session = matches!(self.session, Some(SearchSession::Remote(_)));
        if self.root.as_deref() != Some(root)
            || (self.session.is_some() && remote_session != remote.is_some())
        {
            self.stop();
            self.root = Some(root.to_path_buf());
        }
        if query == self.progress.query && self.session.is_some() {
            return;
        }
        self.progress.set_query(query);
        if query.is_empty() {
            self.session = None;
            return;
        }
        if self.session.is_none() {
            self.progress.restart();
            let generation = self.progress.generation;
            self.session = match remote {
                Some(backend) => Some(SearchSession::Remote(RemoteSession::start(
                    backend, root, generation,
                ))),
                None => {
                    let reporter = Arc::new(GuiReporter { generation });
                    match codex_file_search::create_session(
                        vec![root.to_path_buf()],
                        FileSearchOptions {
                            compute_indices: true,
                            ..FileSearchOptions::default()
                        },
                        reporter,
                        /*cancel_flag*/ None,
                    ) {
                        Ok(session) => Some(SearchSession::Local(session)),
                        Err(err) => {
                            tracing::warn!(%err, root = %root.display(), "file search failed to start");
                            return;
                        }
                    }
                }
            };
        }
        let generation = self.progress.generation;
        match self.session.as_mut() {
            Some(SearchSession::Local(session)) => session.update_query(query),
            Some(SearchSession::Remote(session)) => {
                if let Some(query) = session.queue.push(query) {
                    session.send_query(query, generation);
                }
            }
            None => {}
        }
    }

    /// Drops the session (popup closed or tab switched).
    pub(crate) fn stop(&mut self) {
        self.session = None;
        self.progress.set_query("");
        self.progress.restart();
    }

    /// Records results for `query` from `generation`; true when they are
    /// still wanted.
    pub(crate) fn accept_results(&mut self, generation: u64, query: &str) -> bool {
        self.session.is_some() && self.progress.accept_results(generation, query)
    }

    /// The session of `generation` finished matching. Returns the current
    /// query when that completes it: completion carries no query, so it
    /// only counts when the newest results were for the current query (a
    /// completion for an older query may arrive after the user typed on).
    pub(crate) fn finish(&mut self, generation: u64) -> Option<String> {
        self.session.as_ref()?;
        self.progress.finish(generation)
    }

    /// Whether the session already reported `query` complete.
    pub(crate) fn is_done(&self, query: &str) -> bool {
        self.progress.done_query.as_deref() == Some(query)
    }

    /// Generation of the remote session `session_id`, when it is current.
    pub(crate) fn remote_generation(&self, session_id: &str) -> Option<u64> {
        match self.session.as_ref() {
            Some(SearchSession::Remote(session)) if session.id == session_id => {
                Some(self.progress.generation)
            }
            _ => None,
        }
    }
}

struct GuiReporter {
    generation: u64,
}

impl SessionReporter for GuiReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        if snapshot.query.is_empty() {
            return;
        }
        let generation = self.generation;
        let query = snapshot.query.clone();
        let hits: Vec<FileHit> = snapshot.matches.iter().map(FileHit::from_match).collect();
        let complete = snapshot.walk_complete;
        crate::ui_thread::post(move |app| {
            app.composer_on_file_matches(generation, &query, hits, complete);
        });
    }

    /// Called after every matcher pass once the walk is done (and when the
    /// session ends). Without it, a walk that finishes after the last
    /// result snapshot would leave the popup on "Searching…".
    fn on_complete(&self) {
        let generation = self.generation;
        crate::ui_thread::post(move |app| app.composer_on_file_search_done(generation));
    }
}

/// A file match rendered for the popup: path relative to the search root,
/// with forward slashes, and matched char indices into that string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileHit {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
    pub(crate) indices: Vec<u32>,
}

impl FileHit {
    pub(crate) fn from_match(found: &FileMatch) -> Self {
        let path = found.path.to_string_lossy().replace('\\', "/");
        Self {
            path,
            is_dir: matches!(found.match_type, codex_file_search::MatchType::Directory),
            indices: found.indices.clone().unwrap_or_default(),
        }
    }

    /// A match from the server's `fuzzyFileSearch/sessionUpdated`.
    pub(crate) fn from_remote(found: &FuzzyFileSearchResult) -> Self {
        Self {
            path: found.path.replace('\\', "/"),
            is_dir: found.match_type == FuzzyFileSearchMatchType::Directory,
            indices: found.indices.clone().unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn converts_matches_for_display() {
        let found = FileMatch {
            score: 10,
            path: PathBuf::from("src/main.rs"),
            match_type: codex_file_search::MatchType::File,
            root: PathBuf::from("/repo"),
            indices: Some(vec![0, 4]),
        };
        assert_eq!(
            FileHit::from_match(&found),
            FileHit {
                path: "src/main.rs".to_string(),
                is_dir: false,
                indices: vec![0, 4],
            }
        );
        let remote = FuzzyFileSearchResult {
            root: "/srv/repo".to_string(),
            path: "docs\\guide".to_string(),
            match_type: FuzzyFileSearchMatchType::Directory,
            file_name: "guide".to_string(),
            score: 3,
            indices: None,
        };
        assert_eq!(
            FileHit::from_remote(&remote),
            FileHit {
                path: "docs/guide".to_string(),
                is_dir: true,
                indices: Vec::new(),
            }
        );
    }

    #[test]
    fn stale_generations_are_ignored() {
        let mut search = FileSearch::default();
        assert!(!search.accept_results(0, ""));
        search.stop();
        let generation = search.progress.generation;
        assert!(!search.accept_results(generation, ""));
        assert_eq!(search.finish(generation), None);
        assert_eq!(search.remote_generation("any"), None);
    }

    #[test]
    fn remote_queries_wait_for_the_session_and_each_other() {
        let mut queue = RequestQueue::default();
        // Typed before `sessionStart` was answered: held back.
        assert_eq!(queue.push("m"), None);
        assert_eq!(queue.push("ma"), None);
        // Only the newest query is sent once the session exists.
        assert_eq!(queue.on_started(), Some("ma".to_string()));
        // One update at a time.
        assert_eq!(queue.push("mai"), None);
        assert_eq!(queue.push("main"), None);
        assert_eq!(queue.on_updated(), Some("main".to_string()));
        assert_eq!(queue.on_updated(), None);
        // Idle: sent right away.
        assert_eq!(queue.push("main.r"), Some("main.r".to_string()));
    }

    #[test]
    fn completion_ends_the_search_for_the_current_query_only() {
        let mut progress = Progress::default();
        progress.restart();
        progress.set_query("zzzq");
        let generation = progress.generation;

        // No results yet (an empty folder never sends any): done.
        assert_eq!(progress.finish(generation), Some("zzzq".to_string()));
        assert_eq!(progress.done_query.as_deref(), Some("zzzq"));
        assert_eq!(progress.finish(generation.wrapping_add(1)), None);

        // The user typed on: the old query's results are recorded but not
        // shown, and a completion that follows them does not end the new
        // query's search.
        progress.set_query("zzzqx");
        assert_eq!(progress.done_query, None);
        assert!(!progress.accept_results(generation, "zzzq"));
        assert_eq!(progress.finish(generation), None);

        // Results for the new query, then completion: done.
        assert!(progress.accept_results(generation, "zzzqx"));
        assert_eq!(progress.finish(generation), Some("zzzqx".to_string()));
        assert_eq!(progress.done_query.as_deref(), Some("zzzqx"));

        // A new session forgets both.
        progress.restart();
        assert_eq!(progress.done_query, None);
        assert!(!progress.accept_results(generation, "zzzqx"));
    }
}
