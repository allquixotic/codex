//! Settings › Windows sandbox: readiness and one-time setup, like the TUI's
//! `/setup-default-sandbox` flow.
//!
//! `windowsSandbox/readiness` says whether the configured sandbox is ready;
//! `windowsSandbox/setupStart` acknowledges right away and reports the
//! outcome later with `windowsSandbox/setupCompleted`. A successful setup
//! persists `windows.sandbox` in `config.toml` on the server side. The page
//! is listed on Windows, and elsewhere only when a (remote) server reports
//! a configured sandbox. Everything here compiles on every OS.

use std::path::PathBuf;
use std::time::Duration;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigRequirements;
use codex_app_server_protocol::WindowsSandboxImplementation;
use codex_app_server_protocol::WindowsSandboxReadiness;
use codex_app_server_protocol::WindowsSandboxReadinessResponse;
use codex_app_server_protocol::WindowsSandboxSetupCompletedNotification;
use codex_app_server_protocol::WindowsSandboxSetupMode;
use codex_app_server_protocol::WindowsSandboxSetupStartParams;
use codex_app_server_protocol::WindowsSandboxSetupStartResponse;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;
use slint::ComponentHandle;

use super::model;
use crate::app::AppController;
use crate::backend::BackendError;
use crate::ui::SettingsState;

/// How long to wait for `setupStart` to acknowledge, as in the TUI.
const SETUP_START_TIMEOUT: Duration = Duration::from_secs(15);

/// JSON-RPC "method not found": the server predates sandbox setup.
const METHOD_NOT_FOUND: i64 = -32601;

/// Whether the settings nav lists the page.
pub(crate) fn page_visible(readiness: Option<WindowsSandboxReadiness>) -> bool {
    cfg!(windows)
        || matches!(
            readiness,
            Some(WindowsSandboxReadiness::Ready | WindowsSandboxReadiness::UpdateRequired)
        )
}

/// Page id of a readiness value for the status badge.
fn readiness_id(readiness: Option<WindowsSandboxReadiness>) -> &'static str {
    match readiness {
        Some(WindowsSandboxReadiness::Ready) => "ready",
        Some(WindowsSandboxReadiness::NotConfigured) if !cfg!(windows) => "unavailable",
        Some(WindowsSandboxReadiness::NotConfigured) => "not-configured",
        Some(WindowsSandboxReadiness::UpdateRequired) => "update-required",
        None => "",
    }
}

/// One-line explanation of the readiness state.
pub(crate) fn readiness_summary(readiness: Option<WindowsSandboxReadiness>) -> &'static str {
    match readiness {
        Some(WindowsSandboxReadiness::Ready) => {
            "The sandbox is set up. Commands the agent runs are isolated according to each thread's permissions."
        }
        Some(WindowsSandboxReadiness::NotConfigured) if !cfg!(windows) => {
            "The Windows sandbox is only used when Codex runs on Windows."
        }
        Some(WindowsSandboxReadiness::NotConfigured) => {
            "No sandbox is set up yet. Until it is, Codex asks before running commands that need isolation."
        }
        Some(WindowsSandboxReadiness::UpdateRequired) => {
            "The elevated sandbox is selected but its setup is missing or out of date. Run setup to finish it."
        }
        None => "Checking the sandbox…",
    }
}

/// Configured `windows.sandbox` mode, for display.
pub(crate) fn configured_mode(effective: &Value) -> Option<String> {
    let mode = model::lookup(effective, &["windows", "sandbox"])?.as_str()?;
    Some(match mode {
        "elevated" => "Configured mode: elevated sandbox.".to_string(),
        "unelevated" => "Configured mode: standard sandbox (restricted token).".to_string(),
        "mxc" => "Configured mode: MXC sandbox.".to_string(),
        other => format!("Configured mode: {other}."),
    })
}

/// (elevated allowed, unelevated allowed) under managed requirements.
pub(crate) fn allowed_modes(requirements: Option<&ConfigRequirements>) -> (bool, bool) {
    match requirements.and_then(|requirements| {
        requirements
            .allowed_windows_sandbox_implementations
            .as_ref()
    }) {
        None => (true, true),
        Some(allowed) => (
            allowed.contains(&WindowsSandboxImplementation::Elevated),
            allowed.contains(&WindowsSandboxImplementation::Unelevated),
        ),
    }
}

fn mode_name(mode: WindowsSandboxSetupMode) -> &'static str {
    match mode {
        WindowsSandboxSetupMode::Elevated => "elevated",
        WindowsSandboxSetupMode::Unelevated => "standard",
    }
}

fn parse_mode(mode: &str) -> Option<WindowsSandboxSetupMode> {
    match mode {
        "elevated" => Some(WindowsSandboxSetupMode::Elevated),
        "unelevated" => Some(WindowsSandboxSetupMode::Unelevated),
        _ => None,
    }
}

