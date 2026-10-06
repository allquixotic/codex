//! Scripted UI automation for development and tests.
//!
//! Enabled only when `CODEX_GUI_AUTOMATION` names a JSON file containing an
//! array of steps. Each step runs on the UI thread after the previous one
//! finished, so a script can open a thread, send a message, wait for the turn
//! to finish, and save window snapshots for inspection:
//!
//! ```json
//! [
//!   {"wait_ready": 30000},
//!   {"new_thread": "/tmp/project"},
//!   {"send": "hello"},
//!   {"wait_idle": 60000},
//!   {"snapshot": "/tmp/after.png"},
//!   {"quit": true}
//! ]
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use slint::ComponentHandle;

use crate::app::AppController;

pub(crate) const AUTOMATION_ENV_VAR: &str = "CODEX_GUI_AUTOMATION";
const POLL: Duration = Duration::from_millis(100);

thread_local! {
    /// Input from `key` and `pointer` steps waiting to be dispatched.
    static INPUT_QUEUE: std::cell::RefCell<VecDeque<slint::platform::WindowEvent>> =
        const { std::cell::RefCell::new(VecDeque::new()) };
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Step {
    /// Sleep for this many milliseconds.
    Wait(u64),
    /// Wait until the embedded server is ready (timeout in ms).
    WaitReady(u64),
    /// Wait until the active thread has an id and no running turn.
    WaitIdle(u64),
    /// Open a new thread tab in this folder.
    NewThread(PathBuf),
    /// Start a thread in this folder through the folder-trust check.
    OpenFolder(PathBuf),
    /// Resume an existing thread id.
    Resume(String),
    /// Send text to the active thread.
    Send(String),
    /// Activate the tab at this index.
    SelectTab(usize),
    /// Run a tab menu action (rename, fork, compact, ...) on the active tab.
    TabAction(String),
    /// Run a global shortcut by name.
    Shortcut(String),
    /// Open the settings tab on a page.
    OpenSettings(String),
    /// Drive a settings control, e.g. `["select", "common", "web_search", "4"]`
    /// (see `AppController::settings_automation`).
    Settings(Vec<String>),
    /// Open a file tab.
    OpenFile(PathBuf),
    /// Open the unified diff stored in this file in a diff tab.
    OpenDiff(PathBuf),
    /// Run a file viewer command on the active file tab (see
    /// `AppController::files_automation`).
    FileCommand(String),
    /// Resize the window to logical `[width, height]`.
    Resize([f32; 2]),
    /// Save a PNG snapshot of the window.
    Snapshot(PathBuf),
    /// Accept (true) or cancel (false) the open dialog.
    Dialog(bool),
    /// Answer the visible approval card ("accept", "decline", "cancel",
    /// "skip", "submit", "escape", "key:<k>", "option:<n>",
    /// "select:<field>:<choice>", "text:<field>:<value>").
    Approval(String),
    /// Show a server request given as JSON (`{"method", "id", "params"}`).
    InjectRequest(serde_json::Value),
    /// Drive the composer: `["type", text]`, `["attach", path]`,
    /// `["accept", row]`, `["choose", picker, id]`, `["open", picker]`,
    /// `["plan"]`, `["send"]`.
    Composer(Vec<String>),
    /// Drive the Providers page: `["method", "2"]`, `["set", "api-key", "x"]`,
    /// `["apply"]`, ... (see `bedrock_automation`).
    Bedrock(Vec<String>),
    /// Open the "Send to tab" dialog from the active thread tab with this text.
    Forward(String),
    /// Finish the "Send to tab" dialog: "send", "queue", or "cancel".
    ForwardSubmit(String),
    /// Turn cross-tab messaging on or off for the active tab.
    XtabEnabled(bool),
    /// Write the active thread's cross-tab mailbox to the log.
    LogMailbox(bool),
    /// Record a perf milestone (printed with `CODEX_GUI_PERF=1`).
    Mark(String),
    /// Write a line to the log.
    Log(String),
    /// Transcript test hook (see `AppController::transcript_automation`).
    Transcript(String),
    /// Press and release a key on the focused widget: `"escape"`,
    /// `"return"`, `"tab"`, or literal text.
    Key(String),
    /// A left-button pointer event at logical `[x, y]`: `["press", x, y]`,
    /// `["move", x, y]`, or `["release", x, y]`.
    Pointer((String, f32, f32)),
    /// Open (true) or close (false) Help › About.
    About(bool),
    /// Write the tab titles (in order), the window title, and whether a
    /// dialog is open to the log.
    LogTabs(bool),
    /// Ask the OS to terminate the app, as Cmd+Q and Dock › Quit do (macOS).
    Terminate(bool),
    /// Press a status-overlay button: `"retry"` or `"use-embedded"`.
    Server(String),
    /// Switch the theme preference: `"system"`, `"light"`, or `"dark"`.
    Theme(crate::prefs::ThemeChoice),
    /// Write an environment variable of the process to the log.
    LogEnv(String),
    /// Drive the picker overlay: `["browse", path]`, `["input", text]`,
    /// `["enter"]`, `["activate", row]`, `["select", row]`,
    /// `["button", name]` (see `AppController::picker_automation`).
    Picker(Vec<String>),
    /// Restart the app-server (like Retry or a provider change).
    RestartServer(bool),
    /// Quit the app.
    Quit(bool),
}

pub(crate) struct Automation {
    steps: VecDeque<Step>,
    waiting: Option<(Step, Instant)>,
    timer: slint::Timer,
}

impl Automation {
    /// Loads the script named by `CODEX_GUI_AUTOMATION`, if set.
    pub(crate) fn from_env() -> Option<Self> {
        let path = std::env::var_os(AUTOMATION_ENV_VAR)?;
        let steps: Vec<Step> = match std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(anyhow::Error::from))
        {
            Ok(steps) => steps,
            Err(err) => {
                tracing::error!(%err, "invalid automation script");
                eprintln!("codex-gui: invalid automation script: {err}");
                return None;
            }
        };
        Some(Self {
            steps: steps.into(),
            waiting: None,
            timer: slint::Timer::default(),
        })
    }
}

