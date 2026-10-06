//! Settings › Connection: where the app-server runs (GUI.md §5.10).
//!
//! The choice is saved to `gui.json` (`app_server_address`,
//! `remote_auth_token_env`) and read at startup, so changes apply the next
//! time Codex starts. "Test connection" opens a short-lived connection to
//! a daemon or remote server on a Tokio task and shuts it down again.

use std::path::Path;
use std::time::Duration;

use codex_app_server_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::VecModel;

use super::kv;
use super::kv_mono;
use crate::app::AppController;
use crate::connection::ConnectionTarget;
use crate::connection::parse_connection;
use crate::ui::KeyValue;
use crate::ui::SettingsState;

/// Upper bound for a connection test, including the initialize handshake.
const TEST_TIMEOUT: Duration = Duration::from_secs(20);

const TONE_SUCCESS: i32 = 1;
const TONE_DANGER: i32 = 3;

/// The three kinds of connection the page offers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ConnectionMode {
    #[default]
    Embedded,
    Daemon,
    Remote,
}

impl ConnectionMode {
    fn index(self) -> i32 {
        match self {
            Self::Embedded => 0,
            Self::Daemon => 1,
            Self::Remote => 2,
        }
    }

    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Daemon,
            2 => Self::Remote,
            _ => Self::Embedded,
        }
    }
}

/// The editable form: a mode plus the fields that mode uses.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConnectionForm {
    pub(crate) mode: ConnectionMode,
    /// Socket path for the daemon (empty = default socket) or the
    /// WebSocket URL for a remote server.
    pub(crate) address: String,
    pub(crate) token_env: String,
}

impl ConnectionForm {
    /// The form for the saved preferences.
    pub(crate) fn from_prefs(address: &str, token_env: Option<&str>) -> Self {
        let address = address.trim();
        if address.is_empty() || address.eq_ignore_ascii_case("embedded") {
            return Self::default();
        }
        if let Some(socket) = address.strip_prefix("unix://") {
            return Self {
                mode: ConnectionMode::Daemon,
                address: socket.to_string(),
                token_env: String::new(),
            };
        }
        Self {
            mode: ConnectionMode::Remote,
            address: address.to_string(),
            token_env: token_env.unwrap_or_default().to_string(),
        }
    }
}

/// A form that passed validation, with the preference values to save.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ValidConnection {
    pub(crate) target: ConnectionTarget,
    /// Value for `prefs.app_server_address`.
    pub(crate) address: String,
    /// Value for `prefs.remote_auth_token_env`.
    pub(crate) token_env: Option<String>,
}

