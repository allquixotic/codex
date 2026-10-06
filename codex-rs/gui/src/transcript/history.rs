//! Paged history: initial hydration, "load older" on scroll, refetching
//! trimmed content, and lag resyncs.
//!
//! Paginated threads are read newest-first with `thread/items/list` (100
//! items per page, the server maximum) plus `thread/turns/list` for turn
//! metadata (status, duration, errors), exactly like the TUI. Legacy threads
//! (no pagination) are read with `thread/read(includeTurns)` and sliced
//! locally. Every request runs on Tokio and posts its result back; results
//! carry the tab's history *generation* so a reset (resume, trim) discards
//! responses that are still in flight. Cursors already used are remembered
//! so a misbehaving server cannot make the loader loop.
//!
//! A legacy `thread/read` gives user, agent and reasoning items synthetic
//! ids (`item-N`) instead of the ids their live events carried. Legacy reads
//! therefore also return a *snapshot* of those items in the turns that have
//! live entries, which the store matches to its entries
//! (`Transcript::adopt_snapshot`), and a trimmed live item that a legacy
//! read cannot find by id is located by its turn.

use std::collections::HashMap;
use std::collections::HashSet;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemsListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::Turn;

use crate::backend::Backend;
use crate::backend::BackendError;
use crate::session;

/// Items collected before an older-history request returns.
const OLDER_PAGE_BUDGET: usize = session::HISTORY_ITEM_PAGE_LIMIT as usize;
/// Items collected on open: one page, like the TUI. When they render to
/// few rows, the controller fetches more right away (`FILL_ROWS`).
const INITIAL_ITEM_BUDGET: usize = session::HISTORY_ITEM_PAGE_LIMIT as usize;
/// Upper bound of pages fetched by one request (refetching after a trim may
/// need to skip already-shown pages first).
const MAX_PAGES_PER_REQUEST: usize = 40;
/// Rounds of `thread/turns/list` fetched to describe the items of one page.
const MAX_TURN_PAGES_PER_REQUEST: usize = 4;
/// Items taken per step from a legacy (non-paginated) thread.
const LEGACY_CHUNK: usize = 200;
/// Items reconciled after dropped events.
const RESYNC_ITEMS: u32 = session::HISTORY_ITEM_PAGE_LIMIT;

/// Where older history comes from.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum Older {
    /// Nothing older exists.
    #[default]
    None,
    /// Page newest-first from `cursor` (`None` = the newest end). With
    /// `include_from`, items are skipped until that item id is seen; it and
    /// everything older are new to the transcript (used after trimming).
    Paged {
        cursor: Option<String>,
        include_from: Option<String>,
    },
    /// Legacy thread: re-read it and take the items up to `include_from`
    /// (inclusive; `None` = the newest end).
    Legacy { include_from: Option<String> },
}

/// Pagination state of one tab.
#[derive(Debug, Default)]
pub(crate) struct HistoryState {
    pub(crate) thread_id: Option<String>,
    pub(crate) mode: Option<ThreadHistoryMode>,
    pub(crate) older: Older,
    pub(crate) loading_initial: bool,
    pub(crate) loading_older: bool,
    pub(crate) generation: u64,
    /// Consecutive older loads that added no rows (bounded auto-continue).
    pub(crate) empty_loads: u32,
    pub(crate) cursors: CursorState,
    /// Turn metadata (items stripped) for separators and failure notices.
    pub(crate) turns: HashMap<String, Turn>,
    /// Turn of the newest trimmed item; a legacy read that cannot find
    /// that item by id (it came from a live event) resumes after this turn.
    pub(crate) boundary_turn: Option<String>,
    /// The last history request failed. Scrolling does not load again
    /// until the user retries (the "load earlier messages" pill).
    pub(crate) error: Option<String>,
}

impl HistoryState {
    pub(crate) fn has_older(&self) -> bool {
        self.older != Older::None
    }

