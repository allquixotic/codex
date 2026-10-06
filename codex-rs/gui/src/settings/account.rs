//! Settings › Account: who Codex is signed in as, ChatGPT browser and
//! device-code sign-in, API key sign-in, sign out, and usage limits.
//!
//! The app-server fixes its model provider at startup, so a login or logout
//! that changes the effective `model_provider` (signing out of Bedrock
//! clears it) restarts the embedded server, like the TUI does.

use codex_app_server_protocol::Account;
use codex_app_server_protocol::AccountLoginCompletedNotification;
use codex_app_server_protocol::CancelLoginAccountParams;
use codex_app_server_protocol::CancelLoginAccountResponse;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::GetAccountParams;
use codex_app_server_protocol::GetAccountRateLimitsResponse;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::LoginAccountParams;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::LogoutAccountResponse;
use codex_app_server_protocol::RateLimitSnapshot;
use codex_app_server_protocol::RateLimitWindow;
use codex_protocol::config_types::ForcedLoginMethod;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use super::LoadState;
use super::kv;
use super::model;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::backend::BackendError;
use crate::ui::KeyValue;
use crate::ui::SettingsState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoginKind {
    Browser,
    DeviceCode,
    ApiKey,
}

#[derive(Clone, Debug)]
struct PendingLogin {
    kind: LoginKind,
    /// Known once `account/login/start` returns (browser and device code).
    login_id: Option<String>,
    url: String,
    code: String,
}

#[derive(Default)]
pub(crate) struct AccountState {
    pub(super) load: LoadState,
    account: Option<GetAccountResponse>,
    error: Option<String>,
    notice: Option<String>,
    login: Option<PendingLogin>,
    rate_limits: Vec<(String, String)>,
    rate_limit_status: String,
}

/// "5-hour window", "weekly window", ...
pub(crate) fn window_label(minutes: Option<i64>) -> String {
    match minutes {
        None | Some(0) => "Window".to_string(),
        Some(10_080) => "Weekly window".to_string(),
        Some(1_440) => "Daily window".to_string(),
        Some(minutes) if minutes % 1_440 == 0 => format!("{}-day window", minutes / 1_440),
        Some(minutes) if minutes % 60 == 0 => format!("{}-hour window", minutes / 60),
        Some(minutes) => format!("{minutes}-minute window"),
    }
}

/// "resets in 2h 5m" relative to `now` (Unix seconds).
pub(crate) fn reset_text(resets_at: Option<i64>, now: i64) -> Option<String> {
    let remaining = resets_at? - now;
    if remaining <= 0 {
        return Some("resets now".to_string());
    }
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3_600;
    let minutes = (remaining % 3_600) / 60;
    Some(match (days, hours, minutes) {
        (0, 0, 0) => "resets in under a minute".to_string(),
        (0, 0, minutes) => format!("resets in {minutes}m"),
        (0, hours, minutes) => format!("resets in {hours}h {minutes}m"),
        (days, hours, _) => format!("resets in {days}d {hours}h"),
    })
}

fn window_row(name: &str, window: &RateLimitWindow, now: i64) -> (String, String) {
    let mut value = format!("{}% used", window.used_percent.clamp(0, 100));
    if let Some(reset) = reset_text(window.resets_at, now) {
        value.push_str(" · ");
        value.push_str(&reset);
    }
    (
        format!(
            "{name}, {}",
            window_label(window.window_duration_mins).to_lowercase()
        ),
        value,
    )
}

fn snapshot_rows(snapshot: &RateLimitSnapshot, now: i64, out: &mut Vec<(String, String)>) {
    let name = snapshot
        .limit_name
        .clone()
        .or_else(|| snapshot.limit_id.clone())
        .map_or_else(|| "Usage".to_string(), |name| capitalize(&name));
    if let Some(primary) = &snapshot.primary {
        out.push(window_row(&name, primary, now));
    }
    if let Some(secondary) = &snapshot.secondary {
        out.push(window_row(&name, secondary, now));
    }
    if let Some(credits) = &snapshot.credits {
        let value = if credits.unlimited {
            "Unlimited".to_string()
        } else if credits.has_credits {
            credits.balance.clone().map_or_else(
                || "Available".to_string(),
                |balance| format!("{balance} remaining"),
            )
        } else {
            "None".to_string()
        };
        out.push((format!("{name}, credits"), value));
    }
    if snapshot.rate_limit_reached_type.is_some() {
        out.push((format!("{name}, status"), "Limit reached".to_string()));
    }
}

