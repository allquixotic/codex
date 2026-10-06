//! UI-thread application state: tabs, routing of server events to features,
//! and the shared window chrome (tab strip, dialogs, toasts, status overlay).
//!
//! Feature modules extend [`AppController`] with their own `impl` blocks and
//! keep per-tab state inside [`ThreadTab`] or [`crate::files::FileTab`]. This
//! file owns the hooks that call into them; see `codex-rs/gui/README.md`
//! for the architecture and `docs/gui.md` for the user guide.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use codex_app_server_client::AppServerEvent;
use codex_app_server_client::legacy_core::config::Config;
use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::GetAccountParams;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::openai_models::ReasoningEffort;
use slint::ComponentHandle;
use slint::Model;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use crate::approvals::PendingApprovals;
use crate::backend::Backend;
use crate::backend::BackendError;
use crate::backend::ServerReady;
use crate::composer::ComposerDraft;
use crate::connection::ConnectionChoice;
use crate::connection::ConnectionTarget;
use crate::files::FileTab;
use crate::info::ThreadInfo;
use crate::newtab::NewTabController;
use crate::prefs::Prefs;
use crate::prefs::ThemeChoice;
use crate::settings::SettingsController;
use crate::settings::bedrock::BedrockController;
use crate::sidebar::SidebarController;
use crate::transcript::Transcript;
use crate::ui::AppState;
use crate::ui::DialogData;
use crate::ui::MainWindow;
use crate::ui::TabInfo;
use crate::ui::TabKindCode;
use crate::ui::TabStatusCode;
use crate::xtab::XtabController;

pub(crate) type TabId = u64;

/// Work requested on the command line, run once the server is ready.
#[derive(Clone, Debug)]
pub(crate) enum StartupAction {
    NewThread(PathBuf),
    Resume(String),
}

const TOAST_DURATION: Duration = Duration::from_millis(2200);

/// The app icon, embedded relative to this file so Cargo and Bazel builds
/// find it the same way (a Slint `@image-url` would embed the build
/// script's absolute path).
const APP_ICON_PNG: &[u8] = include_bytes!("../ui/assets/icon.png");
/// Window icons are shown at most this large (taskbar, Alt+Tab).
const WINDOW_ICON_SIZE: u32 = 256;

/// Banner shown while command-line work waits for a sign-in.
const SIGN_IN_WARNING: &str = "Sign in to Codex to continue: the folder or thread from the command line opens once you are signed in (Settings › Account).";

/// Lifecycle of the thread bound to a tab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThreadPhase {
    /// `thread/start` or `thread/resume` in flight.
    Starting,
    Idle,
    /// A turn is running.
    Running,
    /// A turn is blocked on an approval or a question.
    WaitingOnUser,
    /// Start/resume failed or the thread hit a system error.
    Error,
    /// The server closed or archived the thread.
    Closed,
}

/// A thread tab: one agent conversation bound to one folder.
pub(crate) struct ThreadTab {
    /// `None` until `thread/start` returns.
    pub(crate) thread_id: Option<String>,
    pub(crate) cwd: PathBuf,
    /// User-facing name (`thread/name/set`).
    pub(crate) name: Option<String>,
    /// First user message, used as a fallback title.
    pub(crate) preview: String,
    pub(crate) phase: ThreadPhase,
    pub(crate) active_turn_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) model_provider: Option<String>,
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) approval_policy: Option<AskForApproval>,
    /// Who reviews approval requests (the user or auto-review).
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
    pub(crate) sandbox: Option<SandboxPolicy>,
    pub(crate) last_error: Option<String>,
    /// Message from an `error` notification that a later `turn/completed`
    /// failure repeats; used to avoid showing it twice.
    pub(crate) reported_turn_error: Option<(String, String)>,
    /// Overrides the composer applies to the next turn (model, effort, ...).
    pub(crate) turn_overrides: crate::session::TurnOverrides,
    /// Inputs submitted before the thread finished starting.
    pub(crate) pending_inputs: Vec<crate::threads::PendingInput>,
    /// Whether cross-tab tools were registered for this thread.
    pub(crate) xtab_enabled: bool,
    pub(crate) transcript: Transcript,
    pub(crate) composer: ComposerDraft,
    pub(crate) approvals: PendingApprovals,
    pub(crate) info: ThreadInfo,
    /// Side chat, recap and worktree state (`crate::threads`).
    pub(crate) extras: crate::threads::ThreadExtras,
}

impl ThreadTab {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self {
            thread_id: None,
            cwd,
            name: None,
            preview: String::new(),
            phase: ThreadPhase::Starting,
            active_turn_id: None,
            model: None,
            model_provider: None,
            effort: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox: None,
            last_error: None,
            reported_turn_error: None,
            turn_overrides: crate::session::TurnOverrides::default(),
            pending_inputs: Vec::new(),
            xtab_enabled: false,
            transcript: Transcript::new(),
            composer: ComposerDraft::default(),
            approvals: PendingApprovals::default(),
            info: ThreadInfo::default(),
            extras: crate::threads::ThreadExtras::default(),
        }
    }

    pub(crate) fn title(&self) -> String {
        if let Some(name) = self.name.as_deref().filter(|name| !name.trim().is_empty()) {
            return name.trim().to_string();
        }
        let preview = crate::xtab::tools::preview_text(self.preview.trim());
        let preview = preview.trim();
        if !preview.is_empty() {
            return truncate_chars(preview.lines().next().unwrap_or(preview), 48);
        }
        folder_label(&self.cwd)
    }

    pub(crate) fn is_busy(&self) -> bool {
        matches!(
            self.phase,
            ThreadPhase::Running | ThreadPhase::WaitingOnUser
        )
    }
}

pub(crate) enum TabKind {
    Thread(Box<ThreadTab>),
    File(Box<FileTab>),
    Settings,
    NewTab,
}

pub(crate) struct Tab {
    pub(crate) id: TabId,
    pub(crate) kind: TabKind,
    /// Activity happened while the tab was in the background.
    pub(crate) unread: bool,
}

impl Tab {
    pub(crate) fn thread(&self) -> Option<&ThreadTab> {
        match &self.kind {
            TabKind::Thread(thread) => Some(thread),
            _ => None,
        }
    }

    pub(crate) fn thread_mut(&mut self) -> Option<&mut ThreadTab> {
        match &mut self.kind {
            TabKind::Thread(thread) => Some(thread),
            _ => None,
        }
    }

    fn info(&self) -> TabInfo {
        let (kind, title, tooltip, status) = match &self.kind {
            TabKind::Thread(thread) => (
                TabKindCode::Thread,
                thread.title(),
                thread.cwd.display().to_string(),
                match thread.phase {
                    ThreadPhase::Starting => TabStatusCode::Starting,
                    ThreadPhase::Idle => TabStatusCode::Idle,
                    ThreadPhase::Running => TabStatusCode::Running,
                    ThreadPhase::WaitingOnUser => TabStatusCode::Waiting,
                    ThreadPhase::Error => TabStatusCode::Error,
                    ThreadPhase::Closed => TabStatusCode::Closed,
                },
            ),
            TabKind::File(file) => (
                TabKindCode::File,
                file.title(),
                file.tooltip(),
                TabStatusCode::Idle,
            ),
            TabKind::Settings => (
                TabKindCode::Settings,
                "Settings".to_string(),
                String::new(),
                TabStatusCode::Idle,
            ),
            TabKind::NewTab => (
                TabKindCode::NewTab,
                "New tab".to_string(),
                String::new(),
                TabStatusCode::Idle,
            ),
        };
        TabInfo {
            id: i32::try_from(self.id).unwrap_or(i32::MAX),
            kind,
            title: title.into(),
            tooltip: tooltip.into(),
            status,
            unread: self.unread,
        }
    }
}