#[derive(Default)]
pub(crate) struct SandboxState {
    readiness: Option<WindowsSandboxReadiness>,
    readiness_error: Option<String>,
    /// Setup in progress, by mode.
    running: Option<WindowsSandboxSetupMode>,
    error: Option<String>,
    notice: Option<String>,
    /// The elevated setup failed; offer the standard one instead.
    offer_unelevated: bool,
    generation: u64,
}

impl AppController {
    pub(super) fn settings_sandbox_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.on_sandbox_setup(|mode| {
            let mode = mode.to_string();
            crate::ui_thread::with_app(move |app| {
                if let Some(mode) = parse_mode(&mode) {
                    app.settings_sandbox_setup(mode);
                }
            });
        });
        state.on_sandbox_refresh(|| {
            crate::ui_thread::with_app(AppController::settings_sandbox_check);
        });
        self.settings_sandbox_refresh();
    }

    pub(super) fn settings_sandbox_activate(&mut self) {
        if self.settings.sandbox.readiness.is_none() {
            self.settings_sandbox_check();
        }
        self.settings_sandbox_refresh();
    }

    /// Asks the server whether the sandbox is ready.
    pub(super) fn settings_sandbox_check(&mut self) {
        if self.settings.server_error.is_some() {
            return;
        }
        self.settings.sandbox.generation += 1;
        let generation = self.settings.sandbox.generation;
        self.backend.call(
            |request_id| ClientRequest::WindowsSandboxReadiness {
                request_id,
                params: None,
            },
            move |app, result: Result<WindowsSandboxReadinessResponse, BackendError>| {
                let sandbox = &mut app.settings.sandbox;
                if sandbox.generation != generation {
                    return;
                }
                match result {
                    Ok(response) => {
                        sandbox.readiness = Some(response.status);
                        sandbox.readiness_error = None;
                    }
                    Err(err) => {
                        tracing::debug!(%err, "windowsSandbox/readiness failed");
                        sandbox.readiness = None;
                        sandbox.readiness_error = Some(err.user_message());
                    }
                }
                app.settings_sandbox_refresh();
            },
        );
    }

    /// Forgets server state after a restart or failure.
    pub(super) fn settings_sandbox_reset(&mut self) {
        let sandbox = &mut self.settings.sandbox;
        sandbox.generation += 1;
        sandbox.readiness = None;
        sandbox.readiness_error = None;
        if sandbox.running.take().is_some() {
            sandbox.error = Some(
                "Codex stopped before the sandbox setup finished. Check the status, then run setup again if needed."
                    .to_string(),
            );
        }
        self.settings_sandbox_refresh();
    }

    /// Folder whose access the setup grants: the settings folder, else the
    /// first open thread's folder.
    fn settings_sandbox_folder(&self) -> Option<PathBuf> {
        self.settings_context_cwd().or_else(|| {
            self.tabs
                .iter()
                .find_map(|tab| tab.thread().map(|thread| thread.cwd.clone()))
        })
    }

    fn settings_sandbox_setup(&mut self, mode: WindowsSandboxSetupMode) {
        if self.settings.sandbox.running.is_some() {
            return;
        }
        let (elevated, unelevated) = allowed_modes(self.settings.requirements.as_ref());
        let allowed = match mode {
            WindowsSandboxSetupMode::Elevated => elevated,
            WindowsSandboxSetupMode::Unelevated => unelevated,
        };
        let sandbox = &mut self.settings.sandbox;
        if !allowed {
            sandbox.error =
                Some("Your organization's requirements do not allow that sandbox.".to_string());
            self.settings_sandbox_refresh();
            return;
        }
        sandbox.running = Some(mode);
        sandbox.error = None;
        sandbox.notice = None;
        sandbox.offer_unelevated = false;
        let cwd = self
            .settings_sandbox_folder()
            .and_then(|folder| AbsolutePathBuf::from_absolute_path(folder).ok());
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let request = ClientRequest::WindowsSandboxSetupStart {
                request_id: backend.next_request_id(),
                params: WindowsSandboxSetupStartParams { mode, cwd },
            };
            let result = tokio::time::timeout(
                SETUP_START_TIMEOUT,
                backend.request::<WindowsSandboxSetupStartResponse>(request),
            )
            .await;
            crate::ui_thread::post(move |app| app.settings_sandbox_setup_started(mode, result));
        });
        self.settings_sandbox_refresh();
    }

    fn settings_sandbox_setup_started(
        &mut self,
        mode: WindowsSandboxSetupMode,
        result: Result<
            Result<WindowsSandboxSetupStartResponse, BackendError>,
            tokio::time::error::Elapsed,
        >,
    ) {
        let sandbox = &mut self.settings.sandbox;
        if sandbox.running != Some(mode) {
            return;
        }
        let error = match result {
            // Wait for `windowsSandbox/setupCompleted`.
            Ok(Ok(WindowsSandboxSetupStartResponse { started: true })) => None,
            Ok(Ok(WindowsSandboxSetupStartResponse { started: false })) => {
                Some("The sandbox setup did not start.".to_string())
            }
            Ok(Err(err))
                if err
                    .server_error()
                    .is_some_and(|error| error.code == METHOD_NOT_FOUND) =>
            {
                Some(
                    "This Codex server cannot set up the Windows sandbox. Update it and try again."
                        .to_string(),
                )
            }
            Ok(Err(err)) => Some(format!(
                "The sandbox setup could not start: {}",
                err.user_message()
            )),
            Err(_) => {
                // The server may still be working on it; keep waiting for
                // the completion notification.
                sandbox.notice = Some(
                    "The setup request is taking a while. Codex keeps waiting for it to finish."
                        .to_string(),
                );
                None
            }
        };
        if let Some(error) = error {
            sandbox.running = None;
            sandbox.error = Some(error);
        }
        self.settings_sandbox_refresh();
    }

    pub(super) fn settings_sandbox_on_completed(
        &mut self,
        completed: &WindowsSandboxSetupCompletedNotification,
    ) {
        let sandbox = &mut self.settings.sandbox;
        if sandbox.running.is_some() && sandbox.running != Some(completed.mode) {
            // A different setup's result; keep waiting for ours.
            return;
        }
        sandbox.running = None;
        if completed.success {
            sandbox.error = None;
            sandbox.offer_unelevated = false;
            sandbox.notice = Some(format!(
                "The {} sandbox is set up. New commands run inside it.",
                mode_name(completed.mode)
            ));
        } else {
            sandbox.notice = None;
            sandbox.error = Some(format!(
                "The {} sandbox setup failed: {}",
                mode_name(completed.mode),
                completed
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
            sandbox.offer_unelevated = completed.mode == WindowsSandboxSetupMode::Elevated;
        }
        // Setup persists `windows.sandbox`; show the new state.
        self.settings_reload_config();
        self.settings_sandbox_check();
        self.settings_sandbox_refresh();
    }

    pub(super) fn settings_sandbox_refresh(&mut self) {
        let sandbox = &self.settings.sandbox;
        let state = self.window.global::<SettingsState>();
        state.set_sandbox_visible(page_visible(sandbox.readiness));
        let readiness = match (&sandbox.readiness, &sandbox.readiness_error) {
            (None, Some(_)) => "error",
            (readiness, _) => readiness_id(*readiness),
        };
        state.set_sandbox_readiness(readiness.into());
        let summary = match &sandbox.readiness_error {
            Some(error) if sandbox.readiness.is_none() => {
                format!("Could not check the sandbox: {error}")
            }
            _ => readiness_summary(sandbox.readiness).to_string(),
        };
        state.set_sandbox_summary(summary.into());
        let mode = self
            .settings
            .snapshot
            .as_ref()
            .and_then(|snapshot| configured_mode(&snapshot.effective))
            .unwrap_or_default();
        state.set_sandbox_mode(mode.into());
        let folder = self
            .settings_sandbox_folder()
            .map(|folder| super::short_path(&folder))
            .unwrap_or_default();
        state.set_sandbox_folder(folder.into());
        let (elevated, unelevated) = allowed_modes(self.settings.requirements.as_ref());
        state.set_sandbox_elevated_allowed(elevated);
        state.set_sandbox_unelevated_allowed(unelevated);
        state.set_sandbox_busy(sandbox.running.is_some());
        state.set_sandbox_error(sandbox.error.clone().unwrap_or_default().into());
        state.set_sandbox_notice(sandbox.notice.clone().unwrap_or_default().into());
        state.set_sandbox_offer_unelevated(sandbox.offer_unelevated);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn visibility_follows_the_platform_or_a_configured_sandbox() {
        assert!(page_visible(Some(WindowsSandboxReadiness::Ready)));
        assert!(page_visible(Some(WindowsSandboxReadiness::UpdateRequired)));
        assert_eq!(
            page_visible(Some(WindowsSandboxReadiness::NotConfigured)),
            cfg!(windows)
        );
        assert_eq!(page_visible(None), cfg!(windows));
    }

    #[test]
    fn requirements_limit_setup_modes() -> serde_json::Result<()> {
        assert_eq!(allowed_modes(None), (true, true));
        let requirements: ConfigRequirements = serde_json::from_value(
            json!({"allowedWindowsSandboxImplementations": ["unelevated"]}),
        )?;
        assert_eq!(allowed_modes(Some(&requirements)), (false, true));
        Ok(())
    }

    #[test]
    fn configured_mode_reads_windows_sandbox() {
        assert_eq!(configured_mode(&json!({})), None);
        assert_eq!(
            configured_mode(&json!({"windows": {"sandbox": "elevated"}})).as_deref(),
            Some("Configured mode: elevated sandbox.")
        );
    }

    #[test]
    fn modes_parse_from_page_ids() {
        assert_eq!(
            parse_mode("elevated"),
            Some(WindowsSandboxSetupMode::Elevated)
        );
        assert_eq!(
            parse_mode("unelevated"),
            Some(WindowsSandboxSetupMode::Unelevated)
        );
        assert_eq!(parse_mode("other"), None);
        assert_eq!(
            readiness_id(Some(WindowsSandboxReadiness::UpdateRequired)),
            "update-required"
        );
    }
}
