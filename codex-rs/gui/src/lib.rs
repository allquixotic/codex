// Async layout of the embedded app-server futures is deep (same as the TUI).
#![recursion_limit = "256"]

//! `codex-gui`: a native, tabbed desktop front end for Codex.
//!
//! The GUI embeds the app-server in-process (the same path the TUI uses) and
//! renders it with Slint. See `codex-rs/gui/README.md` for the architecture
//! and `docs/gui.md` for the user guide.

mod app;
mod approvals;
mod automation;
mod backend;
mod composer;
mod connection;
mod files;
mod info;
mod newtab;
mod notify;
mod perf;
mod platform;
mod prefs;
mod session;
mod settings;
mod shortcuts;
mod sidebar;
mod startup;
mod threads;
mod transcript;
mod ui;
mod ui_thread;
mod window_runtime;
mod xtab;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_utils_absolute_path::canonicalize_existing_preserving_symlinks;
use codex_utils_cli::CliConfigOverrides;
use slint::ComponentHandle;

use crate::app::AppController;
use crate::backend::Backend;
use crate::connection::ConnectionChoice;
use crate::prefs::LoadedPrefs;
use crate::prefs::Prefs;
use crate::prefs::RendererChoice;
use crate::startup::StartupOptions;

/// How long shutdown may take before the process exits anyway.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(50);
/// Shutdown budget when the user logs out: macOS waits only briefly before
/// it reports the app as not responding.
#[cfg(target_os = "macos")]
const SESSION_END_BUDGET: Duration = Duration::from_secs(5);

/// Command line for `codex-gui`.
#[derive(Debug, Parser)]
#[command(name = "codex-gui", version, about = "Codex desktop app")]
pub struct Cli {
    /// Folder to open a new thread in.
    pub folder: Option<PathBuf>,

    /// Resume an existing thread by id.
    #[arg(long = "resume", value_name = "THREAD_ID")]
    pub resume: Option<String>,

    /// Renderer: `auto` (OpenGL, falling back to the CPU renderer when
    /// OpenGL is unavailable), `software` (CPU only, least memory), or `gpu`
    /// (WGPU: Metal, D3D12, Vulkan; also allows software adapters such as
    /// Windows WARP).
    #[arg(long, value_name = "auto|software|gpu")]
    pub renderer: Option<String>,

    /// Use an app-server other than the embedded one: `embedded`, `unix://`
    /// (local daemon), `unix://PATH`, `ws://host:port`, or `wss://host:port`.
    /// Overrides the connection saved under Settings › Connection.
    #[arg(long = "remote", value_name = "ADDR")]
    pub remote: Option<String>,

    /// Environment variable that holds the auth token for `--remote`.
    #[arg(
        long = "remote-auth-token-env",
        value_name = "ENV_VAR",
        requires = "remote"
    )]
    pub remote_auth_token_env: Option<String>,

    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,
}

/// Prepares process-wide environment before any thread exists.
///
/// `main` passes this to the arg0 dispatcher, which runs it only for the
/// app itself (not for helper re-execs) and before it builds the Tokio
/// runtime: `std::env::set_var` is only sound while single-threaded. It
/// also handles `--help`, `--version` and argument errors, exiting early.
pub fn prepare_process_env() {
    perf::mark_process_start();
    let cli = parse_cli_or_exit();
    let codex_home = codex_utils_home_dir::find_codex_home().ok();
    let prefs = Prefs::load(codex_home.as_deref().map(AsRef::as_ref));
    let renderer = cli
        .renderer
        .as_deref()
        .and_then(parse_renderer)
        .unwrap_or(prefs.renderer);
    if renderer == RendererChoice::Gpu && std::env::var_os("SLINT_WGPU_CPU").is_none() {
        // Let femtovg-wgpu use CPU adapters such as D3D12 WARP on GPU-less VMs.
        // SAFETY: called before any other thread is spawned (see above).
        unsafe {
            std::env::set_var("SLINT_WGPU_CPU", "1");
        }
    }
    // SAFETY: still single-threaded (see above).
    #[cfg(target_os = "macos")]
    unsafe {
        platform::import_login_shell_path();
    }
    perf::mark("process-env-ready");
}