/// A modal dialog request shown by [`AppController::show_dialog`].
pub(crate) struct DialogRequest {
    pub(crate) title: String,
    pub(crate) message: String,
    /// `Some(initial)` shows a single-line text input.
    pub(crate) input: Option<String>,
    pub(crate) input_placeholder: String,
    pub(crate) accept_label: String,
    /// Empty hides the cancel button.
    pub(crate) cancel_label: String,
    pub(crate) destructive: bool,
}

impl DialogRequest {
    pub(crate) fn confirm(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            input: None,
            input_placeholder: String::new(),
            accept_label: "OK".to_string(),
            cancel_label: "Cancel".to_string(),
            destructive: false,
        }
    }

    pub(crate) fn prompt(title: impl Into<String>, initial: impl Into<String>) -> Self {
        Self {
            input: Some(initial.into()),
            ..Self::confirm(title, "")
        }
    }

    pub(crate) fn accept_label(mut self, label: impl Into<String>) -> Self {
        self.accept_label = label.into();
        self
    }

    pub(crate) fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
}

/// Called with `Some(input_text)` when accepted (empty when there is no
/// input field) and `None` when cancelled.
pub(crate) type DialogCallback = Box<dyn FnOnce(&mut AppController, Option<String>)>;

/// All UI-thread state. Lives in a thread-local; see [`crate::ui_thread`].
pub(crate) struct AppController {
    pub(crate) window: MainWindow,
    pub(crate) backend: Backend,
    pub(crate) prefs: Prefs,
    pub(crate) codex_home: Option<PathBuf>,
    /// Config of the running server; `None` while starting or after failure.
    pub(crate) config: Option<Arc<Config>>,
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: Option<usize>,
    pub(crate) warnings: Vec<String>,
    pub(crate) settings: SettingsController,
    pub(crate) bedrock: BedrockController,
    pub(crate) sidebar: SidebarController,
    pub(crate) newtab: NewTabController,
    pub(crate) xtab: XtabController,
    pub(crate) composer_shared: crate::composer::ComposerShared,
    pub(crate) approvals: crate::approvals::ApprovalsController,
    pub(crate) info_shared: crate::info::InfoShared,
    next_tab_id: TabId,
    tab_model: Rc<VecModel<TabInfo>>,
    warnings_model: Rc<VecModel<SharedString>>,
    dialog_callback: Option<DialogCallback>,
    toast_timer: slint::Timer,
    /// Set once the user confirmed quitting.
    pub(crate) quitting: bool,
    /// The palette was pinned to light or dark; returning to "System" must
    /// release it explicitly.
    palette_pinned: bool,
    /// Whether the OS window is focused (winit `Focused` events).
    window_focused: bool,
    /// Kept alive so X11/Wayland selections survive after copying.
    clipboard: Option<arboard::Clipboard>,
    /// Command-line requests, run on the first `on_server_ready` once the
    /// sign-in check passed.
    pub(crate) startup_actions: Vec<StartupAction>,
    /// Command-line requests waiting for the user to sign in.
    pending_sign_in: Vec<StartupAction>,
    /// Command line and saved settings the connection was resolved from.
    pub(crate) connection_choice: ConnectionChoice,
    /// The backend targets the embedded server (not a daemon, a remote
    /// server, or a connection that could not be resolved).
    pub(crate) embedded_target: bool,
    /// Scripted UI automation (`CODEX_GUI_AUTOMATION`), for tests.
    pub(crate) automation: Option<crate::automation::Automation>,
    /// Where the app-server runs ("Embedded (in-process)", daemon, remote).
    pub(crate) connection_label: String,
    /// Resolved global shortcuts (defaults plus `prefs.keymap`).
    pub(crate) keymap: crate::shortcuts::Keymap,
}

impl AppController {
    pub(crate) fn new(
        window: MainWindow,
        backend: Backend,
        prefs: Prefs,
        codex_home: Option<PathBuf>,
    ) -> Self {
        let tab_model = Rc::new(VecModel::<TabInfo>::default());
        let warnings_model = Rc::new(VecModel::<SharedString>::default());
        {
            let state = window.global::<AppState>();
            state.set_tabs(ModelRc::from(tab_model.clone()));
            state.set_warnings(ModelRc::from(warnings_model.clone()));
            state.set_sidebar_visible(prefs.sidebar_visible);
            state.set_info_visible(prefs.info_pane_visible);
            state.set_server_status("starting".into());
        }
        Self {
            window,
            backend,
            prefs,
            codex_home,
            config: None,
            tabs: Vec::new(),
            active: None,
            warnings: Vec::new(),
            settings: SettingsController::default(),
            bedrock: BedrockController::default(),
            sidebar: SidebarController::default(),
            newtab: NewTabController::default(),
            xtab: XtabController::default(),
            composer_shared: crate::composer::ComposerShared::default(),
            approvals: crate::approvals::ApprovalsController::default(),
            info_shared: crate::info::InfoShared::default(),
            next_tab_id: 1,
            tab_model,
            warnings_model,
            dialog_callback: None,
            toast_timer: slint::Timer::default(),
            quitting: false,
            palette_pinned: false,
            window_focused: true,
            clipboard: None,
            startup_actions: Vec::new(),
            pending_sign_in: Vec::new(),
            connection_choice: ConnectionChoice::default(),
            embedded_target: true,
            automation: crate::automation::Automation::from_env(),
            connection_label: String::new(),
            keymap: crate::shortcuts::Keymap::new(&std::collections::BTreeMap::new()).0,
        }
    }

    /// Wires Slint callbacks. Called once after the controller is installed
    /// in the UI-thread slot so callbacks can reach it.
    pub(crate) fn bind(&mut self) {
        self.bind_chrome();
        self.load_window_icon();
        self.apply_theme();
        self.reload_keymap();
        self.transcript_bind();
        self.composer_bind();
        self.approvals_bind();
        self.settings_bind();
        self.bedrock_bind();
        self.files_bind();
        self.sidebar_bind();
        self.info_bind();
        self.newtab_bind();
        self.xtab_bind();
        self.show_active();
        self.automation_start();
    }