impl AppController {
    /// Starts the automation script, if one was configured.
    pub(crate) fn automation_start(&mut self) {
        let Some(automation) = self.automation.as_ref() else {
            return;
        };
        automation
            .timer
            .start(slint::TimerMode::Repeated, POLL, || {
                crate::ui_thread::with_app(AppController::automation_tick);
            });
    }

    fn automation_tick(&mut self) {
        loop {
            let Some(automation) = self.automation.as_mut() else {
                return;
            };
            if let Some((step, started)) = automation.waiting.take() {
                if self.automation_wait_done(&step, started) {
                    continue;
                }
                if let Some(automation) = self.automation.as_mut() {
                    automation.waiting = Some((step, started));
                }
                return;
            }
            let Some(step) = automation.steps.pop_front() else {
                automation.timer.stop();
                return;
            };
            eprintln!("codex-gui automation: {step:?}");
            match step {
                Step::Wait(_) | Step::WaitReady(_) | Step::WaitIdle(_) => {
                    if let Some(automation) = self.automation.as_mut() {
                        automation.waiting = Some((step, Instant::now()));
                    }
                    return;
                }
                Step::NewThread(folder) => self.start_thread_in_folder(folder),
                Step::OpenFolder(folder) => self.start_thread_checked(folder),
                Step::Resume(thread_id) => self.open_thread(thread_id, None),
                Step::Send(text) => {
                    if let Some(index) = self.active_thread_index() {
                        self.send_user_input(index, vec![crate::session::text_input(text)]);
                    }
                }
                Step::SelectTab(index) => self.activate_tab(index),
                Step::TabAction(action) => {
                    if let Some(index) = self.active {
                        self.tab_action(index, &action);
                    }
                }
                Step::Shortcut(name) => {
                    self.window
                        .global::<crate::ui::AppState>()
                        .invoke_shortcut(name.into());
                }
                Step::OpenSettings(page) => self.open_settings(Some(&page)),
                Step::Settings(args) => self.settings_automation(&args),
                Step::OpenFile(path) => self.open_file_tab(path, None),
                Step::OpenDiff(path) => match std::fs::read_to_string(&path) {
                    Ok(diff) => self.open_diff_tab(crate::app::folder_label(&path), diff),
                    Err(err) => eprintln!(
                        "codex-gui automation: cannot read {}: {err}",
                        path.display()
                    ),
                },
                Step::FileCommand(command) => self.files_automation(&command),
                Step::Resize([width, height]) => {
                    self.window
                        .window()
                        .set_size(slint::LogicalSize::new(width, height));
                }
                Step::Snapshot(path) => self.automation_snapshot(&path),
                Step::Dialog(accept) => {
                    self.window
                        .global::<crate::ui::AppState>()
                        .invoke_dialog_finished(accept);
                }
                Step::Approval(action) => self.approvals_automation(&action),
                Step::InjectRequest(request) => self.approvals_automation_inject(request),
                Step::Composer(args) => self.composer_automation(&args),
                Step::Bedrock(args) => self.bedrock_automation(&args),
                Step::Forward(text) => {
                    if let Some(index) = self.active_thread_index() {
                        self.xtab_open_forward_dialog(index, text);
                    }
                }
                Step::ForwardSubmit(mode) => self.xtab_automation_submit(&mode),
                Step::XtabEnabled(enabled) => {
                    if let Some(index) = self.active_thread_index() {
                        self.xtab_set_enabled(index, enabled);
                    }
                }
                Step::LogMailbox(false) => {}
                Step::LogMailbox(true) => {
                    if let Some(thread_id) = self
                        .active_thread_index()
                        .and_then(|index| self.thread_tab(index))
                        .and_then(|thread| thread.thread_id.clone())
                    {
                        for entry in self.xtab_mailbox_for_thread(&thread_id) {
                            eprintln!(
                                "codex-gui automation: mailbox {:?} {} ({}) at {}: {}",
                                entry.direction,
                                entry.peer_title,
                                entry.peer_thread_id,
                                entry.timestamp,
                                entry.text
                            );
                        }
                    }
                }
                Step::Mark(name) => crate::perf::mark(&name),
                Step::Log(message) => eprintln!("codex-gui automation: {message}"),
                Step::Transcript(command) => self.transcript_automation(&command),
                Step::Key(key) => {
                    let text = key_text(&key);
                    self.automation_dispatch(vec![
                        slint::platform::WindowEvent::KeyPressed { text: text.clone() },
                        slint::platform::WindowEvent::KeyReleased { text },
                    ]);
                }
                Step::Pointer((kind, x, y)) => {
                    let position = slint::LogicalPosition::new(x, y);
                    let button = slint::platform::PointerEventButton::Left;
                    let event = match kind.as_str() {
                        "press" => {
                            Some(slint::platform::WindowEvent::PointerPressed { position, button })
                        }
                        "move" => Some(slint::platform::WindowEvent::PointerMoved { position }),
                        "release" => {
                            Some(slint::platform::WindowEvent::PointerReleased { position, button })
                        }
                        other => {
                            eprintln!("codex-gui automation: unknown pointer event {other}");
                            None
                        }
                    };
                    if let Some(event) = event {
                        self.automation_dispatch(vec![event]);
                    }
                }
                Step::About(open) => {
                    self.window
                        .global::<crate::ui::AppState>()
                        .set_about_open(open);
                }
                Step::LogTabs(false) | Step::Terminate(false) => {}
                Step::LogTabs(true) => {
                    let titles: Vec<String> = self
                        .tabs
                        .iter()
                        .map(|tab| match &tab.kind {
                            crate::app::TabKind::Thread(thread) => {
                                format!("{} [{:?}]", thread.title(), thread.phase)
                            }
                            crate::app::TabKind::File(file) => file.title(),
                            crate::app::TabKind::Settings => "Settings".to_string(),
                            crate::app::TabKind::NewTab => "New tab".to_string(),
                        })
                        .collect();
                    let state = self.window.global::<crate::ui::AppState>();
                    eprintln!(
                        "codex-gui automation: tabs {titles:?} active {:?} window title {:?} dialog open {} ({:?}) server {} warnings {:?}",
                        self.active,
                        state.get_window_title().as_str(),
                        state.get_dialog_open(),
                        state.get_dialog().title.as_str(),
                        state.get_server_status().as_str(),
                        self.warnings
                    );
                }
                Step::LogEnv(name) => {
                    eprintln!(
                        "codex-gui automation: env {name}={:?}",
                        std::env::var_os(&name)
                    );
                }
                Step::Theme(theme) => {
                    self.prefs.theme = theme;
                    self.apply_theme();
                    let ui_theme = self.window.global::<crate::ui::Theme>();
                    eprintln!(
                        "codex-gui automation: theme {theme:?} dark {} system-dark {}",
                        ui_theme.get_dark(),
                        ui_theme.get_system_dark()
                    );
                }
                Step::Server(action) => {
                    let state = self.window.global::<crate::ui::AppState>();
                    match action.as_str() {
                        "retry" => state.invoke_retry_server(),
                        "use-embedded" => state.invoke_use_embedded_server(),
                        other => eprintln!("codex-gui automation: unknown server action {other}"),
                    }
                    eprintln!(
                        "codex-gui automation: server {} {:?}",
                        state.get_server_status().as_str(),
                        state.get_server_message().as_str()
                    );
                }
                Step::Terminate(true) => {
                    #[cfg(target_os = "macos")]
                    crate::platform::request_app_termination();
                    #[cfg(not(target_os = "macos"))]
                    eprintln!("codex-gui automation: terminate is macOS only");
                }
                Step::Picker(args) => self.picker_automation(&args),
                Step::RestartServer(false) => {}
                Step::RestartServer(true) => self.backend.restart(),
                Step::Quit(false) => {}
                Step::Quit(true) => {
                    self.quitting = true;
                    let _ = slint::quit_event_loop();
                    return;
                }
            }
        }
    }