fn parse_renderer(value: &str) -> Option<RendererChoice> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(RendererChoice::Auto),
        "software" | "sw" => Some(RendererChoice::Software),
        "gpu" => Some(RendererChoice::Gpu),
        _ => None,
    }
}

/// Parses the command line, or shows help, the version, or the argument
/// error and exits.
fn parse_cli_or_exit() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => exit_with_cli_message(&err),
    }
}

/// Shows clap's output for `--help`, `--version` or an argument error and
/// exits with clap's exit code. On Windows the app has no console of its
/// own: the text goes to the console of the shell that started it, or to a
/// message box when there is none (a shortcut or Explorer).
fn exit_with_cli_message(err: &clap::Error) -> ! {
    #[cfg(windows)]
    let printed = platform::attach_parent_console() && err.print().is_ok();
    #[cfg(not(windows))]
    let printed = err.print().is_ok();
    if !printed {
        let informational = matches!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        show_message(&err.render().to_string(), informational);
    }
    std::process::exit(err.exit_code());
}

/// Entry point called on the process main thread by `main.rs`.
pub fn run_main(arg0_paths: Arg0DispatchPaths, rt: tokio::runtime::Handle) -> anyhow::Result<()> {
    let cli = parse_cli_or_exit();
    let result = run_app(cli, arg0_paths, rt);
    if let Err(err) = &result {
        report_fatal(&format!("{err:#}"));
    }
    result
}

fn run_app(
    cli: Cli,
    arg0_paths: Arg0DispatchPaths,
    rt: tokio::runtime::Handle,
) -> anyhow::Result<()> {
    let codex_home = codex_utils_home_dir::find_codex_home()
        .ok()
        .map(|home| home.to_path_buf());
    let LoadedPrefs { prefs, problems } =
        Prefs::load_checked(codex_home.as_deref(), /*backup_unreadable*/ true);
    let requested_renderer = cli
        .renderer
        .as_deref()
        .and_then(parse_renderer)
        .unwrap_or(prefs.renderer);
    let renderer = available_renderer(requested_renderer);
    select_backend(renderer)?;
    notify::init();

    let cli_overrides = cli
        .config_overrides
        .parse_overrides()
        .map_err(|err| anyhow::anyhow!("invalid -c override: {err}"))?;
    let folder = match cli.folder {
        // Keeps the path the user typed when it goes through a symlink, and
        // avoids Windows `\\?\` verbatim paths, which Explorer rejects.
        Some(folder) => Some(
            canonicalize_existing_preserving_symlinks(&folder)
                .with_context(|| format!("cannot open folder {}", folder.display()))?,
        ),
        None => None,
    };

    let window = ui::MainWindow::new().context("failed to create the main window")?;
    perf::mark("window-created");
    window.window().set_size(slint::LogicalSize::new(
        prefs.window_width,
        prefs.window_height,
    ));

    // A connection that cannot be used is reported in the window (with a
    // way back to the embedded server), never by refusing to start.
    let connection_choice = ConnectionChoice {
        cli_address: cli.remote.clone(),
        cli_token_env: cli.remote_auth_token_env.clone(),
        saved_address: prefs.app_server_address.clone(),
        saved_token_env: prefs.remote_auth_token_env.clone(),
    };
    let connection = connection_choice
        .resolve(codex_home.as_deref(), |var| std::env::var(var).ok())
        .map_err(|err| connection_choice.error_message(&err));
    let embedded_target = matches!(&connection, Ok(target) if target.is_embedded());
    let startup_cwd = folder.clone().or_else(|| {
        prefs
            .recent_folders
            .first()
            .cloned()
            .filter(|path| path.is_dir())
    });
    let backend = Backend::launch(
        rt.clone(),
        arg0_paths,
        StartupOptions {
            connection,
            cwd: startup_cwd,
            cli_overrides,
        },
    );

    let mut app = AppController::new(window.clone_strong(), backend.clone(), prefs, codex_home);
    app.startup_actions = startup_actions(folder, cli.resume);
    app.connection_choice = connection_choice;
    app.embedded_target = embedded_target;
    for problem in problems {
        app.push_warning(problem);
    }
    ui_thread::install(app);
    ui_thread::with_app_now(AppController::bind);

    #[cfg(target_os = "macos")]
    install_termination_handler(&rt, &backend);

    if let Err(err) = window.show() {
        #[cfg(target_os = "linux")]
        if renderer == RendererChoice::Auto {
            // OpenGL failed only now (for example no GLX over X forwarding);
            // the renderer is fixed for this process, so start a new one.
            tracing::warn!(%err, "OpenGL renderer failed; restarting with the software renderer");
            stop_backend(&rt, &backend, Vec::new());
            drop(ui_thread::take());
            relaunch_with_software_renderer()
                .with_context(|| format!("failed to show the main window: {err}"))?;
            return Ok(());
        }
        return Err(anyhow::anyhow!(
            "failed to show the main window: {err}. Try `--renderer software` (or Settings › Appearance › Renderer › Software)."
        ));
    }
    perf::mark("window-shown");
    perf::mark_first_frame(window.window());
    // The window system knows the OS light/dark setting only now.
    ui_thread::with_app_now(|app| app.sync_system_dark());
    slint::run_event_loop().context("event loop failed")?;
    let _ = window.hide();
    perf::mark("event-loop-exit");

    let thread_ids = ui_thread::take()
        .map(|app| app.open_thread_ids())
        .unwrap_or_default();
    stop_backend(&rt, &backend, thread_ids);
    perf::mark("shutdown-done");
    Ok(())
}