    fn bind_chrome(&self) {
        let state = self.window.global::<AppState>();
        state.on_select_tab(|index| {
            crate::ui_thread::with_app(move |app| {
                if let Ok(index) = usize::try_from(index) {
                    app.activate_tab(index);
                }
            });
        });
        state.on_close_tab(|index| {
            crate::ui_thread::with_app(move |app| {
                if let Ok(index) = usize::try_from(index) {
                    app.request_close_tab(index);
                }
            });
        });
        state.on_new_tab(|| crate::ui_thread::with_app(AppController::open_new_tab_page));
        state.on_move_tab(|from, to| {
            crate::ui_thread::with_app(move |app| {
                if let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) {
                    app.move_tab(from, to);
                }
            });
        });
        state.on_tab_action(|index, action| {
            let action = action.to_string();
            crate::ui_thread::with_app(move |app| {
                if let Ok(index) = usize::try_from(index) {
                    app.tab_action(index, &action);
                }
            });
        });
        state.on_open_settings(|| crate::ui_thread::with_app(|app| app.open_settings(None)));
        state.on_dismiss_warning(|index| {
            crate::ui_thread::with_app(move |app| {
                if let Ok(index) = usize::try_from(index)
                    && index < app.warnings.len()
                {
                    app.warnings.remove(index);
                    app.refresh_warnings();
                }
            });
        });
        state.on_retry_server(|| crate::ui_thread::with_app(AppController::retry_server));
        state.on_use_embedded_server(|| {
            crate::ui_thread::with_app(AppController::use_embedded_server);
        });
        state.on_dialog_finished(|accepted| {
            crate::ui_thread::with_app(move |app| app.finish_dialog(accepted));
        });
        state.on_overlay_closed(|| crate::ui_thread::with_app(AppController::restore_focus));
        state.on_menu_action(|action| {
            let action = action.to_string();
            crate::ui_thread::with_app(move |app| app.menu_action(&action));
        });
        state.set_version(env!("CARGO_PKG_VERSION").into());
        state.on_toggle_sidebar(|| crate::ui_thread::with_app(AppController::toggle_sidebar));
        state.on_toggle_info(|| crate::ui_thread::with_app(AppController::toggle_info));
        state.on_shortcut(|name| {
            // Must answer synchronously; the shortcut runs on the next tick.
            let name = name.to_string();
            let handled = crate::shortcuts::is_known(&name);
            if handled {
                crate::ui_thread::with_app(move |app| app.run_shortcut(&name));
            }
            handled
        });
        state.on_key_pressed(|text, primary, shift, alt, secondary| {
            let press = crate::shortcuts::KeyPress {
                text: text.to_string(),
                primary,
                shift,
                alt,
                secondary,
            };
            let mut action = None;
            crate::ui_thread::with_app_now(|app| action = app.shortcut_for(&press));
            match action {
                Some(action) => {
                    crate::ui_thread::with_app(move |app| app.run_shortcut(&action));
                    true
                }
                None => false,
            }
        });

