//! Settings › MCP servers: status, tools, sign-in, enable/disable, add and
//! remove.
//!
//! Status comes from `mcpServerStatus/list` (paged) merged with the
//! effective `mcp_servers` config, and live `mcpServer/startupStatus/updated`
//! notifications. Edits are config writes to `mcp_servers.<name>` followed by
//! `config/mcpServer/reload`.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ListMcpServerStatusParams;
use codex_app_server_protocol::ListMcpServerStatusResponse;
use codex_app_server_protocol::McpAuthStatus;
use codex_app_server_protocol::McpServerConnectionStatus;
use codex_app_server_protocol::McpServerOauthLoginCompletedNotification;
use codex_app_server_protocol::McpServerOauthLoginParams;
use codex_app_server_protocol::McpServerOauthLoginResponse;
use codex_app_server_protocol::McpServerRefreshResponse;
use codex_app_server_protocol::McpServerStartupState;
use codex_app_server_protocol::McpServerStatus;
use codex_app_server_protocol::McpServerStatusDetail;
use codex_app_server_protocol::McpServerStatusUpdatedNotification;
use serde_json::Value;
use serde_json::json;
use slint::ComponentHandle;
use slint::SharedString;

use super::ItemList;
use super::ListItem;
use super::LoadState;
use super::MAX_PAGES;
use super::PAGE_SIZE;
use super::PendingValues;
use super::model;
use super::replace_edit;
use super::tone;
use super::words;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::backend::BackendError;
use crate::ui::SettingsState;

/// Fields of the "Add server" form.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct McpForm {
    pub(crate) name: String,
    /// `false` = stdio command, `true` = streamable HTTP.
    pub(crate) http: bool,
    pub(crate) command: String,
    pub(crate) args: String,
    pub(crate) env: String,
    pub(crate) url: String,
    pub(crate) token_env: String,
}

/// Validates the form and builds the `mcp_servers.<name>` table.
pub(crate) fn build_server(form: &McpForm) -> Result<(String, Value), String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err("Enter a name for the server.".to_string());
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err("Use only letters, numbers, '-' and '_' in the name.".to_string());
    }
    if form.http {
        let url = form.url.trim();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err("Enter an http:// or https:// URL.".to_string());
        }
        let mut server = json!({ "url": url });
        let token_env = form.token_env.trim();
        if !token_env.is_empty() {
            if !is_env_name(token_env) {
                return Err(
                    "The bearer token variable must be an environment variable name.".to_string(),
                );
            }
            server["bearer_token_env_var"] = json!(token_env);
        }
        return Ok((name.to_string(), server));
    }
    let command = form.command.trim();
    if command.is_empty() {
        return Err("Enter the command that starts the server.".to_string());
    }
    let args =
        words::split_words(form.args.trim()).ok_or("The arguments have an unbalanced quote.")?;
    let mut server = json!({ "command": command });
    if !args.is_empty() {
        server["args"] = json!(args);
    }
    let env = parse_env(&form.env)?;
    if !env.is_empty() {
        server["env"] = json!(env);
    }
    Ok((name.to_string(), server))
}

fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|ch: char| ch.is_ascii_digit())
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Parses `KEY=value` pairs separated by whitespace (quoting as in
/// [`words::split_words`], so Windows paths keep their backslashes).
fn parse_env(text: &str) -> Result<serde_json::Map<String, Value>, String> {
    let words =
        words::split_words(text.trim()).ok_or("The environment has an unbalanced quote.")?;
    let mut env = serde_json::Map::new();
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            return Err(format!("`{word}` is not KEY=value."));
        };
        if !is_env_name(key) {
            return Err(format!("`{key}` is not a valid environment variable name."));
        }
        env.insert(key.to_string(), Value::String(value.to_string()));
    }
    Ok(env)
}

/// Live startup state reported by `mcpServer/startupStatus/updated`.
#[derive(Clone, Debug)]
struct LiveStatus {
    state: McpServerStartupState,
    error: Option<String>,
}

