//! Sidebar thread list.
//!
//! Pages of `thread/list` (most recently updated first) are grouped by
//! folder and flattened into one virtualized list of folder and thread rows.
//! Search is server-side (`search_term`, debounced), and an archived view
//! lists archived threads. Rows of threads that are open in tabs carry a
//! status dot; [`AppController::sidebar_on_tabs_changed`] updates those marks
//! in place without refetching.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadDeleteParams;
use codex_app_server_protocol::ThreadDeleteResponse;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadSetNameParams;
use codex_app_server_protocol::ThreadSetNameResponse;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use codex_app_server_protocol::ThreadUnarchiveParams;
use codex_app_server_protocol::ThreadUnarchiveResponse;
use slint::ComponentHandle;
use slint::Model;
use slint::ModelRc;
use slint::VecModel;

use crate::app::AppController;
use crate::app::DialogRequest;
use crate::app::ThreadPhase;
use crate::backend::BackendError;
use crate::ui::SidebarRow;
use crate::ui::SidebarRowKind;
use crate::ui::SidebarState;
use crate::ui::SidebarThreadStatus;

/// Threads per `thread/list` page.
const PAGE_SIZE: u32 = 50;
/// Largest page the server accepts; caps how much a refresh reloads at once.
const MAX_PAGE_SIZE: u32 = 100;
/// Coalesces bursts of list-changing notifications into one reload.
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(500);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// How often relative timestamps ("5m ago") are re-rendered.
const CLOCK_TICK: Duration = Duration::from_secs(30);
const UNTITLED_THREAD: &str = "(no message yet)";
const TITLE_MAX_CHARS: usize = 80;

/// One thread as listed in the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ThreadSummary {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) cwd: PathBuf,
    /// Unix seconds.
    pub(crate) updated_at: i64,
}

impl ThreadSummary {
    fn from_thread(thread: &Thread) -> Self {
        Self {
            id: thread.id.clone(),
            title: thread_title(thread.name.as_deref(), &thread.preview),
            cwd: thread.cwd.as_path().to_path_buf(),
            updated_at: thread.updated_at,
        }
    }
}

/// Display title: the thread name, else the first line of the preview.
pub(crate) fn thread_title(name: Option<&str>, preview: &str) -> String {
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        return crate::app::truncate_chars(name, TITLE_MAX_CHARS);
    }
    crate::xtab::tools::preview_text(preview)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| crate::app::truncate_chars(line, TITLE_MAX_CHARS))
        .unwrap_or_else(|| UNTITLED_THREAD.to_string())
}

/// Threads of one folder, newest first.
#[derive(Debug, PartialEq)]
pub(crate) struct FolderGroup<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) threads: Vec<&'a ThreadSummary>,
}

/// Groups threads by folder. Folders are ordered by their most recently
/// updated thread and threads within a folder newest first; ties keep the
/// list order.
pub(crate) fn group_by_folder(threads: &[ThreadSummary]) -> Vec<FolderGroup<'_>> {
    let mut sorted: Vec<&ThreadSummary> = threads.iter().collect();
    sorted.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
    let mut groups: Vec<FolderGroup<'_>> = Vec::new();
    let mut group_index: HashMap<&Path, usize> = HashMap::new();
    for thread in sorted {
        match group_index.get(thread.cwd.as_path()) {
            Some(&index) => groups[index].threads.push(thread),
            None => {
                group_index.insert(thread.cwd.as_path(), groups.len());
                groups.push(FolderGroup {
                    cwd: thread.cwd.as_path(),
                    threads: vec![thread],
                });
            }
        }
    }
    groups
}

/// One sidebar row before relative times and open-tab marks are applied.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RowSpec {
    Folder {
        path: PathBuf,
        count: usize,
        collapsed: bool,
    },
    Thread {
        id: String,
        title: String,
        cwd: PathBuf,
        updated_at: i64,
    },
    /// "Load more" footer, present while the server has more pages.
    More,
}

/// Flattens grouped threads into rows, hiding threads of collapsed folders.
pub(crate) fn build_rows(
    threads: &[ThreadSummary],
    collapsed: &HashSet<PathBuf>,
    has_more: bool,
) -> Vec<RowSpec> {
    let mut rows = Vec::new();
    for group in group_by_folder(threads) {
        let is_collapsed = collapsed.contains(group.cwd);
        rows.push(RowSpec::Folder {
            path: group.cwd.to_path_buf(),
            count: group.threads.len(),
            collapsed: is_collapsed,
        });
        if is_collapsed {
            continue;
        }
        rows.extend(group.threads.into_iter().map(|thread| RowSpec::Thread {
            id: thread.id.clone(),
            title: thread.title.clone(),
            cwd: thread.cwd.clone(),
            updated_at: thread.updated_at,
        }));
    }
    if has_more {
        rows.push(RowSpec::More);
    }
    rows
}

