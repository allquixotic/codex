//! Background terminals of a thread (`/ps`, `/stop`, GUI.md §6): commands
//! the agent left running in unified-exec sessions.
//!
//! The list comes from `thread/backgroundTerminals/list` (experimental). It
//! is refreshed when a notification hints at a change (a command with a
//! process id started or finished, or the agent wrote to one), and on a slow
//! timer only while the section is on screen. At most one request per tab
//! is in flight; changes during it trigger one more.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadBackgroundTerminal;
use codex_app_server_protocol::ThreadBackgroundTerminalsCleanParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsCleanResponse;
use codex_app_server_protocol::ThreadBackgroundTerminalsListParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsListResponse;
use codex_app_server_protocol::ThreadBackgroundTerminalsTerminateParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsTerminateResponse;
use codex_app_server_protocol::ThreadItem;
use slint::ComponentHandle;

use super::Fetch;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::app::TabId;
use crate::app::ThreadPhase;
use crate::backend::BackendError;
use crate::ui::AppState;
use crate::ui::InfoState;
use crate::ui::InfoTerminal;

/// Refresh period while the section is visible.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
/// Delay before re-listing after "Stop all" (cleaning is asynchronous).
const CLEAN_SETTLE: Duration = Duration::from_millis(750);
const PAGE_LIMIT: u32 = 100;
/// Commands listed in confirmations before "and N more".
const MAX_LISTED_COMMANDS: usize = 6;

/// A command running in the background of a thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BackgroundTerminal {
    pub(crate) process_id: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
}

impl From<ThreadBackgroundTerminal> for BackgroundTerminal {
    fn from(terminal: ThreadBackgroundTerminal) -> Self {
        Self {
            process_id: terminal.process_id,
            command: terminal.command.trim().to_string(),
            cwd: terminal.cwd.as_str().to_string(),
        }
    }
}

/// Background terminals of one tab.
#[derive(Debug, Default)]
pub(crate) struct Terminals {
    list: Vec<BackgroundTerminal>,
    state: Fetch,
    /// A list request is in flight.
    loading: bool,
    /// Something may have changed since the last list.
    stale: bool,
    /// `/ps` asked for the section, even while it is empty.
    pinned: bool,
    /// The info pane cannot show the section: report the next list in a
    /// dialog instead.
    report_in_dialog: bool,
    /// Process ids with a stop request in flight.
    stopping: BTreeSet<String>,
    error: Option<String>,
}

/// What the section shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TerminalsView {
    pub(crate) shown: bool,
    pub(crate) summary: String,
    pub(crate) note: String,
    pub(crate) rows: Vec<(BackgroundTerminal, bool)>,
}

impl Terminals {
    /// Takes a fresh list from the server.
    fn apply(&mut self, list: Vec<BackgroundTerminal>) {
        let had_terminals = !self.list.is_empty();
        self.list = list;
        self.state = Fetch::Done;
        self.error = None;
        // Everything `/ps` showed has stopped: hide the section again.
        if had_terminals && self.list.is_empty() {
            self.pinned = false;
        }
        let list = &self.list;
        self.stopping
            .retain(|id| list.iter().any(|terminal| terminal.process_id == *id));
    }

    /// Section contents: hidden while empty unless `/ps` pinned it.
    pub(crate) fn view(&self) -> TerminalsView {
        let shown = self.pinned || !self.list.is_empty();
        if !shown {
            return TerminalsView::default();
        }
        let note = match (&self.error, self.state) {
            (Some(error), _) => format!("Background terminals are not available: {error}"),
            (None, Fetch::NotRequested | Fetch::Loading) if self.list.is_empty() => {
                "Loading…".to_string()
            }
            (None, _) if self.list.is_empty() => {
                "No commands are running in the background.".to_string()
            }
            _ => String::new(),
        };
        TerminalsView {
            shown,
            summary: running_summary(self.list.len()),
            note,
            rows: self
                .list
                .iter()
                .map(|terminal| {
                    (
                        terminal.clone(),
                        self.stopping.contains(&terminal.process_id),
                    )
                })
                .collect(),
        }
    }
}

/// "2 running".
pub(crate) fn running_summary(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!("{count} running")
    }
}

/// Whether `notification` (for the tab's thread) may change its list.
pub(crate) fn may_change_terminals(notification: &ServerNotification) -> bool {
    match notification {
        ServerNotification::TerminalInteraction(_) => true,
        ServerNotification::ItemStarted(started) => has_process(&started.item),
        ServerNotification::ItemCompleted(completed) => has_process(&completed.item),
        _ => false,
    }
}

