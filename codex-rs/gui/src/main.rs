// No console window on Windows; errors are shown in a message box and logged.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::time::Duration;

/// Grace period for runtime tasks after the UI has shut the server down.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> anyhow::Result<()> {
    codex_build_info::initialize!();
    // arg0 handles helper re-execs (apply_patch, sandbox, fs helper), then
    // prepares the environment for the app while it is still
    // single-threaded, and hands the real main thread to the UI, which
    // macOS requires.
    codex_arg0::arg0_dispatch_or_else_keep_main_thread(
        RUNTIME_SHUTDOWN_TIMEOUT,
        codex_gui::prepare_process_env,
        codex_gui::run_main,
    )
}