#[derive(Default)]
pub(crate) struct McpState {
    pub(crate) items: ItemList,
    pub(super) load: LoadState,
    generation: u64,
    statuses: Vec<McpServerStatus>,
    /// `mcpServerStatus/list` failed.
    error: Option<String>,
    /// `config/mcpServer/reload` failed: threads did not pick up the last
    /// change. Kept until a reload succeeds.
    reload_error: Option<String>,
    live: HashMap<String, LiveStatus>,
    busy: HashSet<String>,
    errors: HashMap<String, String>,
    /// Servers with an OAuth login in progress.
    oauth: HashSet<String>,
    /// Rows to republish even when unchanged (to reset a switch).
    force: HashSet<String>,
    /// Enable switches being saved, shown until the config is re-read.
    pending: PendingValues<bool>,
}

impl McpState {
    /// Drops saved switch values that the config read of `generation`
    /// shows.
    pub(crate) fn settle_pending(&mut self, generation: u64) {
        self.pending.settle(generation);
    }

    /// Forgets work tied to a server that stopped or restarted: its OAuth
    /// callbacks and pending requests never complete.
    fn reset_for_new_server(&mut self) {
        self.oauth.clear();
        self.busy.clear();
        self.pending.clear();
        self.reload_error = None;
        self.load = LoadState::Stale;
    }
}

/// Status line of the MCP page: a failed list wins over a failed reload.
fn mcp_status(
    list_error: Option<&str>,
    reload_error: Option<&str>,
    loading: bool,
    empty: bool,
) -> String {
    match (list_error, reload_error) {
        (Some(error), _) => format!("Could not list MCP servers: {error}"),
        (None, Some(error)) => format!("Could not reload MCP servers: {error}"),
        (None, None) if loading && empty => "Loading MCP servers…".to_string(),
        _ => String::new(),
    }
}

/// Inputs for building MCP rows.
pub(crate) struct McpInputs<'a> {
    pub(crate) statuses: &'a [McpServerStatus],
    pub(crate) effective: &'a Value,
    pub(crate) user: &'a Value,
    pub(crate) origins: &'a HashMap<String, codex_app_server_protocol::ConfigLayerMetadata>,
}

fn transport_summary(config: Option<&Value>, status: Option<&McpServerStatus>) -> String {
    if let Some(config) = config {
        if let Some(url) = config.get("url").and_then(Value::as_str) {
            return url.to_string();
        }
        if let Some(command) = config.get("command").and_then(Value::as_str) {
            let mut argv = vec![command.to_string()];
            if let Some(args) = config.get("args").and_then(Value::as_array) {
                argv.extend(args.iter().map(model::display_value));
            }
            return words::join_words(&argv);
        }
    }
    status
        .and_then(|status| status.http_origin.clone())
        .unwrap_or_default()
}

