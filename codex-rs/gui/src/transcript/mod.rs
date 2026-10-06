//! Transcript of a thread tab: block model, markdown rendering, streaming,
//! history paging, copy and export.
//!
//! - [`store`]: entries and the Slint model kept in sync with them.
//! - [`render`] / [`markdown`] / [`streaming`]: items → rows.
//! - [`history`]: paged hydration, "load older", lag resync.
//! - [`export`]: copy as markdown, export, last reply.
//!
//! # Memory policy (sliding window)
//!
//! A tab keeps at most [`MAX_HOT_ROWS`] rows and [`MAX_HOT_BYTES`] of text
//! while it is active. When live output pushes it past either cap and the
//! user is following the tail, the oldest entries are dropped down to 75%
//! of the caps. When the user has scrolled up to read, trimming waits until
//! they return to the bottom, unless the tab grows past [`HARD_ROWS`] /
//! [`HARD_BYTES`]. Tabs in the background are cut to a tail window of
//! [`BACKGROUND_TAIL_ROWS`] / [`BACKGROUND_TAIL_BYTES`] whenever the active
//! tab changes ([`AppController::transcript_trim_background`]) and while
//! they receive events. Trimmed content stays on disk: the history state
//! switches to "refetch" mode, so scrolling to the top pages it back in
//! through `thread/items/list` (newest first, skipping what is still shown).

mod blocks;
mod crosstab;
mod diff;
mod directives;
mod export;
mod history;
mod links;
mod markdown;
mod model;
mod output;
mod render;
mod store;
mod streaming;

use std::collections::HashMap;
use std::path::PathBuf;

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::UserInput;
use slint::ComponentHandle;

use crate::app::AppController;
use crate::app::TabId;
use crate::app::ThreadPhase;
use crate::app::ThreadTab;
use crate::ui::TranscriptState;

use self::blocks::BlockKind;
use self::history::Older;
use self::history::PageRequest;
use self::history::PageResult;
use self::history::ResyncResult;
use self::links::LinkTarget;
use self::render::RenderContext;
pub(crate) use self::store::Transcript;

/// Rows kept for the active tab before the oldest are dropped.
pub(crate) const MAX_HOT_ROWS: usize = 2000;
/// Text kept for the active tab before the oldest entries are dropped.
pub(crate) const MAX_HOT_BYTES: usize = 8 * 1024 * 1024;
/// Limits that apply even while the user reads older content.
const HARD_ROWS: usize = 3 * MAX_HOT_ROWS;
const HARD_BYTES: usize = 3 * MAX_HOT_BYTES;
/// Tail window kept for tabs in the background.
pub(crate) const BACKGROUND_TAIL_ROWS: usize = 300;
pub(crate) const BACKGROUND_TAIL_BYTES: usize = 1024 * 1024;
/// When the initial history renders fewer rows than this, older pages are
/// fetched right away so the view is filled.
const FILL_ROWS: usize = 40;
/// Consecutive older loads that added nothing before auto-loading stops.
const MAX_EMPTY_LOADS: u32 = 3;
/// Key of the notice row that reports failed history loads.
const HISTORY_ERROR_KEY: &str = "history-error";

/// Severity of a GUI-generated notice row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoticeKind {
    Info,
    Warning,
    Error,
}

/// Where to start loading history for a freshly opened thread.
#[derive(Clone, Debug)]
pub(crate) struct HistoryStart {
    pub(crate) thread_id: String,
    pub(crate) history_mode: ThreadHistoryMode,
    /// Turns returned inline (legacy threads, or forks without paging).
    pub(crate) turns: Vec<Turn>,
    /// `turns_backwards_cursor` from `thread/resume`; `None` = newest end.
    pub(crate) turns_cursor: Option<String>,
    /// `items_backwards_cursor` from `thread/resume`; `None` = newest end.
    pub(crate) items_cursor: Option<String>,
}

/// Status line while a turn runs.
fn status_text(thread: &ThreadTab) -> String {
    match thread.phase {
        ThreadPhase::Running => thread
            .transcript
            .activity
            .clone()
            .unwrap_or_else(|| "Working".to_string()),
        ThreadPhase::WaitingOnUser => "Waiting for your decision".to_string(),
        _ => String::new(),
    }
}