/// Key/value rows describing usage limits.
pub(crate) fn rate_limit_rows(
    response: &GetAccountRateLimitsResponse,
    now: i64,
) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    match response
        .rate_limits_by_limit_id
        .as_ref()
        .filter(|limits| !limits.is_empty())
    {
        Some(limits) => {
            let mut ids: Vec<&String> = limits.keys().collect();
            // The Codex limit first, then the rest by id.
            ids.sort_by_key(|id| (id.as_str() != "codex", (*id).clone()));
            for id in ids {
                snapshot_rows(&limits[id], now, &mut rows);
            }
        }
        None => snapshot_rows(&response.rate_limits, now, &mut rows),
    }
    rows
}

fn capitalize(text: &str) -> String {
    let text = text.replace('_', " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Title, detail, and facts shown for an `account/read` result.
pub(crate) fn describe_account(
    response: &GetAccountResponse,
    provider: Option<&str>,
) -> (String, String, Vec<(String, String)>) {
    let mut facts = Vec::new();
    if let Some(provider) = provider {
        facts.push(("Model provider".to_string(), provider.to_string()));
    }
    let (title, detail) = match &response.account {
        None if response.requires_openai_auth => (
            "Not signed in".to_string(),
            "Sign in with ChatGPT or an API key to use OpenAI models.".to_string(),
        ),
        None => (
            "No OpenAI sign-in needed".to_string(),
            "The current model provider does not use an OpenAI account.".to_string(),
        ),
        Some(Account::ApiKey {}) => (
            "Signed in with an API key".to_string(),
            "Usage is billed to your OpenAI Platform account.".to_string(),
        ),
        Some(Account::Chatgpt { email, plan_type }) => {
            let plan = capitalize(&model::wire_name(plan_type));
            facts.push(("Plan".to_string(), plan));
            if let Some(email) = email {
                facts.push(("Email".to_string(), email.clone()));
            }
            (
                "Signed in with ChatGPT".to_string(),
                email.clone().unwrap_or_default(),
            )
        }
        Some(Account::AmazonBedrock {
            uses_codex_managed_credentials,
        }) => (
            "Using Amazon Bedrock".to_string(),
            if *uses_codex_managed_credentials {
                "Codex stores your Bedrock credentials. Signing out removes them.".to_string()
            } else {
                "Credentials come from your AWS configuration.".to_string()
            },
        ),
    };
    (title, detail, facts)
}

fn to_kv(rows: &[(String, String)]) -> ModelRc<KeyValue> {
    let rows: Vec<KeyValue> = rows
        .iter()
        .map(|(key, value)| kv(key.as_str(), value.as_str()))
        .collect();
    ModelRc::new(VecModel::from(rows))
}

impl AppController {
    pub(super) fn settings_account_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.on_account_login(|kind| {
            let kind = if kind == "device" {
                LoginKind::DeviceCode
            } else {
                LoginKind::Browser
            };
            crate::ui_thread::with_app(move |app| app.settings_account_login(kind, None));
        });
        state.on_account_api_key(|key| {
            let key = key.trim().to_string();
            crate::ui_thread::with_app(move |app| {
                if !key.is_empty() {
                    app.settings_account_login(LoginKind::ApiKey, Some(key));
                }
            });
        });
        state.on_account_cancel_login(|| {
            crate::ui_thread::with_app(AppController::settings_account_cancel_login);
        });
        state.on_account_logout(|| {
            crate::ui_thread::with_app(AppController::settings_account_confirm_logout)
        });
        state.on_account_refresh(|| {
            crate::ui_thread::with_app(|app| {
                app.settings.account.error = None;
                app.settings.account.notice = None;
                app.settings_account_refresh();
            });
        });
    }

    pub(super) fn settings_account_activate(&mut self) {
        if self.settings.account.load == LoadState::Stale {
            self.settings_account_refresh();
        }
        self.settings_account_render();
    }

    pub(super) fn settings_account_on_server_ready(&mut self) {
        // A login cannot survive a server restart.
        self.settings.account.login = None;
        self.settings.account.load = LoadState::Stale;
        self.settings_account_render();
    }

    pub(super) fn settings_account_on_server_failed(&mut self) {
        self.settings.account.login = None;
        self.settings.account.load = LoadState::Stale;
        self.settings_account_render();
    }

    fn settings_account_refresh(&mut self) {
        if self.settings.server_error.is_some() {
            return;
        }
        self.settings.account.load = LoadState::Loading;
        self.settings_account_render();
        self.backend.call(
            |request_id| ClientRequest::GetAccount {
                request_id,
                params: GetAccountParams {
                    refresh_token: false,
                },
            },
            |app, result: Result<GetAccountResponse, BackendError>| {
                app.settings.account.load = LoadState::Loaded;
                match result {
                    Ok(response) => {
                        let chatgpt = matches!(response.account, Some(Account::Chatgpt { .. }));
                        app.settings.account.account = Some(response);
                        if chatgpt {
                            app.settings_account_load_rate_limits();
                        } else {
                            app.settings.account.rate_limits.clear();
                            app.settings.account.rate_limit_status =
                                "Usage limits are shown for ChatGPT sign-ins.".to_string();
                        }
                    }
                    Err(err) => {
                        app.settings.account.error = Some(format!(
                            "Could not read the account: {}",
                            err.user_message()
                        ));
                    }
                }
                app.settings_account_render();
            },
        );
    }

    fn settings_account_load_rate_limits(&mut self) {
        self.settings.account.rate_limit_status = "Loading usage limits…".to_string();
        self.backend.call(
            |request_id| ClientRequest::GetAccountRateLimits {
                request_id,
                params: None,
            },
            |app, result: Result<GetAccountRateLimitsResponse, BackendError>| {
                match result {
                    Ok(response) => {
                        app.settings.account.rate_limits = rate_limit_rows(&response, now_secs());
                        app.settings.account.rate_limit_status = String::new();
                    }
                    Err(err) => {
                        app.settings.account.rate_limits.clear();
                        app.settings.account.rate_limit_status =
                            format!("Usage limits are unavailable: {}", err.user_message());
                    }
                }
                app.settings_account_render();
            },
        );
    }

    pub(super) fn settings_account_on_rate_limits(&mut self, snapshot: &RateLimitSnapshot) {
        let mut rows = Vec::new();
        snapshot_rows(snapshot, now_secs(), &mut rows);
        if rows.is_empty() {
            return;
        }
        // Replace the rows of this limit, keep the others.
        let prefix = snapshot
            .limit_name
            .clone()
            .or_else(|| snapshot.limit_id.clone())
            .map_or_else(|| "Usage".to_string(), |name| capitalize(&name));
        let account = &mut self.settings.account;
        account
            .rate_limits
            .retain(|(key, _)| !key.starts_with(&format!("{prefix},")));
        account.rate_limits.extend(rows);
        account.rate_limit_status.clear();
        self.settings_account_render();
    }

    /// Whether ChatGPT and API key sign-in are allowed by requirements and
    /// `forced_login_method`.
    fn settings_account_allowed(&self) -> (bool, bool) {
        let mut chatgpt = true;
        let mut api = true;
        if let Some(allowed) = self
            .settings
            .requirements
            .as_ref()
            .and_then(|requirements| requirements.allowed_login_methods.as_ref())
        {
            chatgpt = allowed.contains(&ForcedLoginMethod::Chatgpt);
            api = allowed.contains(&ForcedLoginMethod::Api);
        }
        match self
            .settings
            .snapshot
            .as_ref()
            .and_then(|snapshot| model::lookup(&snapshot.effective, &["forced_login_method"]))
            .and_then(serde_json::Value::as_str)
        {
            Some("chatgpt") => api = false,
            Some("api") => chatgpt = false,
            _ => {}
        }
        (chatgpt, api)
    }

    pub(super) fn settings_account_apply_requirements(&mut self) {
        self.settings_account_render();
    }

    fn settings_account_render(&self) {
        let account = &self.settings.account;
        let state = self.window.global::<SettingsState>();
        let provider = self
            .config
            .as_ref()
            .map(|config| config.model_provider_id.as_str());
        let (kind, title, detail, facts) = match (&account.account, account.load) {
            (_, _) if self.settings.server_error.is_some() => (
                "error",
                "Account unavailable".to_string(),
                "Codex is not running.".to_string(),
                Vec::new(),
            ),
            (Some(response), _) => {
                let (title, detail, facts) = describe_account(response, provider);
                let kind = match &response.account {
                    None => "none",
                    Some(Account::ApiKey {}) => "apikey",
                    Some(Account::Chatgpt { .. }) => "chatgpt",
                    Some(Account::AmazonBedrock { .. }) => "bedrock",
                };
                (kind, title, detail, facts)
            }
            (None, LoadState::Loaded) => (
                "error",
                "Account unavailable".to_string(),
                String::new(),
                Vec::new(),
            ),
            (None, _) => (
                "loading",
                "Loading account…".to_string(),
                String::new(),
                Vec::new(),
            ),
        };
        state.set_account_kind(kind.into());
        state.set_account_title(title.into());
        state.set_account_detail(detail.into());
        state.set_account_facts(to_kv(&facts));
        state.set_can_logout(
            account
                .account
                .as_ref()
                .is_some_and(|response| response.account.is_some()),
        );
        let (chatgpt, api) = self.settings_account_allowed();
        state.set_chatgpt_login_allowed(chatgpt);
        state.set_api_login_allowed(api);
        state.set_account_busy(account.load == LoadState::Loading);
        state.set_account_error(account.error.clone().unwrap_or_default().into());
        state.set_account_notice(account.notice.clone().unwrap_or_default().into());
        let (login_state, url, code) = match &account.login {
            None => ("", String::new(), String::new()),
            Some(login) if login.login_id.is_none() => ("starting", String::new(), String::new()),
            Some(login) => match login.kind {
                LoginKind::Browser => ("browser", login.url.clone(), String::new()),
                LoginKind::DeviceCode => ("device", login.url.clone(), login.code.clone()),
                LoginKind::ApiKey => ("starting", String::new(), String::new()),
            },
        };
        state.set_login_state(login_state.into());
        state.set_login_url(url.into());
        state.set_login_code(code.into());
        state.set_rate_limits(to_kv(&account.rate_limits));
        state.set_rate_limits_status(SharedString::from(account.rate_limit_status.as_str()));
    }

    fn settings_account_login(&mut self, kind: LoginKind, api_key: Option<String>) {
        if self.settings.account.login.is_some() {
            return;
        }
        let params = match (kind, api_key) {
            (LoginKind::ApiKey, Some(api_key)) => LoginAccountParams::ApiKey { api_key },
            (LoginKind::ApiKey, None) => return,
            (LoginKind::Browser, _) => LoginAccountParams::Chatgpt {
                codex_streamlined_login: false,
                use_hosted_login_success_page: false,
                app_brand: None,
            },
            (LoginKind::DeviceCode, _) => LoginAccountParams::ChatgptDeviceCode,
        };
        self.settings.account.error = None;
        self.settings.account.notice = None;
        self.settings.account.login = Some(PendingLogin {
            kind,
            login_id: None,
            url: String::new(),
            code: String::new(),
        });
        self.settings_account_render();
        self.backend.call(
            move |request_id| ClientRequest::LoginAccount { request_id, params },
            move |app, result: Result<LoginAccountResponse, BackendError>| {
                app.settings_account_login_started(kind, result);
            },
        );
    }

    fn settings_account_login_started(
        &mut self,
        kind: LoginKind,
        result: Result<LoginAccountResponse, BackendError>,
    ) {
        let still_pending = self
            .settings
            .account
            .login
            .as_ref()
            .is_some_and(|login| login.kind == kind && login.login_id.is_none());
        match result {
            Ok(LoginAccountResponse::Chatgpt { login_id, auth_url }) => {
                if !still_pending {
                    // Cancelled before the server answered.
                    self.settings_account_cancel_on_server(login_id);
                    return;
                }
                if let Err(err) = webbrowser::open(&auth_url) {
                    self.settings.account.notice = Some(format!(
                        "Could not open a browser ({err}). Copy the link below into one."
                    ));
                }
                self.settings.account.login = Some(PendingLogin {
                    kind,
                    login_id: Some(login_id),
                    url: auth_url,
                    code: String::new(),
                });
            }
            Ok(LoginAccountResponse::ChatgptDeviceCode {
                login_id,
                verification_url,
                user_code,
            }) => {
                if !still_pending {
                    self.settings_account_cancel_on_server(login_id);
                    return;
                }
                self.settings.account.login = Some(PendingLogin {
                    kind,
                    login_id: Some(login_id),
                    url: verification_url,
                    code: user_code,
                });
            }
            Ok(_) => {
                // API key (and Bedrock) logins complete synchronously.
                self.settings.account.login = None;
                self.settings.account.notice = Some(match kind {
                    LoginKind::ApiKey => "API key saved.".to_string(),
                    _ => "Signed in.".to_string(),
                });
                self.settings_account_after_auth_change();
            }
            Err(err) => {
                self.settings.account.login = None;
                self.settings.account.error =
                    Some(format!("Could not sign in: {}", err.user_message()));
            }
        }
        self.settings_account_render();
    }

    fn settings_account_cancel_login(&mut self) {
        let Some(login) = self.settings.account.login.take() else {
            return;
        };
        if let Some(login_id) = login.login_id {
            self.settings_account_cancel_on_server(login_id);
        }
        self.settings_account_render();
    }

    fn settings_account_cancel_on_server(&self, login_id: String) {
        self.backend
            .fire::<CancelLoginAccountResponse, _>(move |request_id| {
                ClientRequest::CancelLoginAccount {
                    request_id,
                    params: CancelLoginAccountParams { login_id },
                }
            });
    }

    pub(super) fn settings_account_on_login_completed(
        &mut self,
        completed: &AccountLoginCompletedNotification,
    ) {
        let matches =
            self.settings.account.login.as_ref().is_some_and(|login| {
                login.login_id.is_some() && login.login_id == completed.login_id
            });
        if matches {
            self.settings.account.login = None;
            if completed.success {
                self.settings.account.notice = Some("Signed in.".to_string());
                self.settings_account_after_auth_change();
            } else {
                self.settings.account.error = Some(format!(
                    "Sign-in did not complete: {}",
                    completed
                        .error
                        .clone()
                        .unwrap_or_else(|| "cancelled or timed out".to_string())
                ));
            }
            self.settings_account_render();
        } else if completed.success {
            // Signed in elsewhere (for example the Providers page).
            self.settings.account.load = LoadState::Stale;
            if self.settings.page == "account" && self.settings_tab_active() {
                self.settings_account_refresh();
            }
        }
    }

    pub(super) fn settings_account_on_updated(&mut self) {
        self.settings.account.load = LoadState::Stale;
        if self.settings.page == "account" && self.settings_tab_active() {
            self.settings_account_refresh();
        }
    }

    fn settings_account_confirm_logout(&mut self) {
        let detail = match self
            .settings
            .account
            .account
            .as_ref()
            .and_then(|response| response.account.as_ref())
        {
            Some(Account::AmazonBedrock { .. }) => {
                "Codex removes its stored Bedrock credentials and the Bedrock provider settings it wrote, then restarts."
            }
            _ => "You will need to sign in again before using OpenAI models.",
        };
        self.show_dialog(
            DialogRequest::confirm("Sign out?", detail)
                .accept_label("Sign out")
                .destructive(),
            Box::new(|app, accepted| {
                if accepted.is_some() {
                    app.settings_account_logout();
                }
            }),
        );
    }

    fn settings_account_logout(&mut self) {
        self.settings.account.error = None;
        self.settings.account.notice = None;
        self.settings.account.load = LoadState::Loading;
        self.settings_account_render();
        self.backend.call(
            |request_id| ClientRequest::LogoutAccount {
                request_id,
                params: None,
            },
            |app, result: Result<LogoutAccountResponse, BackendError>| {
                match result {
                    Ok(_) => {
                        app.settings.account.notice = Some("Signed out.".to_string());
                        app.settings_account_after_auth_change();
                    }
                    Err(err) => {
                        app.settings.account.load = LoadState::Loaded;
                        app.settings.account.error =
                            Some(format!("Could not sign out: {}", err.user_message()));
                    }
                }
                app.settings_account_render();
            },
        );
    }

    /// Refreshes the account and restarts the server when the effective
    /// model provider no longer matches the one it started with.
    fn settings_account_after_auth_change(&mut self) {
        self.settings_account_refresh();
        self.settings_reload_config();
        let running = self
            .config
            .as_ref()
            .map(|config| config.model_provider_id.clone());
        self.backend.call(
            |request_id| ClientRequest::ConfigRead {
                request_id,
                params: ConfigReadParams {
                    include_layers: false,
                    cwd: None,
                },
            },
            move |app, result: Result<ConfigReadResponse, BackendError>| {
                let Ok(response) = result else {
                    return;
                };
                let provider = response
                    .config
                    .model_provider
                    .unwrap_or_else(|| "openai".to_string());
                if running.is_some_and(|running| running != provider) {
                    tracing::info!(provider, "model provider changed; restarting app-server");
                    app.toast("The model provider changed. Restarting Codex…");
                    app.backend.restart();
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn window_labels_and_reset_times() {
        assert_eq!(window_label(Some(300)), "5-hour window");
        assert_eq!(window_label(Some(10_080)), "Weekly window");
        assert_eq!(window_label(Some(2_880)), "2-day window");
        assert_eq!(window_label(Some(45)), "45-minute window");
        assert_eq!(window_label(None), "Window");
        assert_eq!(reset_text(None, 0), None);
        assert_eq!(reset_text(Some(10), 20), Some("resets now".to_string()));
        assert_eq!(
            reset_text(Some(30), 0),
            Some("resets in under a minute".to_string())
        );
        assert_eq!(
            reset_text(Some(7_500), 0),
            Some("resets in 2h 5m".to_string())
        );
        assert_eq!(reset_text(Some(600), 0), Some("resets in 10m".to_string()));
        assert_eq!(
            reset_text(Some(3 * 86_400 + 7_200), 0),
            Some("resets in 3d 2h".to_string())
        );
    }

    #[test]
    fn rate_limits_prefer_per_limit_snapshots() {
        let response: GetAccountRateLimitsResponse = serde_json::from_value(json!({
            "ordinaryUsageAllowed": true,
            "rateLimits": {
                "limitId": null, "limitName": null, "normalModelSlug": null,
                "primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": 3_600},
                "secondary": null, "credits": null, "individualLimit": null,
                "spendControlReached": null, "planType": null, "rateLimitReachedType": null
            },
            "rateLimitsByLimitId": null,
            "rateLimitResetCredits": null,
            "accountId": null
        }))
        .expect("response");
        assert_eq!(
            rate_limit_rows(&response, 0),
            vec![(
                "Usage, 5-hour window".to_string(),
                "10% used · resets in 1h 0m".to_string()
            )]
        );
    }

    #[test]
    fn accounts_are_described() {
        let chatgpt: GetAccountResponse = serde_json::from_value(json!({
            "account": {"type": "chatgpt", "email": "a@b.c", "planType": "pro"},
            "requiresOpenaiAuth": true,
        }))
        .expect("response");
        let (title, detail, facts) = describe_account(&chatgpt, Some("openai"));
        assert_eq!(title, "Signed in with ChatGPT");
        assert_eq!(detail, "a@b.c");
        assert_eq!(
            facts,
            vec![
                ("Model provider".to_string(), "openai".to_string()),
                ("Plan".to_string(), "Pro".to_string()),
                ("Email".to_string(), "a@b.c".to_string()),
            ]
        );
        let none: GetAccountResponse = serde_json::from_value(json!({
            "account": null,
            "requiresOpenaiAuth": true,
        }))
        .expect("response");
        assert_eq!(describe_account(&none, None).0, "Not signed in");
        let bedrock: GetAccountResponse = serde_json::from_value(json!({
            "account": {"type": "amazonBedrock", "usesCodexManagedCredentials": true},
            "requiresOpenaiAuth": false,
        }))
        .expect("response");
        assert_eq!(describe_account(&bedrock, None).0, "Using Amazon Bedrock");
    }
}
