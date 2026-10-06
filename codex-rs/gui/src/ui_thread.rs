//! Access to the UI-thread application state from anywhere in the process.
//!
//! Slint models and component handles are `!Send`, so every piece of UI state
//! lives in one [`AppController`] owned by the main thread. Tokio tasks reach
//! it by posting closures with [`post`]; Slint callbacks reach it with
//! [`with_app`]. Both run the closure on the UI thread with exclusive access.
//!
//! A closure that arrives while the controller is already borrowed (for
//! example a Slint callback fired synchronously from inside another update) is
//! deferred to the next event-loop turn instead of panicking.

use std::cell::RefCell;

use crate::app::AppController;

thread_local! {
    static APP: RefCell<Option<AppController>> = const { RefCell::new(None) };
}

/// Installs the controller for the current (UI) thread.
pub(crate) fn install(app: AppController) {
    APP.with(|slot| {
        *slot.borrow_mut() = Some(app);
    });
}

/// Removes and returns the controller, typically during shutdown.
pub(crate) fn take() -> Option<AppController> {
    APP.with(|slot| slot.borrow_mut().take())
}

/// Runs `f` with exclusive access to the controller on the UI thread.
///
/// Must be called on the UI thread. If the controller is busy, the call is
/// re-queued on the event loop. If no controller is installed (startup or
/// shutdown), `f` is dropped.
pub(crate) fn with_app(f: impl FnOnce(&mut AppController) + 'static) {
    let deferred = APP.with(|slot| match slot.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(app) = guard.as_mut() {
                f(app);
            }
            None
        }
        Err(_) => Some(f),
    });
    if let Some(f) = deferred {
        // A zero-duration single shot runs on the next event-loop iteration,
        // after the current borrow has been released.
        slint::Timer::single_shot(std::time::Duration::ZERO, move || with_app(f));
    }
}

/// Posts `f` to the UI thread from any thread.
///
/// Returns `false` when the event loop has already shut down.
pub(crate) fn post(f: impl FnOnce(&mut AppController) + Send + 'static) -> bool {
    slint::invoke_from_event_loop(move || with_app(f)).is_ok()
}

/// Runs `f` immediately when the controller is free; otherwise skips it.
///
/// For callbacks that must answer synchronously (for example the window
/// close request). Must be called on the UI thread.
pub(crate) fn with_app_now(f: impl FnOnce(&mut AppController)) {
    APP.with(|slot| {
        if let Ok(mut guard) = slot.try_borrow_mut()
            && let Some(app) = guard.as_mut()
        {
            f(app);
        }
    });
}