/// Appends `page` to `threads`, skipping ids already present (a thread that
/// was updated between page requests can appear on two pages).
pub(crate) fn merge_page(threads: &mut Vec<ThreadSummary>, page: Vec<ThreadSummary>) {
    let mut seen: HashSet<String> = threads.iter().map(|thread| thread.id.clone()).collect();
    threads.extend(
        page.into_iter()
            .filter(|thread| seen.insert(thread.id.clone())),
    );
}

/// The list after reloading its first page while more may be shown.
///
/// `thread/list` cursors are keysets (update time and id), so the threads
/// older than the refreshed page, and the cursor after them, stay valid:
/// the page replaces the head and the loaded tail stays, so a refresh never
/// drops rows the user scrolled to. Threads that moved into the page are
/// not repeated.
pub(crate) fn merge_refreshed_head(
    old: Vec<ThreadSummary>,
    old_cursor: Option<String>,
    page: Vec<ThreadSummary>,
    page_cursor: Option<String>,
) -> (Vec<ThreadSummary>, Option<String>) {
    // The page reached the end: it is the whole list.
    let Some(page_cursor) = page_cursor else {
        return (page, None);
    };
    let Some(oldest_in_page) = page.last().map(|thread| thread.updated_at) else {
        return (page, Some(page_cursor));
    };
    let in_page: HashSet<&str> = page.iter().map(|thread| thread.id.as_str()).collect();
    let tail: Vec<ThreadSummary> = old
        .into_iter()
        .filter(|thread| {
            thread.updated_at <= oldest_in_page && !in_page.contains(thread.id.as_str())
        })
        .collect();
    if tail.is_empty() {
        return (page, Some(page_cursor));
    }
    let mut threads = page;
    threads.extend(tail);
    (threads, old_cursor)
}

/// Whether a listing reads only the state DB (fast). An unfiltered first
/// page always tries it first and falls back to a rollout scan when it is
/// empty. Filtered listings (search, archive) never fall back, since an
/// empty result is their normal answer; they scan rollouts directly only
/// once the DB is known to be unpopulated.
fn use_state_db_only(unfiltered: bool, db_populated: Option<bool>) -> bool {
    unfiltered || db_populated != Some(false)
}

/// Whether an empty first page from the state DB is retried as a rollout
/// scan (the DB may not be populated yet, like the TUI's picker).
fn falls_back_to_rollouts(
    first_page: bool,
    state_db_only: bool,
    unfiltered: bool,
    empty: bool,
) -> bool {
    first_page && state_db_only && unfiltered && empty
}

/// Short relative time such as "now", "5m", "3h", "2d", "4w", "5mo", "2y".
pub(crate) fn relative_time(now: i64, then: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    let elapsed = now.saturating_sub(then).max(0);
    match elapsed {
        e if e < MINUTE => "now".to_string(),
        e if e < HOUR => format!("{}m", e / MINUTE),
        e if e < DAY => format!("{}h", e / HOUR),
        e if e < WEEK => format!("{}d", e / DAY),
        e if e < MONTH => format!("{}w", e / WEEK),
        e if e < YEAR => format!("{}mo", (e / MONTH).max(1)),
        e => format!("{}y", e / YEAR),
    }
}

/// Current Unix time in seconds.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Replaces the contents of `model` with `rows`, touching only rows that
/// changed so list views keep their scroll position and row instances.
pub(crate) fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let old_len = model.row_count();
    // Large shrinks are cheaper as one reset than as many single removals.
    if rows.len() + 32 < old_len {
        model.set_vec(rows);
        return;
    }
    for (index, row) in rows.iter().enumerate().take(old_len) {
        if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    if rows.len() > old_len {
        model.extend(rows.into_iter().skip(old_len));
    } else {
        for index in (rows.len()..old_len).rev() {
            model.remove(index);
        }
    }
}

/// Open-tab status of a listed thread.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ThreadMark {
    status: SidebarThreadStatus,
    selected: bool,
}

impl Default for ThreadMark {
    fn default() -> Self {
        Self {
            status: SidebarThreadStatus::NotOpen,
            selected: false,
        }
    }
}

fn status_for_phase(phase: ThreadPhase) -> SidebarThreadStatus {
    match phase {
        ThreadPhase::Starting => SidebarThreadStatus::Starting,
        ThreadPhase::Idle | ThreadPhase::Closed => SidebarThreadStatus::Idle,
        ThreadPhase::Running => SidebarThreadStatus::Running,
        ThreadPhase::WaitingOnUser => SidebarThreadStatus::Waiting,
        ThreadPhase::Error => SidebarThreadStatus::Error,
    }
}