        self.window.window().on_close_requested(|| {
            let mut response = slint::CloseRequestResponse::HideWindow;
            crate::ui_thread::with_app_now(|app| {
                if !app.confirm_quit() {
                    response = slint::CloseRequestResponse::KeepWindowShown;
                }
            });
            response
        });
    }

    // ----- window state -----------------------------------------------------

    /// The OS window gained or lost focus.
    pub(crate) fn on_window_focus_changed(&mut self, focused: bool) {
        self.window_focused = focused;
        if focused && self.prefs.theme == ThemeChoice::System && self.palette_pinned {
            // Some platforms report scheme changes only through the window;
            // catch up when the user comes back.
            self.sync_system_dark();
        }
    }

    /// Whether the user is looking at the window: shown, not minimized, and
    /// focused. Desktop notifications are only useful otherwise.
    pub(crate) fn window_in_foreground(&self) -> bool {
        let window = self.window.window();
        window.is_visible() && !window.is_minimized() && self.window_focused
    }

    /// The OS switched between light and dark.
    pub(crate) fn on_system_theme_changed(&mut self, dark: bool) {
        self.window
            .global::<crate::ui::Theme>()
            .set_system_dark(dark);
    }

    /// Reads the OS light/dark setting from the window, when it is known.
    pub(crate) fn sync_system_dark(&self) {
        use slint::winit_030::WinitWindowAccessor;
        use slint::winit_030::winit::window::Theme as WinitTheme;

        let theme = self
            .window
            .window()
            .with_winit_window(slint::winit_030::winit::window::Window::theme)
            .flatten();
        if let Some(theme) = theme {
            self.window
                .global::<crate::ui::Theme>()
                .set_system_dark(theme == WinitTheme::Dark);
        }
    }

    // ----- server lifecycle -------------------------------------------------

    pub(crate) fn on_server_ready(&mut self, ready: ServerReady) {
        crate::perf::mark(if ready.restarted {
            "server-restarted"
        } else {
            "server-ready"
        });
        if ready.restarted {
            self.xtab_on_server_reset();
        }
        self.config = ready.config;
        self.connection_label = ready.connection;
        self.set_server_status("ready", "");
        self.settings_on_server_ready();
        self.bedrock_on_server_ready();
        self.sidebar_on_server_ready();
        self.composer_on_server_ready();
        self.files_on_server_ready(ready.restarted);
        self.info_on_server_ready();
        self.newtab_on_server_ready();
        if ready.restarted {
            self.approvals_reset();
            self.resume_open_threads();
        }
        // A provider change (for example Bedrock) may have removed the need
        // to sign in.
        let mut actions = std::mem::take(&mut self.startup_actions);
        actions.append(&mut self.pending_sign_in);
        self.run_startup_actions(actions);
    }

    /// Runs command-line work once the account check passed: a first run
    /// without a login asks the user to sign in instead of opening a thread
    /// whose first turn would fail.
    fn run_startup_actions(&mut self, actions: Vec<StartupAction>) {
        if actions.is_empty() {
            return;
        }
        self.backend.call(
            |request_id| ClientRequest::GetAccount {
                request_id,
                params: GetAccountParams {
                    refresh_token: false,
                },
            },
            move |app, result: Result<GetAccountResponse, BackendError>| {
                let needs_sign_in = match result {
                    Ok(response) => crate::newtab::sign_in_needed(&response),
                    Err(err) => {
                        // Let the thread itself report the problem.
                        tracing::warn!(error = %err.user_message(), "account/read failed");
                        false
                    }
                };
                if needs_sign_in {
                    app.pending_sign_in = actions;
                    app.push_warning(SIGN_IN_WARNING.to_string());
                    app.open_settings(Some("account"));
                    return;
                }
                app.dismiss_warning_text(SIGN_IN_WARNING);
                for action in actions {
                    match action {
                        StartupAction::NewThread(folder) => app.start_thread_checked(folder),
                        StartupAction::Resume(thread_id) => app.open_thread(thread_id, None),
                    }
                }
            },
        );
    }

    /// Re-checks command-line work that waits for a sign-in.
    fn retry_pending_sign_in(&mut self) {
        let actions = std::mem::take(&mut self.pending_sign_in);
        self.run_startup_actions(actions);
    }

    pub(crate) fn on_server_failed(&mut self, message: String) {
        self.config = None;
        self.xtab_on_server_reset();
        self.approvals_reset();
        self.set_server_status("failed", &message);
        // A daemon or remote server that cannot be reached (or a saved
        // connection that cannot be used) must not lock the user out.
        self.window
            .global::<AppState>()
            .set_server_can_use_embedded(!self.embedded_target);
        self.settings_on_server_failed(&message);
        self.bedrock_on_server_failed(&message);
        for tab in &mut self.tabs {
            if let Some(thread) = tab.thread_mut()
                && thread.phase == ThreadPhase::Starting
            {
                thread.phase = ThreadPhase::Error;
                thread.last_error = Some(message.clone());
            }
        }
        self.refresh_tabs();
    }

    fn set_server_status(&self, status: &str, message: &str) {
        let state = self.window.global::<AppState>();
        state.set_server_status(status.into());
        state.set_server_message(message.into());
    }

    /// "Retry" on the status overlay. The connection is resolved again, so
    /// a fix made under Settings › Connection (or a token variable that is
    /// now set) takes effect.
    fn retry_server(&mut self) {
        // Pick up a connection saved under Settings › Connection meanwhile.
        self.connection_choice.saved_address = self.prefs.app_server_address.clone();
        self.connection_choice.saved_token_env = self.prefs.remote_auth_token_env.clone();
        let resolved = self
            .connection_choice
            .resolve(self.codex_home.as_deref(), |var| std::env::var(var).ok());
        match resolved {
            Ok(target) => {
                self.set_server_status("restarting", "");
                let embedded = target.is_embedded();
                // Same target: a plain restart keeps the startup recipe.
                if embedded == self.embedded_target && self.connection_label == target.label() {
                    self.backend.restart();
                } else {
                    self.backend.reconnect(target);
                }
                self.embedded_target = embedded;
            }
            Err(err) => {
                self.embedded_target = false;
                let message = self.connection_choice.error_message(&err);
                self.on_server_failed(message);
            }
        }
    }

    /// "Use the embedded server" on the status overlay: stops using a
    /// daemon or remote server that cannot be reached, for this session.
    /// The saved connection is left alone (it may work when Codex is started
    /// from a terminal); Settings › Connection changes it for good.
    fn use_embedded_server(&mut self) {
        self.connection_choice.cli_address = Some("embedded".to_string());
        self.connection_choice.cli_token_env = None;
        self.embedded_target = true;
        self.set_server_status("restarting", "");
        self.backend.reconnect(ConnectionTarget::Embedded);
        self.toast("Using the embedded server for this session");
    }

    /// Removes a banner warning by its text.
    fn dismiss_warning_text(&mut self, text: &str) {
        let before = self.warnings.len();
        self.warnings.retain(|warning| warning != text);
        if self.warnings.len() != before {
            self.refresh_warnings();
        }
    }

    pub(crate) fn handle_server_events(&mut self, events: Vec<AppServerEvent>) {
        for event in events {
            match event {
                AppServerEvent::ServerNotification(notification) => {
                    self.handle_notification(*notification);
                }
                AppServerEvent::ServerRequest(request) => self.handle_server_request(*request),
                AppServerEvent::Lagged { skipped } => {
                    tracing::warn!(skipped, "app-server events were dropped; resyncing tabs");
                    for index in 0..self.tabs.len() {
                        if self.tabs[index].thread().is_some() {
                            self.transcript_on_lagged(index);
                        }
                    }
                    self.info_on_lagged();
                    self.sidebar_refresh();
                }
                AppServerEvent::Disconnected { message } => self.on_server_failed(message),
            }
        }
        self.refresh_tabs();
    }

    fn handle_notification(&mut self, notification: ServerNotification) {
        let tab = notification_thread_id(&notification)
            .and_then(|thread_id| self.tab_index_for_thread(thread_id));
        if let Some(index) = tab {
            self.thread_on_notification(index, &notification);
            self.transcript_on_notification(index, &notification);
            self.info_on_notification(index, &notification);
            if self.active != Some(index) && is_visible_activity(&notification) {
                self.tabs[index].unread = true;
            }
        }
        match &notification {
            ServerNotification::ConfigWarning(warning) => {
                let text = match &warning.details {
                    Some(details) => format!("{}: {details}", warning.summary),
                    None => warning.summary.clone(),
                };
                self.push_warning(text);
            }
            ServerNotification::Warning(warning) if warning.thread_id.is_none() => {
                self.push_warning(warning.message.clone());
            }
            ServerNotification::DeprecationNotice(notice) => {
                self.push_warning(notice.summary.clone());
            }
            ServerNotification::AccountLoginCompleted(_)
            | ServerNotification::AccountUpdated(_)
                if !self.pending_sign_in.is_empty() =>
            {
                self.retry_pending_sign_in();
            }
            _ => {}
        }
        self.threads_on_notification(&notification);
        self.approvals_on_notification(&notification);
        self.composer_on_notification(&notification);
        self.sidebar_on_notification(&notification);
        self.info_on_app_notification(&notification);
        self.newtab_on_notification(&notification);
        self.settings_on_notification(&notification);
        self.bedrock_on_notification(&notification);
        self.files_on_notification(&notification);
        self.xtab_on_notification(&notification);
    }

    fn handle_server_request(&mut self, request: ServerRequest) {
        match request {
            ServerRequest::DynamicToolCall { request_id, params } => {
                self.xtab_on_tool_call(request_id, params);
            }
            other => self.approvals_on_request(other),
        }
        self.refresh_tabs();
    }

    /// Generic per-thread state shared by every feature.
    fn thread_on_notification(&mut self, index: usize, notification: &ServerNotification) {
        let focused = self.window_in_foreground() && self.active == Some(index);
        let mut notify: Option<(String, String)> = None;
        let Some(thread) = self.tabs[index].thread_mut() else {
            return;
        };
        match notification {
            ServerNotification::TurnStarted(started) => {
                thread.active_turn_id = Some(started.turn.id.clone());
                thread.phase = ThreadPhase::Running;
                thread.last_error = None;
            }
            ServerNotification::TurnCompleted(completed) => {
                if thread.active_turn_id.as_deref() == Some(completed.turn.id.as_str()) {
                    thread.active_turn_id = None;
                }
                if thread.active_turn_id.is_none() {
                    thread.phase = ThreadPhase::Idle;
                }
                if completed.turn.status == TurnStatus::Failed {
                    thread.last_error = completed.turn.error.as_ref().map(|e| e.message.clone());
                }
                if !focused {
                    let body = match completed.turn.status {
                        TurnStatus::Failed => "Turn failed".to_string(),
                        TurnStatus::Interrupted => "Turn interrupted".to_string(),
                        _ => "Turn complete".to_string(),
                    };
                    notify = Some((thread.title(), body));
                }
            }
            ServerNotification::ThreadStatusChanged(changed) => match &changed.status {
                ThreadStatus::Active { active_flags } => {
                    let waiting = active_flags.iter().any(|flag| {
                        matches!(
                            flag,
                            ThreadActiveFlag::WaitingOnApproval
                                | ThreadActiveFlag::WaitingOnUserInput
                        )
                    });
                    // The approvals feature notifies when the request arrives.
                    thread.phase = if waiting {
                        ThreadPhase::WaitingOnUser
                    } else {
                        ThreadPhase::Running
                    };
                }
                ThreadStatus::Idle => {
                    if thread.phase != ThreadPhase::Starting {
                        thread.phase = ThreadPhase::Idle;
                    }
                    thread.active_turn_id = None;
                }
                ThreadStatus::SystemError => {
                    thread.phase = ThreadPhase::Error;
                }
                ThreadStatus::NotLoaded => {}
            },
            ServerNotification::ThreadNameUpdated(updated) => {
                thread.name.clone_from(&updated.thread_name);
            }
            ServerNotification::ThreadSettingsUpdated(updated) => {
                let settings = &updated.thread_settings;
                thread.model = Some(settings.model.clone());
                thread.model_provider = Some(settings.model_provider.clone());
                thread.effort.clone_from(&settings.effort);
                thread.approval_policy = Some(settings.approval_policy);
                thread.approvals_reviewer = Some(settings.approvals_reviewer);
                thread.sandbox = Some(settings.sandbox_policy.clone());
            }
            ServerNotification::ThreadClosed(_) | ServerNotification::ThreadArchived(_) => {
                thread.phase = ThreadPhase::Closed;
                thread.active_turn_id = None;
            }
            ServerNotification::ThreadUnarchived(_) if thread.phase == ThreadPhase::Closed => {
                thread.phase = ThreadPhase::Idle;
            }
            ServerNotification::Error(error) if !error.will_retry => {
                thread.last_error = Some(error.error.message.clone());
            }
            ServerNotification::ItemStarted(started)
                if thread.preview.is_empty()
                    && let codex_app_server_protocol::ThreadItem::UserMessage {
                        content, ..
                    } = &started.item =>
            {
                thread.preview = crate::threads::user_input_preview(content);
            }
            _ => {}
        }
        if let Some((title, body)) = notify {
            self.notify_desktop(&title, &body);
        }
    }

    // ----- tabs -------------------------------------------------------------

    pub(crate) fn push_tab(&mut self, kind: TabKind, activate: bool) -> usize {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let insert_at = self.active.map_or(self.tabs.len(), |active| active + 1);
        self.tabs.insert(
            insert_at,
            Tab {
                id,
                kind,
                unread: false,
            },
        );
        if let Some(active) = self.active
            && active >= insert_at
        {
            self.active = Some(active + 1);
        }
        if activate {
            self.activate_tab(insert_at);
        } else {
            self.refresh_tabs();
        }
        insert_at
    }

    /// Replaces the active "New tab" page with `kind`, or opens a new tab.
    pub(crate) fn replace_new_tab_page_or_push(&mut self, kind: TabKind) -> usize {
        if let Some(active) = self.active
            && matches!(self.tabs[active].kind, TabKind::NewTab)
        {
            self.tabs[active].kind = kind;
            self.show_active();
            self.refresh_tabs();
            return active;
        }
        self.push_tab(kind, /*activate*/ true)
    }

    pub(crate) fn tab_index_by_id(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    pub(crate) fn tab_index_for_thread(&self, thread_id: &str) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            tab.thread()
                .and_then(|thread| thread.thread_id.as_deref())
                .is_some_and(|id| id == thread_id)
        })
    }

    pub(crate) fn thread_tab(&self, index: usize) -> Option<&ThreadTab> {
        self.tabs.get(index).and_then(Tab::thread)
    }

    pub(crate) fn thread_tab_mut(&mut self, index: usize) -> Option<&mut ThreadTab> {
        self.tabs.get_mut(index).and_then(Tab::thread_mut)
    }

    /// Index of the active tab when it is a thread tab.
    pub(crate) fn active_thread_index(&self) -> Option<usize> {
        self.active.filter(|&index| {
            self.tabs
                .get(index)
                .is_some_and(|tab| tab.thread().is_some())
        })
    }

    pub(crate) fn activate_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.active != Some(index) {
            self.composer_stash();
        }
        self.active = Some(index);
        self.tabs[index].unread = false;
        self.show_active();
        self.refresh_tabs();
        self.transcript_trim_background();
    }

    /// Pushes the active tab's state into every feature's Slint global.
    pub(crate) fn show_active(&mut self) {
        let (kind, has_active) = match self.active.and_then(|index| self.tabs.get(index)) {
            Some(tab) => (
                match tab.kind {
                    TabKind::Thread(_) => TabKindCode::Thread,
                    TabKind::File(_) => TabKindCode::File,
                    TabKind::Settings => TabKindCode::Settings,
                    TabKind::NewTab => TabKindCode::NewTab,
                },
                true,
            ),
            None => (TabKindCode::NewTab, false),
        };
        {
            let state = self.window.global::<AppState>();
            state.set_active_kind(kind);
            state.set_has_active_tab(has_active);
            state.set_active_tab(
                self.active
                    .and_then(|index| i32::try_from(index).ok())
                    .unwrap_or(-1),
            );
        }
        self.transcript_show();
        self.composer_show();
        self.approvals_show();
        self.info_show();
        match kind {
            TabKindCode::File => self.files_show(),
            TabKindCode::Settings => self.settings_show(),
            TabKindCode::NewTab => self.newtab_show(),
            TabKindCode::Thread => {}
        }
        self.sync_window_title();
    }

    /// Keeps the OS window title (title bar, Dock/taskbar, window lists)
    /// in step with the active tab's title, which changes on rename and
    /// with the first message.
    fn sync_window_title(&self) {
        let title = window_title(
            self.active
                .and_then(|index| self.tabs.get(index))
                .map(|tab| tab.info().title.to_string())
                .as_deref(),
        );
        let state = self.window.global::<AppState>();
        if state.get_window_title() != title.as_str() {
            state.set_window_title(title.into());
        }
    }

    pub(crate) fn refresh_tabs(&self) {
        let infos: Vec<TabInfo> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let mut info = tab.info();
                // Requests routed here from sub-agents also need attention.
                if tab.thread().is_some() && self.approvals_pending_count(index) > 0 {
                    info.status = TabStatusCode::Waiting;
                }
                info
            })
            .collect();
        let unchanged = self.tab_model.row_count() == infos.len()
            && infos
                .iter()
                .enumerate()
                .all(|(index, info)| self.tab_model.row_data(index).as_ref() == Some(info));
        if !unchanged {
            self.tab_model.set_vec(infos);
        }
        self.sync_window_title();
        self.sidebar_on_tabs_changed();
    }

    pub(crate) fn open_new_tab_page(&mut self) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| matches!(tab.kind, TabKind::NewTab))
        {
            self.activate_tab(index);
            return;
        }
        self.push_tab(TabKind::NewTab, /*activate*/ true);
    }

    pub(crate) fn open_settings(&mut self, page: Option<&str>) {
        let index = match self
            .tabs
            .iter()
            .position(|tab| matches!(tab.kind, TabKind::Settings))
        {
            Some(index) => {
                self.activate_tab(index);
                index
            }
            None => self.push_tab(TabKind::Settings, /*activate*/ true),
        };
        let _ = index;
        if let Some(page) = page {
            self.settings_open_page(page);
        }
    }

    fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let active_id = self.active.map(|index| self.tabs[index].id);
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = active_id.and_then(|id| self.tab_index_by_id(id));
        self.show_active();
        self.refresh_tabs();
    }

    /// Closes a tab, asking first when its thread is mid-turn or the tab
    /// shows requests waiting for an answer.
    pub(crate) fn request_close_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let busy = tab.thread().is_some_and(ThreadTab::is_busy);
        let pending = self.approvals_close_warning(index);
        if busy || pending.is_some() {
            let id = tab.id;
            let (title, message, accept) = if busy {
                (
                    "Close running thread?",
                    "The agent is still working. Closing the tab stops the turn. The thread stays in your history.",
                    "Stop and close",
                )
            } else {
                ("Close tab with pending requests?", "", "Close")
            };
            let message = match pending {
                Some(pending) if message.is_empty() => pending,
                Some(pending) => format!("{message}\n\n{pending}"),
                None => message.to_string(),
            };
            self.show_dialog(
                DialogRequest::confirm(title, message)
                    .accept_label(accept)
                    .destructive(),
                Box::new(move |app, accepted| {
                    if accepted.is_some()
                        && let Some(index) = app.tab_index_by_id(id)
                    {
                        // Closing a busy tab stops its turn before detaching.
                        app.close_tab(index);
                    }
                }),
            );
            return;
        }
        self.close_tab(index);
    }

    pub(crate) fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.active == Some(index) {
            self.composer_stash();
        }
        let tab = self.tabs.remove(index);
        // Fixed before the close hooks run: they may move the closed tab's
        // requests to the tab that becomes active.
        self.active = match self.active {
            _ if self.tabs.is_empty() => None,
            Some(active) if active > index => Some(active - 1),
            Some(active) if active == index => Some(index.min(self.tabs.len() - 1)),
            other => other,
        };
        match tab.kind {
            TabKind::Thread(thread) => self.on_thread_tab_closed(*thread),
            TabKind::File(file) => self.files_on_tab_closed(*file),
            TabKind::Settings | TabKind::NewTab => {}
        }
        self.show_active();
        self.refresh_tabs();
    }

    pub(crate) fn tab_action(&mut self, index: usize, action: &str) {
        match action {
            "close-others" => {
                let Some(keep) = self.tabs.get(index).map(|tab| tab.id) else {
                    return;
                };
                let others: Vec<(TabId, bool)> = self
                    .tabs
                    .iter()
                    .filter(|tab| tab.id != keep)
                    .map(|tab| (tab.id, tab.thread().is_some_and(ThreadTab::is_busy)))
                    .collect();
                self.close_tabs(others);
            }
            action => self.thread_tab_action(index, action),
        }
    }

    /// Closes several tabs, given as `(id, busy)`. Idle tabs close at once;
    /// running ones are confirmed together in one dialog (one dialog per tab
    /// would cancel each other, since a new dialog dismisses the open one).
    fn close_tabs(&mut self, tabs: Vec<(TabId, bool)>) {
        let mut busy: Vec<TabId> = Vec::new();
        for (id, running) in tabs {
            if running {
                busy.push(id);
            } else if let Some(index) = self.tab_index_by_id(id) {
                self.close_tab(index);
            }
        }
        if busy.is_empty() {
            return;
        }
        let message = format!(
            "{}. Closing stops them; the threads stay in your history.",
            running_threads_phrase(busy.len())
        );
        self.show_dialog(
            DialogRequest::confirm(
                if busy.len() == 1 {
                    "Close running thread?"
                } else {
                    "Close running threads?"
                },
                message,
            )
            .accept_label("Stop and close")
            .destructive(),
            Box::new(move |app, accepted| {
                if accepted.is_none() {
                    return;
                }
                for id in busy {
                    if let Some(index) = app.tab_index_by_id(id) {
                        app.interrupt_tab(index);
                        app.close_tab(index);
                    }
                }
            }),
        );
    }

    /// Shows or hides the sidebar. When the window is too narrow to dock it,
    /// it opens as a drawer over the content instead (not saved).
    fn toggle_sidebar(&mut self) {
        let state = self.window.global::<AppState>();
        if !state.get_sidebar_fits() {
            let open = !state.get_sidebar_drawer_open();
            state.set_info_drawer_open(false);
            state.set_sidebar_drawer_open(open);
            return;
        }
        let visible = !state.get_sidebar_visible();
        state.set_sidebar_visible(visible);
        self.prefs.sidebar_visible = visible;
        self.save_prefs();
    }

    /// Shows or hides the info pane; a drawer in narrow windows.
    fn toggle_info(&mut self) {
        let state = self.window.global::<AppState>();
        if !state.get_info_fits() {
            let open = !state.get_info_drawer_open();
            state.set_sidebar_drawer_open(false);
            state.set_info_drawer_open(open);
            return;
        }
        let visible = !state.get_info_visible();
        state.set_info_visible(visible);
        self.prefs.info_pane_visible = visible;
        self.save_prefs();
    }

    /// Makes the info pane visible (`/status` and similar), docked when it
    /// fits and as a drawer otherwise.
    pub(crate) fn show_info_pane(&mut self) {
        let state = self.window.global::<AppState>();
        let shown = if state.get_info_fits() {
            state.get_info_visible()
        } else {
            state.get_info_drawer_open()
        };
        if !shown {
            self.toggle_info();
        }
    }

    /// Makes the sidebar visible (for example to search threads).
    pub(crate) fn show_sidebar(&mut self) {
        let state = self.window.global::<AppState>();
        let shown = if state.get_sidebar_fits() {
            state.get_sidebar_visible()
        } else {
            state.get_sidebar_drawer_open()
        };
        if !shown {
            self.toggle_sidebar();
        }
    }

    /// Whether the info pane is on screen, docked or as a drawer.
    pub(crate) fn info_pane_visible(&self) -> bool {
        let state = self.window.global::<AppState>();
        if state.get_info_fits() {
            state.get_info_visible()
        } else {
            state.get_info_drawer_open()
        }
    }

    /// Action bound to `press`, if the app should consume it now.
    fn shortcut_for(&self, press: &crate::shortcuts::KeyPress) -> Option<String> {
        // The Keyboard settings page is capturing a new binding.
        if self.settings_is_recording_shortcut() {
            return None;
        }
        let action = self.keymap.lookup(press)?;
        // Escape only interrupts a running turn; otherwise widgets keep it.
        if action == "escape"
            && !self
                .active_thread_index()
                .and_then(|index| self.thread_tab(index))
                .is_some_and(ThreadTab::is_busy)
        {
            return None;
        }
        Some(action.to_string())
    }

    /// Rebuilds the keymap from preferences, reporting invalid bindings.
    pub(crate) fn reload_keymap(&mut self) {
        let (keymap, errors) = crate::shortcuts::Keymap::new(&self.prefs.keymap);
        self.keymap = keymap;
        // Drop warnings about bindings the Keyboard settings page has fixed.
        self.warnings
            .retain(|warning| !warning.starts_with("keymap: "));
        self.refresh_warnings();
        for error in errors {
            self.push_warning(error);
        }
    }

    fn run_shortcut(&mut self, name: &str) {
        match name {
            "new-tab" => self.open_new_tab_page(),
            "close-tab" => {
                if let Some(index) = self.active {
                    self.request_close_tab(index);
                }
            }
            "next-tab" | "prev-tab" => {
                let count = self.tabs.len();
                if count > 0 {
                    let current = self.active.unwrap_or(0);
                    let next = if name == "next-tab" {
                        (current + 1) % count
                    } else {
                        (current + count - 1) % count
                    };
                    self.activate_tab(next);
                }
            }
            "settings" => self.open_settings(None),
            "open-file" => self.files_pick_and_open(),
            "toggle-sidebar" => self.toggle_sidebar(),
            "toggle-info" => self.toggle_info(),
            "escape" => {
                if self.approvals_on_escape() {
                    return;
                }
                if let Some(index) = self.active_thread_index()
                    && self.thread_tab(index).is_some_and(ThreadTab::is_busy)
                {
                    self.interrupt_tab(index);
                }
            }
            other => {
                if let Some(number) = other.strip_prefix("tab-")
                    && let Ok(number) = number.parse::<usize>()
                    && number >= 1
                {
                    let index = if number == 9 {
                        self.tabs.len().saturating_sub(1)
                    } else {
                        number - 1
                    };
                    self.activate_tab(index);
                }
            }
        }
    }

    fn menu_action(&mut self, action: &str) {
        match action {
            "quit" => self.request_quit(),
            "open-file" => self.files_pick_and_open(),
            "open-logs" => match self.settings_log_dir() {
                // Logs are written by this process, even with a remote server.
                Some(dir) => {
                    let _ = std::fs::create_dir_all(&dir);
                    self.open_local_folder(&dir);
                }
                None => self.toast("The log folder is not known (CODEX_HOME could not be found)."),
            },
            "keyboard" => self.open_settings(Some("keyboard")),
            "feedback" => self.open_settings(Some("feedback")),
            "docs" => {
                let _ = webbrowser::open("https://developers.openai.com/codex");
            }
            "fork" | "compact" | "review" | "export" => {
                if let Some(index) = self.active_thread_index() {
                    self.thread_tab_action(index, action);
                }
            }
            other => self.run_shortcut(other),
        }
    }

    // ----- chrome helpers ---------------------------------------------------

    /// Shows a transient message near the bottom of the window.
    pub(crate) fn toast(&mut self, message: impl Into<String>) {
        let message: String = message.into();
        self.window.global::<AppState>().set_toast(message.into());
        self.toast_timer
            .start(slint::TimerMode::SingleShot, TOAST_DURATION, || {
                crate::ui_thread::with_app(|app| {
                    app.window
                        .global::<AppState>()
                        .set_toast(SharedString::new());
                });
            });
    }

    /// Adds a dismissible banner warning (deduplicated).
    pub(crate) fn push_warning(&mut self, warning: String) {
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
            self.refresh_warnings();
        }
    }

    fn refresh_warnings(&self) {
        self.warnings_model.set_vec(
            self.warnings
                .iter()
                .map(|warning| SharedString::from(warning.as_str()))
                .collect::<Vec<_>>(),
        );
    }

    /// Shows a modal dialog. A dialog already open is cancelled first.
    pub(crate) fn show_dialog(&mut self, request: DialogRequest, on_result: DialogCallback) {
        if let Some(previous) = self.dialog_callback.take() {
            previous(self, None);
        }
        let state = self.window.global::<AppState>();
        state.set_dialog_input(request.input.clone().unwrap_or_default().into());
        state.set_dialog(DialogData {
            id: SharedString::new(),
            title: request.title.into(),
            message: request.message.into(),
            input_visible: request.input.is_some(),
            input_text: request.input.unwrap_or_default().into(),
            input_placeholder: request.input_placeholder.into(),
            accept_label: request.accept_label.into(),
            cancel_label: request.cancel_label.into(),
            destructive: request.destructive,
        });
        state.set_dialog_open(true);
        self.dialog_callback = Some(on_result);
    }

    fn finish_dialog(&mut self, accepted: bool) {
        let state = self.window.global::<AppState>();
        state.set_dialog_open(false);
        let input = state.get_dialog_input().to_string();
        if let Some(callback) = self.dialog_callback.take() {
            callback(self, accepted.then_some(input));
        }
        // The callback may have opened another dialog.
        if !self.window.global::<AppState>().get_dialog_open() {
            self.restore_focus();
        }
    }

    /// Gives keyboard focus back to the active thread's composer after a
    /// modal overlay (which held the focus) closed.
    fn restore_focus(&mut self) {
        if self.active_thread_index().is_some() {
            self.composer_focus();
        }
    }

    /// Sets the window icon (Windows taskbar, X11 window lists; macOS uses
    /// the app bundle's icon). Decoded off the UI thread.
    fn load_window_icon(&self) {
        if cfg!(target_os = "macos") {
            return;
        }
        self.backend.runtime().spawn_blocking(|| {
            match decode_icon(APP_ICON_PNG, WINDOW_ICON_SIZE) {
                Some(buffer) => {
                    crate::ui_thread::post(move |app| {
                        app.window.set_app_icon(slint::Image::from_rgba8(buffer));
                    });
                }
                None => tracing::warn!("could not decode the window icon"),
            }
        });
    }

    /// Opens a folder on this machine in Finder, Explorer, or the desktop's
    /// file manager (never a web browser). The launcher runs off the UI
    /// thread.
    pub(crate) fn open_local_folder(&mut self, path: &std::path::Path) {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        };
        let path = path.to_path_buf();
        self.backend.runtime().spawn_blocking(move || {
            match std::process::Command::new(program).arg(&path).spawn() {
                // Reap the child; the exit status is not meaningful (explorer
                // reports 1 on success).
                Ok(mut child) => {
                    let _ = child.wait();
                }
                Err(err) => {
                    let message = format!("Could not open {}: {err}", path.display());
                    crate::ui_thread::post(move |app| app.toast(message));
                }
            }
        });
    }

    pub(crate) fn copy_to_clipboard(&mut self, text: &str) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let result = match self.clipboard.as_mut() {
            Some(clipboard) => clipboard.set_text(text),
            None => Err(arboard::Error::ClipboardNotSupported),
        };
        match result {
            Ok(()) => self.toast("Copied"),
            Err(err) => self.toast(format!("Copy failed: {err}")),
        }
    }

    /// Reads text from the system clipboard.
    pub(crate) fn clipboard_text(&mut self) -> Option<String> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()?.get_text().ok()
    }

    pub(crate) fn save_prefs(&self) {
        if let Some(codex_home) = self.codex_home.as_deref()
            && let Err(err) = self.prefs.save(codex_home)
        {
            tracing::warn!(%err, "failed to save gui.json");
        }
    }

    pub(crate) fn apply_theme(&mut self) {
        match self.prefs.theme {
            ThemeChoice::System => {
                self.window.invoke_apply_theme(0);
                if self.palette_pinned {
                    // Release the light/dark override: the palette follows
                    // the system again, and so does `Theme.dark` through
                    // `Theme.system-dark`, read from the window now and
                    // updated when the OS scheme changes.
                    self.sync_system_dark();
                    self.window.invoke_follow_system_palette();
                }
            }
            ThemeChoice::Light | ThemeChoice::Dark => {
                let mode = if self.prefs.theme == ThemeChoice::Dark {
                    2
                } else {
                    1
                };
                self.window.invoke_apply_theme(mode);
                self.palette_pinned = true;
            }
        }
        let theme = self.window.global::<crate::ui::Theme>();
        theme.set_font_size(self.prefs.font_size);
        theme.set_mono_font(default_mono_font().into());
    }

    /// File › Quit, Cmd+Q and Dock › Quit on macOS: quits after the
    /// confirmation [`AppController::confirm_quit`] asks for, leaving the
    /// event loop so the normal shutdown runs.
    pub(crate) fn request_quit(&mut self) {
        if self.confirm_quit() {
            let _ = slint::quit_event_loop();
        }
    }

    /// Last UI work before the process ends without leaving the event loop
    /// (macOS logout): saves the window size and stops running turns.
    /// Returns the thread ids to unsubscribe.
    #[cfg(target_os = "macos")]
    pub(crate) fn prepare_for_exit(&mut self) -> Vec<String> {
        if !self.quitting {
            self.quitting = true;
            self.remember_window_size();
            self.interrupt_running_tabs();
        }
        self.open_thread_ids()
    }

    #[cfg(target_os = "macos")]
    fn interrupt_running_tabs(&mut self) {
        for index in 0..self.tabs.len() {
            if self.thread_tab(index).is_some_and(ThreadTab::is_busy) {
                self.interrupt_tab(index);
            }
        }
    }

    /// Returns true when the window may close now.
    fn confirm_quit(&mut self) -> bool {
        if self.quitting {
            return true;
        }
        let running = self
            .tabs
            .iter()
            .filter(|tab| tab.thread().is_some_and(ThreadTab::is_busy))
            .count();
        if running == 0 {
            self.quitting = true;
            self.remember_window_size();
            return true;
        }
        self.show_dialog(
            DialogRequest::confirm(
                "Quit Codex?",
                format!(
                    "{}. Quitting stops them; the threads stay in your history.",
                    running_threads_phrase(running)
                ),
            )
            .accept_label("Quit")
            .destructive(),
            Box::new(|app, accepted| {
                if accepted.is_some() {
                    app.quitting = true;
                    app.remember_window_size();
                    app.toast("Stopping running threads…");
                    app.stop_running_turns_then(|_| {
                        let _ = slint::quit_event_loop();
                    });
                }
            }),
        );
        false
    }
    fn remember_window_size(&mut self) {
        let size = self.window.window().size();
        let scale = self.window.window().scale_factor();
        if scale > 0.0 {
            self.prefs.window_width = size.width as f32 / scale;
            self.prefs.window_height = size.height as f32 / scale;
            self.save_prefs();
        }
    }

    /// Thread ids to unsubscribe when the app exits.
    pub(crate) fn open_thread_ids(&self) -> Vec<String> {
        self.tabs
            .iter()
            .filter_map(|tab| tab.thread().and_then(|thread| thread.thread_id.clone()))
            .collect()
    }
}