fn phase_code(phase: ThreadPhase) -> i32 {
    match phase {
        ThreadPhase::Starting => 1,
        ThreadPhase::Idle => 2,
        ThreadPhase::Running => 3,
        ThreadPhase::WaitingOnUser => 4,
        ThreadPhase::Error => 5,
        ThreadPhase::Closed => 6,
    }
}

impl AppController {
    pub(crate) fn transcript_bind(&mut self) {
        let state = self.window.global::<TranscriptState>();
        state.on_link_clicked(|url| {
            let url = url.to_string();
            crate::ui_thread::with_app(move |app| app.transcript_open_link(&url));
        });
        state.on_load_older(|requested| {
            crate::ui_thread::with_app(move |app| {
                if let Some(index) = app.active_thread_index() {
                    if requested {
                        app.transcript_load_older(index);
                    } else {
                        app.transcript_auto_load_older(index);
                    }
                }
            });
        });
        state.on_toggle_block(|id| {
            let id = id.to_string();
            crate::ui_thread::with_app(move |app| app.transcript_toggle(&id));
        });
        state.on_copy_block(|id| {
            let id = id.to_string();
            crate::ui_thread::with_app(move |app| app.transcript_block_action(&id, "copy"));
        });
        state.on_block_action(|id, action| {
            let id = id.to_string();
            let action = action.to_string();
            crate::ui_thread::with_app(move |app| app.transcript_block_action(&id, &action));
        });
        state.on_line_clicked(|id, line| {
            let id = id.to_string();
            crate::ui_thread::with_app(move |app| app.transcript_line_clicked(&id, line));
        });
        state.on_suggestion(|text| {
            let text = text.to_string();
            crate::ui_thread::with_app(move |app| app.composer_insert_text(&text));
        });
    }

    fn transcript_flags(&self) -> (bool, bool) {
        self.config.as_ref().map_or((false, false), |config| {
            (config.show_raw_agent_reasoning, config.hide_agent_reasoning)
        })
    }

    /// Threads open in a tab of this window.
    fn transcript_open_threads(&self) -> Vec<String> {
        self.tabs
            .iter()
            .filter_map(|tab| tab.thread().and_then(|thread| thread.thread_id.clone()))
            .collect()
    }

    /// Runs `f` with the tab's transcript and a render context.
    fn with_transcript<R>(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut Transcript, RenderContext<'_>, &mut Option<(String, String)>) -> R,
    ) -> Option<R> {
        let (show_raw_reasoning, hide_reasoning) = self.transcript_flags();
        let open_threads = self.transcript_open_threads();
        let thread = self.thread_tab_mut(index)?;
        let ThreadTab {
            transcript,
            cwd,
            reported_turn_error,
            ..
        } = thread;
        let ctx = RenderContext {
            cwd,
            show_raw_reasoning,
            hide_reasoning,
            open_threads: &open_threads,
        };
        Some(f(transcript, ctx, reported_turn_error))
    }

    /// Binds the active tab's transcript to the view.
    pub(crate) fn transcript_show(&mut self) {
        // Tabs opened or closed since the transcript was rendered change
        // where cross-tab cards link to.
        if let Some(index) = self.active_thread_index() {
            self.with_transcript(index, |transcript, ctx, _| {
                transcript.refresh_cross_tab_links(ctx);
            });
        }
        let model = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
            .map(|thread| thread.transcript.model())
            .unwrap_or_default();
        let state = self.window.global::<TranscriptState>();
        // `show_active` also runs for unrelated tab-strip changes; only a
        // different transcript resets the scroll position.
        if state.get_blocks() != model {
            state.set_blocks(model);
            state.set_follow_tail(true);
            state.set_scroll_request(state.get_scroll_request().wrapping_add(1));
        }
        self.transcript_refresh_state();
    }

    /// Pushes the active tab's paging / status flags to the view.
    fn transcript_refresh_state(&self) {
        let state = self.window.global::<TranscriptState>();
        let Some(thread) = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
        else {
            state.set_has_older(false);
            state.set_loading_older(false);
            state.set_loading(false);
            state.set_older_failed(false);
            state.set_status_text(Default::default());
            state.set_phase(0);
            return;
        };
        let history = &thread.transcript.history;
        state.set_has_older(history.has_older());
        state.set_loading_older(history.loading_older);
        state.set_loading(history.loading_initial);
        state.set_older_failed(!history.can_auto_load());
        state.set_status_text(status_text(thread).into());
        state.set_phase(phase_code(thread.phase));
        state.set_folder(crate::app::folder_label(&thread.cwd).into());
        state.set_error_text(thread.last_error.clone().unwrap_or_default().into());
    }