    /// Dispatches input on the next event-loop turn, like real input:
    /// callbacks that answer synchronously need the controller, which is
    /// borrowed while a step runs. Events keep their order across steps.
    fn automation_dispatch(&self, events: Vec<slint::platform::WindowEvent>) {
        let schedule = INPUT_QUEUE.with(|queue| {
            let mut queue = queue.borrow_mut();
            let was_empty = queue.is_empty();
            queue.extend(events);
            was_empty
        });
        if !schedule {
            return;
        }
        let window = self.window.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            let events: Vec<_> = INPUT_QUEUE.with(|queue| queue.borrow_mut().drain(..).collect());
            if let Some(window) = window.upgrade() {
                for event in events {
                    window.window().dispatch_event(event);
                }
            }
        });
    }

    fn automation_wait_done(&self, step: &Step, started: Instant) -> bool {
        let elapsed = started.elapsed();
        match step {
            Step::Wait(ms) => elapsed >= Duration::from_millis(*ms),
            Step::WaitReady(timeout) => {
                self.backend.is_ready() || timed_out(elapsed, *timeout, "server ready")
            }
            Step::WaitIdle(timeout) => {
                let idle = self
                    .active_thread_index()
                    .and_then(|index| self.thread_tab(index))
                    .is_some_and(|thread| {
                        thread.thread_id.is_some()
                            && thread.active_turn_id.is_none()
                            && !thread.is_busy()
                            && thread.phase != crate::app::ThreadPhase::Starting
                    });
                idle || timed_out(elapsed, *timeout, "thread idle")
            }
            _ => true,
        }
    }

    fn automation_snapshot(&self, path: &std::path::Path) {
        match self.window.window().take_snapshot() {
            Ok(buffer) => {
                let image = image::RgbaImage::from_raw(
                    buffer.width(),
                    buffer.height(),
                    buffer.as_bytes().to_vec(),
                );
                let result = match image {
                    Some(image) => image.save(path).map_err(anyhow::Error::from),
                    None => Err(anyhow::anyhow!("snapshot buffer had an unexpected size")),
                };
                if let Err(err) = result {
                    eprintln!("codex-gui automation: snapshot failed: {err}");
                }
            }
            Err(err) => eprintln!("codex-gui automation: snapshot failed: {err}"),
        }
    }
}

