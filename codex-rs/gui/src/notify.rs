//! Desktop notifications (turn finished, approval needed) while the window
//! is in the background.
//!
//! On macOS, notifications are attributed to the app bundle's identifier
//! (`Codex.app`); without one, `notify-rust` would attribute them to Finder.
//! On Windows they are shown as Windows PowerShell's until an installer
//! registers an AppUserModelID for the app.

use crate::app::AppController;

/// The bundle identifier of `Codex.app`, read on the main thread before any
/// notification (the notification library swizzles it afterwards). `None`
/// when not running from an app bundle.
#[cfg(target_os = "macos")]
static BUNDLE_ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Records what notifications need from the main thread. Call once at
/// startup, before the first notification.
pub(crate) fn init() {
    #[cfg(target_os = "macos")]
    BUNDLE_ID.get_or_init(crate::platform::main_bundle_identifier);
}

/// Attributes notifications to the app bundle (once per process).
#[cfg(target_os = "macos")]
fn attribute_to_app_bundle() {
    static SET: std::sync::Once = std::sync::Once::new();
    SET.call_once(|| {
        if let Some(Some(bundle_id)) = BUNDLE_ID.get()
            && let Err(err) = notify_rust::set_application(bundle_id)
        {
            tracing::debug!(%err, "could not attribute notifications to the app bundle");
        }
    });
}

impl AppController {
    /// Shows an OS notification when enabled in preferences.
    pub(crate) fn notify_desktop(&self, title: &str, body: &str) {
        if !self.prefs.desktop_notifications {
            return;
        }
        let title = title.to_string();
        let body = body.to_string();
        // Some platforms block while the notification is delivered.
        let spawned = std::thread::Builder::new()
            .name("codex-gui-notify".to_string())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                attribute_to_app_bundle();
                if let Err(err) = notify_rust::Notification::new()
                    .summary(&title)
                    .body(&body)
                    .appname("Codex")
                    .show()
                {
                    tracing::debug!(%err, "desktop notification failed");
                }
            });
        if let Err(err) = spawned {
            tracing::debug!(%err, "could not spawn notification thread");
        }
    }
}