    /// Starts a new generation, cancelling requests in flight.
    pub(crate) fn reset(&mut self, thread_id: String, mode: ThreadHistoryMode) {
        self.generation += 1;
        self.thread_id = Some(thread_id);
        self.mode = Some(mode);
        self.older = Older::None;
        self.loading_initial = false;
        self.loading_older = false;
        self.empty_loads = 0;
        self.cursors = CursorState::default();
        self.boundary_turn = None;
        self.error = None;
    }

    /// After trimming the oldest rows: older content has to be refetched,
    /// resuming at the newest dropped item (inclusive). Requests in flight,
    /// an initial load included, are cancelled.
    pub(crate) fn after_trim(&mut self, newest_dropped: Option<TrimBoundary>) {
        let Some(TrimBoundary { id, turn_id }) = newest_dropped else {
            return;
        };
        self.generation += 1;
        self.loading_initial = false;
        self.loading_older = false;
        self.error = None;
        self.empty_loads = 0;
        self.cursors.items_seen.clear();
        self.boundary_turn = turn_id;
        self.older = match self.mode {
            Some(ThreadHistoryMode::Legacy) => Older::Legacy {
                include_from: Some(id),
            },
            _ => Older::Paged {
                cursor: None,
                include_from: Some(id),
            },
        };
    }

    /// Whether loading older history may start without the user asking.
    pub(crate) fn can_auto_load(&self) -> bool {
        self.error.is_none()
    }

    /// Threads started in the tab never loaded history; paging and lag
    /// resyncs use the tab's thread for them. Returns whether the history
    /// belongs to `thread_id`.
    pub(crate) fn attach(&mut self, thread_id: &str) -> bool {
        match &self.thread_id {
            Some(current) => current == thread_id,
            None => {
                self.thread_id = Some(thread_id.to_string());
                true
            }
        }
    }
}

/// The newest server item dropped by trimming.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrimBoundary {
    /// The item's id as history reads report it.
    pub(crate) id: String,
    pub(crate) turn_id: Option<String>,
}

/// Cursor chains with loop guards.
#[derive(Clone, Debug, Default)]
pub(crate) struct CursorState {
    items_seen: HashSet<String>,
    turn_cursor: Option<String>,
    turns_seen: HashSet<String>,
    turns_exhausted: bool,
}

/// Advances a cursor chain, refusing cursors that were already used.
pub(crate) fn advancing_cursor(
    current: Option<&str>,
    next: Option<String>,
    seen: &mut HashSet<String>,
) -> Option<String> {
    if let Some(current) = current {
        seen.insert(current.to_string());
    }
    next.filter(|next| seen.insert(next.clone()))
}

/// What to load.
#[derive(Clone, Debug)]
pub(crate) struct PageRequest {
    pub(crate) thread_id: String,
    pub(crate) generation: u64,
    pub(crate) source: Older,
    pub(crate) cursors: CursorState,
    pub(crate) known_turns: HashSet<String>,
    /// Also fetch the newest turns (open / resume).
    pub(crate) initial_turns: Option<InitialTurns>,
    pub(crate) budget: usize,
    /// Turns with entries from live events; legacy reads return their
    /// snapshot items ([`PageResult::snapshot`]).
    pub(crate) live_turns: HashSet<String>,
    /// See [`HistoryState::boundary_turn`].
    pub(crate) boundary_turn: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct InitialTurns {
    pub(crate) cursor: Option<String>,
    pub(crate) limit: u32,
}

/// Loaded history, chronological.
#[derive(Debug, Default)]
pub(crate) struct PageResult {
    pub(crate) generation: u64,
    pub(crate) items: Vec<(String, ThreadItem)>,
    pub(crate) older: Older,
    pub(crate) cursors: CursorState,
    pub(crate) turns: Vec<Turn>,
    pub(crate) error: Option<String>,
    /// The server cannot page this thread; it was read as a legacy thread.
    pub(crate) switched_to_legacy: bool,
    /// Legacy reads: user, agent and reasoning items of the live turns.
    pub(crate) snapshot: Vec<(String, ThreadItem)>,
}

impl PageRequest {
    pub(crate) fn initial(
        state: &HistoryState,
        source: Older,
        turns_cursor: Option<String>,
        live_turns: HashSet<String>,
    ) -> Self {
        Self {
            source,
            initial_turns: Some(InitialTurns {
                cursor: turns_cursor,
                limit: session::INITIAL_HISTORY_TURN_LIMIT,
            }),
            budget: INITIAL_ITEM_BUDGET,
            ..Self::older(state, live_turns)
        }
    }