/// Slint key text for a `key` step.
fn key_text(key: &str) -> slint::SharedString {
    match key.to_ascii_lowercase().as_str() {
        "escape" | "esc" => slint::platform::Key::Escape.into(),
        "return" | "enter" => slint::platform::Key::Return.into(),
        "tab" => slint::platform::Key::Tab.into(),
        _ => key.into(),
    }
}

fn timed_out(elapsed: Duration, timeout_ms: u64, what: &str) -> bool {
    let done = elapsed >= Duration::from_millis(timeout_ms);
    if done {
        eprintln!("codex-gui automation: timed out waiting for {what}");
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_script_steps() -> serde_json::Result<()> {
        let steps: Vec<Step> = serde_json::from_str(
            r#"[{"wait_ready": 1000}, {"new_thread": "/tmp"}, {"send": "hi"},
                {"resize": [800, 600]}, {"snapshot": "/tmp/x.png"}, {"quit": true}]"#,
        )?;
        assert_eq!(steps.len(), 6);
        assert!(matches!(steps[3], Step::Resize([800.0, 600.0])));
        Ok(())
    }

    #[test]
    fn parses_input_steps() -> serde_json::Result<()> {
        let steps: Vec<Step> = serde_json::from_str(
            r#"[{"key": "escape"}, {"pointer": ["press", 10, 20.5]}, {"about": true},
                {"terminate": true}, {"log_tabs": true}]"#,
        )?;
        assert!(matches!(&steps[0], Step::Key(key) if key == "escape"));
        assert!(
            matches!(&steps[1], Step::Pointer((kind, x, y)) if kind == "press" && *x == 10.0 && *y == 20.5)
        );
        assert!(matches!(steps[2], Step::About(true)));
        assert!(matches!(steps[3], Step::Terminate(true)));
        assert_eq!(
            key_text("Escape"),
            slint::SharedString::from(slint::platform::Key::Escape)
        );
        assert_eq!(key_text("y"), slint::SharedString::from("y"));
        Ok(())
    }
}
