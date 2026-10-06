//! Startup timing marks for the performance budgets in GUI.md §7.
//!
//! Marks are always logged at debug level. With `CODEX_GUI_PERF=1` they are
//! also printed to stderr, which `gui/dev/measure.sh` parses.

use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();
static FIRST_FRAME_REPORTED: AtomicBool = AtomicBool::new(false);

const PERF_ENV_VAR: &str = "CODEX_GUI_PERF";

/// Records the process start time. Call first thing in `main`.
pub(crate) fn mark_process_start() {
    let _ = PROCESS_START.set(Instant::now());
}

/// Records a named milestone relative to process start.
pub(crate) fn mark(name: &str) {
    let Some(start) = PROCESS_START.get() else {
        return;
    };
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::debug!(milestone = name, elapsed_ms, "startup milestone");
    if std::env::var_os(PERF_ENV_VAR).is_some() {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("codex-gui perf: {name} {elapsed_ms:.1} ms");
        }
    }
}

/// Marks the first rendered frame of `window`.
///
/// Uses the rendering notifier where the renderer supports it (GPU
/// renderers) and otherwise the first event-loop turn after `show`, which
/// is when the software renderer paints.
pub(crate) fn mark_first_frame(window: &slint::Window) {
    let notifier = window.set_rendering_notifier(|state, _| {
        if matches!(state, slint::RenderingState::AfterRendering) {
            mark_first_frame_once();
        }
    });
    if notifier.is_err() {
        slint::Timer::single_shot(std::time::Duration::ZERO, mark_first_frame_once);
    }
}

fn mark_first_frame_once() {
    if !FIRST_FRAME_REPORTED.swap(true, Ordering::Relaxed) {
        mark("first-frame");
    }
}