/// Thread id a notification belongs to, when it is thread-scoped.
pub(crate) fn notification_thread_id(notification: &ServerNotification) -> Option<&str> {
    use ServerNotification as N;
    let id = match notification {
        N::Error(n) => &n.thread_id,
        N::ThreadStarted(n) => &n.thread.id,
        N::ThreadStatusChanged(n) => &n.thread_id,
        N::ThreadArchived(n) => &n.thread_id,
        N::ThreadDeleted(n) => &n.thread_id,
        N::ThreadUnarchived(n) => &n.thread_id,
        N::ThreadClosed(n) => &n.thread_id,
        N::ThreadReverted(n) => &n.thread_id,
        N::ThreadNameUpdated(n) => &n.thread_id,
        N::ThreadGoalUpdated(n) => &n.thread_id,
        N::ThreadGoalCleared(n) => &n.thread_id,
        N::ThreadQueueChanged(n) => &n.thread_id,
        N::ThreadSettingsUpdated(n) => &n.thread_id,
        N::ThreadTokenUsageUpdated(n) => &n.thread_id,
        N::TurnStarted(n) => &n.thread_id,
        N::TurnCompleted(n) => &n.thread_id,
        N::HookStarted(n) => &n.thread_id,
        N::HookCompleted(n) => &n.thread_id,
        N::TurnDiffUpdated(n) => &n.thread_id,
        N::TurnPlanUpdated(n) => &n.thread_id,
        N::ItemStarted(n) => &n.thread_id,
        N::ItemCompleted(n) => &n.thread_id,
        N::ItemGuardianApprovalReviewStarted(n) => &n.thread_id,
        N::ItemGuardianApprovalReviewCompleted(n) => &n.thread_id,
        N::AgentMessageDelta(n) => &n.thread_id,
        N::PlanDelta(n) => &n.thread_id,
        N::CommandExecutionOutputDelta(n) => &n.thread_id,
        N::TerminalInteraction(n) => &n.thread_id,
        N::FileChangePatchUpdated(n) => &n.thread_id,
        N::ServerRequestResolved(n) => &n.thread_id,
        N::McpToolCallProgress(n) => &n.thread_id,
        N::ReasoningSummaryTextDelta(n) => &n.thread_id,
        N::ReasoningSummaryPartAdded(n) => &n.thread_id,
        N::ReasoningTextDelta(n) => &n.thread_id,
        N::ContextCompacted(n) => &n.thread_id,
        N::ModelRerouted(n) => &n.thread_id,
        N::ModelVerification(n) => &n.thread_id,
        N::AuthRecoveryStarted(n) => &n.thread_id,
        N::AuthRecoveryCompleted(n) => &n.thread_id,
        N::ModelSafetyBufferingUpdated(n) => &n.thread_id,
        N::GuardianWarning(n) => &n.thread_id,
        N::Warning(n) => return n.thread_id.as_deref(),
        N::McpServerStatusUpdated(n) => return n.thread_id.as_deref(),
        _ => return None,
    };
    Some(id.as_str())
}