/// Sidebar state shared by all tabs.
#[derive(Default)]
pub(crate) struct SidebarController {
    /// Loaded threads in server order (all pages).
    threads: Vec<ThreadSummary>,
    /// Last unfiltered list of active threads; kept while a search or the
    /// archived view is shown.
    recent: Vec<ThreadSummary>,
    next_cursor: Option<String>,
    /// A first-page (re)load is in flight.
    loading: bool,
    /// A next-page load is in flight.
    loading_more: bool,
    /// Bumped by every reload; responses of older generations are dropped.
    generation: u64,
    /// Applied search term (trimmed).
    search_term: String,
    /// Search box text waiting for the debounce timer.
    pending_search: String,
    archived: bool,
    /// Read the list from the state DB only (see [`use_state_db_only`]).
    state_db_only: bool,
    /// Whether the state DB lists threads: learned from unfiltered first
    /// pages (`Some(false)`: empty while the rollout scan found threads).
    state_db_populated: Option<bool>,
    /// Ephemeral and sub-agent threads seen starting; they are not listed,
    /// so their turns do not change the list.
    unlisted: HashSet<String>,
    error: Option<String>,
    collapsed: HashSet<PathBuf>,
    rows: Rc<VecModel<SidebarRow>>,
    /// Row index of each thread id currently in `rows`.
    row_index: HashMap<String, usize>,
    /// Marks last applied to rows, by thread id.
    marks: RefCell<HashMap<String, ThreadMark>>,
    refresh_timer: slint::Timer,
    search_timer: slint::Timer,
    clock_timer: slint::Timer,
}

impl SidebarController {
    /// Recently updated active threads, ignoring the current search and
    /// archived view (feeds the new-tab page).
    pub(crate) fn recent_threads(&self) -> &[ThreadSummary] {
        &self.recent
    }

    fn is_unfiltered(&self) -> bool {
        self.search_term.is_empty() && !self.archived
    }
}

impl AppController {
    pub(crate) fn sidebar_bind(&mut self) {
        let state = self.window.global::<SidebarState>();
        state.set_rows(ModelRc::from(self.sidebar.rows.clone()));
        state.on_search_edited(|text| {
            let text = text.to_string();
            crate::ui_thread::with_app(move |app| app.sidebar_search_edited(text));
        });
        state.on_toggle_archived(|| {
            crate::ui_thread::with_app(|app| {
                app.sidebar.archived = !app.sidebar.archived;
                app.sidebar.threads.clear();
                app.sidebar.next_cursor = None;
                app.sidebar_render();
                app.sidebar_reload();
            });
        });
        state.on_refresh(|| crate::ui_thread::with_app(AppController::sidebar_reload));
        state.on_load_more(|| {
            // Deferred: this can fire from inside a scroll update of the list.
            slint::Timer::single_shot(Duration::ZERO, || {
                crate::ui_thread::with_app(AppController::sidebar_load_more);
            });
        });
        state.on_open_thread(|id, folder| {
            let (id, folder) = (id.to_string(), PathBuf::from(folder.as_str()));
            crate::ui_thread::with_app(move |app| app.open_thread(id, Some(folder)));
        });
        state.on_thread_action(|id, action| {
            let (id, action) = (id.to_string(), action.to_string());
            crate::ui_thread::with_app(move |app| app.sidebar_thread_action(&id, &action));
        });
        state.on_folder_action(|path, action| {
            let (path, action) = (PathBuf::from(path.as_str()), action.to_string());
            crate::ui_thread::with_app(move |app| app.sidebar_folder_action(path, &action));
        });
        self.sidebar
            .clock_timer
            .start(slint::TimerMode::Repeated, CLOCK_TICK, || {
                crate::ui_thread::with_app(AppController::sidebar_render);
            });
        self.sidebar_render();
    }

    pub(crate) fn sidebar_on_server_ready(&mut self) {
        self.sidebar_reload();
    }

    /// Re-fetches the thread list (debounced).
    pub(crate) fn sidebar_refresh(&mut self) {
        self.sidebar
            .refresh_timer
            .start(slint::TimerMode::SingleShot, REFRESH_DEBOUNCE, || {
                crate::ui_thread::with_app(AppController::sidebar_reload)
            });
    }