/// Unsubscribes threads and stops the server, waiting at most
/// [`SHUTDOWN_BUDGET`].
fn stop_backend(rt: &tokio::runtime::Handle, backend: &Backend, thread_ids: Vec<String>) {
    let shutdown = rt.block_on(async {
        tokio::time::timeout(SHUTDOWN_BUDGET, backend.shutdown(thread_ids)).await
    });
    if shutdown.is_err() {
        tracing::warn!("timed out waiting for the embedded app-server to stop");
    }
}

/// Cmd+Q, Dock › Quit and logout on macOS (see `platform.rs`).
#[cfg(target_os = "macos")]
fn install_termination_handler(rt: &tokio::runtime::Handle, backend: &Backend) {
    let rt = rt.clone();
    let backend = backend.clone();
    platform::install_termination_handler(
        || {
            // Leave the AppKit callback before showing a dialog.
            ui_thread::post(AppController::request_quit);
        },
        move || {
            // The session is ending: AppKit exits right after this returns,
            // so shut down here, within a short budget.
            let mut thread_ids = Vec::new();
            ui_thread::with_app_now(|app| thread_ids = app.prepare_for_exit());
            let shutdown = rt.block_on(async {
                tokio::time::timeout(SESSION_END_BUDGET, backend.shutdown(thread_ids)).await
            });
            if shutdown.is_err() {
                tracing::warn!("timed out stopping the app-server at logout");
            }
        },
    );
}

/// The renderer to use for `requested`: `Auto` skips OpenGL on Linux
/// machines without OpenGL libraries (containers, minimal VMs).
fn available_renderer(requested: RendererChoice) -> RendererChoice {
    #[cfg(target_os = "linux")]
    if requested == RendererChoice::Auto && !platform::opengl_available() {
        return RendererChoice::Software;
    }
    requested
}

/// Arguments for a new instance that uses the software renderer: any
/// `--renderer` option is replaced, and the new one goes first so it can
/// never follow `--`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn software_renderer_args(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut out: Vec<std::ffi::OsString> = vec!["--renderer".into(), "software".into()];
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            out.push(arg);
            out.extend(args);
            break;
        }
        if arg == "--renderer" {
            let _ = args.next();
            continue;
        }
        if arg
            .to_str()
            .is_some_and(|arg| arg.starts_with("--renderer="))
        {
            continue;
        }
        out.push(arg);
    }
    out
}

#[cfg(target_os = "linux")]
fn relaunch_with_software_renderer() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot find the codex-gui executable")?;
    let args = software_renderer_args(std::env::args_os().skip(1).collect());
    std::process::Command::new(exe)
        .args(args)
        .spawn()
        .context("failed to restart with the software renderer")?;
    Ok(())
}