fn has_process(item: &ThreadItem) -> bool {
    matches!(
        item,
        ThreadItem::CommandExecution {
            process_id: Some(_),
            ..
        }
    )
}

/// Lists `terminals` for a confirmation dialog.
pub(crate) fn command_list(terminals: &[BackgroundTerminal]) -> String {
    let mut lines: Vec<String> = terminals
        .iter()
        .take(MAX_LISTED_COMMANDS)
        .map(|terminal| format!("• {}", first_line(&terminal.command)))
        .collect();
    let hidden = terminals.len().saturating_sub(MAX_LISTED_COMMANDS);
    if hidden > 0 {
        lines.push(format!("…and {hidden} more"));
    }
    lines.join("\n")
}

/// Where a terminal runs, relative to the thread's folder: empty for the
/// folder itself, `sub/dir` inside it, a `~`-shortened path elsewhere.
pub(crate) fn terminal_location(cwd: &str, thread_cwd: &Path) -> String {
    let cwd = Path::new(cwd);
    match cwd.strip_prefix(thread_cwd) {
        Ok(relative) => relative.to_string_lossy().into_owned(),
        Err(_) => crate::newtab::display_path(cwd),
    }
}

fn first_line(command: &str) -> String {
    let mut lines = command.lines();
    let first = lines.next().unwrap_or_default().trim();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_string()
    }
}

impl AppController {
    /// Hooks notifications of tab `index` that may change its list.
    pub(super) fn info_terminals_on_notification(
        &mut self,
        index: usize,
        notification: &ServerNotification,
    ) {
        if may_change_terminals(notification) {
            self.info_refresh_terminals(index);
        }
    }

    /// Refreshes now for the active tab, later (when shown) otherwise.
    fn info_refresh_terminals(&mut self, index: usize) {
        let active = self.active == Some(index);
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let terminals = &mut thread.info.terminals;
        if !active || terminals.loading {
            terminals.stale = true;
            return;
        }
        self.info_fetch_terminals(index);
    }

    /// First display of a tab, or a refresh that waited for it.
    pub(super) fn info_terminals_load_lazy(&mut self, index: usize) {
        let needed = self.thread_tab(index).is_some_and(|thread| {
            let terminals = &thread.info.terminals;
            thread.thread_id.is_some()
                && thread.phase != ThreadPhase::Starting
                && !terminals.loading
                && (terminals.state == Fetch::NotRequested || terminals.stale)
        });
        if needed {
            self.info_fetch_terminals(index);
        }
    }

    pub(super) fn info_terminals_reset(&mut self, index: usize) {
        if let Some(thread) = self.thread_tab_mut(index) {
            let terminals = &mut thread.info.terminals;
            terminals.state = Fetch::NotRequested;
            terminals.loading = false;
            terminals.stopping.clear();
            terminals.list.clear();
            terminals.error = None;
        }
    }

    fn info_fetch_terminals(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        let terminals = &mut thread.info.terminals;
        terminals.loading = true;
        terminals.stale = false;
        if terminals.state == Fetch::NotRequested {
            terminals.state = Fetch::Loading;
        }
        self.backend.call(
            |request_id| ClientRequest::ThreadBackgroundTerminalsList {
                request_id,
                params: ThreadBackgroundTerminalsListParams {
                    thread_id,
                    cursor: None,
                    limit: Some(PAGE_LIMIT),
                },
            },
            move |app, result: Result<ThreadBackgroundTerminalsListResponse, BackendError>| {
                app.info_on_terminals(tab_id, result);
            },
        );
    }

    fn info_on_terminals(
        &mut self,
        tab_id: TabId,
        result: Result<ThreadBackgroundTerminalsListResponse, BackendError>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let active = self.active == Some(index);
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let terminals = &mut thread.info.terminals;
        terminals.loading = false;
        match result {
            Ok(response) => terminals.apply(
                response
                    .data
                    .into_iter()
                    .map(BackgroundTerminal::from)
                    .collect(),
            ),
            Err(err) => {
                // Servers without the experimental API (older daemons) have
                // no list; the section stays hidden unless `/ps` asked.
                tracing::debug!(error = %err.user_message(), "background terminal list failed");
                terminals.list.clear();
                terminals.state = Fetch::Unavailable;
                terminals.error = Some(err.user_message());
            }
        }
        let report = std::mem::take(&mut terminals.report_in_dialog);
        let again = terminals.stale && active;
        if again {
            self.info_fetch_terminals(index);
        }
        if report {
            self.info_report_terminals(index);
        }
        if active {
            self.info_render();
        }
    }