    pub(crate) fn older(state: &HistoryState, live_turns: HashSet<String>) -> Self {
        Self {
            thread_id: state.thread_id.clone().unwrap_or_default(),
            generation: state.generation,
            source: state.older.clone(),
            cursors: state.cursors.clone(),
            known_turns: state.turns.keys().cloned().collect(),
            initial_turns: None,
            budget: OLDER_PAGE_BUDGET,
            live_turns,
            boundary_turn: state.boundary_turn.clone(),
        }
    }
}

/// Whether the server cannot page this thread (old server, ephemeral).
pub(crate) fn is_unsupported(err: &BackendError) -> bool {
    match err.server_error() {
        Some(error) => {
            error.code == -32601
                || error.message.contains("not supported")
                || error.message.contains("do not support")
        }
        None => false,
    }
}

/// Runs one history request on the Tokio side.
pub(crate) async fn load(backend: Backend, mut request: PageRequest) -> PageResult {
    let mut result = PageResult {
        generation: request.generation,
        ..PageResult::default()
    };
    match request.source.clone() {
        Older::None => {}
        Older::Legacy { include_from } => {
            load_legacy(&backend, &request, include_from, &mut result).await;
        }
        Older::Paged {
            cursor,
            include_from,
        } => {
            if let Some(initial) = request.initial_turns.take() {
                match fetch_turns(
                    &backend,
                    &request.thread_id,
                    initial.cursor.clone(),
                    initial.limit,
                )
                .await
                {
                    Ok(page) => {
                        let next = advancing_cursor(
                            initial.cursor.as_deref(),
                            page.next_cursor,
                            &mut request.cursors.turns_seen,
                        );
                        request.cursors.turns_exhausted = next.is_none();
                        request.cursors.turn_cursor = next;
                        result.turns.extend(page.data);
                    }
                    Err(err) if is_unsupported(&err) => {
                        result.switched_to_legacy = true;
                        load_legacy(&backend, &request, /*include_from*/ None, &mut result).await;
                        return result;
                    }
                    Err(err) => tracing::debug!(%err, "thread/turns/list failed"),
                }
            }
            let thread_id = request.thread_id.clone();
            let fetch = |cursor: Option<String>| {
                backend.request::<ThreadItemsListResponse>(session::items_page(
                    backend.next_request_id(),
                    &thread_id,
                    cursor,
                    session::HISTORY_ITEM_PAGE_LIMIT,
                ))
            };
            let outcome = load_paged_or_rollback(
                fetch,
                &mut request,
                cursor,
                include_from.clone(),
                &mut result,
            )
            .await;
            if let Err(err) = outcome {
                // Threads the server cannot page (older servers, ephemeral
                // threads) are read whole instead.
                if is_unsupported(&err) {
                    result.switched_to_legacy = true;
                    result.items.clear();
                    load_legacy(&backend, &request, include_from, &mut result).await;
                    return result;
                }
                result.error = Some(err.user_message());
                result.older = request.source.clone();
            }
            fill_turn_metadata(&backend, &mut request, &mut result).await;
            result.cursors = request.cursors;
        }
    }
    result
}

/// [`load_paged`] that leaves the cursor loop guard as it was when the
/// request fails: the retry starts from the same cursor and has to be able
/// to follow the cursors this attempt already followed.
async fn load_paged_or_rollback<F, Fut>(
    fetch: F,
    request: &mut PageRequest,
    cursor: Option<String>,
    include_from: Option<String>,
    result: &mut PageResult,
) -> Result<(), BackendError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<ThreadItemsListResponse, BackendError>>,
{
    let items_seen = request.cursors.items_seen.clone();
    let outcome = load_paged(fetch, request, cursor, include_from, result).await;
    if outcome.is_err() {
        request.cursors.items_seen = items_seen;
    }
    outcome
}

/// Pages newest-first from `cursor` with `fetch` until the request's
/// budget is collected (see [`Older::Paged`]).
async fn load_paged<F, Fut>(
    mut fetch: F,
    request: &mut PageRequest,
    mut cursor: Option<String>,
    mut include_from: Option<String>,
    result: &mut PageResult,
) -> Result<(), BackendError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<ThreadItemsListResponse, BackendError>>,
{
    // Newest first while paging; reversed at the end.
    let mut collected: Vec<(String, ThreadItem)> = Vec::new();
    let mut older = Older::None;
    for _ in 0..MAX_PAGES_PER_REQUEST {
        let page = fetch(cursor.clone()).await?;
        if page.data.is_empty() {
            older = Older::None;
            break;
        }
        for entry in page.data {
            if let Some(boundary) = &include_from {
                if entry.item.id() != boundary {
                    continue;
                }
                include_from = None;
            }
            collected.push((entry.turn_id, entry.item));
        }
        cursor = advancing_cursor(
            cursor.as_deref(),
            page.next_cursor,
            &mut request.cursors.items_seen,
        );
        older = match &cursor {
            Some(cursor) => Older::Paged {
                cursor: Some(cursor.clone()),
                include_from: include_from.clone(),
            },
            None => Older::None,
        };
        if cursor.is_none() || collected.len() >= request.budget {
            break;
        }
    }
    collected.reverse();
    result.items = collected;
    result.older = older;
    Ok(())
}

/// Fetches turn metadata until every item's turn is known (bounded).
async fn fill_turn_metadata(backend: &Backend, request: &mut PageRequest, result: &mut PageResult) {
    for _ in 0..MAX_TURN_PAGES_PER_REQUEST {
        let known: HashSet<&str> = request
            .known_turns
            .iter()
            .map(String::as_str)
            .chain(result.turns.iter().map(|turn| turn.id.as_str()))
            .collect();
        let missing = result
            .items
            .iter()
            .filter(|(turn_id, _)| !known.contains(turn_id.as_str()))
            .map(|(turn_id, _)| turn_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        if missing == 0 || request.cursors.turns_exhausted {
            return;
        }
        let cursor = request.cursors.turn_cursor.clone();
        let limit = u32::try_from(missing.clamp(1, 100)).unwrap_or(100);
        match fetch_turns(backend, &request.thread_id, cursor.clone(), limit).await {
            Ok(page) => {
                let next = advancing_cursor(
                    cursor.as_deref(),
                    page.next_cursor,
                    &mut request.cursors.turns_seen,
                );
                request.cursors.turns_exhausted = next.is_none() || page.data.is_empty();
                request.cursors.turn_cursor = next;
                result.turns.extend(page.data);
            }
            Err(err) => {
                tracing::debug!(%err, "thread/turns/list failed");
                return;
            }
        }
    }
}

async fn fetch_turns(
    backend: &Backend,
    thread_id: &str,
    cursor: Option<String>,
    limit: u32,
) -> Result<ThreadTurnsListResponse, BackendError> {
    backend
        .request(session::turns_page(
            backend.next_request_id(),
            thread_id,
            cursor,
            limit,
        ))
        .await
}

async fn read_thread(
    backend: &Backend,
    thread_id: &str,
    include_turns: bool,
) -> Result<ThreadReadResponse, BackendError> {
    backend
        .request(ClientRequest::ThreadRead {
            request_id: backend.next_request_id(),
            params: ThreadReadParams {
                thread_id: thread_id.to_string(),
                include_turns,
            },
        })
        .await
}

async fn load_legacy(
    backend: &Backend,
    request: &PageRequest,
    include_from: Option<String>,
    result: &mut PageResult,
) {
    match read_thread(backend, &request.thread_id, /*include_turns*/ true).await {
        Ok(response) => {
            let slice = legacy_slice(
                response.thread.turns,
                LegacyBoundary {
                    include_from: include_from.as_deref(),
                    turn_id: request.boundary_turn.as_deref(),
                },
                &request.live_turns,
            );
            result.items = slice.items;
            result.older = slice.older;
            result.turns = slice.turns;
            result.snapshot = slice.snapshot;
        }
        Err(err) => {
            result.error = Some(err.user_message());
            result.older = Older::Legacy { include_from };
        }
    }
}

/// Where a legacy chunk ends.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LegacyBoundary<'a> {
    /// Id of the chunk's last item (inclusive); `None` = the newest item.
    pub(crate) include_from: Option<&'a str>,
    /// When the thread has no item `include_from` (it is the id of a live
    /// event), the chunk ends with the last item of this turn instead.
    /// Items of that turn that are still shown are matched to the snapshot
    /// and skipped when the chunk is prepended.
    pub(crate) turn_id: Option<&'a str>,
}

/// A chunk of a legacy thread.
#[derive(Debug, Default)]
pub(crate) struct LegacySlice {
    /// Items of the chunk, chronological.
    pub(crate) items: Vec<(String, ThreadItem)>,
    /// Where the next older chunk starts.
    pub(crate) older: Older,
    /// Every turn, without its items.
    pub(crate) turns: Vec<Turn>,
    /// User, agent and reasoning items of the live turns (whole thread).
    pub(crate) snapshot: Vec<(String, ThreadItem)>,
}

/// Takes up to [`LEGACY_CHUNK`] items ending at `boundary`, plus the
/// snapshot items of `live_turns`.
pub(crate) fn legacy_slice(
    turns: Vec<Turn>,
    boundary: LegacyBoundary<'_>,
    live_turns: &HashSet<String>,
) -> LegacySlice {
    let mut all = Vec::new();
    let mut metadata = Vec::with_capacity(turns.len());
    for mut turn in turns {
        for item in std::mem::take(&mut turn.items) {
            all.push((turn.id.clone(), item));
        }
        metadata.push(turn);
    }
    let snapshot: Vec<(String, ThreadItem)> = all
        .iter()
        .filter(|(turn_id, item)| live_turns.contains(turn_id) && has_synthetic_legacy_id(item))
        .cloned()
        .collect();
    let end = match boundary.include_from {
        None => Some(all.len()),
        Some(id) => all
            .iter()
            .position(|(_, item)| item.id() == id)
            .or_else(|| {
                let turn = boundary.turn_id?;
                all.iter().rposition(|(turn_id, _)| turn_id == turn)
            })
            .map(|position| position + 1),
    };
    let Some(end) = end else {
        tracing::debug!(?boundary, "legacy history boundary not found");
        return LegacySlice {
            turns: metadata,
            snapshot,
            ..LegacySlice::default()
        };
    };
    let start = end.saturating_sub(LEGACY_CHUNK);
    let older = match start {
        0 => Older::None,
        start => Older::Legacy {
            include_from: Some(all[start - 1].1.id().to_string()),
        },
    };
    all.truncate(end);
    let items = all.split_off(start);
    LegacySlice {
        items,
        older,
        turns: metadata,
        snapshot,
    }
}

/// Item kinds a legacy `thread/read` numbers itself (`item-N`) instead of
/// reporting the ids of their live events.
pub(crate) fn has_synthetic_legacy_id(item: &ThreadItem) -> bool {
    matches!(
        item,
        ThreadItem::UserMessage { .. }
            | ThreadItem::AgentMessage { .. }
            | ThreadItem::Reasoning { .. }
    )
}

/// Result of a lag resync.
#[derive(Debug, Default)]
pub(crate) struct ResyncResult {
    pub(crate) thread_id: String,
    pub(crate) items: Vec<(String, ThreadItem)>,
    pub(crate) status: Option<ThreadStatus>,
    pub(crate) error: Option<String>,
    /// The thread was read whole (legacy); `snapshot` is set.
    pub(crate) legacy: bool,
    /// See [`PageResult::snapshot`].
    pub(crate) snapshot: Vec<(String, ThreadItem)>,
}

/// Re-reads thread status and the newest items after dropped events.
/// `mode` is `None` for threads started in the tab: they are paged when
/// the server can page them, and read whole otherwise.
pub(crate) async fn resync(
    backend: Backend,
    thread_id: String,
    mode: Option<ThreadHistoryMode>,
    live_turns: HashSet<String>,
) -> ResyncResult {
    let mut result = ResyncResult {
        thread_id: thread_id.clone(),
        ..ResyncResult::default()
    };
    let legacy = mode == Some(ThreadHistoryMode::Legacy);
    let response = match read_thread(&backend, &thread_id, /*include_turns*/ legacy).await {
        Ok(response) => response,
        Err(err) => {
            result.error = Some(err.user_message());
            return result;
        }
    };
    result.status = Some(response.thread.status.clone());
    let turns = if legacy {
        response.thread.turns
    } else {
        let page: Result<ThreadItemsListResponse, BackendError> = backend
            .request(session::items_page(
                backend.next_request_id(),
                &thread_id,
                /*cursor*/ None,
                RESYNC_ITEMS,
            ))
            .await;
        match page {
            Ok(page) => {
                result.items = page
                    .data
                    .into_iter()
                    .rev()
                    .map(|entry| (entry.turn_id, entry.item))
                    .collect();
                return result;
            }
            Err(err) if mode.is_none() && is_unsupported(&err) => {
                match read_thread(&backend, &thread_id, /*include_turns*/ true).await {
                    Ok(response) => response.thread.turns,
                    Err(err) => {
                        result.error = Some(err.user_message());
                        return result;
                    }
                }
            }
            Err(err) => {
                result.error = Some(err.user_message());
                return result;
            }
        }
    };
    let slice = legacy_slice(turns, LegacyBoundary::default(), &live_turns);
    let keep = slice.items.len().saturating_sub(RESYNC_ITEMS as usize);
    result.items = slice.items.into_iter().skip(keep).collect();
    result.snapshot = slice.snapshot;
    result.legacy = true;
    result
}

/// Every item of the thread, chronological (for a complete export).
pub(crate) async fn load_all(
    backend: Backend,
    thread_id: String,
    mode: Option<ThreadHistoryMode>,
) -> Result<Vec<(String, ThreadItem)>, String> {
    let legacy = |turns: Vec<Turn>| -> Vec<(String, ThreadItem)> {
        turns
            .into_iter()
            .flat_map(|turn| {
                let id = turn.id;
                turn.items.into_iter().map(move |item| (id.clone(), item))
            })
            .collect()
    };
    if mode == Some(ThreadHistoryMode::Legacy) {
        return read_thread(&backend, &thread_id, /*include_turns*/ true)
            .await
            .map(|response| legacy(response.thread.turns))
            .map_err(|err| err.user_message());
    }
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let page: Result<ThreadItemsListResponse, BackendError> = backend
            .request(session::items_page(
                backend.next_request_id(),
                &thread_id,
                cursor.clone(),
                session::HISTORY_ITEM_PAGE_LIMIT,
            ))
            .await;
        let page = match page {
            Ok(page) => page,
            Err(err) if is_unsupported(&err) && items.is_empty() => {
                return read_thread(&backend, &thread_id, /*include_turns*/ true)
                    .await
                    .map(|response| legacy(response.thread.turns))
                    .map_err(|err| err.user_message());
            }
            Err(err) => return Err(err.user_message()),
        };
        if page.data.is_empty() {
            break;
        }
        items.extend(
            page.data
                .into_iter()
                .map(|entry| (entry.turn_id, entry.item)),
        );
        cursor = advancing_cursor(cursor.as_deref(), page.next_cursor, &mut seen);
        if cursor.is_none() {
            break;
        }
    }
    items.reverse();
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::TurnItemsView;
    use codex_app_server_protocol::TurnStatus;
    use pretty_assertions::assert_eq;