fn status_label(
    live: Option<&LiveStatus>,
    status: Option<&McpServerStatus>,
    enabled: bool,
) -> (String, i32) {
    // Statuses listed without a thread have no runtime state; infer it
    // from tool discovery.
    let inferred = |status: &McpServerStatus| {
        if status.tools_error.is_some() {
            ("Failed".to_string(), tone::DANGER)
        } else if matches!(status.auth_status, McpAuthStatus::NotLoggedIn) {
            ("Sign-in required".to_string(), tone::WARNING)
        } else if !status.tools.is_empty() {
            ("Available".to_string(), tone::SUCCESS)
        } else {
            ("Not started".to_string(), tone::NEUTRAL)
        }
    };
    if !enabled {
        return ("Disabled".to_string(), tone::NEUTRAL);
    }
    if let Some(live) = live {
        return match live.state {
            McpServerStartupState::Starting => ("Starting".to_string(), tone::ACCENT),
            McpServerStartupState::Ready => ("Connected".to_string(), tone::SUCCESS),
            McpServerStartupState::Failed => ("Failed".to_string(), tone::DANGER),
            McpServerStartupState::Cancelled => ("Cancelled".to_string(), tone::WARNING),
        };
    }
    match status.and_then(|status| status.runtime_status) {
        Some(McpServerConnectionStatus::Connected) => ("Connected".to_string(), tone::SUCCESS),
        Some(McpServerConnectionStatus::Starting) => ("Starting".to_string(), tone::ACCENT),
        Some(McpServerConnectionStatus::AuthenticationRequired) => {
            ("Sign-in required".to_string(), tone::WARNING)
        }
        Some(McpServerConnectionStatus::Failed) => ("Failed".to_string(), tone::DANGER),
        Some(McpServerConnectionStatus::Cancelled) => ("Cancelled".to_string(), tone::WARNING),
        Some(McpServerConnectionStatus::Disabled) => ("Disabled".to_string(), tone::NEUTRAL),
        Some(McpServerConnectionStatus::NotStarted) => ("Not started".to_string(), tone::NEUTRAL),
        None => status.map_or_else(|| ("Not started".to_string(), tone::NEUTRAL), inferred),
    }
}

fn auth_label(status: McpAuthStatus) -> Option<&'static str> {
    match status {
        McpAuthStatus::Unknown | McpAuthStatus::Unsupported => None,
        McpAuthStatus::NotLoggedIn => Some("not signed in"),
        McpAuthStatus::BearerToken => Some("bearer token"),
        McpAuthStatus::OAuth => Some("signed in with OAuth"),
    }
}