    /// Pushes the section of the active tab and keeps the refresh timer in
    /// step with it.
    pub(super) fn info_render_terminals(&mut self, index: usize) {
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let view = thread.info.terminals.view();
        let thread_cwd = thread.cwd.clone();
        let state = self.window.global::<InfoState>();
        state.set_terminals_shown(view.shown);
        state.set_terminals_summary(view.summary.into());
        state.set_terminals_note(view.note.into());
        crate::sidebar::sync_model(
            &self.info_shared.models.terminals,
            view.rows
                .into_iter()
                .map(|(terminal, stopping)| InfoTerminal {
                    process_id: terminal.process_id.into(),
                    command: first_line(&terminal.command).into(),
                    cwd: terminal_location(&terminal.cwd, &thread_cwd).into(),
                    stopping,
                })
                .collect(),
        );
        let timer = &self.info_shared.terminal_timer;
        if view.shown && !timer.running() {
            timer.start(slint::TimerMode::Repeated, REFRESH_INTERVAL, || {
                crate::ui_thread::with_app(AppController::info_terminals_tick);
            });
        } else if !view.shown && timer.running() {
            timer.stop();
        }
    }

    fn info_terminals_tick(&mut self) {
        let Some(index) = self.active_thread_index() else {
            self.info_shared.terminal_timer.stop();
            return;
        };
        let collapsed = self.window.global::<InfoState>().get_terminals_collapsed();
        let visible = self.info_pane_visible() && !collapsed;
        let idle = self
            .thread_tab(index)
            .is_some_and(|thread| !thread.info.terminals.loading);
        if visible && idle {
            self.info_fetch_terminals(index);
        }
    }

    /// Whether the window is wide enough for the info pane.
    pub(super) fn info_window_is_wide(&self) -> bool {
        // Same rule as the layout in ui/app.slint (it depends on the sidebar).
        self.window.global::<AppState>().get_info_fits()
    }

    /// `/ps`: shows the section (opening the info pane), or a dialog when
    /// the window is too narrow for the pane.
    pub(crate) fn info_show_terminals(&mut self, index: usize) {
        let narrow = !self.info_window_is_wide();
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        if thread.thread_id.is_none() {
            self.toast("Wait for the thread to start");
            return;
        }
        let terminals = &mut thread.info.terminals;
        terminals.pinned = true;
        terminals.report_in_dialog = narrow;
        let loading = terminals.loading;
        self.window
            .global::<InfoState>()
            .set_terminals_collapsed(false);
        let app_state = self.window.global::<AppState>();
        if !narrow && !app_state.get_info_visible() {
            app_state.invoke_toggle_info();
        }
        if loading {
            if let Some(thread) = self.thread_tab_mut(index) {
                thread.info.terminals.stale = true;
            }
        } else {
            self.info_fetch_terminals(index);
        }
        if self.active == Some(index) {
            self.info_render();
        }
    }

    /// The list in a dialog, for windows too narrow for the info pane.
    fn info_report_terminals(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        let terminals = &thread.info.terminals;
        if let Some(error) = terminals.error.clone() {
            self.toast(format!("Could not list background terminals: {error}"));
            return;
        }
        if terminals.list.is_empty() {
            self.toast("No commands are running in the background");
            return;
        }
        let message = command_list(&terminals.list);
        let title = format!(
            "Background terminals ({})",
            running_summary(terminals.list.len())
        );
        self.show_dialog(
            DialogRequest {
                cancel_label: "Close".to_string(),
                ..DialogRequest::confirm(title, message)
            }
            .accept_label("Stop all")
            .destructive(),
            Box::new(move |app, accepted| {
                if accepted.is_some() {
                    app.info_clean_terminals(tab_id);
                }
            }),
        );
    }