    fn message(id: &str) -> ThreadItem {
        ThreadItem::AgentMessage {
            id: id.to_string(),
            text: id.to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        }
    }

    fn turn(id: &str, items: &[&str]) -> Turn {
        Turn {
            id: id.to_string(),
            items: items.iter().map(|id| message(id)).collect(),
            items_view: TurnItemsView::Full,
            status: TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    fn ids(items: &[(String, ThreadItem)]) -> Vec<String> {
        items
            .iter()
            .map(|(_, item)| item.id().to_string())
            .collect()
    }

    #[test]
    fn cursor_loops_are_cut() {
        let mut seen = HashSet::new();
        assert_eq!(
            advancing_cursor(None, Some("a".to_string()), &mut seen),
            Some("a".to_string())
        );
        assert_eq!(
            advancing_cursor(Some("a"), Some("b".to_string()), &mut seen),
            Some("b".to_string())
        );
        assert_eq!(
            advancing_cursor(Some("b"), Some("a".to_string()), &mut seen),
            None
        );
        assert_eq!(advancing_cursor(Some("b"), None, &mut seen), None);
    }

    fn ending_at<'a>(include_from: &'a str, turn_id: Option<&'a str>) -> LegacyBoundary<'a> {
        LegacyBoundary {
            include_from: Some(include_from),
            turn_id,
        }
    }

    #[test]
    fn legacy_slices_newest_chunk_and_resumes_inclusively() {
        let names: Vec<String> = (0..450).map(|index| format!("i{index}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let turns = vec![turn("t0", &refs[..300]), turn("t1", &refs[300..])];
        let none = HashSet::new();
        let slice = legacy_slice(turns.clone(), LegacyBoundary::default(), &none);
        assert_eq!(slice.items.len(), LEGACY_CHUNK);
        assert_eq!(
            slice.items.first().map(|(turn, _)| turn.as_str()),
            Some("t0")
        );
        assert_eq!(ids(&slice.items).last().map(String::as_str), Some("i449"));
        assert_eq!(
            slice.older,
            Older::Legacy {
                include_from: Some("i249".to_string())
            }
        );
        assert!(slice.turns.iter().all(|turn| turn.items.is_empty()));
        assert!(slice.snapshot.is_empty());

        let slice = legacy_slice(turns.clone(), ending_at("i249", None), &none);
        assert_eq!(ids(&slice.items).first().map(String::as_str), Some("i50"));
        assert_eq!(ids(&slice.items).last().map(String::as_str), Some("i249"));
        assert_eq!(
            slice.older,
            Older::Legacy {
                include_from: Some("i49".to_string())
            }
        );

        let slice = legacy_slice(turns.clone(), ending_at("i49", None), &none);
        assert_eq!(slice.items.len(), 50);
        assert_eq!(slice.older, Older::None);

        let slice = legacy_slice(turns, ending_at("missing", None), &none);
        assert!(slice.items.is_empty());
        assert_eq!(slice.older, Older::None);
    }

    #[test]
    fn legacy_slice_falls_back_to_the_boundary_turn() {
        // The newest trimmed item came from a live event: its id is not in
        // the thread, so the chunk ends with the last item of its turn.
        let turns = vec![
            turn("t0", &["item-0", "item-1"]),
            turn("t1", &["item-2", "item-3"]),
            turn("t2", &["item-4"]),
        ];
        let live: HashSet<String> = ["t1".to_string()].into();
        let slice = legacy_slice(turns, ending_at("live-uuid", Some("t1")), &live);
        assert_eq!(
            ids(&slice.items),
            vec!["item-0", "item-1", "item-2", "item-3"]
        );
        assert_eq!(slice.older, Older::None);
        // Only the live turn's items are returned for matching.
        assert_eq!(ids(&slice.snapshot), vec!["item-2", "item-3"]);
    }

    #[test]
    fn trimming_switches_to_refetch() {
        let mut state = HistoryState::default();
        state.reset("t".to_string(), ThreadHistoryMode::Paginated);
        let generation = state.generation;
        state.after_trim(Some(TrimBoundary {
            id: "item-9".to_string(),
            turn_id: Some("turn-2".to_string()),
        }));
        assert_eq!(state.generation, generation + 1);
        assert_eq!(
            state.older,
            Older::Paged {
                cursor: None,
                include_from: Some("item-9".to_string())
            }
        );
        assert_eq!(state.boundary_turn.as_deref(), Some("turn-2"));
        state.after_trim(None);
        assert!(state.has_older());
    }

    #[test]
    fn threads_started_in_the_tab_attach_their_history() {
        let mut state = HistoryState::default();
        // No history was loaded: the tab's thread is adopted.
        assert!(state.attach("t1"));
        assert_eq!(state.thread_id.as_deref(), Some("t1"));
        assert!(state.attach("t1"));
        // Results for another thread are not applied.
        assert!(!state.attach("t2"));
    }

    #[test]
    fn trimming_during_the_initial_load_cancels_it() {
        let mut state = HistoryState::default();
        state.reset("t".to_string(), ThreadHistoryMode::Paginated);
        state.loading_initial = true;
        state.error = Some("boom".to_string());
        state.after_trim(Some(TrimBoundary {
            id: "a".to_string(),
            turn_id: None,
        }));
        // The initial page is dropped (new generation), so nothing would
        // ever clear the flag and "load older" would stay blocked.
        assert!(!state.loading_initial);
        assert!(!state.loading_older);
        assert!(state.can_auto_load());
        assert!(state.has_older());
    }

    fn page(items: &[&str], next: Option<&str>) -> ThreadItemsListResponse {
        ThreadItemsListResponse {
            data: items
                .iter()
                .map(|id| codex_app_server_protocol::ThreadItemEntry {
                    turn_id: "t".to_string(),
                    item: message(id),
                    started_at_ms: None,
                    completed_at_ms: None,
                })
                .collect(),
            next_cursor: next.map(str::to_string),
            backwards_cursor: None,
        }
    }

    #[test]
    fn failed_requests_do_not_mark_cursors_used() {
        let mut state = HistoryState::default();
        state.reset("t".to_string(), ThreadHistoryMode::Paginated);
        state.after_trim(Some(TrimBoundary {
            id: "a1".to_string(),
            turn_id: None,
        }));
        let mut request = PageRequest::older(&state, HashSet::new());
        // Refetching after a trim skips the newest page (still shown) and
        // fails on the next one.
        let fetch = |cursor: Option<String>| {
            let page = match cursor.as_deref() {
                None => Ok(page(&["b2", "b1"], Some("c1"))),
                Some("c1") => Err(BackendError::Unavailable),
                Some(_) => Ok(page(&[], None)),
            };
            std::future::ready(page)
        };
        let mut result = PageResult::default();
        let outcome = futures::executor::block_on(load_paged_or_rollback(
            fetch,
            &mut request,
            /*cursor*/ None,
            Some("a1".to_string()),
            &mut result,
        ));
        assert!(outcome.is_err());
        // The retry starts from the newest end again and can follow `c1`.
        let mut retry = PageRequest::older(&state, HashSet::new());
        retry.cursors = request.cursors;
        let fetch = |cursor: Option<String>| {
            std::future::ready(Ok(match cursor.as_deref() {
                None => page(&["b2", "b1"], Some("c1")),
                Some("c1") => page(&["a2", "a1", "a0"], Some("c2")),
                Some(_) => page(&[], None),
            }))
        };
        let mut result = PageResult::default();
        let outcome = futures::executor::block_on(load_paged_or_rollback(
            fetch,
            &mut retry,
            /*cursor*/ None,
            Some("a1".to_string()),
            &mut result,
        ));
        assert!(outcome.is_ok());
        assert_eq!(ids(&result.items), vec!["a0", "a1"]);
        assert_eq!(result.older, Older::None);
    }
}