/// Whether a notification represents something the user would want to see
/// (drives the unread dot on background tabs).
fn is_visible_activity(notification: &ServerNotification) -> bool {
    matches!(
        notification,
        ServerNotification::ItemCompleted(_)
            | ServerNotification::TurnCompleted(_)
            | ServerNotification::Error(_)
    )
}

pub(crate) fn folder_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Decodes a PNG into an RGBA buffer of at most `max_size` pixels square.
fn decode_icon(png: &[u8], max_size: u32) -> Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;
    let image = if image.width() > max_size || image.height() > max_size {
        image.resize(max_size, max_size, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let image = image.into_rgba8();
    Some(slint::SharedPixelBuffer::clone_from_slice(
        image.as_raw(),
        image.width(),
        image.height(),
    ))
}

/// OS window title for the active tab's title.
fn window_title(tab_title: Option<&str>) -> String {
    match tab_title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => format!("{title} — Codex"),
        None => "Codex".to_string(),
    }
}

/// "1 thread is still working" / "3 threads are still working".
fn running_threads_phrase(count: usize) -> String {
    if count == 1 {
        "1 thread is still working".to_string()
    } else {
        format!("{count} threads are still working")
    }
}

/// Monospace family for code, diffs and terminal output.
fn default_mono_font() -> String {
    if cfg!(target_os = "macos") {
        "Menlo".to_string()
    } else if cfg!(windows) {
        "Consolas".to_string()
    } else {
        static FAMILY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        FAMILY.get_or_init(linux_mono_font).clone()
    }
}