    pub(crate) fn sidebar_on_notification(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::ThreadStarted(started) => {
                let thread = &started.thread;
                if !thread.ephemeral && thread.parent_thread_id.is_none() {
                    self.sidebar_refresh();
                } else {
                    self.sidebar.unlisted.insert(thread.id.clone());
                }
            }
            ServerNotification::ThreadClosed(closed) => {
                self.sidebar.unlisted.remove(&closed.thread_id);
            }
            ServerNotification::ThreadArchived(archived) => {
                self.sidebar
                    .recent
                    .retain(|thread| thread.id != archived.thread_id);
                if !self.sidebar.archived {
                    self.sidebar_remove_thread(&archived.thread_id);
                }
                self.sidebar_refresh();
            }
            ServerNotification::ThreadUnarchived(unarchived) => {
                if self.sidebar.archived {
                    self.sidebar_remove_thread(&unarchived.thread_id);
                }
                self.sidebar_refresh();
            }
            ServerNotification::ThreadDeleted(deleted) => {
                self.sidebar_remove_thread(&deleted.thread_id);
            }
            ServerNotification::ThreadNameUpdated(updated) => {
                match updated
                    .thread_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                {
                    Some(name) => self.sidebar_set_title(&updated.thread_id, name),
                    // The fallback title needs the preview: refetch.
                    None => self.sidebar_refresh(),
                }
            }
            // A finished turn reorders the list (or adds a thread's first
            // entry), except in threads that are never listed (recaps,
            // side chats, sub-agents).
            ServerNotification::TurnCompleted(completed)
                if !self.sidebar.unlisted.contains(&completed.thread_id) =>
            {
                self.sidebar_refresh();
            }
            _ => {}
        }
    }

    /// Called whenever the open tabs change (to mark open threads).
    ///
    /// Runs on every tab-strip refresh, so it only rewrites rows whose mark
    /// actually changed.
    pub(crate) fn sidebar_on_tabs_changed(&self) {
        let marks = self.sidebar_marks();
        let mut previous = self.sidebar.marks.borrow_mut();
        if *previous == marks {
            return;
        }
        let changed: HashSet<&String> = marks
            .iter()
            .filter(|(id, mark)| previous.get(*id) != Some(*mark))
            .map(|(id, _)| id)
            .chain(previous.keys().filter(|id| !marks.contains_key(*id)))
            .collect();
        for id in changed {
            let mark = marks.get(id).copied().unwrap_or_default();
            if let Some(&index) = self.sidebar.row_index.get(id)
                && let Some(mut row) = self.sidebar.rows.row_data(index)
                && (row.status != mark.status || row.selected != mark.selected)
            {
                row.status = mark.status;
                row.selected = mark.selected;
                self.sidebar.rows.set_row_data(index, row);
            }
        }
        *previous = marks;
    }

    /// Shows the sidebar and focuses its search box.
    pub(crate) fn sidebar_focus_search(&mut self) {
        // Docked, or as a drawer when the window is too narrow.
        self.show_sidebar();
        self.window
            .global::<SidebarState>()
            .set_focus_search_requested(true);
    }

    /// Reloads the first page with the current search and archived filter.
    fn sidebar_reload(&mut self) {
        self.sidebar.refresh_timer.stop();
        self.sidebar.generation += 1;
        self.sidebar.loading = true;
        self.sidebar.loading_more = false;
        self.sidebar.state_db_only = use_state_db_only(
            self.sidebar.is_unfiltered(),
            self.sidebar.state_db_populated,
        );
        // Reload up to one full page of what is shown; rows beyond it stay
        // (see `merge_refreshed_head`), so a refresh does not shrink the
        // list under the user.
        let loaded = u32::try_from(self.sidebar.threads.len()).unwrap_or(u32::MAX);
        let limit = loaded.clamp(PAGE_SIZE, MAX_PAGE_SIZE);
        self.sidebar_fetch(/*cursor*/ None, limit);
        self.sidebar_render_state();
    }

    fn sidebar_load_more(&mut self) {
        if self.sidebar.loading || self.sidebar.loading_more {
            return;
        }
        let Some(cursor) = self.sidebar.next_cursor.clone() else {
            return;
        };
        self.sidebar.loading_more = true;
        self.sidebar_fetch(Some(cursor), PAGE_SIZE);
        self.sidebar_render_state();
    }

    fn sidebar_fetch(&mut self, cursor: Option<String>, limit: u32) {
        let generation = self.sidebar.generation;
        let first_page = cursor.is_none();
        let search_term =
            (!self.sidebar.search_term.is_empty()).then(|| self.sidebar.search_term.clone());
        let params = list_params(
            cursor,
            limit,
            search_term,
            self.sidebar.archived,
            self.sidebar.state_db_only,
        );
        self.backend.call(
            |request_id| ClientRequest::ThreadList { request_id, params },
            move |app, result: Result<ThreadListResponse, BackendError>| {
                if app.sidebar.generation != generation {
                    return;
                }
                app.sidebar_on_page(first_page, limit, result);
            },
        );
    }

    fn sidebar_on_page(
        &mut self,
        first_page: bool,
        limit: u32,
        result: Result<ThreadListResponse, BackendError>,
    ) {
        let unfiltered = self.sidebar.is_unfiltered();
        let empty = result
            .as_ref()
            .is_ok_and(|response| response.data.is_empty());
        if falls_back_to_rollouts(first_page, self.sidebar.state_db_only, unfiltered, empty) {
            // An empty state DB may just not be populated yet: scan rollouts.
            self.sidebar.state_db_only = false;
            self.sidebar_fetch(/*cursor*/ None, limit);
            return;
        }
        if first_page && unfiltered && !empty && result.is_ok() {
            // Threads found in the DB, or only by the scan behind it.
            self.sidebar.state_db_populated = Some(self.sidebar.state_db_only);
        }
        if first_page {
            self.sidebar.loading = false;
        } else {
            self.sidebar.loading_more = false;
        }
        match result {
            Ok(response) => {
                let page: Vec<ThreadSummary> = response
                    .data
                    .iter()
                    .filter(|thread| !thread.ephemeral)
                    .map(ThreadSummary::from_thread)
                    .collect();
                if first_page {
                    let old = std::mem::take(&mut self.sidebar.threads);
                    let old_cursor = self.sidebar.next_cursor.take();
                    let (threads, cursor) =
                        merge_refreshed_head(old, old_cursor, page, response.next_cursor);
                    self.sidebar.threads = threads;
                    self.sidebar.next_cursor = cursor;
                } else {
                    merge_page(&mut self.sidebar.threads, page);
                    self.sidebar.next_cursor = response.next_cursor;
                }
                if unfiltered {
                    self.sidebar.recent.clone_from(&self.sidebar.threads);
                }
                self.sidebar.error = None;
            }
            Err(err) => {
                let message = err.user_message();
                tracing::warn!(%message, "thread/list failed");
                if first_page || self.sidebar.threads.is_empty() {
                    self.sidebar.error = Some(format!("Could not load threads: {message}"));
                } else {
                    self.toast(format!("Could not load more threads: {message}"));
                }
            }
        }
        self.sidebar_render();
        self.newtab_on_threads_changed();
    }

    fn sidebar_search_edited(&mut self, text: String) {
        self.sidebar.pending_search = text;
        self.sidebar
            .search_timer
            .start(slint::TimerMode::SingleShot, SEARCH_DEBOUNCE, || {
                crate::ui_thread::with_app(AppController::sidebar_apply_search)
            });
    }

    fn sidebar_apply_search(&mut self) {
        let term = self.sidebar.pending_search.trim().to_string();
        if term == self.sidebar.search_term {
            return;
        }
        self.sidebar.search_term = term;
        // A different query starts from one page again.
        self.sidebar.threads.clear();
        self.sidebar.next_cursor = None;
        self.sidebar_reload();
    }

    /// Rebuilds the rows from the loaded threads.
    fn sidebar_render(&mut self) {
        let now = unix_now();
        let has_more = self.sidebar.next_cursor.is_some();
        let specs = build_rows(&self.sidebar.threads, &self.sidebar.collapsed, has_more);
        let marks = self.sidebar_marks();
        let mut row_index = HashMap::new();
        let rows: Vec<SidebarRow> = specs
            .into_iter()
            .enumerate()
            .map(|(index, spec)| {
                if let RowSpec::Thread { id, .. } = &spec {
                    row_index.insert(id.clone(), index);
                }
                to_slint_row(spec, now, &marks)
            })
            .collect();
        sync_model(&self.sidebar.rows, rows);
        self.sidebar.row_index = row_index;
        *self.sidebar.marks.borrow_mut() = marks;
        self.sidebar_render_state();
    }

    fn sidebar_render_state(&self) {
        let state = self.window.global::<SidebarState>();
        state.set_loading(self.sidebar.loading);
        state.set_loading_more(self.sidebar.loading_more);
        state.set_has_more(self.sidebar.next_cursor.is_some());
        state.set_archived(self.sidebar.archived);
        state.set_error(self.sidebar.error.clone().unwrap_or_default().into());
        let empty = match (self.sidebar.search_term.is_empty(), self.sidebar.archived) {
            (false, _) => "No threads match your search",
            (true, true) => "No archived threads",
            (true, false) => "No threads yet. Start one from the new tab page.",
        };
        state.set_empty_text(empty.into());
    }

    fn sidebar_marks(&self) -> HashMap<String, ThreadMark> {
        let active_thread = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
            .and_then(|thread| thread.thread_id.as_deref());
        self.tabs
            .iter()
            .filter_map(crate::app::Tab::thread)
            .filter_map(|thread| {
                let id = thread.thread_id.as_deref()?;
                Some((
                    id.to_string(),
                    ThreadMark {
                        status: status_for_phase(thread.phase),
                        selected: active_thread == Some(id),
                    },
                ))
            })
            .collect()
    }

    fn sidebar_remove_thread(&mut self, thread_id: &str) {
        self.sidebar.recent.retain(|thread| thread.id != thread_id);
        let before = self.sidebar.threads.len();
        self.sidebar.threads.retain(|thread| thread.id != thread_id);
        if self.sidebar.threads.len() != before {
            self.sidebar_render();
            self.newtab_on_threads_changed();
        }
    }

    fn sidebar_set_title(&mut self, thread_id: &str, name: &str) {
        let title = thread_title(Some(name), "");
        if let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            && thread.title != title
        {
            thread.title = title;
            self.sidebar_render();
        }
    }

    fn sidebar_thread_action(&mut self, thread_id: &str, action: &str) {
        let Some(thread) = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .cloned()
        else {
            return;
        };
        match action {
            "open" => self.open_thread(thread.id, Some(thread.cwd)),
            "new-thread" => self.start_thread_checked(thread.cwd),
            "rename" => self.sidebar_rename(thread),
            "archive" => self.sidebar_set_archived(thread, /*archive*/ true),
            "unarchive" => self.sidebar_set_archived(thread, /*archive*/ false),
            "delete" => self.sidebar_delete(thread),
            "copy-id" => self.copy_to_clipboard(&thread.id),
            other => tracing::debug!(action = other, "unknown sidebar thread action"),
        }
    }

    fn sidebar_folder_action(&mut self, path: PathBuf, action: &str) {
        match action {
            "toggle" => {
                if !self.sidebar.collapsed.remove(&path) {
                    self.sidebar.collapsed.insert(path);
                }
                self.sidebar_render();
            }
            "new-thread" => self.start_thread_checked(path),
            "open-folder" => self.open_in_file_manager(&path),
            "copy-path" => self.copy_to_clipboard(&path.to_string_lossy()),
            other => tracing::debug!(action = other, "unknown sidebar folder action"),
        }
    }

    fn sidebar_rename(&mut self, thread: ThreadSummary) {
        let current = if thread.title == UNTITLED_THREAD {
            String::new()
        } else {
            thread.title.clone()
        };
        self.show_dialog(
            DialogRequest::prompt("Rename thread", current).accept_label("Rename"),
            Box::new(move |app, value| {
                let Some(name) = value.map(|value| value.trim().to_string()) else {
                    return;
                };
                if name.is_empty() {
                    return;
                }
                let thread_id = thread.id;
                let params = ThreadSetNameParams {
                    thread_id: thread_id.clone(),
                    name: name.clone(),
                };
                app.backend.call(
                    |request_id| ClientRequest::ThreadSetName { request_id, params },
                    move |app, result: Result<ThreadSetNameResponse, BackendError>| match result {
                        Ok(_) => {
                            app.sidebar_set_title(&thread_id, &name);
                            if let Some(index) = app.tab_index_for_thread(&thread_id)
                                && let Some(tab) = app.thread_tab_mut(index)
                            {
                                tab.name = Some(name);
                                app.refresh_tabs();
                            }
                        }
                        Err(err) => app.toast(format!("Could not rename: {}", err.user_message())),
                    },
                );
            }),
        );
    }

    fn sidebar_set_archived(&mut self, thread: ThreadSummary, archive: bool) {
        let thread_id = thread.id;
        let id = thread_id.clone();
        let title = thread.title;
        let on_done = move |app: &mut AppController, result: Result<(), BackendError>| match result
        {
            Ok(()) => {
                app.sidebar_remove_thread(&thread_id);
                if archive {
                    // Same as archiving from the tab menu: the tab goes away.
                    if let Some(index) = app.tab_index_for_thread(&thread_id) {
                        app.close_tab(index);
                    }
                    app.toast(format!("Archived “{title}”"));
                } else {
                    app.toast(format!("Restored “{title}”"));
                }
                app.sidebar_refresh();
            }
            Err(err) => {
                let verb = if archive { "archive" } else { "unarchive" };
                app.toast(format!("Could not {verb}: {}", err.user_message()));
            }
        };
        if archive {
            self.backend.call(
                |request_id| ClientRequest::ThreadArchive {
                    request_id,
                    params: ThreadArchiveParams { thread_id: id },
                },
                move |app, result: Result<ThreadArchiveResponse, BackendError>| {
                    on_done(app, result.map(|_| ()));
                },
            );
        } else {
            self.backend.call(
                |request_id| ClientRequest::ThreadUnarchive {
                    request_id,
                    params: ThreadUnarchiveParams { thread_id: id },
                },
                move |app, result: Result<ThreadUnarchiveResponse, BackendError>| {
                    on_done(app, result.map(|_| ()));
                },
            );
        }
    }

    fn sidebar_delete(&mut self, thread: ThreadSummary) {
        self.show_dialog(
            DialogRequest::confirm(
                "Delete thread?",
                format!(
                    "“{}” and its history will be permanently deleted. This cannot be undone.",
                    thread.title
                ),
            )
            .accept_label("Delete")
            .destructive(),
            Box::new(move |app, accepted| {
                if accepted.is_none() {
                    return;
                }
                let thread_id = thread.id;
                let params = ThreadDeleteParams {
                    thread_id: thread_id.clone(),
                };
                app.backend.call(
                    |request_id| ClientRequest::ThreadDelete { request_id, params },
                    move |app, result: Result<ThreadDeleteResponse, BackendError>| match result {
                        Ok(_) => {
                            if let Some(index) = app.tab_index_for_thread(&thread_id) {
                                app.close_tab(index);
                            }
                            app.sidebar_remove_thread(&thread_id);
                        }
                        Err(err) => app.toast(format!("Could not delete: {}", err.user_message())),
                    },
                );
            }),
        );
    }
}