/// Validates the form the same way startup parses the preferences
/// ([`parse_connection`]). `env` looks up environment variables so the
/// auth token can be checked without touching the process environment in
/// tests.
pub(crate) fn validate_form(
    form: &ConnectionForm,
    codex_home: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ValidConnection, String> {
    match form.mode {
        ConnectionMode::Embedded => Ok(ValidConnection {
            target: ConnectionTarget::Embedded,
            address: String::new(),
            token_env: None,
        }),
        ConnectionMode::Daemon => {
            let socket = form.address.trim();
            if !socket.is_empty() && !Path::new(socket).is_absolute() {
                return Err(
                    "Enter the full path of the socket, or leave it empty for the default daemon."
                        .to_string(),
                );
            }
            let address = format!("unix://{socket}");
            let target = parse_connection(&address, codex_home, /*auth_token*/ None)
                .map_err(|err| format!("{err:#}"))?;
            Ok(ValidConnection {
                target,
                address,
                token_env: None,
            })
        }
        ConnectionMode::Remote => {
            let address = form.address.trim();
            if address.is_empty() {
                return Err("Enter the server address, for example wss://devbox:4500.".to_string());
            }
            if !(address.starts_with("ws://") || address.starts_with("wss://")) {
                return Err("The address must start with ws:// or wss://.".to_string());
            }
            let token_env = form.token_env.trim();
            let token = if token_env.is_empty() {
                None
            } else {
                if !is_env_var_name(token_env) {
                    return Err(format!(
                        "“{token_env}” is not a valid environment variable name."
                    ));
                }
                // Startup refuses to run when the variable is missing.
                Some(env(token_env).ok_or_else(|| {
                    format!(
                        "{token_env} is not set in this app's environment. Set it before saving; otherwise Codex cannot start."
                    )
                })?)
            };
            let target =
                parse_connection(address, codex_home, token).map_err(|err| format!("{err:#}"))?;
            Ok(ValidConnection {
                target,
                address: address.to_string(),
                token_env: (!token_env.is_empty()).then(|| token_env.to_string()),
            })
        }
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Note shown when the saved choice differs from the running connection.
pub(crate) fn pending_restart_note(saved: &ConnectionTarget, running_label: &str) -> String {
    let saved_label = saved.label();
    if running_label.is_empty() || saved_label == running_label {
        return String::new();
    }
    let what = if saved.is_embedded() {
        "the embedded server".to_string()
    } else {
        saved_label
    };
    format!("Saved: Codex switches to {what} the next time it starts.")
}

/// Facts reported by a successful connection test.
#[derive(Clone, Debug, Default, PartialEq)]
struct ServerFacts {
    version: Option<String>,
    platform: Option<String>,
    codex_home: Option<String>,
}

#[derive(Default)]
pub(crate) struct ConnectionState {
    /// The form was filled from the preferences at least once.
    loaded: bool,
    /// Bumped per test so a late result for an older form is ignored.
    test_generation: u64,
    testing: bool,
}

impl AppController {
    pub(super) fn settings_connection_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.on_connection_edited(|| {
            crate::ui_thread::with_app(|app| {
                app.settings.connection.test_generation += 1;
                app.settings.connection.testing = false;
                let state = app.window.global::<SettingsState>();
                state.set_connection_result("".into());
                state.set_connection_facts(ModelRc::default());
                app.settings_connection_validate();
            });
        });
        state.on_connection_test(|| {
            crate::ui_thread::with_app(AppController::settings_connection_test);
        });
        state.on_connection_save(|| {
            crate::ui_thread::with_app(AppController::settings_connection_save);
        });
        state.on_connection_revert(|| {
            crate::ui_thread::with_app(|app| {
                app.settings.connection.loaded = false;
                app.settings_connection_activate();
            });
        });
    }

    pub(super) fn settings_connection_activate(&mut self) {
        if !self.settings.connection.loaded {
            self.settings.connection.loaded = true;
            let form = self.settings_connection_saved_form();
            let state = self.window.global::<SettingsState>();
            state.set_connection_mode(form.mode.index());
            state.set_connection_address(form.address.into());
            state.set_connection_token_env(form.token_env.into());
            state.set_connection_result("".into());
            state.set_connection_facts(ModelRc::default());
        }
        self.settings_connection_refresh_status();
        self.settings_connection_validate();
    }

    /// Updates the "Connected to" card; called on server start and failure.
    pub(super) fn settings_connection_refresh_status(&mut self) {
        let state = self.window.global::<SettingsState>();
        let current = if self.connection_label.is_empty() {
            "Not connected yet".to_string()
        } else {
            self.connection_label.clone()
        };
        let status = match (&self.settings.server_error, self.backend.is_ready()) {
            (Some(_), _) => "Not running",
            (None, true) => "Running",
            (None, false) => "Starting",
        };
        state.set_connection_current(current.into());
        state.set_connection_state(status.into());
        let saved = parse_connection(
            &self.prefs.app_server_address,
            self.codex_home.as_deref(),
            /*auth_token*/ None,
        );
        let note = match saved {
            Ok(saved) => pending_restart_note(&saved, &self.connection_label),
            Err(err) => format!("The saved address is invalid: {err:#}"),
        };
        state.set_connection_saved(note.into());
    }

    fn settings_connection_saved_form(&self) -> ConnectionForm {
        ConnectionForm::from_prefs(
            &self.prefs.app_server_address,
            self.prefs.remote_auth_token_env.as_deref(),
        )
    }

    fn settings_connection_form(&self) -> ConnectionForm {
        let state = self.window.global::<SettingsState>();
        ConnectionForm {
            mode: ConnectionMode::from_index(state.get_connection_mode()),
            address: state.get_connection_address().to_string(),
            token_env: state.get_connection_token_env().to_string(),
        }
    }

    fn settings_connection_check(&self) -> Result<ValidConnection, String> {
        validate_form(
            &self.settings_connection_form(),
            self.codex_home.as_deref(),
            |name| std::env::var(name).ok(),
        )
    }

    /// Validates the form and updates the hint, Save, and Test buttons.
    fn settings_connection_validate(&mut self) {
        let form = self.settings_connection_form();
        let dirty = form != self.settings_connection_saved_form();
        let state = self.window.global::<SettingsState>();
        match self.settings_connection_check() {
            Ok(valid) => {
                // The embedded option explains itself.
                let hint = if valid.target.is_embedded() {
                    String::new()
                } else {
                    format!("Connects to {}.", valid.target.label())
                };
                state.set_connection_check(hint.into());
                state.set_connection_valid(true);
            }
            Err(err) => {
                state.set_connection_check(err.into());
                state.set_connection_valid(false);
            }
        }
        state.set_connection_dirty(dirty);
        state.set_connection_testing(self.settings.connection.testing);
    }

    fn settings_connection_save(&mut self) {
        let valid = match self.settings_connection_check() {
            Ok(valid) => valid,
            Err(err) => {
                self.settings_connection_validate();
                self.toast(err);
                return;
            }
        };
        self.prefs.app_server_address = valid.address;
        self.prefs.remote_auth_token_env = valid.token_env;
        self.save_prefs();
        let state = self.window.global::<SettingsState>();
        let message = if valid.target.label() == self.connection_label {
            "Saved.".to_string()
        } else {
            "Saved. The new connection takes effect after you restart Codex.".to_string()
        };
        state.set_connection_result(message.into());
        state.set_connection_result_tone(TONE_SUCCESS);
        self.settings_connection_refresh_status();
        self.settings_connection_validate();
    }

    fn settings_connection_test(&mut self) {
        let valid = match self.settings_connection_check() {
            Ok(valid) => valid,
            Err(err) => {
                self.settings_connection_validate();
                self.toast(err);
                return;
            }
        };
        let ConnectionTarget::Remote(endpoint) = valid.target else {
            return;
        };
        let connection = &mut self.settings.connection;
        connection.test_generation += 1;
        connection.testing = true;
        let generation = connection.test_generation;
        let state = self.window.global::<SettingsState>();
        state.set_connection_testing(true);
        state.set_connection_result("".into());
        state.set_connection_facts(ModelRc::default());
        let label = ConnectionTarget::Remote(endpoint.clone()).label();
        self.backend.spawn(async move {
            let result = test_connection(endpoint).await;
            crate::ui_thread::post(move |app| {
                app.settings_connection_test_done(generation, &label, result);
            });
        });
    }

    fn settings_connection_test_done(
        &mut self,
        generation: u64,
        label: &str,
        result: Result<ServerFacts, String>,
    ) {
        if self.settings.connection.test_generation != generation {
            return;
        }
        self.settings.connection.testing = false;
        let state = self.window.global::<SettingsState>();
        state.set_connection_testing(false);
        match result {
            Ok(facts) => {
                state.set_connection_result(format!("Connected to {label}.").into());
                state.set_connection_result_tone(TONE_SUCCESS);
                let mut rows: Vec<KeyValue> = Vec::new();
                if let Some(version) = facts.version {
                    rows.push(kv("Server version", version));
                }
                if let Some(platform) = facts.platform {
                    rows.push(kv("Server platform", platform));
                }
                if let Some(home) = facts.codex_home {
                    rows.push(kv_mono("Server Codex home", home));
                }
                state.set_connection_facts(ModelRc::new(VecModel::from(rows)));
            }
            Err(err) => {
                state.set_connection_result(err.into());
                state.set_connection_result_tone(TONE_DANGER);
            }
        }
    }

    /// Scripted input for UI automation: `["mode", "0|1|2"]`,
    /// `["address", text]`, `["token-env", name]`, `["test"]`, `["save"]`.
    pub(super) fn settings_connection_automation(&mut self, args: &[String]) {
        let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
        let state = self.window.global::<SettingsState>();
        match arg(0) {
            "mode" => {
                state.set_connection_mode(arg(1).parse().unwrap_or(0));
                state.invoke_connection_edited();
            }
            "address" => {
                state.set_connection_address(arg(1).into());
                state.invoke_connection_edited();
            }
            "token-env" => {
                state.set_connection_token_env(arg(1).into());
                state.invoke_connection_edited();
            }
            "test" => state.invoke_connection_test(),
            "save" => state.invoke_connection_save(),
            other => tracing::warn!(action = other, "unknown connection automation action"),
        }
    }
}

/// Connects, reads the server's metadata, and disconnects.
async fn test_connection(endpoint: RemoteAppServerEndpoint) -> Result<ServerFacts, String> {
    // The client reports every failure as a generic I/O error; a missing
    // socket file is the common case worth naming.
    if let RemoteAppServerEndpoint::UnixSocket { socket_path } = &endpoint
        && !tokio::fs::try_exists(socket_path.as_path())
            .await
            .unwrap_or(true)
    {
        return Err(format!(
            "No daemon is running at {}. Start one with the codex CLI, then test again.",
            socket_path.display()
        ));
    }
    let args = RemoteAppServerConnectArgs {
        endpoint,
        client_name: crate::startup::GUI_CLIENT_NAME.to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
    };
    let client =
        match tokio::time::timeout(TEST_TIMEOUT, RemoteAppServerClient::connect(args)).await {
            Ok(Ok(client)) => client,
            Ok(Err(err)) => return Err(format!("Could not connect.\n{err}")),
            Err(_) => {
                return Err(format!(
                    "No answer within {} seconds.",
                    TEST_TIMEOUT.as_secs()
                ));
            }
        };
    let platform = match (client.platform_os(), client.platform_family()) {
        (Some(os), Some(family)) if !os.eq_ignore_ascii_case(family) => {
            Some(format!("{os} ({family})"))
        }
        (Some(os), _) => Some(os.to_string()),
        (None, family) => family.map(str::to_string),
    };
    let facts = ServerFacts {
        version: client.server_version().map(str::to_string),
        platform,
        codex_home: client.codex_home().map(str::to_string),
    };
    if let Err(err) = client.shutdown().await {
        tracing::debug!(%err, "connection test shutdown failed");
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn form(mode: ConnectionMode, address: &str, token_env: &str) -> ConnectionForm {
        ConnectionForm {
            mode,
            address: address.to_string(),
            token_env: token_env.to_string(),
        }
    }

    #[test]
    fn prefs_round_trip_through_the_form() -> Result<(), String> {
        let home = tempfile::tempdir().map_err(|err| err.to_string())?;
        for (address, token_env) in [
            ("", None),
            ("unix://", None),
            ("ws://127.0.0.1:4500", None),
            ("wss://box.tailnet:4500", Some("TOKEN")),
        ] {
            let parsed = ConnectionForm::from_prefs(address, token_env);
            let valid = validate_form(&parsed, Some(home.path()), |_| Some("t".to_string()))?;
            assert_eq!(valid.address, address);
            assert_eq!(valid.token_env.as_deref(), token_env);
        }
        assert_eq!(
            ConnectionForm::from_prefs(" embedded ", Some("X")),
            ConnectionForm::default()
        );
        assert_eq!(
            ConnectionForm::from_prefs("unix:///tmp/codex.sock", None),
            form(ConnectionMode::Daemon, "/tmp/codex.sock", "")
        );
        Ok(())
    }

    #[test]
    fn embedded_needs_no_fields() -> Result<(), String> {
        let valid = validate_form(
            &form(ConnectionMode::Embedded, "ignored", "X"),
            None,
            no_env,
        )?;
        assert!(valid.target.is_embedded());
        assert_eq!(valid.address, "");
        assert_eq!(valid.token_env, None);
        Ok(())
    }

    #[test]
    fn daemon_sockets_must_be_absolute() -> Result<(), String> {
        let home = tempfile::tempdir().map_err(|err| err.to_string())?;
        let valid = validate_form(
            &form(ConnectionMode::Daemon, "  ", "X"),
            Some(home.path()),
            no_env,
        )?;
        assert_eq!(valid.address, "unix://");
        assert_eq!(valid.token_env, None);
        assert!(!valid.target.is_embedded());
        assert!(
            validate_form(
                &form(ConnectionMode::Daemon, "relative.sock", ""),
                None,
                no_env
            )
            .is_err()
        );
        // The default socket lives in CODEX_HOME.
        assert!(validate_form(&form(ConnectionMode::Daemon, "", ""), None, no_env).is_err());
        let socket = home.path().join("custom.sock");
        let socket = socket.to_string_lossy();
        let valid = validate_form(&form(ConnectionMode::Daemon, &socket, ""), None, no_env)?;
        assert_eq!(valid.address, format!("unix://{socket}"));
        Ok(())
    }

    #[test]
    fn remote_addresses_are_checked() {
        let remote = |address: &str, token_env: &str| {
            validate_form(
                &form(ConnectionMode::Remote, address, token_env),
                None,
                |name| (name == "TOKEN").then(|| "secret".to_string()),
            )
        };
        assert!(remote("", "").is_err());
        assert!(remote("http://box:80", "").is_err());
        assert!(remote("ws://box", "").is_err());
        assert!(remote("ws://box:4500", "").is_ok());
        assert!(remote("wss://box:4500", "TOKEN").is_ok());
        // Tokens never travel over plain WebSocket to another machine.
        assert!(remote("ws://box:4500", "TOKEN").is_err());
        assert!(remote("ws://127.0.0.1:4500", "TOKEN").is_ok());
        assert!(remote("wss://box:4500", "MISSING").is_err());
        assert!(remote("wss://box:4500", "1BAD").is_err());
        assert!(remote("wss://box:4500", "BAD-NAME").is_err());
    }

    #[test]
    fn restart_note_compares_with_the_running_connection() -> anyhow::Result<()> {
        let embedded = ConnectionTarget::Embedded;
        assert_eq!(pending_restart_note(&embedded, &embedded.label()), "");
        assert_eq!(pending_restart_note(&embedded, ""), "");
        let remote = parse_connection("ws://127.0.0.1:4500", None, None)?;
        assert_eq!(
            pending_restart_note(&remote, &embedded.label()),
            "Saved: Codex switches to Remote (ws://127.0.0.1:4500/) the next time it starts."
        );
        assert_eq!(
            pending_restart_note(&embedded, &remote.label()),
            "Saved: Codex switches to the embedded server the next time it starts."
        );
        Ok(())
    }

    #[test]
    fn env_var_names() {
        assert!(is_env_var_name("CODEX_TOKEN"));
        assert!(is_env_var_name("_x1"));
        assert!(!is_env_var_name(""));
        assert!(!is_env_var_name("9X"));
        assert!(!is_env_var_name("A B"));
    }
}