/// Families tried when fontconfig has no `monospace` match, most common
/// first.
const LINUX_MONO_FALLBACKS: &[&str] = &[
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
    "Adwaita Mono",
];

/// The system's monospace family. Slint looks fonts up by family name only
/// (no generic `monospace`), and distributions ship different defaults, so
/// ask fontconfig (through Slint's font collection) for its `monospace`
/// match, then try the common families.
fn linux_mono_font() -> String {
    use slint::fontique_011::fontique::GenericFamily;

    let mut collection = slint::fontique_011::shared_collection();
    let monospace = collection.generic_families(GenericFamily::Monospace).next();
    let mut installed: Vec<String> = monospace
        .and_then(|id| collection.family_name(id).map(str::to_string))
        .into_iter()
        .collect();
    for name in LINUX_MONO_FALLBACKS {
        if collection.family_by_name(name).is_some() {
            installed.push((*name).to_string());
        }
    }
    pick_mono_font(&installed)
}

/// First installed candidate, or the most common family when nothing is
/// known (fonts may still appear through fallbacks).
fn pick_mono_font(installed: &[String]) -> String {
    installed
        .first()
        .cloned()
        .unwrap_or_else(|| LINUX_MONO_FALLBACKS[0].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn thread_title_prefers_name_then_preview_then_folder() {
        let mut thread = ThreadTab::new(PathBuf::from("/work/repo-a"));
        assert_eq!(thread.title(), "repo-a");
        thread.preview = "Fix the flaky test in auth\nmore".to_string();
        assert_eq!(thread.title(), "Fix the flaky test in auth");
        thread.name = Some("  Auth fixes ".to_string());
        assert_eq!(thread.title(), "Auth fixes");
    }

    #[test]
    fn truncate_chars_adds_ellipsis() {
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("abc", 4), "abc");
    }

    #[test]
    fn window_title_follows_the_tab_title() {
        assert_eq!(
            window_title(Some("Fix auth tests")),
            "Fix auth tests — Codex"
        );
        assert_eq!(window_title(Some("  ")), "Codex");
        assert_eq!(window_title(None), "Codex");
    }

    #[test]
    fn window_icon_is_embedded_and_scaled_down() {
        let icon = decode_icon(APP_ICON_PNG, WINDOW_ICON_SIZE);
        assert_eq!(
            icon.map(|buffer| (buffer.width(), buffer.height())),
            Some((WINDOW_ICON_SIZE, WINDOW_ICON_SIZE))
        );
    }

    #[test]
    fn running_threads_phrase_counts() {
        assert_eq!(running_threads_phrase(1), "1 thread is still working");
        assert_eq!(running_threads_phrase(3), "3 threads are still working");
    }

    #[test]
    fn mono_font_prefers_the_first_installed_family() {
        assert_eq!(
            pick_mono_font(&["Noto Sans Mono".to_string(), "DejaVu Sans Mono".to_string()]),
            "Noto Sans Mono"
        );
        assert_eq!(pick_mono_font(&[]), "DejaVu Sans Mono");
    }
}