/// What to open once the server is ready.
fn startup_actions(folder: Option<PathBuf>, resume: Option<String>) -> Vec<app::StartupAction> {
    let mut actions = Vec::new();
    if let Some(thread_id) = resume {
        actions.push(app::StartupAction::Resume(thread_id));
    } else if let Some(folder) = folder {
        actions.push(app::StartupAction::NewThread(folder));
    }
    actions
}

/// Chooses the Slint backend and renderer. Must run before any window exists.
///
/// Measured on macOS (Retina, one streaming thread): FemtoVG on OpenGL used
/// the least CPU (about 5x less than the software renderer while streaming,
/// and near zero when idle); the software renderer used the least memory;
/// FemtoVG on WGPU used the most of both, so it is only used on request,
/// where it can run on Windows' WARP software adapter. `Auto` falls back to
/// the software renderer when OpenGL is missing: Slint's own fallback covers
/// Windows (it probes for OpenGL 2), [`available_renderer`] covers Linux
/// machines without GL libraries, and `run_app` restarts with the software
/// renderer when OpenGL still fails when the window opens. `SLINT_BACKEND`
/// still overrides.
fn select_backend(renderer: RendererChoice) -> anyhow::Result<()> {
    let selector = slint::BackendSelector::new()
        .backend_name("winit".to_string())
        .with_winit_custom_application_handler(WindowEvents::default());
    let selector = if std::env::var_os("SLINT_BACKEND").is_some() {
        selector
    } else {
        match renderer {
            RendererChoice::Software => selector.renderer_name("software".to_string()),
            RendererChoice::Gpu => selector.renderer_name("femtovg-wgpu".to_string()),
            RendererChoice::Auto if platform::remote_desktop_session() => {
                selector.renderer_name("software".to_string())
            }
            RendererChoice::Auto => selector.renderer_name("femtovg".to_string()),
        }
    };
    selector
        .select()
        .map_err(|err| anyhow::anyhow!("failed to initialize the window system: {err}"))
}

/// Window-system events Slint does not expose: focus (desktop notifications
/// only fire in the background) and OS light/dark changes (`Theme`).
#[derive(Default)]
struct WindowEvents {
    pointer: Option<slint::LogicalPosition>,
}

impl slint::winit_030::CustomApplicationHandler for WindowEvents {
    fn window_event(
        &mut self,
        _event_loop: &slint::winit_030::winit::event_loop::ActiveEventLoop,
        _window_id: slint::winit_030::winit::window::WindowId,
        winit_window: Option<&slint::winit_030::winit::window::Window>,
        slint_window: Option<&slint::Window>,
        event: &slint::winit_030::winit::event::WindowEvent,
    ) -> slint::winit_030::EventResult {
        use slint::winit_030::winit::event::WindowEvent;
        use slint::winit_030::winit::window::Theme;

        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let scale = slint_window.map_or(1.0, slint::Window::scale_factor);
                self.pointer = Some(slint::LogicalPosition::new(
                    position.x as f32 / scale,
                    position.y as f32 / scale,
                ));
            }
            WindowEvent::CursorLeft { .. } => self.pointer = None,
            WindowEvent::MouseWheel { delta, .. } => {
                use slint::winit_030::winit::event::MouseScrollDelta;
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, dy) => *dy,
                    MouseScrollDelta::PixelDelta(delta) => delta.y as f32,
                };
                if let Some(position) = self.pointer {
                    ui_thread::with_app(move |app| app.transcript_before_scroll(position, dy));
                }
            }
            WindowEvent::MouseInput { state, button, .. }
                if *state == slint::winit_030::winit::event::ElementState::Pressed
                    && *button == slint::winit_030::winit::event::MouseButton::Left =>
            {
                if let Some(position) = self.pointer {
                    ui_thread::with_app(move |app| app.transcript_before_scrollbar_press(position));
                }
            }
            WindowEvent::Focused(focused) => {
                let focused = *focused;
                ui_thread::with_app(move |app| app.on_window_focus_changed(focused));
            }
            WindowEvent::ThemeChanged(theme) => {
                let dark = *theme == Theme::Dark;
                ui_thread::with_app(move |app| app.on_system_theme_changed(dark));
            }
            _ => {}
        }
        // Windows/RDP can lose a retained surface without reporting buffer age
        // changes. Dirty every existing frame; this schedules no periodic frames.
        if cfg!(windows) {
            if matches!(event, WindowEvent::RedrawRequested)
                && let Some(window) = slint_window
            {
                window_runtime::invalidate_frame(window);
            }
            if matches!(
                event,
                WindowEvent::Focused(true)
                    | WindowEvent::Occluded(false)
                    | WindowEvent::Resized(_)
                    | WindowEvent::ScaleFactorChanged { .. }
            ) && let Some(window) = winit_window
                && !window.is_minimized().unwrap_or(false)
            {
                window.request_redraw();
            }
        }
        slint::winit_030::EventResult::Propagate
    }
}