    pub(crate) fn transcript_on_notification(
        &mut self,
        index: usize,
        notification: &ServerNotification,
    ) {
        let changed = self.with_transcript(index, |transcript, ctx, reported| {
            transcript.apply(notification, reported, ctx)
        });
        let Some(changed) = changed else {
            return;
        };
        if changed {
            self.transcript_enforce_caps(index);
        }
        if self.active == Some(index) {
            self.transcript_refresh_state();
        }
    }

    /// Keeps tab `index` within the memory policy (see the module docs).
    fn transcript_enforce_caps(&mut self, index: usize) {
        let active = self.active == Some(index);
        let following = self.window.global::<TranscriptState>().get_follow_tail();
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let transcript = &mut thread.transcript;
        let (rows, bytes) = (transcript.rows(), transcript.bytes());
        let (target_rows, target_bytes) = if active {
            if rows <= MAX_HOT_ROWS && bytes <= MAX_HOT_BYTES {
                return;
            }
            if !following && rows <= HARD_ROWS && bytes <= HARD_BYTES {
                return;
            }
            (MAX_HOT_ROWS * 3 / 4, MAX_HOT_BYTES * 3 / 4)
        } else {
            if rows <= 2 * BACKGROUND_TAIL_ROWS && bytes <= 2 * BACKGROUND_TAIL_BYTES {
                return;
            }
            (BACKGROUND_TAIL_ROWS, BACKGROUND_TAIL_BYTES)
        };
        if let Some(newest_dropped) = transcript.trim_front(target_rows, target_bytes) {
            transcript.history.after_trim(newest_dropped);
        }
        if active {
            self.transcript_refresh_state();
        }
    }