/// Rows of the MCP page, sorted by server name.
fn mcp_rows(
    inputs: &McpInputs<'_>,
    live: &HashMap<String, LiveStatus>,
    busy: &HashSet<String>,
    errors: &HashMap<String, String>,
    oauth: &HashSet<String>,
    pending: &PendingValues<bool>,
    server_ready: bool,
) -> Vec<ListItem> {
    let configured = inputs
        .effective
        .get("mcp_servers")
        .and_then(Value::as_object);
    let user_servers = inputs.user.get("mcp_servers").and_then(Value::as_object);
    let mut names: BTreeSet<String> = inputs
        .statuses
        .iter()
        .map(|status| status.name.clone())
        .collect();
    if let Some(configured) = configured {
        names.extend(configured.keys().cloned());
    }
    names
        .into_iter()
        .map(|name| {
            let status = inputs.statuses.iter().find(|status| status.name == name);
            let config = configured.and_then(|servers| servers.get(&name));
            let in_user = user_servers.is_some_and(|servers| servers.contains_key(&name));
            let enabled = config
                .and_then(|config| config.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let (status_text, status_tone) = status_label(live.get(&name), status, enabled);
            let source = if let Some(plugin) = status.and_then(|status| status.plugin_id.as_ref()) {
                format!("Plugin {plugin}")
            } else if in_user {
                "User config".to_string()
            } else {
                model::origins_for(inputs.origins, &["mcp_servers", name.as_str()])
                    .first()
                    .map_or_else(String::new, |metadata| model::origin_label(&metadata.name))
            };
            let mut detail = Vec::new();
            if let Some(status) = status {
                let connected = status.runtime_status == Some(McpServerConnectionStatus::Connected);
                if status.tools_error.is_none() && (connected || !status.tools.is_empty()) {
                    detail.push(match status.tools.len() {
                        1 => "1 tool".to_string(),
                        count => format!("{count} tools"),
                    });
                }
                if let Some(auth) = auth_label(status.auth_status) {
                    detail.push(format!("auth: {auth}"));
                }
                if let Some(info) = status.server_info.as_ref() {
                    detail.push(format!("{} {}", info.name, info.version));
                }
            }
            let mut error = errors.get(&name).cloned().unwrap_or_default();
            if error.is_empty() {
                error = live
                    .get(&name)
                    .and_then(|live| live.error.clone())
                    .or_else(|| status.and_then(|status| status.tools_error.clone()))
                    .unwrap_or_default();
            }
            let needs_login = enabled
                && status.is_some_and(|status| {
                    matches!(status.auth_status, McpAuthStatus::NotLoggedIn)
                        || status.runtime_status
                            == Some(McpServerConnectionStatus::AuthenticationRequired)
                });
            let is_http = config.is_some_and(|config| config.get("url").is_some())
                || status.is_some_and(|status| status.http_origin.is_some());
            let signing_in = oauth.contains(&name);
            ListItem {
                id: name.clone(),
                title: name.clone(),
                subtitle: transport_summary(config, status),
                status: if signing_in {
                    "Waiting for browser sign-in".to_string()
                } else {
                    status_text
                },
                status_tone: if signing_in {
                    tone::ACCENT
                } else {
                    status_tone
                },
                tag: source,
                detail: detail.join(" · "),
                error,
                toggle: in_user.then(|| pending.get(&name).copied().unwrap_or(enabled)),
                toggle_enabled: server_ready,
                action: (needs_login && is_http && server_ready && !signing_in)
                    .then(|| ("Sign in".to_string(), "login".to_string())),
                secondary: in_user.then(|| ("Remove".to_string(), "remove".to_string())),
                secondary_danger: false,
                busy: busy.contains(&name),
                ..ListItem::default()
            }
        })
        .collect()
}

impl AppController {
    pub(super) fn settings_mcp_bind(&mut self) {
        self.window
            .global::<SettingsState>()
            .on_mcp_form_submit(|| crate::ui_thread::with_app(AppController::settings_mcp_add));
    }

    pub(super) fn settings_mcp_activate(&mut self) {
        if self.settings.mcp.load == LoadState::Stale {
            self.settings_mcp_load();
        }
        self.settings_mcp_refresh_rows();
    }

    /// The server stopped or restarted: sign-ins it was waiting for and
    /// requests to it will not finish.
    pub(super) fn settings_mcp_on_server_reset(&mut self) {
        self.settings.mcp.reset_for_new_server();
        self.settings_mcp_refresh_rows();
    }

    fn settings_mcp_load(&mut self) {
        if self.settings.server_error.is_some() {
            return;
        }
        self.settings.mcp.load = LoadState::Loading;
        self.settings.mcp.generation += 1;
        let generation = self.settings.mcp.generation;
        let backend = self.backend.clone();
        self.backend.spawn(async move {
            let mut statuses = Vec::new();
            let mut cursor = None;
            let mut error = None;
            for _ in 0..MAX_PAGES {
                let request = ClientRequest::McpServerStatusList {
                    request_id: backend.next_request_id(),
                    params: ListMcpServerStatusParams {
                        cursor: cursor.take(),
                        limit: Some(PAGE_SIZE),
                        detail: Some(McpServerStatusDetail::ToolsAndAuthOnly),
                        thread_id: None,
                        server_name: None,
                    },
                };
                match backend
                    .request::<ListMcpServerStatusResponse>(request)
                    .await
                {
                    Ok(page) => {
                        statuses.extend(page.data);
                        cursor = page.next_cursor;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    Err(err) => {
                        error = Some(err.user_message());
                        break;
                    }
                }
            }
            crate::ui_thread::post(move |app| {
                if app.settings.mcp.generation != generation {
                    return;
                }
                let mcp = &mut app.settings.mcp;
                mcp.load = LoadState::Loaded;
                mcp.statuses = statuses;
                mcp.error = error;
                // Fresh statuses supersede earlier live updates.
                mcp.live.clear();
                app.settings_mcp_refresh_rows();
            });
        });
    }

    /// Reloads MCP config into running threads, then re-lists servers.
    pub(super) fn settings_mcp_reload_servers(&mut self) {
        self.settings.mcp.load = LoadState::Loading;
        self.settings_mcp_refresh_rows();
        self.backend.call(
            |request_id| ClientRequest::McpServerRefresh {
                request_id,
                params: None,
            },
            |app, result: Result<McpServerRefreshResponse, BackendError>| {
                app.settings.mcp.reload_error = result.err().map(|err| err.user_message());
                app.settings_mcp_load();
            },
        );
    }

    pub(super) fn settings_mcp_refresh_rows(&mut self) {
        let empty = Value::Null;
        let empty_origins = HashMap::new();
        let snapshot = self.settings.snapshot.as_ref();
        let mcp = &self.settings.mcp;
        let inputs = McpInputs {
            statuses: &mcp.statuses,
            effective: snapshot.map_or(&empty, |snapshot| &snapshot.effective),
            user: snapshot.map_or(&empty, |snapshot| &snapshot.user_config),
            origins: snapshot.map_or(&empty_origins, |snapshot| &snapshot.origins),
        };
        let server_ready = self.settings.server_error.is_none() && self.backend.is_ready();
        let rows = mcp_rows(
            &inputs,
            &mcp.live,
            &mcp.busy,
            &mcp.errors,
            &mcp.oauth,
            &mcp.pending,
            server_ready,
        );
        let status = mcp_status(
            mcp.error.as_deref(),
            mcp.reload_error.as_deref(),
            mcp.load == LoadState::Loading,
            rows.is_empty(),
        );
        let force = std::mem::take(&mut self.settings.mcp.force);
        let mut revision = self.settings.revision;
        self.settings
            .mcp
            .items
            .sync_items(rows, &force, &mut revision);
        self.settings.revision = revision;
        self.window
            .global::<SettingsState>()
            .set_mcp_status(status.into());
    }

    pub(super) fn settings_mcp_on_status(&mut self, updated: &McpServerStatusUpdatedNotification) {
        self.settings.mcp.live.insert(
            updated.name.clone(),
            LiveStatus {
                state: updated.status,
                error: updated.error.clone(),
            },
        );
        if self.settings.mcp.load != LoadState::Stale {
            self.settings_mcp_refresh_rows();
        }
    }

    pub(super) fn settings_mcp_on_oauth_completed(
        &mut self,
        completed: &McpServerOauthLoginCompletedNotification,
    ) {
        if !self.settings.mcp.oauth.remove(&completed.name) {
            return;
        }
        if completed.success {
            self.toast(format!("Signed in to {}", completed.name));
            self.settings.mcp.errors.remove(&completed.name);
            self.settings_mcp_reload_servers();
        } else {
            self.settings.mcp.errors.insert(
                completed.name.clone(),
                format!(
                    "Sign-in failed: {}",
                    completed
                        .error
                        .clone()
                        .unwrap_or_else(|| "unknown error".to_string())
                ),
            );
            self.settings_mcp_refresh_rows();
        }
    }

    pub(super) fn settings_mcp_toggle(&mut self, name: &str, on: bool) {
        // `enabled` defaults to true, so enabling removes the key.
        let value = if on { Value::Null } else { Value::Bool(false) };
        let edit = replace_edit(model::key_path(&["mcp_servers", name, "enabled"]), value);
        self.settings.mcp.pending.begin(name, on);
        self.settings_mcp_write(name, vec![edit], None);
    }

    pub(super) fn settings_mcp_action(&mut self, name: &str, action: &str) {
        match action {
            "login" => self.settings_mcp_login(name),
            "remove" => {
                let name = name.to_string();
                self.show_dialog(
                    DialogRequest::confirm(
                        format!("Remove {name}?"),
                        "The server is deleted from your config.toml. Threads lose its tools after the reload.",
                    )
                    .accept_label("Remove")
                    .destructive(),
                    Box::new(move |app, accepted| {
                        if accepted.is_some() {
                            let edit = replace_edit(model::key_path(&["mcp_servers", name.as_str()]), Value::Null);
                            app.settings_mcp_write(&name, vec![edit], Some(format!("Removed {name}")));
                        }
                    }),
                );
            }
            _ => {}
        }
    }

    fn settings_mcp_write(
        &mut self,
        name: &str,
        edits: Vec<codex_app_server_protocol::ConfigEdit>,
        success_toast: Option<String>,
    ) {
        let name = name.to_string();
        self.settings.mcp.busy.insert(name.clone());
        self.settings.mcp.errors.remove(&name);
        self.settings_mcp_refresh_rows();
        self.settings_write(
            edits,
            Box::new(move |app, result| {
                let generation = app.settings.snapshot_generation;
                let mcp = &mut app.settings.mcp;
                mcp.busy.remove(&name);
                match result {
                    Ok(_) => {
                        // The switch keeps the saved value until the config
                        // re-read has it.
                        mcp.pending.saved(&name, generation);
                        if let Some(message) = success_toast {
                            app.toast(message);
                        }
                        app.settings_mcp_reload_servers();
                    }
                    Err(message) => {
                        mcp.pending.cancel(&name);
                        mcp.errors.insert(name.clone(), message);
                        // Republish so the switch snaps back.
                        mcp.force.insert(name);
                        app.settings_mcp_refresh_rows();
                    }
                }
            }),
        );
    }

    fn settings_mcp_login(&mut self, name: &str) {
        let name = name.to_string();
        self.settings.mcp.oauth.insert(name.clone());
        self.settings.mcp.errors.remove(&name);
        self.settings_mcp_refresh_rows();
        let request_name = name.clone();
        self.backend.call(
            move |request_id| ClientRequest::McpServerOauthLogin {
                request_id,
                params: McpServerOauthLoginParams {
                    name: request_name,
                    thread_id: None,
                    client_registration: None,
                    scopes: None,
                    timeout_secs: None,
                },
            },
            move |app, result: Result<McpServerOauthLoginResponse, BackendError>| {
                match result {
                    Ok(response) => {
                        if let Err(err) = webbrowser::open(&response.authorization_url) {
                            app.copy_to_clipboard(&response.authorization_url);
                            app.settings.mcp.errors.insert(
                                name,
                                format!(
                                    "Could not open a browser ({err}); the sign-in link was copied."
                                ),
                            );
                        }
                    }
                    Err(err) => {
                        app.settings.mcp.oauth.remove(&name);
                        app.settings.mcp.errors.insert(
                            name,
                            format!("Could not start sign-in: {}", err.user_message()),
                        );
                    }
                }
                app.settings_mcp_refresh_rows();
            },
        );
    }

    fn settings_mcp_add(&mut self) {
        let state = self.window.global::<SettingsState>();
        let form = McpForm {
            name: state.get_mcp_form_name().to_string(),
            http: state.get_mcp_form_kind() == 1,
            command: state.get_mcp_form_command().to_string(),
            args: state.get_mcp_form_args().to_string(),
            env: state.get_mcp_form_env().to_string(),
            url: state.get_mcp_form_url().to_string(),
            token_env: state.get_mcp_form_token_env().to_string(),
        };
        let (name, server) = match build_server(&form) {
            Ok(built) => built,
            Err(message) => {
                state.set_mcp_form_error(message.into());
                return;
            }
        };
        let exists = self
            .settings
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                model::lookup(&snapshot.effective, &["mcp_servers", name.as_str()])
            })
            .is_some()
            || self
                .settings
                .mcp
                .statuses
                .iter()
                .any(|status| status.name == name);
        if exists {
            state.set_mcp_form_error(format!("A server named {name} already exists.").into());
            return;
        }
        state.set_mcp_form_error(SharedString::new());
        state.set_mcp_form_busy(true);
        let edit = replace_edit(model::key_path(&["mcp_servers", name.as_str()]), server);
        self.settings_write(
            vec![edit],
            Box::new(move |app, result| {
                let state = app.window.global::<SettingsState>();
                state.set_mcp_form_busy(false);
                match result {
                    Ok(_) => {
                        for setter in [
                            SettingsState::set_mcp_form_name,
                            SettingsState::set_mcp_form_command,
                            SettingsState::set_mcp_form_args,
                            SettingsState::set_mcp_form_env,
                            SettingsState::set_mcp_form_url,
                            SettingsState::set_mcp_form_token_env,
                        ] {
                            setter(&state, SharedString::new());
                        }
                        state.set_mcp_form_open(false);
                        app.toast(format!("Added {name}"));
                        app.settings_mcp_reload_servers();
                    }
                    Err(message) => state.set_mcp_form_error(message.into()),
                }
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn form() -> McpForm {
        McpForm {
            name: "docs".to_string(),
            command: "npx".to_string(),
            args: r#"-y "@scope/server docs""#.to_string(),
            env: r#"API_KEY=abc MODE="a b""#.to_string(),
            ..McpForm::default()
        }
    }

    #[test]
    fn builds_stdio_servers() {
        assert_eq!(
            build_server(&form()),
            Ok((
                "docs".to_string(),
                json!({
                    "command": "npx",
                    "args": ["-y", "@scope/server docs"],
                    "env": {"API_KEY": "abc", "MODE": "a b"},
                })
            ))
        );
        let minimal = McpForm {
            args: String::new(),
            env: String::new(),
            ..form()
        };
        assert_eq!(
            build_server(&minimal),
            Ok(("docs".to_string(), json!({"command": "npx"})))
        );
    }

    #[test]
    fn builds_http_servers() {
        let http = McpForm {
            name: "remote".to_string(),
            http: true,
            url: "https://example.com/mcp".to_string(),
            token_env: "EXAMPLE_TOKEN".to_string(),
            ..McpForm::default()
        };
        assert_eq!(
            build_server(&http),
            Ok((
                "remote".to_string(),
                json!({"url": "https://example.com/mcp", "bearer_token_env_var": "EXAMPLE_TOKEN"})
            ))
        );
    }

    #[test]
    fn rejects_invalid_forms() {
        let bad = |change: fn(&mut McpForm)| {
            let mut form = form();
            change(&mut form);
            build_server(&form).expect_err("invalid")
        };
        assert_eq!(
            bad(|form| form.name = String::new()),
            "Enter a name for the server."
        );
        assert!(bad(|form| form.name = "a b".to_string()).starts_with("Use only"));
        assert_eq!(
            bad(|form| form.command = " ".to_string()),
            "Enter the command that starts the server."
        );
        assert_eq!(
            bad(|form| form.env = "NOVALUE".to_string()),
            "`NOVALUE` is not KEY=value."
        );
        assert!(
            bad(|form| form.env = "1X=a".to_string()).contains("not a valid environment variable")
        );
        assert!(bad(|form| form.args = "\"open".to_string()).contains("unbalanced"));
        assert_eq!(
            bad(|form| {
                form.http = true;
                form.url = "ftp://x".to_string();
            }),
            "Enter an http:// or https:// URL."
        );
    }

    fn status(
        name: &str,
        runtime: Option<McpServerConnectionStatus>,
        auth: McpAuthStatus,
    ) -> McpServerStatus {
        serde_json::from_value(json!({
            "name": name,
            "runtimeStatus": runtime,
            "pluginId": null,
            "httpOrigin": null,
            "serverInfo": null,
            "serverCapabilities": null,
            "tools": {},
            "toolsError": null,
            "resources": [],
            "resourceTemplates": [],
            "authStatus": auth,
        }))
        .expect("status")
    }

    #[test]
    fn rows_merge_config_status_and_live_updates() {
        let effective = json!({"mcp_servers": {
            "local": {"command": "srv", "args": ["--port", "1"]},
            "remote": {"url": "https://x/mcp"},
            "off": {"command": "x", "enabled": false},
        }});
        let user = json!({"mcp_servers": {"local": {}, "remote": {}, "off": {}}});
        let statuses = vec![
            status(
                "local",
                Some(McpServerConnectionStatus::Connected),
                McpAuthStatus::Unsupported,
            ),
            status(
                "remote",
                Some(McpServerConnectionStatus::AuthenticationRequired),
                McpAuthStatus::NotLoggedIn,
            ),
        ];
        let origins = HashMap::new();
        let inputs = McpInputs {
            statuses: &statuses,
            effective: &effective,
            user: &user,
            origins: &origins,
        };
        let mut live = HashMap::new();
        live.insert(
            "local".to_string(),
            LiveStatus {
                state: McpServerStartupState::Failed,
                error: Some("exited".to_string()),
            },
        );
        let rows = mcp_rows(
            &inputs,
            &live,
            &HashSet::new(),
            &HashMap::new(),
            &HashSet::new(),
            &PendingValues::default(),
            true,
        );
        let names: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(names, vec!["local", "off", "remote"]);
        assert_eq!(rows[0].subtitle, "srv --port 1");
        assert_eq!(
            (rows[0].status.as_str(), rows[0].status_tone),
            ("Failed", tone::DANGER)
        );
        assert_eq!(rows[0].error, "exited");
        assert_eq!(rows[0].toggle, Some(true));
        assert_eq!(rows[1].status, "Disabled");
        assert_eq!(rows[1].toggle, Some(false));
        assert_eq!(rows[2].status, "Sign-in required");
        assert_eq!(
            rows[2].action,
            Some(("Sign in".to_string(), "login".to_string()))
        );
        assert_eq!(rows[2].detail, "auth: not signed in");
        assert_eq!(rows[0].detail, "0 tools");
        assert_eq!(
            rows[2].secondary,
            Some(("Remove".to_string(), "remove".to_string()))
        );
    }

    #[test]
    fn reload_failures_stay_visible_after_the_list_refreshes() {
        assert_eq!(
            mcp_status(
                /*list_error*/ None,
                Some("server x failed"),
                /*loading*/ false,
                /*empty*/ false
            ),
            "Could not reload MCP servers: server x failed"
        );
        assert_eq!(
            mcp_status(Some("boom"), Some("server x failed"), false, false),
            "Could not list MCP servers: boom"
        );
        assert_eq!(mcp_status(None, None, true, true), "Loading MCP servers…");
        assert_eq!(mcp_status(None, None, false, true), "");
    }

    #[test]
    fn a_server_restart_ends_pending_sign_ins() {
        let mut state = McpState::default();
        state.oauth.insert("remote".to_string());
        state.busy.insert("local".to_string());
        state.pending.begin("local", false);
        state.reload_error = Some("x".to_string());
        state.load = LoadState::Loaded;
        state.reset_for_new_server();
        assert!(state.oauth.is_empty());
        assert!(state.busy.is_empty());
        assert_eq!(state.pending.get("local"), None);
        assert_eq!(state.reload_error, None);
        assert_eq!(state.load, LoadState::Stale);
    }

    #[test]
    fn switches_show_the_value_being_saved() {
        let effective = json!({"mcp_servers": {"local": {"command": "srv"}}});
        let user = effective.clone();
        let origins = HashMap::new();
        let inputs = McpInputs {
            statuses: &[],
            effective: &effective,
            user: &user,
            origins: &origins,
        };
        let mut pending = PendingValues::default();
        pending.begin("local", false);
        let rows = |pending: &PendingValues<bool>| {
            mcp_rows(
                &inputs,
                &HashMap::new(),
                &HashSet::new(),
                &HashMap::new(),
                &HashSet::new(),
                pending,
                true,
            )
        };
        assert_eq!(rows(&pending)[0].toggle, Some(false));
        pending.saved("local", 4);
        pending.settle(4);
        assert_eq!(rows(&pending)[0].toggle, Some(false), "until a newer read");
        pending.settle(5);
        assert_eq!(rows(&pending)[0].toggle, Some(true));
    }
}