/// Shows a fatal error even when there is no console (Windows GUI
/// subsystem), and records it in the log file.
fn report_fatal(message: &str) {
    startup::log_startup_error(message);
    show_message(message, /*informational*/ false);
}

fn show_message(message: &str, informational: bool) {
    let _ = rfd::MessageDialog::new()
        .set_level(if informational {
            rfd::MessageLevel::Info
        } else {
            rfd::MessageLevel::Error
        })
        .set_title("Codex")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;

    #[test]
    fn parses_renderer_names() {
        assert_eq!(parse_renderer("Software"), Some(RendererChoice::Software));
        assert_eq!(parse_renderer("sw"), Some(RendererChoice::Software));
        assert_eq!(parse_renderer("gpu"), Some(RendererChoice::Gpu));
        assert_eq!(parse_renderer("auto"), Some(RendererChoice::Auto));
        assert_eq!(parse_renderer("vulkan"), None);
    }

    #[test]
    fn cli_accepts_folder_and_overrides() {
        let cli = Cli::parse_from([
            "codex-gui",
            "/tmp",
            "-c",
            "model=\"x\"",
            "--renderer",
            "software",
        ]);
        assert_eq!(cli.folder, Some(PathBuf::from("/tmp")));
        assert_eq!(cli.renderer.as_deref(), Some("software"));
        assert_eq!(
            cli.config_overrides.raw_overrides,
            vec!["model=\"x\"".to_string()]
        );
    }

    #[test]
    fn cli_errors_are_returned_instead_of_exiting() {
        // `try_parse` lets `exit_with_cli_message` show them without a console.
        let help = Cli::try_parse_from(["codex-gui", "--help"]).map(|_| ());
        assert!(matches!(
            help.map_err(|err| err.kind()),
            Err(clap::error::ErrorKind::DisplayHelp)
        ));
        let typo = Cli::try_parse_from(["codex-gui", "--renderr", "gpu"]).map(|_| ());
        assert!(matches!(
            typo.map_err(|err| err.kind()),
            Err(clap::error::ErrorKind::UnknownArgument)
        ));
    }

    #[test]
    fn software_relaunch_replaces_the_renderer_option() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            software_renderer_args(args(&["--renderer", "auto", "/repo"])),
            args(&["--renderer", "software", "/repo"])
        );
        assert_eq!(
            software_renderer_args(args(&["--renderer=auto", "-c", "x=1"])),
            args(&["--renderer", "software", "-c", "x=1"])
        );
        assert_eq!(
            software_renderer_args(args(&["--", "--renderer"])),
            args(&["--renderer", "software", "--", "--renderer"])
        );
    }

    #[test]
    fn auto_renderer_is_kept_where_opengl_is_assumed() {
        assert_eq!(
            available_renderer(RendererChoice::Software),
            RendererChoice::Software
        );
        assert_eq!(available_renderer(RendererChoice::Gpu), RendererChoice::Gpu);
        if !cfg!(target_os = "linux") {
            assert_eq!(
                available_renderer(RendererChoice::Auto),
                RendererChoice::Auto
            );
        }
    }
}