    /// Loads the history of a resumed or forked thread into tab `index`.
    pub(crate) fn transcript_load_history(&mut self, index: usize, start: HistoryStart) {
        let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
            return;
        };
        let request = self.with_transcript(index, |transcript, ctx, _| {
            let live_turns = transcript.live_turns();
            transcript.clear_notice(HISTORY_ERROR_KEY);
            let history = &mut transcript.history;
            history.reset(start.thread_id.clone(), start.history_mode);
            match start.history_mode {
                ThreadHistoryMode::Legacy if !start.turns.is_empty() => {
                    let slice = history::legacy_slice(
                        start.turns,
                        history::LegacyBoundary::default(),
                        &live_turns,
                    );
                    let turns: HashMap<String, Turn> = slice
                        .turns
                        .into_iter()
                        .map(|turn| (turn.id.clone(), turn))
                        .collect();
                    transcript.adopt_snapshot(&slice.snapshot);
                    transcript.prepend_history(slice.items, &turns, ctx);
                    transcript.history.turns.extend(turns);
                    transcript.history.older = slice.older;
                    None
                }
                ThreadHistoryMode::Legacy => {
                    history.loading_initial = true;
                    Some(PageRequest::initial(
                        history,
                        Older::Legacy { include_from: None },
                        /*turns_cursor*/ None,
                        live_turns,
                    ))
                }
                ThreadHistoryMode::Paginated => {
                    history.loading_initial = true;
                    Some(PageRequest::initial(
                        history,
                        Older::Paged {
                            cursor: start.items_cursor.clone(),
                            include_from: None,
                        },
                        start.turns_cursor.clone(),
                        live_turns,
                    ))
                }
            }
        });
        if let Some(Some(request)) = request {
            self.transcript_spawn_page(tab_id, request);
        }
        if self.active == Some(index) {
            self.transcript_refresh_state();
        }
    }

    fn transcript_spawn_page(&self, tab_id: TabId, request: PageRequest) {
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let result = history::load(backend, request).await;
            crate::ui_thread::post(move |app| app.transcript_apply_page(tab_id, result));
        });
    }

    fn transcript_apply_page(&mut self, tab_id: TabId, result: PageResult) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let applied = self.with_transcript(index, |transcript, ctx, _| {
            if result.generation != transcript.history.generation {
                return None;
            }
            let history = &mut transcript.history;
            history.loading_initial = false;
            history.loading_older = false;
            if result.switched_to_legacy {
                history.mode = Some(ThreadHistoryMode::Legacy);
            }
            history.cursors = result.cursors;
            for turn in result.turns {
                history.turns.insert(turn.id.clone(), turn);
            }
            history.older = result.older;
            history.error.clone_from(&result.error);
            if result.error.is_none() {
                history.boundary_turn = None;
            }
            transcript.adopt_snapshot(&result.snapshot);
            let turns = transcript.history.turns.clone();
            let rows_before = transcript.rows();
            transcript.prepend_history(result.items, &turns, ctx);
            let grew = transcript.rows() > rows_before;
            let history = &mut transcript.history;
            if grew {
                history.empty_loads = 0;
            } else {
                history.empty_loads += 1;
            }
            // One row for history errors, updated by every failed retry
            // (scrolling no longer retries on its own; see `can_auto_load`).
            match &result.error {
                Some(error) => transcript.set_notice(
                    HISTORY_ERROR_KEY,
                    NoticeKind::Warning,
                    "History",
                    &format!("Could not load earlier messages: {error}"),
                    ctx,
                ),
                None => transcript.clear_notice(HISTORY_ERROR_KEY),
            }
            let wants_more = result.error.is_none()
                && transcript.history.has_older()
                && transcript.rows() < FILL_ROWS
                && transcript.history.empty_loads < MAX_EMPTY_LOADS;
            Some(wants_more)
        });
        let Some(Some(wants_more)) = applied else {
            return;
        };
        if wants_more {
            self.transcript_load_older(index);
        }
        if self.active == Some(index) {
            self.transcript_refresh_state();
        }
    }

    /// Loads older history because the view scrolled near the top. After a
    /// failed load this waits for the user to ask again
    /// ([`AppController::transcript_load_older`]), so scrolling does not
    /// retry a broken request over and over.
    fn transcript_auto_load_older(&mut self, index: usize) {
        let can = self
            .thread_tab(index)
            .is_some_and(|thread| thread.transcript.history.can_auto_load());
        if can {
            self.transcript_load_older(index);
        }
    }

    /// Fetches the next page of older history for tab `index`, if any.
    pub(crate) fn transcript_load_older(&mut self, index: usize) {
        let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
            return;
        };
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let thread_id = thread.thread_id.clone();
        let transcript = &mut thread.transcript;
        let history = &mut transcript.history;
        if history.loading_older || history.loading_initial || !history.has_older() {
            return;
        }
        // Threads started in this tab never loaded history; page by the
        // tab's thread id once trimming made older content necessary.
        if !thread_id.is_some_and(|thread_id| history.attach(&thread_id)) {
            return;
        }
        if transcript.rows() >= HARD_ROWS || transcript.bytes() >= HARD_BYTES {
            self.toast("Scroll back to the latest messages before loading more history");
            return;
        }
        let live_turns = transcript.live_turns();
        let history = &mut transcript.history;
        history.loading_older = true;
        let request = PageRequest::older(history, live_turns);
        self.transcript_spawn_page(tab_id, request);
        if self.active == Some(index) {
            self.transcript_refresh_state();
        }
    }

    /// Optimistic echo of input the user just sent.
    pub(crate) fn transcript_push_local_user_message(
        &mut self,
        index: usize,
        client_id: &str,
        input: &[UserInput],
    ) {
        self.with_transcript(index, |transcript, ctx, _| {
            transcript.push_local_user_message(client_id, input, ctx);
        });
        if self.active == Some(index) {
            let state = self.window.global::<TranscriptState>();
            state.set_follow_tail(true);
            state.set_scroll_request(state.get_scroll_request().wrapping_add(1));
            self.transcript_refresh_state();
        }
    }

    /// Adds a titled Markdown card (for example a conversation recap): a
    /// framed block with the rendered Markdown and a Copy action. Cards are
    /// GUI-only; they survive trimming but are not part of exports.
    pub(crate) fn transcript_push_card(&mut self, index: usize, title: String, markdown: String) {
        self.with_transcript(index, |transcript, ctx, _| {
            transcript.push_card(&title, &markdown, ctx);
        });
        if self.active == Some(index) {
            let state = self.window.global::<TranscriptState>();
            if state.get_follow_tail() {
                state.set_scroll_request(state.get_scroll_request().wrapping_add(1));
            }
            self.transcript_refresh_state();
        }
    }

    /// Opens the whole thread of tab `index` (all history pages) as one
    /// selectable text tab, in the export format.
    pub(crate) fn transcript_view_thread_as_text(&mut self, index: usize) {
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let title = thread.title();
        if thread.thread_id.is_some() {
            self.toast("Loading the thread…");
        }
        self.transcript_export_markdown_complete(index, move |app, result| match result {
            Ok(markdown) => app.open_text_tab(title, markdown),
            Err(err) if err == export::NOTHING_TO_EXPORT => {
                app.toast("This thread has no messages yet");
            }
            Err(err) => app.toast(format!("Could not load the thread: {err}")),
        });
    }

    pub(crate) fn transcript_push_notice(&mut self, index: usize, kind: NoticeKind, text: String) {
        self.with_transcript(index, |transcript, ctx, _| {
            transcript.push_notice(kind, "", &text, ctx);
        });
        if self.active == Some(index) {
            self.transcript_refresh_state();
        }
    }

    /// The message sent as `client_id` did not reach the server (the turn,
    /// steer or queue request failed): its pending bubble is marked as not
    /// sent instead of waiting forever.
    pub(crate) fn transcript_echo_unsent(&mut self, index: usize, client_id: &str) {
        self.with_transcript(index, |transcript, ctx, _| {
            transcript.mark_echo_unsent(client_id, ctx);
        });
    }

    /// The queued message sent as `client_id` was removed from the queue.
    pub(crate) fn transcript_remove_echo(&mut self, index: usize, client_id: &str) {
        self.with_transcript(index, |transcript, _, _| {
            transcript.remove_echo(client_id);
        });
    }

    /// Events were dropped: re-read the thread and reconcile the newest
    /// items without duplicating rows.
    pub(crate) fn transcript_on_lagged(&mut self, index: usize) {
        let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
            return;
        };
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        let transcript = &mut thread.transcript;
        if !transcript.history.attach(&thread_id) {
            return;
        }
        let mode = transcript.history.mode;
        let live_turns = transcript.live_turns();
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let result = history::resync(backend, thread_id, mode, live_turns).await;
            crate::ui_thread::post(move |app| app.transcript_apply_resync(tab_id, result));
        });
    }

    fn transcript_apply_resync(&mut self, tab_id: TabId, result: ResyncResult) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        if let Some(error) = &result.error {
            tracing::warn!(%error, "transcript resync failed");
        }
        // Matched against the tab's thread (not the history state, which
        // threads started in the tab do not have).
        if self
            .thread_tab(index)
            .is_none_or(|thread| thread.thread_id.as_deref() != Some(result.thread_id.as_str()))
        {
            return;
        }
        let mut phase_changed = false;
        if let Some(thread) = self.thread_tab_mut(index)
            && let Some(status) = &result.status
            && !matches!(status, ThreadStatus::Active { .. })
            && matches!(
                thread.phase,
                ThreadPhase::Running | ThreadPhase::WaitingOnUser
            )
        {
            thread.phase = match status {
                ThreadStatus::SystemError => ThreadPhase::Error,
                _ => ThreadPhase::Idle,
            };
            thread.active_turn_id = None;
            phase_changed = true;
        }
        self.with_transcript(index, |transcript, ctx, _| {
            let history = &mut transcript.history;
            if !history.attach(&result.thread_id) {
                return;
            }
            if result.legacy {
                history.mode = Some(ThreadHistoryMode::Legacy);
                transcript.adopt_snapshot(&result.snapshot);
            }
            transcript.resync(result.items, ctx);
        });
        if phase_changed {
            self.refresh_tabs();
            self.composer_show();
        }
        if self.active == Some(index) {
            self.transcript_refresh_state();
        }
    }

    /// The loaded transcript of tab `index` in the TUI's export format.
    ///
    /// Only what is loaded is included; use
    /// [`AppController::transcript_export_markdown_complete`] to page in the
    /// whole thread first.
    pub(crate) fn transcript_export_markdown(&self, index: usize) -> String {
        let Some(thread) = self.thread_tab(index) else {
            return String::new();
        };
        let sources = export::entry_sources(thread.transcript.entries());
        let mut markdown = export::render_transcript(&sources, &thread.cwd)
            .unwrap_or_else(|_| export::EXPORT_TITLE.to_string());
        if thread.transcript.history.has_older() {
            markdown.insert_str(
                export::EXPORT_TITLE.len(),
                "\n_Earlier messages were not loaded when this was exported._\n",
            );
        }
        markdown
    }

    /// Exports the whole thread of tab `index` (all history pages) and calls
    /// `on_done` on the UI thread with the markdown or an error message.
    pub(crate) fn transcript_export_markdown_complete(
        &mut self,
        index: usize,
        on_done: impl FnOnce(&mut AppController, Result<String, String>) + Send + 'static,
    ) {
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            let markdown = self.transcript_export_markdown(index);
            on_done(self, Ok(markdown));
            return;
        };
        let mode = thread.transcript.history.mode;
        let cwd = thread.cwd.clone();
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let result = history::load_all(backend, thread_id, mode).await;
            let markdown = result.and_then(|items| {
                let sources: Vec<export::ExportSource<'_>> = items
                    .iter()
                    .map(|(_, item)| export::ExportSource::Item {
                        item,
                        live_text: None,
                    })
                    .collect();
                export::render_transcript(&sources, &cwd)
            });
            crate::ui_thread::post(move |app| on_done(app, markdown));
        });
    }

    /// Drops background tabs to a tail window (see the module docs).
    pub(crate) fn transcript_trim_background(&mut self) {
        let active = self.active;
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if Some(index) == active {
                continue;
            }
            let Some(thread) = tab.thread_mut() else {
                continue;
            };
            let transcript = &mut thread.transcript;
            if let Some(newest_dropped) =
                transcript.trim_front(BACKGROUND_TAIL_ROWS, BACKGROUND_TAIL_BYTES)
            {
                transcript.history.after_trim(newest_dropped);
            }
        }
    }

    /// Markdown of the latest completed agent message (for "copy last reply").
    pub(crate) fn transcript_last_agent_message(&self, index: usize) -> Option<String> {
        let thread = self.thread_tab(index)?;
        thread
            .transcript
            .entries()
            .iter()
            .rev()
            .filter(|entry| {
                entry.item().is_some_and(|item| {
                    item.completed
                        && matches!(
                            item.item,
                            codex_app_server_protocol::ThreadItem::AgentMessage { .. }
                        )
                })
            })
            .find_map(|entry| export::entry_markdown(entry, &thread.cwd))
    }

    /// Test hook for scripted UI runs (`{"transcript": "<command>"}`):
    /// `expand-all`, `collapse-all`, `load-older`, `scroll-top`,
    /// `scroll-bottom`, `trim:<rows>`, `export:<path>`,
    /// `export-complete:<path>`, `copy-last`, `card:<title>|<markdown>`
    /// (`\n` for newlines), `view-thread`, `view-message:<n>` (n-th message
    /// from the end), `open-source` (the newest cross-tab card's tab),
    /// `wheel:<x>,<y>,<dx>,<dy>` (a scroll event at a logical position),
    /// `echo-unsent:<text>` (a message whose sending failed),
    /// `history-error:<message>` (an older-history page that failed).
    pub(crate) fn transcript_automation(&mut self, command: &str) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let (name, argument) = command.split_once(':').unwrap_or((command, ""));
        match name {
            "expand-all" | "collapse-all" => {
                let expand = name == "expand-all";
                let ids: Vec<String> = self
                    .thread_tab(index)
                    .map(|thread| {
                        thread
                            .transcript
                            .entries()
                            .iter()
                            .flat_map(|entry| {
                                entry
                                    .blocks
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, block)| {
                                        block.toggle.is_some() && block.expanded != expand
                                    })
                                    .map(|(local, _)| format!("{}#{local}", entry.key))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Toggling can add rows to an entry; ids of later rows in the
                // same entry stay valid because toggles only grow the tail.
                for id in ids.into_iter().rev() {
                    self.transcript_toggle(&id);
                }
            }
            "load-older" => self.transcript_load_older(index),
            "scroll-top" => {
                let state = self.window.global::<TranscriptState>();
                state.set_follow_tail(false);
                state.set_scroll_top_request(state.get_scroll_top_request().wrapping_add(1));
            }
            "scroll-bottom" => {
                let state = self.window.global::<TranscriptState>();
                state.set_follow_tail(true);
                state.set_scroll_request(state.get_scroll_request().wrapping_add(1));
            }
            "trim" => {
                let rows = argument.parse().unwrap_or(BACKGROUND_TAIL_ROWS);
                if let Some(thread) = self.thread_tab_mut(index)
                    && let Some(dropped) = thread.transcript.trim_front(rows, usize::MAX)
                {
                    thread.transcript.history.after_trim(dropped);
                }
                self.transcript_refresh_state();
            }
            "export" => {
                let markdown = self.transcript_export_markdown(index);
                self.transcript_write_file(argument, markdown);
            }
            "export-complete" => {
                let path = argument.to_string();
                self.transcript_export_markdown_complete(index, move |app, result| {
                    let markdown = result.unwrap_or_else(|err| format!("error: {err}\n"));
                    app.transcript_write_file(&path, markdown);
                });
            }
            "copy-last" => {
                if let Some(markdown) = self.transcript_last_agent_message(index) {
                    self.copy_to_clipboard(&markdown);
                }
            }
            "card" => {
                let (title, markdown) = argument.split_once('|').unwrap_or((argument, ""));
                self.transcript_push_card(index, title.to_string(), markdown.replace("\\n", "\n"));
            }
            "view-thread" => self.transcript_view_thread_as_text(index),
            "view-message" => {
                // `view-message:<n>`: the n-th message from the end (1 = last).
                let nth = argument.parse::<usize>().unwrap_or(1).max(1);
                let row = self.thread_tab(index).and_then(|thread| {
                    thread
                        .transcript
                        .entries()
                        .iter()
                        .rev()
                        .filter(|entry| entry.blocks.iter().any(|block| block.message))
                        .nth(nth - 1)
                        .map(|entry| format!("{}#0", entry.key))
                });
                if let Some(row) = row {
                    self.transcript_block_action(&row, "view-text");
                }
            }
            "open-source" => {
                let row = self.thread_tab(index).and_then(|thread| {
                    thread
                        .transcript
                        .entries()
                        .iter()
                        .rev()
                        .find(|entry| {
                            entry.blocks.first().is_some_and(|block| {
                                block.kind == BlockKind::CardHeader && !block.target.is_empty()
                            })
                        })
                        .map(|entry| format!("{}#0", entry.key))
                });
                if let Some(row) = row {
                    self.transcript_block_action(&row, "open-source");
                }
            }
            "wheel" => {
                // `wheel:<x>,<y>,<dx>,<dy>`: a scroll event at a logical position.
                let values: Vec<f32> = argument
                    .split(',')
                    .filter_map(|value| value.trim().parse().ok())
                    .collect();
                if let [x, y, delta_x, delta_y] = values[..] {
                    self.window.window().dispatch_event(
                        slint::platform::WindowEvent::PointerScrolled {
                            position: slint::LogicalPosition::new(x, y),
                            delta_x,
                            delta_y,
                        },
                    );
                }
            }
            "echo-unsent" => {
                let client_id = crate::session::new_client_message_id();
                let input = [crate::session::text_input(argument)];
                self.transcript_push_local_user_message(index, &client_id, &input);
                self.transcript_echo_unsent(index, &client_id);
            }
            "history-error" => {
                let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
                    return;
                };
                let Some(thread) = self.thread_tab_mut(index) else {
                    return;
                };
                let history = &mut thread.transcript.history;
                let result = PageResult {
                    generation: history.generation,
                    older: Older::Paged {
                        cursor: None,
                        include_from: None,
                    },
                    cursors: history.cursors.clone(),
                    error: Some(argument.to_string()),
                    ..PageResult::default()
                };
                self.transcript_apply_page(tab_id, result);
            }
            other => tracing::warn!(command = other, "unknown transcript automation command"),
        }
    }

    fn transcript_write_file(&self, path: &str, contents: String) {
        let path = PathBuf::from(path);
        self.backend.spawn(async move {
            if let Err(err) = std::fs::write(&path, contents) {
                tracing::warn!(%err, path = %path.display(), "could not write transcript file");
            }
        });
    }

    // ----- view callbacks ---------------------------------------------------

    fn transcript_toggle(&mut self, row_id: &str) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        self.with_transcript(index, |transcript, ctx, _| transcript.toggle(row_id, ctx));
    }

    fn transcript_open_link(&mut self, url: &str) {
        let cwd = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
            .map(|thread| thread.cwd.clone())
            .unwrap_or_default();
        match links::classify_link(url, &cwd) {
            LinkTarget::Web(url) => {
                if let Err(err) = webbrowser::open(&url) {
                    self.toast(format!("Could not open link: {err}"));
                }
            }
            LinkTarget::File { path, line } => self.open_file_tab(path, line),
            LinkTarget::Unknown(url) => self.toast(format!("Cannot open {url}")),
        }
    }

    fn transcript_block_action(&mut self, row_id: &str, action: &str) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let cwd = thread.cwd.clone();
        let Some((entry, block)) = thread.transcript.block_for_row(row_id) else {
            return;
        };
        // A long code block spans several rows; its actions use all of it.
        let code = store::split_row_id(row_id)
            .and_then(|(_, local)| export::code_block_text(&entry.blocks, local));
        match action {
            "copy" => {
                let text = code.unwrap_or_else(|| export::block_copy_text(block));
                self.copy_to_clipboard(&text);
            }
            "copy-message" => {
                if let Some(markdown) = export::entry_markdown(entry, &cwd) {
                    self.copy_to_clipboard(&markdown);
                }
            }
            "quote" => {
                if let Some(markdown) = export::entry_markdown(entry, &cwd) {
                    let quoted = export::quote(&markdown);
                    self.composer_insert_text(&quoted);
                }
            }
            "forward" => {
                if let Some(markdown) = export::entry_markdown(entry, &cwd) {
                    self.xtab_open_forward_dialog(index, markdown);
                }
            }
            "view-text" => {
                let view = if block.message {
                    export::entry_text_view(entry, &cwd)
                } else {
                    let text = code.unwrap_or_else(|| export::block_copy_text(block));
                    Some((block_view_title(block), text))
                };
                if let Some((what, text)) = view {
                    let title = self
                        .thread_tab(index)
                        .map(|thread| format!("{what} · {}", thread.title()))
                        .unwrap_or(what);
                    self.open_text_tab(title, text);
                }
            }
            "open-source" => {
                if !block.target.is_empty() {
                    let thread_id = block.target.clone();
                    match self.tab_index_for_thread(&thread_id) {
                        Some(source) => self.activate_tab(source),
                        None => self.open_thread(thread_id, None),
                    }
                }
            }
            "toggle" => self.transcript_toggle(row_id),
            "open-file" => {
                if !block.target.is_empty() {
                    let path = PathBuf::from(&block.target);
                    self.open_file_tab(path, None);
                }
            }
            "open-agent" => {
                if !block.target.is_empty() {
                    let thread_id = block.target.clone();
                    self.open_thread(thread_id, None);
                }
            }
            "open-diff" => {
                let change = match (entry.item().map(|item| &item.item), block.toggle) {
                    (
                        Some(codex_app_server_protocol::ThreadItem::FileChange { changes, .. }),
                        Some(file),
                    ) if block.kind == BlockKind::PatchFile => changes.get(file).cloned(),
                    _ => None,
                };
                if let Some(change) = change {
                    let title = links::display_path(&change.path, &cwd);
                    self.open_diff_tab(title, diff::unified_diff(&change));
                }
            }
            "export" => self.thread_tab_action(index, "export"),
            other => tracing::debug!(action = other, "unknown transcript action"),
        }
    }

    fn transcript_line_clicked(&mut self, row_id: &str, line: i32) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let target = self.thread_tab(index).and_then(|thread| {
            let (_, block) = thread.transcript.block_for_row(row_id)?;
            let line = block.lines.get(usize::try_from(line).ok()?)?;
            (!line.target.is_empty()).then(|| (block.kind, line.target.clone()))
        });
        match target {
            Some((BlockKind::Agent, thread_id)) => self.open_thread(thread_id, None),
            Some((_, path)) => self.open_file_tab(PathBuf::from(path), None),
            None => {}
        }
    }
}

/// Name of a single row shown as text ("Code", "Output", ...).
fn block_view_title(block: &blocks::Block) -> String {
    match block.kind {
        BlockKind::Code if !block.meta.is_empty() => format!("{} code", block.meta),
        BlockKind::Code => "Code".to_string(),
        BlockKind::Exec => "Output".to_string(),
        BlockKind::Table => "Table".to_string(),
        BlockKind::Tool => block.title.clone(),
        _ if !block.title.is_empty() => block.title.clone(),
        _ => "Text".to_string(),
    }
}