/// `thread/list` parameters for the sidebar: every provider, interactive and
/// app-server sources, most recently updated first.
fn list_params(
    cursor: Option<String>,
    limit: u32,
    search_term: Option<String>,
    archived: bool,
    state_db_only: bool,
) -> ThreadListParams {
    ThreadListParams {
        cursor,
        limit: Some(limit),
        sort_key: Some(ThreadSortKey::UpdatedAt),
        sort_direction: Some(SortDirection::Desc),
        model_providers: Some(Vec::new()),
        source_kinds: Some(vec![
            ThreadSourceKind::Cli,
            ThreadSourceKind::VsCode,
            ThreadSourceKind::Exec,
            ThreadSourceKind::AppServer,
        ]),
        originators: None,
        archived: Some(archived),
        section_id: None,
        project_id: None,
        cwd: None,
        use_state_db_only: state_db_only,
        search_term,
        parent_thread_id: None,
        ancestor_thread_id: None,
    }
}

fn to_slint_row(spec: RowSpec, now: i64, marks: &HashMap<String, ThreadMark>) -> SidebarRow {
    match spec {
        RowSpec::Folder {
            path,
            count,
            collapsed,
        } => {
            let folder = path.to_string_lossy().into_owned();
            SidebarRow {
                kind: SidebarRowKind::Folder,
                id: folder.as_str().into(),
                title: crate::app::folder_label(&path).into(),
                detail: crate::newtab::display_path(&path).into(),
                folder: folder.into(),
                count: i32::try_from(count).unwrap_or(i32::MAX),
                collapsed,
                status: SidebarThreadStatus::NotOpen,
                selected: false,
            }
        }
        RowSpec::Thread {
            id,
            title,
            cwd,
            updated_at,
        } => {
            let mark = marks.get(&id).copied().unwrap_or_default();
            SidebarRow {
                kind: SidebarRowKind::Thread,
                id: id.into(),
                title: title.into(),
                detail: relative_time(now, updated_at).into(),
                folder: cwd.to_string_lossy().into_owned().into(),
                count: 0,
                collapsed: false,
                status: mark.status,
                selected: mark.selected,
            }
        }
        RowSpec::More => SidebarRow {
            kind: SidebarRowKind::More,
            id: "more".into(),
            status: SidebarThreadStatus::NotOpen,
            ..SidebarRow::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn summary(id: &str, cwd: &str, updated_at: i64) -> ThreadSummary {
        ThreadSummary {
            id: id.to_string(),
            title: format!("title {id}"),
            cwd: PathBuf::from(cwd),
            updated_at,
        }
    }

    fn ids(group: &FolderGroup<'_>) -> Vec<String> {
        group
            .threads
            .iter()
            .map(|thread| thread.id.clone())
            .collect()
    }

    #[test]
    fn groups_by_folder_ordered_by_newest_thread() {
        let threads = vec![
            summary("a1", "/a", 100),
            summary("b1", "/b", 300),
            summary("a2", "/a", 200),
            summary("c1", "/c", 50),
        ];
        let groups = group_by_folder(&threads);
        let folders: Vec<&Path> = groups.iter().map(|group| group.cwd).collect();
        assert_eq!(
            folders,
            vec![Path::new("/b"), Path::new("/a"), Path::new("/c")]
        );
        assert_eq!(ids(&groups[1]), vec!["a2".to_string(), "a1".to_string()]);
    }

    #[test]
    fn equal_timestamps_keep_list_order() {
        let threads = vec![summary("x", "/a", 10), summary("y", "/a", 10)];
        let groups = group_by_folder(&threads);
        assert_eq!(ids(&groups[0]), vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn rows_hide_collapsed_folders_and_end_with_more() {
        let threads = vec![
            summary("a1", "/a", 100),
            summary("b1", "/b", 90),
            summary("b2", "/b", 80),
        ];
        let collapsed: HashSet<PathBuf> = [PathBuf::from("/b")].into_iter().collect();
        let rows = build_rows(&threads, &collapsed, /*has_more*/ true);
        assert_eq!(
            rows,
            vec![
                RowSpec::Folder {
                    path: PathBuf::from("/a"),
                    count: 1,
                    collapsed: false,
                },
                RowSpec::Thread {
                    id: "a1".to_string(),
                    title: "title a1".to_string(),
                    cwd: PathBuf::from("/a"),
                    updated_at: 100,
                },
                RowSpec::Folder {
                    path: PathBuf::from("/b"),
                    count: 2,
                    collapsed: true,
                },
                RowSpec::More,
            ]
        );
        assert_eq!(
            build_rows(&[], &HashSet::new(), /*has_more*/ false),
            Vec::new()
        );
    }

    #[test]
    fn merge_page_skips_duplicates() {
        let mut threads = vec![summary("a", "/a", 3), summary("b", "/a", 2)];
        merge_page(
            &mut threads,
            vec![summary("b", "/a", 2), summary("c", "/a", 1)],
        );
        let ids: Vec<&str> = threads.iter().map(|thread| thread.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    fn ids_of(threads: &[ThreadSummary]) -> Vec<&str> {
        threads.iter().map(|thread| thread.id.as_str()).collect()
    }

    #[test]
    fn refreshing_the_head_keeps_the_loaded_tail() {
        // 5 loaded (newest first); the refresh fetched 3, one of them a
        // tail thread that was just updated ("d" moved to the top).
        let old = vec![
            summary("a", "/r", 50),
            summary("b", "/r", 40),
            summary("c", "/r", 30),
            summary("d", "/r", 20),
            summary("e", "/r", 10),
        ];
        let page = vec![
            summary("d", "/r", 60),
            summary("a", "/r", 50),
            summary("b", "/r", 40),
        ];
        let (threads, cursor) = merge_refreshed_head(
            old.clone(),
            Some("after-e".to_string()),
            page.clone(),
            Some("after-b".to_string()),
        );
        assert_eq!(ids_of(&threads), vec!["d", "a", "b", "c", "e"]);
        assert_eq!(cursor.as_deref(), Some("after-e"));

        // A page that reached the end is the whole list (deletions drop).
        let (threads, cursor) =
            merge_refreshed_head(old.clone(), Some("after-e".to_string()), page.clone(), None);
        assert_eq!(ids_of(&threads), vec!["d", "a", "b"]);
        assert_eq!(cursor, None);

        // Nothing loaded beyond the page: the page and its cursor.
        let (threads, cursor) = merge_refreshed_head(
            old[..2].to_vec(),
            Some("after-b".to_string()),
            page,
            Some("after-b2".to_string()),
        );
        assert_eq!(ids_of(&threads), vec!["d", "a", "b"]);
        assert_eq!(cursor.as_deref(), Some("after-b2"));

        // A new filter starts from an empty list.
        let (threads, _) = merge_refreshed_head(
            Vec::new(),
            None,
            vec![summary("x", "/r", 1)],
            Some("after-x".to_string()),
        );
        assert_eq!(ids_of(&threads), vec!["x"]);
    }

    #[test]
    fn only_unfiltered_listings_fall_back_to_a_rollout_scan() {
        // Unfiltered: DB first, then a scan when it is empty.
        assert!(use_state_db_only(/*unfiltered*/ true, None));
        assert!(use_state_db_only(/*unfiltered*/ true, Some(false)));
        assert!(falls_back_to_rollouts(
            true, true, /*unfiltered*/ true, true
        ));
        // A search or the archive with no match is a normal answer.
        assert!(!falls_back_to_rollouts(
            true, true, /*unfiltered*/ false, true
        ));
        assert!(!falls_back_to_rollouts(
            true, true, true, /*empty*/ false
        ));
        assert!(!falls_back_to_rollouts(
            /*first_page*/ false, true, true, true
        ));
        // Filtered listings scan rollouts only when the DB is known empty.
        assert!(use_state_db_only(/*unfiltered*/ false, None));
        assert!(use_state_db_only(/*unfiltered*/ false, Some(true)));
        assert!(!use_state_db_only(/*unfiltered*/ false, Some(false)));
    }

    #[test]
    fn relative_time_uses_compact_units() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now), "now");
        assert_eq!(relative_time(now, now + 30), "now");
        assert_eq!(relative_time(now, now - 59), "now");
        assert_eq!(relative_time(now, now - 60), "1m");
        assert_eq!(relative_time(now, now - 3_599), "59m");
        assert_eq!(relative_time(now, now - 3_600), "1h");
        assert_eq!(relative_time(now, now - 86_399), "23h");
        assert_eq!(relative_time(now, now - 86_400 * 2), "2d");
        assert_eq!(relative_time(now, now - 86_400 * 14), "2w");
        assert_eq!(relative_time(now, now - 86_400 * 95), "3mo");
        assert_eq!(relative_time(now, now - 86_400 * 800), "2y");
    }

    #[test]
    fn titles_prefer_name_then_first_preview_line() {
        assert_eq!(thread_title(Some("  Named "), "preview"), "Named");
        assert_eq!(
            thread_title(Some(" "), "\n  first line \nsecond"),
            "first line"
        );
        assert_eq!(thread_title(None, "   "), UNTITLED_THREAD);
        let long = "x".repeat(200);
        assert_eq!(thread_title(None, &long).chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn list_params_cover_all_providers_and_sources() {
        let params = list_params(
            None,
            50,
            Some("auth".to_string()),
            /*archived*/ false,
            /*state_db_only*/ true,
        );
        assert_eq!(params.model_providers, Some(Vec::new()));
        assert_eq!(params.sort_key, Some(ThreadSortKey::UpdatedAt));
        assert_eq!(params.archived, Some(false));
        assert_eq!(params.search_term.as_deref(), Some("auth"));
        assert!(params.use_state_db_only);
        assert_eq!(
            params.source_kinds,
            Some(vec![
                ThreadSourceKind::Cli,
                ThreadSourceKind::VsCode,
                ThreadSourceKind::Exec,
                ThreadSourceKind::AppServer,
            ])
        );
    }

    #[test]
    fn sync_model_updates_in_place() {
        let model = VecModel::from(vec![1, 2, 3]);
        sync_model(&model, vec![1, 5, 3, 4]);
        assert_eq!(model.iter().collect::<Vec<_>>(), vec![1, 5, 3, 4]);
        sync_model(&model, vec![7]);
        assert_eq!(model.iter().collect::<Vec<_>>(), vec![7]);
        let big = VecModel::from((0..100).collect::<Vec<i32>>());
        sync_model(&big, vec![1, 2]);
        assert_eq!(big.iter().collect::<Vec<_>>(), vec![1, 2]);
    }
}