    /// `/stop` and "Stop all": confirms, then stops every background
    /// terminal of tab `index`.
    pub(crate) fn info_stop_all_terminals(&mut self, index: usize) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        if thread.thread_id.is_none() {
            self.toast("Wait for the thread to start");
            return;
        }
        let terminals = &thread.info.terminals;
        let known = terminals.state == Fetch::Done && !terminals.loading;
        if known && terminals.list.is_empty() {
            self.toast("No commands are running in the background");
            return;
        }
        let message = if known {
            format!(
                "These commands are terminated:\n\n{}",
                command_list(&terminals.list)
            )
        } else {
            "Every command this thread left running in the background is terminated.".to_string()
        };
        self.show_dialog(
            DialogRequest::confirm("Stop all background terminals?", message)
                .accept_label("Stop all")
                .destructive(),
            Box::new(move |app, accepted| {
                if accepted.is_some() {
                    app.info_clean_terminals(tab_id);
                }
            }),
        );
    }

    fn info_clean_terminals(&mut self, tab_id: TabId) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(thread_id) = self
            .thread_tab(index)
            .and_then(|thread| thread.thread_id.clone())
        else {
            return;
        };
        self.backend.call(
            |request_id| ClientRequest::ThreadBackgroundTerminalsClean {
                request_id,
                params: ThreadBackgroundTerminalsCleanParams { thread_id },
            },
            move |app, result: Result<ThreadBackgroundTerminalsCleanResponse, BackendError>| {
                match result {
                    Ok(_) => {
                        app.toast("Stopping background terminals…");
                        slint::Timer::single_shot(CLEAN_SETTLE, move || {
                            crate::ui_thread::with_app(move |app| {
                                app.info_relist_terminals(tab_id)
                            });
                        });
                    }
                    Err(err) => app.toast(format!(
                        "Could not stop background terminals: {}",
                        err.user_message()
                    )),
                }
            },
        );
    }

    fn info_relist_terminals(&mut self, tab_id: TabId) {
        if let Some(index) = self.tab_index_by_id(tab_id) {
            self.info_refresh_terminals(index);
        }
    }

    /// Row actions: "stop" one terminal, or "stop-all".
    pub(super) fn info_terminal_action(&mut self, process_id: &str, action: &str) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        match action {
            "stop" => self.info_terminate(index, process_id.to_string()),
            "stop-all" => self.info_stop_all_terminals(index),
            other => tracing::debug!(action = other, "unknown terminal action"),
        }
    }

    fn info_terminate(&mut self, index: usize, process_id: String) {
        let tab_id = self.tabs[index].id;
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(thread_id) = thread.thread_id.clone() else {
            return;
        };
        if !thread.info.terminals.stopping.insert(process_id.clone()) {
            return;
        }
        self.info_render();
        let stopped_id = process_id.clone();
        self.backend.call(
            |request_id| ClientRequest::ThreadBackgroundTerminalsTerminate {
                request_id,
                params: ThreadBackgroundTerminalsTerminateParams {
                    thread_id,
                    process_id,
                },
            },
            move |app, result: Result<ThreadBackgroundTerminalsTerminateResponse, BackendError>| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                match result {
                    Ok(response) if !response.terminated => {
                        app.toast("That command had already finished");
                    }
                    Ok(_) => {}
                    Err(err) => {
                        app.toast(format!(
                            "Could not stop the command: {}",
                            err.user_message()
                        ));
                    }
                }
                if let Some(thread) = app.thread_tab_mut(index) {
                    thread.info.terminals.stopping.remove(&stopped_id);
                }
                app.info_refresh_terminals(index);
                if app.active == Some(index) {
                    app.info_render();
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::CommandExecutionSource;
    use codex_app_server_protocol::CommandExecutionStatus;
    use codex_app_server_protocol::ItemStartedNotification;
    use codex_app_server_protocol::TerminalInteractionNotification;
    use codex_app_server_protocol::ThreadClosedNotification;
    use pretty_assertions::assert_eq;

    fn terminal(process_id: &str, command: &str) -> BackgroundTerminal {
        BackgroundTerminal {
            process_id: process_id.to_string(),
            command: command.to_string(),
            cwd: "/work".to_string(),
        }
    }

    #[test]
    fn section_is_hidden_until_there_is_something_or_ps_asked() {
        let mut terminals = Terminals::default();
        assert_eq!(terminals.view(), TerminalsView::default());

        terminals.pinned = true;
        let view = terminals.view();
        assert!(view.shown);
        assert_eq!(view.note, "Loading…");
        assert_eq!(view.summary, "");

        terminals.state = Fetch::Done;
        assert_eq!(
            terminals.view().note,
            "No commands are running in the background."
        );

        terminals.pinned = false;
        terminals.list = vec![terminal("7", "sleep 30"), terminal("8", "npm run dev")];
        terminals.stopping.insert("8".to_string());
        let view = terminals.view();
        assert!(view.shown);
        assert_eq!(view.summary, "2 running");
        assert_eq!(view.note, "");
        assert_eq!(
            view.rows,
            vec![
                (terminal("7", "sleep 30"), false),
                (terminal("8", "npm run dev"), true),
            ]
        );
    }

    #[test]
    fn stopping_everything_unpins_the_section() {
        let mut terminals = Terminals {
            pinned: true,
            ..Terminals::default()
        };
        terminals.apply(Vec::new());
        assert!(terminals.pinned, "an empty first list keeps /ps visible");
        terminals.apply(vec![terminal("7", "sleep 30"), terminal("8", "make")]);
        terminals.stopping.insert("7".to_string());
        terminals.stopping.insert("8".to_string());
        terminals.apply(vec![terminal("8", "make")]);
        assert_eq!(terminals.stopping, BTreeSet::from(["8".to_string()]));
        assert!(terminals.pinned);
        terminals.apply(Vec::new());
        assert!(!terminals.pinned);
        assert!(terminals.stopping.is_empty());
        assert_eq!(terminals.view(), TerminalsView::default());
    }

    #[test]
    fn errors_show_only_when_pinned() {
        let mut terminals = Terminals {
            state: Fetch::Unavailable,
            error: Some("method not found".to_string()),
            ..Terminals::default()
        };
        assert!(!terminals.view().shown);
        terminals.pinned = true;
        assert_eq!(
            terminals.view().note,
            "Background terminals are not available: method not found"
        );
    }

    #[test]
    fn maps_server_terminals() {
        let terminal = BackgroundTerminal::from(ThreadBackgroundTerminal {
            item_id: "item".to_string(),
            process_id: "42".to_string(),
            command: "  sleep 30\n".to_string(),
            cwd: serde_json::from_value(serde_json::json!("/work/app"))
                .unwrap_or_else(|err| panic!("{err}")),
            os_pid: None,
            cpu_percent: None,
            rss_kb: None,
        });
        assert_eq!(terminal.process_id, "42");
        assert_eq!(terminal.command, "sleep 30");
        assert_eq!(terminal.cwd, "/work/app");
    }

    fn exec_started(process_id: Option<&str>) -> ServerNotification {
        ServerNotification::ItemStarted(ItemStartedNotification {
            item: ThreadItem::CommandExecution {
                sandbox_type: None,
                model_context: None,
                id: "c".to_string(),
                plugin_id: None,
                script_path: None,
                command: "sleep 30".to_string(),
                cwd: serde_json::from_value(serde_json::json!("/work"))
                    .unwrap_or_else(|err| panic!("{err}")),
                process_id: process_id.map(str::to_string),
                source: CommandExecutionSource::default(),
                status: CommandExecutionStatus::InProgress,
                command_actions: Vec::new(),
                aggregated_output: None,
                exit_code: None,
                duration_ms: None,
            },
            thread_id: "t".to_string(),
            turn_id: "u".to_string(),
            started_at_ms: 0,
        })
    }

    #[test]
    fn refreshes_on_process_activity_only() {
        assert!(may_change_terminals(&exec_started(Some("7"))));
        assert!(!may_change_terminals(&exec_started(None)));
        assert!(may_change_terminals(
            &ServerNotification::TerminalInteraction(TerminalInteractionNotification {
                thread_id: "t".to_string(),
                turn_id: "u".to_string(),
                item_id: "c".to_string(),
                process_id: "7".to_string(),
                stdin: "q".to_string(),
            })
        ));
        assert!(!may_change_terminals(&ServerNotification::ThreadClosed(
            ThreadClosedNotification {
                thread_id: "t".to_string(),
            }
        )));
    }

    #[test]
    fn locations_are_relative_to_the_thread_folder() {
        let thread_cwd = Path::new("/work/app");
        assert_eq!(terminal_location("/work/app", thread_cwd), "");
        assert_eq!(terminal_location("/work/app/web", thread_cwd), "web");
        assert_eq!(terminal_location("/srv/other", thread_cwd), "/srv/other");
    }

    #[test]
    fn confirmation_lists_commands() {
        let list: Vec<BackgroundTerminal> = (0..8)
            .map(|n| terminal(&n.to_string(), &format!("job {n}")))
            .collect();
        let text = command_list(&list);
        assert!(text.starts_with("• job 0\n• job 1"));
        assert!(text.ends_with("• job 5\n…and 2 more"));
        assert_eq!(
            command_list(&[terminal("1", "python - <<EOF\nprint(1)\nEOF")]),
            "• python - <<EOF …"
        );
        assert_eq!(running_summary(0), "");
        assert_eq!(running_summary(1), "1 running");
    }
}
