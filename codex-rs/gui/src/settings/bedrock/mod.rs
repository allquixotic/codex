//! Providers page (Settings › Providers): the current model provider, Amazon
//! Bedrock setup for the Mantle or Runtime endpoint, switching back to
//! OpenAI, and local models from Ollama or LM Studio.
//!
//! A provider change runs as a [`flow::Flow`]: setup or login RPCs and
//! config writes, an advisory GovCloud check, then a restart of the embedded
//! server, because the app-server fixes its provider and model catalog at
//! startup. Every request runs off the UI thread; results come back tagged
//! with the id of the flow or scan that started them so stale answers are
//! dropped.

mod flow;
mod local;

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use codex_app_server_protocol::BedrockCheckGovCloudRequirementsParams;
use codex_app_server_protocol::BedrockCheckGovCloudRequirementsResponse;
use codex_app_server_protocol::BedrockDiscoverParams;
use codex_app_server_protocol::BedrockDiscoverResponse;
use codex_app_server_protocol::BedrockSetupResponse;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigBatchWriteParams;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::ConfigWriteResponse;
use codex_app_server_protocol::GetAccountParams;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::LogoutAccountResponse;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::WriteStatus;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use crate::app::AppController;
use crate::app::DialogRequest;
use crate::app::ThreadTab;
use crate::backend::BackendError;
use crate::ui::AppState;
use crate::ui::BedrockMethodItem;
use crate::ui::BedrockState;
use crate::ui::LocalModelItem;
use crate::ui::LocalServerView;

use self::flow::Endpoint;
use self::flow::Flow;
use self::flow::FlowKind;
use self::flow::FormContext;
use self::flow::FormInput;
use self::flow::Method;
use self::flow::ProviderStatus;
use self::flow::Step;
use self::local::LocalKind;
use self::local::LocalProbe;
use self::local::LocalState;
use self::local::PullUpdate;

/// Rescan credentials and local servers when the page is shown and the last
/// scan is older than this.
const RESCAN_AFTER: Duration = Duration::from_secs(30);
/// Same budget as the TUI wizard.
const GOV_CLOUD_CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// Profile validation may run `credential_process` or refresh an SSO token.
const PROFILE_VALIDATION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NoticeKind {
    Info,
    Success,
    Error,
}

impl NoticeKind {
    /// `Notice.kind` in `ui/bedrock_widgets.slint`.
    fn code(self) -> i32 {
        match self {
            Self::Info => 0,
            Self::Success => 1,
            Self::Error => 3,
        }
    }
}

/// Part of the page a notice or progress row belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Area {
    Bedrock,
    Status,
    Local,
    Model,
}

impl Area {
    /// `busy-area` / `notice-area` in `ui/bedrock.slint`.
    fn code(self) -> i32 {
        match self {
            Self::Bedrock => 0,
            Self::Status => 1,
            Self::Local => 2,
            Self::Model => 3,
        }
    }

    fn of_flow(kind: &FlowKind) -> Self {
        match kind {
            FlowKind::Bedrock { .. } => Self::Bedrock,
            FlowKind::Revert | FlowKind::Restart => Self::Status,
            FlowKind::Local { .. } => Self::Local,
        }
    }
}

/// Result message shown next to the action that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Notice {
    area: Area,
    kind: NoticeKind,
    text: String,
}

/// A running Ollama pull.
struct PullJob {
    id: u64,
    model: String,
    task: tokio::task::JoinHandle<()>,
}

/// Plain copy of one local server row, used to skip redundant model pushes.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalRow {
    kind: LocalKind,
    state: i32,
    status: String,
    message: String,
    can_use: bool,
    /// `(name, in use)`.
    models: Vec<(String, bool)>,
}

/// State of the Providers page. Lives on the UI thread inside
/// [`AppController`].
#[derive(Default)]
pub(crate) struct BedrockController {
    /// Source of ids that tag async results (scans, flows, validations).
    next_id: u64,
    status: Option<ProviderStatus>,
    status_error: Option<String>,
    status_loading: usize,
    account: Option<GetAccountResponse>,

    discovered: Option<BedrockDiscoverResponse>,
    discover_error: Option<String>,
    discover_id: Option<u64>,
    last_scan: Option<Instant>,
    methods: Vec<Method>,
    method: Option<Method>,
    endpoint: Endpoint,
    /// The user picked the endpoint; status refreshes no longer change it.
    endpoint_chosen: bool,
    region: Option<String>,
    region_note: String,

    flow: Option<Flow>,
    /// Standalone "Validate" run.
    validation: Option<u64>,
    /// Cancellable Tokio work of the current flow step or validation.
    task: Option<tokio::task::JoinHandle<()>>,
    /// The current flow step waits on a dialog.
    awaiting_confirmation: bool,
    notice: Option<Notice>,

    models: Vec<Model>,
    models_loading: bool,
    models_error: Option<String>,
    model_index: Option<usize>,

    local: HashMap<LocalKind, LocalState>,
    local_ids: HashMap<LocalKind, u64>,
    local_rendered: Option<Vec<LocalRow>>,
    pull: Option<PullJob>,
    pull_update: Option<PullUpdate>,
    pull_error: Option<String>,
    pull_success: Option<String>,
}

impl BedrockController {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn busy(&self) -> bool {
        self.flow.is_some() || self.validation.is_some()
    }

    fn configured_provider(&self) -> &str {
        self.status
            .as_ref()
            .map_or(flow::OPENAI_PROVIDER_ID, |status| {
                status.provider_id.as_str()
            })
    }

    fn discovered_or_empty(&self) -> BedrockDiscoverResponse {
        self.discovered
            .clone()
            .unwrap_or_else(|| BedrockDiscoverResponse {
                profiles: Vec::new(),
                environment_credentials: Vec::new(),
            })
    }

    fn notify(&mut self, area: Area, kind: NoticeKind, text: impl Into<String>) {
        self.notice = Some(Notice {
            area,
            kind,
            text: text.into(),
        });
    }

    fn abort_task(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl AppController {
    pub(crate) fn bedrock_bind(&mut self) {
        let state = self.window.global::<BedrockState>();
        state.on_page_shown(|| crate::ui_thread::with_app(AppController::bedrock_page_shown));
        state.on_refresh(|| crate::ui_thread::with_app(AppController::bedrock_refresh_all));
        state.on_rescan(|| crate::ui_thread::with_app(AppController::bedrock_discover));
        state.on_set_endpoint(|code| {
            crate::ui_thread::with_app(move |app| app.bedrock_set_endpoint(code));
        });
        state.on_select_method(|index| {
            crate::ui_thread::with_app(move |app| app.bedrock_select_method_index(index));
        });
        state.on_select_region(|index| {
            crate::ui_thread::with_app(move |app| app.bedrock_select_region_index(index));
        });
        state.on_form_edited(|| crate::ui_thread::with_app(AppController::bedrock_form_edited));
        state.on_validate(|| crate::ui_thread::with_app(AppController::bedrock_validate));
        state.on_apply(|| crate::ui_thread::with_app(AppController::bedrock_apply));
        state.on_cancel(|| crate::ui_thread::with_app(AppController::bedrock_cancel));
        state.on_revert(|| crate::ui_thread::with_app(AppController::bedrock_confirm_revert));
        state.on_restart_now(|| crate::ui_thread::with_app(AppController::bedrock_restart_now));
        state.on_select_model(|index| {
            crate::ui_thread::with_app(move |app| app.bedrock_select_model(index));
        });
        state.on_save_model(|| crate::ui_thread::with_app(AppController::bedrock_save_model));
        state.on_local_refresh(|provider_id| {
            let provider_id = provider_id.to_string();
            crate::ui_thread::with_app(move |app| {
                if let Some(kind) = LocalKind::from_provider_id(&provider_id) {
                    app.bedrock_probe_local(kind);
                }
            });
        });
        state.on_local_use(|provider_id, model| {
            let provider_id = provider_id.to_string();
            let model = model.to_string();
            crate::ui_thread::with_app(move |app| app.bedrock_use_local(&provider_id, &model));
        });
        state.on_pull(|| crate::ui_thread::with_app(AppController::bedrock_start_pull));
        state.on_cancel_pull(|| crate::ui_thread::with_app(AppController::bedrock_cancel_pull));
        state.on_open_link(|link| {
            let url = match link.as_str() {
                "gov-cloud" => flow::GOV_CLOUD_GUIDANCE_URL,
                _ => flow::BEDROCK_SETUP_GUIDE_URL,
            };
            crate::ui_thread::with_app(move |app| app.bedrock_open_url(url));
        });

        let regions: Vec<SharedString> = flow::region_options()
            .iter()
            .map(|option| option.label().into())
            .collect();
        state.set_regions(ModelRc::from(Rc::new(VecModel::from(regions))));
        self.bedrock.methods = flow::method_list(&self.bedrock.discovered_or_empty());
        self.bedrock_render_methods();
        self.bedrock_render();
    }

    pub(crate) fn bedrock_on_server_ready(&mut self) {
        self.bedrock_refresh_status();
        let at_restart = self
            .bedrock
            .flow
            .as_ref()
            .is_some_and(|flow| flow.current() == Some(&Step::Restart));
        let bedrock_running = self
            .bedrock_running_provider()
            .is_some_and(|id| flow::is_bedrock_provider(&id));
        // The catalog belongs to the server that stopped, which may have
        // used another provider; never offer its models under the new one.
        self.bedrock.models.clear();
        self.bedrock.model_index = None;
        self.bedrock.models_error = None;
        self.bedrock_render_models();
        if at_restart {
            // The flow loads the catalog itself, or finishing it does.
            self.bedrock_flow_next();
        } else if bedrock_running {
            self.bedrock_load_models(/*flow_id*/ None);
        }
    }

    /// The embedded server failed to start or restart.
    pub(crate) fn bedrock_on_server_failed(&mut self, message: &str) {
        let flow_id = self.bedrock.flow.as_ref().map(|flow| flow.id);
        if let Some(flow_id) = flow_id {
            self.bedrock_flow_failed(flow_id, message.to_string());
        }
    }

    pub(crate) fn bedrock_on_notification(&mut self, notification: &ServerNotification) {
        if matches!(notification, ServerNotification::AccountUpdated(_)) {
            self.bedrock_refresh_account();
        }
    }

    /// Drives the page from automation scripts (`{"bedrock": [...]}`).
    pub(crate) fn bedrock_automation(&mut self, args: &[String]) {
        let state = self.window.global::<BedrockState>();
        let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
        match arg(0) {
            "endpoint" => self.bedrock_set_endpoint(i32::from(arg(1) == "runtime")),
            "method" => {
                if let Ok(index) = arg(1).parse::<i32>() {
                    self.bedrock_select_method_index(index);
                }
            }
            "region" => {
                if let Some(index) = flow::region_index(arg(1)) {
                    self.bedrock_select_region_index(i32::try_from(index).unwrap_or(-1));
                }
            }
            "set" => {
                let value = SharedString::from(arg(2));
                match arg(1) {
                    "profile" => state.set_manual_profile(value),
                    "api-key" => state.set_api_key(value),
                    "access-key-id" => state.set_access_key_id(value),
                    "secret-access-key" => state.set_secret_access_key(value),
                    "session-token" => state.set_session_token(value),
                    "pull-name" => state.set_pull_name(value),
                    other => tracing::warn!(field = other, "unknown bedrock automation field"),
                }
            }
            "validate" => self.bedrock_validate(),
            "apply" => self.bedrock_apply(),
            "cancel" => self.bedrock_cancel(),
            "revert" => self.bedrock_confirm_revert(),
            "rescan" => self.bedrock_discover(),
            "pull" => self.bedrock_start_pull(),
            "cancel-pull" => self.bedrock_cancel_pull(),
            "model" => {
                if let Ok(index) = arg(1).parse::<i32>() {
                    self.bedrock_select_model(index);
                }
            }
            "save-model" => self.bedrock_save_model(),
            "scroll" => {
                if let Ok(offset) = arg(1).parse::<f32>() {
                    state.set_scroll_y(-offset.max(0.0));
                }
            }
            "use" => self.bedrock_use_local(arg(1), arg(2)),
            other => tracing::warn!(action = other, "unknown bedrock automation action"),
        }
    }

    // ----- loading ----------------------------------------------------------

    fn bedrock_page_shown(&mut self) {
        self.bedrock_refresh_status();
        let stale = self
            .bedrock
            .last_scan
            .is_none_or(|scanned| scanned.elapsed() >= RESCAN_AFTER);
        if stale && !self.bedrock.busy() {
            self.bedrock_discover();
            for kind in LocalKind::ALL {
                self.bedrock_probe_local(kind);
            }
        }
        if self.bedrock.models.is_empty()
            && !self.bedrock.models_loading
            && self.bedrock.models_error.is_none()
            && self
                .bedrock_running_provider()
                .is_some_and(|id| flow::is_bedrock_provider(&id))
        {
            self.bedrock_load_models(/*flow_id*/ None);
        }
        self.bedrock_render();
    }

    fn bedrock_refresh_all(&mut self) {
        self.bedrock.notice = None;
        self.bedrock_refresh_status();
        self.bedrock_discover();
        for kind in LocalKind::ALL {
            self.bedrock_probe_local(kind);
        }
        if self
            .bedrock_running_provider()
            .is_some_and(|id| flow::is_bedrock_provider(&id))
        {
            self.bedrock_load_models(/*flow_id*/ None);
        }
        self.bedrock_render();
    }

    fn bedrock_refresh_status(&mut self) {
        self.bedrock.status_loading += 1;
        self.backend.call(
            |request_id| ClientRequest::ConfigRead {
                request_id,
                params: ConfigReadParams {
                    // Layers show which provider lower layers select.
                    include_layers: true,
                    cwd: None,
                },
            },
            |app, result: Result<ConfigReadResponse, BackendError>| {
                app.bedrock.status_loading = app.bedrock.status_loading.saturating_sub(1);
                match result {
                    Ok(response) => {
                        let status = flow::provider_status(&response);
                        if !app.bedrock.endpoint_chosen
                            && let Some(endpoint) = Endpoint::from_provider_id(&status.provider_id)
                        {
                            app.bedrock.endpoint = endpoint;
                        }
                        if app.bedrock.region.is_none() {
                            app.bedrock.region = status
                                .aws_region
                                .clone()
                                .filter(|region| flow::is_supported_region(region));
                        }
                        app.bedrock.status = Some(status);
                        app.bedrock.status_error = None;
                    }
                    Err(err) => {
                        app.bedrock.status_error = Some(format!(
                            "Unable to read the configuration: {}",
                            err.user_message()
                        ));
                    }
                }
                app.bedrock_render();
                app.bedrock_render_models();
                app.bedrock_render_local();
            },
        );
        self.bedrock_refresh_account();
    }

    fn bedrock_refresh_account(&mut self) {
        self.bedrock.status_loading += 1;
        self.backend.call(
            |request_id| ClientRequest::GetAccount {
                request_id,
                params: GetAccountParams {
                    refresh_token: false,
                },
            },
            |app, result: Result<GetAccountResponse, BackendError>| {
                app.bedrock.status_loading = app.bedrock.status_loading.saturating_sub(1);
                match result {
                    Ok(response) => app.bedrock.account = Some(response),
                    Err(err) => {
                        tracing::warn!(%err, "account/read failed");
                        app.bedrock.account = None;
                    }
                }
                app.bedrock_render();
            },
        );
    }

    fn bedrock_discover(&mut self) {
        let id = self.bedrock.next_id();
        self.bedrock.discover_id = Some(id);
        self.bedrock.last_scan = Some(Instant::now());
        self.bedrock_render();
        self.backend.call(
            |request_id| ClientRequest::BedrockDiscover {
                request_id,
                params: BedrockDiscoverParams {},
            },
            move |app, result: Result<BedrockDiscoverResponse, BackendError>| {
                if app.bedrock.discover_id != Some(id) {
                    return;
                }
                app.bedrock.discover_id = None;
                let previous_profile = app.bedrock_selected_profile_name();
                match result {
                    Ok(response) => {
                        app.bedrock.discovered = Some(response);
                        app.bedrock.discover_error = None;
                    }
                    Err(err) => {
                        // Like the TUI: fall back to manual methods.
                        app.bedrock.discovered = None;
                        app.bedrock.discover_error = Some(format!(
                            "Unable to check AWS credentials: {}",
                            err.user_message()
                        ));
                    }
                }
                app.bedrock_apply_discovery(previous_profile);
            },
        );
    }

    /// Rebuilds the method list after discovery, keeping the selection when
    /// it still exists. `previous_profile` is the profile selected before the
    /// scan (profile methods are indexes into the old list).
    fn bedrock_apply_discovery(&mut self, previous_profile: Option<String>) {
        let discovered = self.bedrock.discovered_or_empty();
        self.bedrock.methods = flow::method_list(&discovered);
        let keep = match self.bedrock.method {
            Some(Method::Profile(_)) => previous_profile.and_then(|name| {
                discovered
                    .profiles
                    .iter()
                    .position(|profile| profile.name == name)
                    .map(Method::Profile)
            }),
            Some(method) if self.bedrock.methods.contains(&method) => Some(method),
            _ => None,
        };
        self.bedrock_render_methods();
        match keep.or_else(|| self.bedrock.methods.first().copied()) {
            Some(method) => {
                self.bedrock_select_method(method, /*suggest_region*/ keep.is_none());
            }
            None => self.bedrock.method = None,
        }
        self.bedrock_render();
    }

    fn bedrock_selected_profile_name(&self) -> Option<String> {
        let Some(Method::Profile(index)) = self.bedrock.method else {
            return None;
        };
        self.bedrock
            .discovered
            .as_ref()
            .and_then(|discovered| discovered.profiles.get(index))
            .map(|profile| profile.name.clone())
    }

    /// Provider of the running server. A daemon or remote server does not
    /// share its config, so its configured provider stands in.
    fn bedrock_running_provider(&self) -> Option<String> {
        match self.config.as_ref() {
            Some(config) => Some(config.model_provider_id.clone()),
            None if self.bedrock_remote() => self
                .bedrock
                .status
                .as_ref()
                .map(|status| status.provider_id.clone()),
            None => None,
        }
    }

    /// Connected to a daemon or remote app-server (no local `Config`).
    fn bedrock_remote(&self) -> bool {
        self.config.is_none() && self.backend.is_ready()
    }

    // ----- form -------------------------------------------------------------

    fn bedrock_set_endpoint(&mut self, code: i32) {
        if self.bedrock.busy() {
            return;
        }
        self.bedrock.endpoint = if code == 1 {
            Endpoint::Runtime
        } else {
            Endpoint::Mantle
        };
        self.bedrock.endpoint_chosen = true;
        self.bedrock.notice = None;
        self.bedrock_render();
    }

    fn bedrock_select_method_index(&mut self, index: i32) {
        if self.bedrock.busy() {
            return;
        }
        let method = usize::try_from(index)
            .ok()
            .and_then(|index| self.bedrock.methods.get(index).copied());
        if let Some(method) = method {
            self.bedrock.notice = None;
            self.bedrock_select_method(method, /*suggest_region*/ true);
            self.bedrock_render();
        }
    }

    fn bedrock_select_method(&mut self, method: Method, suggest_region: bool) {
        self.bedrock.method = Some(method);
        if !suggest_region {
            return;
        }
        let discovered = self.bedrock.discovered_or_empty();
        let source = match method {
            Method::Profile(_) => "the profile",
            _ => "the environment",
        };
        self.bedrock.region_note = match flow::suggested_region(method, &discovered) {
            Some(region) if flow::is_supported_region(&region) => {
                self.bedrock.region = Some(region.trim().to_ascii_lowercase());
                format!("Region from {source}.")
            }
            Some(region) => format!(
                "The region from {source} ({region}) is not available for Amazon Bedrock. Choose a supported region."
            ),
            None => String::new(),
        };
    }

    fn bedrock_select_region_index(&mut self, index: i32) {
        let option = usize::try_from(index)
            .ok()
            .and_then(|index| flow::region_options().get(index).copied());
        if let Some(option) = option {
            self.bedrock.region = Some(option.code.to_string());
            self.bedrock.region_note.clear();
            self.bedrock.notice = None;
            self.bedrock_render();
        }
    }

    fn bedrock_form_edited(&mut self) {
        if !self.bedrock.busy() && self.bedrock.notice.is_some() {
            self.bedrock.notice = None;
            self.bedrock_render();
        }
    }

    fn bedrock_form_input(&self) -> FormInput {
        let state = self.window.global::<BedrockState>();
        FormInput {
            endpoint: self.bedrock.endpoint,
            method: self.bedrock.method,
            manual_profile: state.get_manual_profile().to_string(),
            api_key: state.get_api_key().to_string(),
            access_key_id: state.get_access_key_id().to_string(),
            secret_access_key: state.get_secret_access_key().to_string(),
            session_token: state.get_session_token().to_string(),
            region: self
                .bedrock
                .region
                .clone()
                .unwrap_or_else(|| flow::DEFAULT_REGION.to_string()),
        }
    }

    fn bedrock_validated_plan(&mut self) -> Option<flow::SetupPlan> {
        let discovered = self.bedrock.discovered_or_empty();
        let input = self.bedrock_form_input();
        let context = FormContext {
            discovered: &discovered,
            managed_credentials: flow::uses_managed_credentials(self.bedrock.account.as_ref()),
            discovery_complete: self.bedrock.discovered.is_some(),
        };
        match flow::validate_form(&input, context) {
            Ok(plan) => Some(plan),
            Err(err) => {
                self.bedrock
                    .notify(Area::Bedrock, NoticeKind::Error, err.to_string());
                self.bedrock_render();
                None
            }
        }
    }

    fn bedrock_clear_secrets(&self) {
        let state = self.window.global::<BedrockState>();
        state.set_api_key(SharedString::new());
        state.set_secret_access_key(SharedString::new());
        state.set_session_token(SharedString::new());
    }

    // ----- validate ---------------------------------------------------------

    fn bedrock_validate(&mut self) {
        if self.bedrock.busy() {
            return;
        }
        let Some(plan) = self.bedrock_validated_plan() else {
            return;
        };
        let message = match &plan.credential {
            flow::Credential::Profile(_) if self.bedrock_remote() => {
                "The app-server checks the profile on its own machine when you apply."
            }
            flow::Credential::Profile(profile) => {
                let id = self.bedrock.next_id();
                self.bedrock.validation = Some(id);
                self.bedrock.notice = None;
                let profile = profile.clone();
                let region = plan.region.clone();
                self.bedrock_spawn_profile_check(profile.clone(), region, move |app, result| {
                    if app.bedrock.validation != Some(id) {
                        return;
                    }
                    app.bedrock.validation = None;
                    app.bedrock.task = None;
                    match result {
                        Ok(()) => app.bedrock.notify(
                            Area::Bedrock,
                            NoticeKind::Success,
                            format!("AWS profile \"{profile}\" provided credentials."),
                        ),
                        Err(err) => app.bedrock.notify(Area::Bedrock, NoticeKind::Error, err),
                    }
                    app.bedrock_render();
                });
                self.bedrock_render();
                return;
            }
            flow::Credential::Environment => {
                "AWS credentials were found in the environment. Amazon Bedrock checks them on the first request."
            }
            flow::Credential::ApiKey(_) | flow::Credential::AccessKeys { .. } => {
                "The form is complete. Amazon Bedrock checks these credentials on the first request."
            }
        };
        self.bedrock
            .notify(Area::Bedrock, NoticeKind::Info, message);
        self.bedrock_render();
    }

    /// Resolves the profile's credentials on Tokio (cancellable through
    /// [`BedrockController::task`]) and reports back on the UI thread.
    fn bedrock_spawn_profile_check(
        &mut self,
        profile: String,
        region: String,
        on_done: impl FnOnce(&mut AppController, Result<(), String>) + Send + 'static,
    ) {
        let Some(config) = self.config.clone() else {
            on_done(self, Err("Codex is not running.".to_string()));
            return;
        };
        self.bedrock.abort_task();
        let task = self.backend.spawn(async move {
            let check = codex_aws_auth::validate_aws_profile(
                &profile,
                &region,
                config.http_client_factory(),
            );
            let result = match tokio::time::timeout(PROFILE_VALIDATION_TIMEOUT, check).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(format!(
                    "AWS profile \"{profile}\" could not provide credentials: {err}"
                )),
                Err(_) => Err(format!(
                    "Timed out loading credentials for AWS profile \"{profile}\"."
                )),
            };
            crate::ui_thread::post(move |app| on_done(app, result));
        });
        self.bedrock.task = Some(task);
    }

    fn bedrock_cancel(&mut self) {
        if self.bedrock.validation.take().is_some() {
            self.bedrock.abort_task();
            self.bedrock
                .notify(Area::Bedrock, NoticeKind::Info, "Validation cancelled.");
            self.bedrock_render();
            return;
        }
        let cancellable = self
            .bedrock
            .flow
            .as_ref()
            .and_then(Flow::current)
            .is_some_and(Step::is_cancellable);
        if cancellable && let Some(flow) = self.bedrock.flow.take() {
            self.bedrock.abort_task();
            self.bedrock.notify(
                Area::of_flow(&flow.kind),
                NoticeKind::Info,
                "Cancelled. Nothing was changed.",
            );
            self.bedrock_render();
        }
    }

    // ----- provider changes -------------------------------------------------

    fn bedrock_apply(&mut self) {
        if self.bedrock.busy() || self.bedrock_locked_notice(Area::Bedrock) {
            return;
        }
        let Some(plan) = self.bedrock_validated_plan() else {
            return;
        };
        let steps = flow::plan_apply_steps(&plan, self.bedrock.configured_provider());
        let kind = FlowKind::Bedrock {
            endpoint: plan.endpoint,
            region: plan.region,
        };
        self.bedrock_start_flow(kind, steps);
    }

    fn bedrock_confirm_revert(&mut self) {
        if self.bedrock.busy() || self.bedrock_locked_notice(Area::Status) {
            return;
        }
        let current = self.bedrock.configured_provider().to_string();
        let fallback = self
            .bedrock
            .status
            .as_ref()
            .and_then(|status| status.fallback_provider.clone());
        let remove_credentials = flow::uses_managed_credentials(self.bedrock.account.as_ref());
        let mut message = "Codex restarts, and new threads use OpenAI models. You may need to sign in to ChatGPT or enter an OpenAI API key on the Account page.".to_string();
        if remove_credentials {
            message.push_str(" The Amazon Bedrock credentials stored by Codex are removed.");
        }
        self.show_dialog(
            DialogRequest::confirm("Switch back to OpenAI?", message)
                .accept_label("Switch to OpenAI"),
            Box::new(move |app, accepted| {
                if accepted.is_some() {
                    let steps =
                        flow::plan_revert_steps(&current, remove_credentials, fallback.as_deref());
                    app.bedrock_start_flow(FlowKind::Revert, steps);
                }
            }),
        );
    }

    fn bedrock_use_local(&mut self, provider_id: &str, model: &str) {
        if self.bedrock.busy() || self.bedrock_locked_notice(Area::Local) || model.trim().is_empty()
        {
            return;
        }
        let kind = FlowKind::Local {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
        };
        self.bedrock_start_flow(kind, flow::plan_local_steps(provider_id, model));
    }

    fn bedrock_restart_now(&mut self) {
        if self.bedrock.busy() {
            return;
        }
        let mut steps = vec![Step::Restart];
        // Restarting onto Bedrock replaces the model catalog.
        if flow::is_bedrock_provider(self.bedrock.configured_provider()) {
            steps.push(Step::LoadModels);
        }
        self.bedrock_start_flow(FlowKind::Restart, steps);
    }

    /// Shows why the page cannot change the provider, if it cannot.
    fn bedrock_locked_notice(&mut self, area: Area) -> bool {
        let Some(status) = self
            .bedrock
            .status
            .as_ref()
            .filter(|status| status.provider_locked)
        else {
            return false;
        };
        let origin = status.provider_origin.clone().unwrap_or_default();
        self.bedrock.notify(
            area,
            NoticeKind::Error,
            format!("The model provider is set by {origin}, which overrides changes made here."),
        );
        self.bedrock_render();
        true
    }

    fn bedrock_start_flow(&mut self, kind: FlowKind, steps: Vec<Step>) {
        let steps = if self.bedrock_remote() {
            flow::without_embedded_steps(steps)
        } else {
            steps
        };
        let id = self.bedrock.next_id();
        self.bedrock.notice = None;
        self.bedrock.flow = Some(Flow::new(id, kind, steps));
        self.bedrock_flow_next();
    }

    fn bedrock_flow_next(&mut self) {
        let Some(flow) = self.bedrock.flow.as_mut() else {
            return;
        };
        let id = flow.id;
        let step = flow.advance();
        self.bedrock_render();
        match step {
            Some(step) => self.bedrock_run_step(id, step),
            None => self.bedrock_flow_finished(id),
        }
    }

    fn bedrock_flow_is(&self, id: u64) -> bool {
        self.bedrock.flow.as_ref().is_some_and(|flow| flow.id == id)
    }

    fn bedrock_step_done(&mut self, id: u64) {
        if self.bedrock_flow_is(id) {
            self.bedrock.task = None;
            self.bedrock_flow_next();
        }
    }

    fn bedrock_flow_failed(&mut self, id: u64, message: String) {
        if !self.bedrock_flow_is(id) {
            return;
        }
        self.bedrock.awaiting_confirmation = false;
        let Some(flow) = self.bedrock.flow.take() else {
            return;
        };
        self.bedrock.abort_task();
        self.bedrock.notify(
            Area::of_flow(&flow.kind),
            NoticeKind::Error,
            format!("{}: {message}", flow.kind.failure_prefix()),
        );
        // Earlier steps may have written config; show what is there now.
        self.bedrock_refresh_status();
        self.bedrock_render();
    }

    fn bedrock_flow_finished(&mut self, id: u64) {
        if !self.bedrock_flow_is(id) {
            return;
        }
        self.bedrock.awaiting_confirmation = false;
        if let Some(flow) = self.bedrock.flow.take() {
            let mut message = flow.kind.success_message();
            if self.bedrock_remote() && flow.kind != FlowKind::Restart {
                message = format!(
                    "Saved to the app-server's config. Restart the app-server ({}) so it uses the change.",
                    self.connection_label
                );
            }
            let message = flow.finished_message(message);
            if flow.kind == FlowKind::Revert {
                self.bedrock_verify_revert(message);
            } else {
                self.bedrock
                    .notify(Area::of_flow(&flow.kind), NoticeKind::Success, message);
            }
        }
        if self.bedrock.models.is_empty()
            && !self.bedrock.models_loading
            && self
                .bedrock_running_provider()
                .is_some_and(|id| flow::is_bedrock_provider(&id))
        {
            self.bedrock_load_models(/*flow_id*/ None);
        }
        self.bedrock_refresh_status();
        self.bedrock_render();
    }

    /// Reports a finished switch back to OpenAI only once the config says
    /// so: a layer the page could not change may still select the old
    /// provider.
    fn bedrock_verify_revert(&mut self, success: String) {
        self.bedrock.notify(
            Area::Status,
            NoticeKind::Info,
            "Checking the configuration…",
        );
        self.backend.call(
            |request_id| ClientRequest::ConfigRead {
                request_id,
                params: ConfigReadParams {
                    include_layers: true,
                    cwd: None,
                },
            },
            move |app, result: Result<ConfigReadResponse, BackendError>| {
                let (kind, text) = match result {
                    Ok(response) => revert_outcome(&flow::provider_status(&response), success),
                    Err(err) => (
                        NoticeKind::Error,
                        format!(
                            "Switched, but the configuration could not be checked: {}",
                            err.user_message()
                        ),
                    ),
                };
                app.bedrock.notify(Area::Status, kind, text);
                app.bedrock_render();
            },
        );
    }

    fn bedrock_run_step(&mut self, id: u64, step: Step) {
        match step {
            Step::ValidateProfile { profile, region } => {
                self.bedrock_spawn_profile_check(
                    profile,
                    region,
                    move |app, result| match result {
                        Ok(()) => app.bedrock_step_done(id),
                        Err(err) => app.bedrock_flow_failed(id, err),
                    },
                );
            }
            Step::Setup(params) => self.backend.call(
                |request_id| ClientRequest::BedrockSetup { request_id, params },
                move |app, result: Result<BedrockSetupResponse, BackendError>| match result {
                    Ok(_) => app.bedrock_step_done(id),
                    Err(err) => app.bedrock_flow_failed(id, err.user_message()),
                },
            ),
            Step::Login(params) => self.backend.call(
                |request_id| ClientRequest::LoginAccount { request_id, params },
                move |app, result: Result<LoginAccountResponse, BackendError>| match result {
                    Ok(LoginAccountResponse::AmazonBedrock {}) => {
                        app.bedrock_clear_secrets();
                        app.bedrock_step_done(id);
                    }
                    Ok(other) => app.bedrock_flow_failed(
                        id,
                        format!("the server answered the Bedrock login with {other:?}"),
                    ),
                    Err(err) => app.bedrock_flow_failed(id, err.user_message()),
                },
            ),
            Step::Write(edits) => {
                self.bedrock_write_config(edits, move |app, result| match result {
                    Ok(()) => app.bedrock_step_done(id),
                    Err(err) => app.bedrock_flow_failed(id, err),
                })
            }
            Step::ClearModel => {
                self.bedrock_write_config(flow::clear_model_edits(), move |app, result| {
                    if let Err(err) = result
                        && let Some(flow) = app.bedrock.flow.as_mut().filter(|flow| flow.id == id)
                    {
                        flow.warn(format!(
                            "New threads may keep the previously configured model: {err}"
                        ));
                    }
                    app.bedrock_step_done(id);
                })
            }
            Step::Logout => self.backend.call(
                |request_id| ClientRequest::LogoutAccount {
                    request_id,
                    params: None,
                },
                move |app, result: Result<LogoutAccountResponse, BackendError>| match result {
                    Ok(_) => app.bedrock_step_done(id),
                    Err(err) => app.bedrock_flow_failed(id, err.user_message()),
                },
            ),
            Step::CheckGovCloud => self.bedrock_check_gov_cloud(id),
            Step::Restart => self.bedrock_restart(id),
            Step::LoadModels => self.bedrock_load_models(Some(id)),
        }
    }

    /// `config/batchWrite`; a write that a higher layer overrides is an error
    /// because the change would not take effect.
    fn bedrock_write_config(
        &mut self,
        edits: Vec<ConfigEdit>,
        on_done: impl FnOnce(&mut AppController, Result<(), String>) + Send + 'static,
    ) {
        self.backend.call(
            |request_id| ClientRequest::ConfigBatchWrite {
                request_id,
                params: ConfigBatchWriteParams {
                    edits,
                    file_path: None,
                    expected_version: None,
                    reload_user_config: false,
                },
            },
            move |app, result: Result<ConfigWriteResponse, BackendError>| {
                let result = match result {
                    Ok(response) if response.status == WriteStatus::OkOverridden => Err(response
                        .overridden_metadata
                        .map(|metadata| metadata.message)
                        .unwrap_or_else(|| {
                            "the change was saved, but another config layer overrides it"
                                .to_string()
                        })),
                    Ok(_) => Ok(()),
                    Err(err) => Err(err.user_message()),
                };
                on_done(app, result);
            },
        );
    }

    fn bedrock_check_gov_cloud(&mut self, id: u64) {
        let backend = self.backend.clone();
        let task = self.backend.spawn(async move {
            let request = ClientRequest::BedrockCheckGovCloudRequirements {
                request_id: backend.next_request_id(),
                params: BedrockCheckGovCloudRequirementsParams {},
            };
            let result = tokio::time::timeout(
                GOV_CLOUD_CHECK_TIMEOUT,
                backend.request::<BedrockCheckGovCloudRequirementsResponse>(request),
            )
            .await;
            let response = match result {
                Ok(Ok(response)) => Some(response),
                Ok(Err(err)) => {
                    tracing::warn!(%err, "GovCloud requirements check failed");
                    None
                }
                Err(_) => {
                    tracing::warn!("GovCloud requirements check timed out");
                    None
                }
            };
            crate::ui_thread::post(move |app| app.bedrock_on_gov_cloud(id, response));
        });
        self.bedrock.task = Some(task);
    }

    fn bedrock_on_gov_cloud(
        &mut self,
        id: u64,
        response: Option<BedrockCheckGovCloudRequirementsResponse>,
    ) {
        if !self.bedrock_flow_is(id) {
            return;
        }
        self.bedrock.task = None;
        let Some(response) = response.filter(|response| response.is_gov_cloud) else {
            self.bedrock_flow_next();
            return;
        };
        let mut message = format!(
            "You are using Codex with AWS GovCloud. Make sure you or your administrator have read the application configuration and security guidance before proceeding:\n{}",
            flow::GOV_CLOUD_GUIDANCE_URL
        );
        if response.should_warn {
            message.push_str(
                "\n\nYour Codex requirements do not yet limit sign-in to API credentials and allow only the Bedrock endpoint on the network.",
            );
        }
        let request = DialogRequest {
            cancel_label: String::new(),
            ..DialogRequest::confirm("Using Codex with AWS GovCloud", message)
                .accept_label("Acknowledge")
        };
        self.bedrock.awaiting_confirmation = true;
        self.bedrock_render();
        self.show_dialog(
            request,
            Box::new(move |app, _accepted| {
                // The configuration is already saved; the warning is advisory.
                if app.bedrock_flow_is(id) {
                    app.bedrock.awaiting_confirmation = false;
                    app.bedrock_flow_next();
                }
            }),
        );
    }

    /// Restarts the server for flow `id`, asking first when turns are running.
    fn bedrock_restart(&mut self, id: u64) {
        let running = self
            .tabs
            .iter()
            .filter(|tab| tab.thread().is_some_and(ThreadTab::is_busy))
            .count();
        if running == 0 {
            self.bedrock_restart_server();
            return;
        }
        self.bedrock.awaiting_confirmation = true;
        self.bedrock_render();
        let working = if running == 1 {
            "1 thread is still working".to_string()
        } else {
            format!("{running} threads are still working")
        };
        let request = DialogRequest {
            cancel_label: "Later".to_string(),
            ..DialogRequest::confirm(
                "Restart Codex now?",
                format!(
                    "The new provider takes effect when Codex restarts. {working}; restarting stops the running turns. The threads stay open."
                ),
            )
            .accept_label("Restart now")
        };
        self.show_dialog(
            request,
            Box::new(move |app, accepted| {
                if !app.bedrock_flow_is(id) {
                    return;
                }
                app.bedrock.awaiting_confirmation = false;
                if accepted.is_some() {
                    app.bedrock_restart_server();
                } else {
                    app.bedrock_postpone_restart();
                }
            }),
        );
    }

    fn bedrock_restart_server(&mut self) {
        self.window
            .global::<AppState>()
            .set_server_status("restarting".into());
        self.backend.restart();
        self.bedrock_render();
    }

    fn bedrock_postpone_restart(&mut self) {
        if let Some(mut flow) = self.bedrock.flow.take() {
            flow.skip_restart();
            self.bedrock.notify(
                Area::of_flow(&flow.kind),
                NoticeKind::Info,
                "Saved. Codex keeps the current provider until it restarts.",
            );
        }
        self.bedrock_refresh_status();
        self.bedrock_render();
    }

    // ----- models -----------------------------------------------------------

    /// Loads `model/list` from the running server. With a flow id, the flow
    /// continues afterwards (a failed list does not fail the setup).
    fn bedrock_load_models(&mut self, flow_id: Option<u64>) {
        self.bedrock.models_loading = true;
        self.bedrock_render_models();
        self.backend.call(
            |request_id| ClientRequest::ModelList {
                request_id,
                params: ModelListParams {
                    cursor: None,
                    limit: None,
                    include_hidden: Some(false),
                },
            },
            move |app, result: Result<ModelListResponse, BackendError>| {
                app.bedrock.models_loading = false;
                match result {
                    Ok(response) => {
                        app.bedrock.models = response.data;
                        app.bedrock.models_error = None;
                    }
                    Err(err) => {
                        app.bedrock.models.clear();
                        app.bedrock.models_error =
                            Some(format!("Unable to load models: {}", err.user_message()));
                    }
                }
                app.bedrock.model_index = None;
                app.bedrock_render_models();
                if let Some(flow_id) = flow_id {
                    app.bedrock_step_done(flow_id);
                }
            },
        );
    }

    fn bedrock_configured_model(&self) -> Option<&str> {
        self.bedrock
            .status
            .as_ref()
            .and_then(|status| status.model.as_deref())
    }

    /// Picker index: the user's pick, else the configured model, else the
    /// provider default.
    fn bedrock_effective_model_index(&self) -> Option<usize> {
        let models = &self.bedrock.models;
        self.bedrock
            .model_index
            .filter(|index| *index < models.len())
            .or_else(|| {
                self.bedrock_configured_model().and_then(|configured| {
                    models.iter().position(|model| model.model == configured)
                })
            })
            .or_else(|| models.iter().position(|model| model.is_default))
            .or_else(|| (!models.is_empty()).then_some(0))
    }

    fn bedrock_select_model(&mut self, index: i32) {
        self.bedrock.model_index = usize::try_from(index).ok();
        self.bedrock_render_models();
    }

    fn bedrock_save_model(&mut self) {
        let Some(model) = self
            .bedrock_effective_model_index()
            .and_then(|index| self.bedrock.models.get(index))
            .map(|model| model.model.clone())
        else {
            return;
        };
        self.bedrock_write_config(flow::model_edits(&model), move |app, result| {
            match result {
                Ok(()) => app.bedrock.notify(
                    Area::Model,
                    NoticeKind::Success,
                    format!("Saved. New threads use {model}."),
                ),
                Err(err) => app.bedrock.notify(
                    Area::Model,
                    NoticeKind::Error,
                    format!("Unable to save the model: {err}"),
                ),
            }
            app.bedrock.model_index = None;
            app.bedrock_refresh_status();
            app.bedrock_render();
        });
    }

    // ----- local servers ----------------------------------------------------

    fn bedrock_probe_local(&mut self, kind: LocalKind) {
        let Some(config) = self.config.clone() else {
            let message = if self.bedrock_remote() {
                "Local models are detected only when Codex runs its own app-server."
            } else {
                "Codex is not running."
            };
            self.bedrock
                .local
                .insert(kind, LocalState::Unavailable(message.to_string()));
            self.bedrock_render_local();
            return;
        };
        let id = self.bedrock.next_id();
        self.bedrock.local_ids.insert(kind, id);
        self.bedrock.local.insert(kind, LocalState::Checking);
        self.bedrock_render_local();
        self.backend.spawn(async move {
            let result = local::probe(kind, config).await;
            crate::ui_thread::post(move |app| app.bedrock_on_local_probe(kind, id, result));
        });
    }

    fn bedrock_on_local_probe(
        &mut self,
        kind: LocalKind,
        id: u64,
        result: Result<LocalProbe, String>,
    ) {
        if self.bedrock.local_ids.get(&kind) != Some(&id) {
            return;
        }
        let state = match result {
            Ok(probe) => LocalState::Running(probe),
            Err(err) => LocalState::Unavailable(err),
        };
        self.bedrock.local.insert(kind, state);
        self.bedrock_render_local();
    }

    fn bedrock_start_pull(&mut self) {
        if self.bedrock.pull.is_some() {
            return;
        }
        let state = self.window.global::<BedrockState>();
        let model = state.get_pull_name().trim().to_string();
        self.bedrock.pull_success = None;
        if !local::is_valid_ollama_model_name(&model) {
            self.bedrock.pull_error = Some(
                "Enter a model name such as gpt-oss:20b (letters, digits, and . _ - : /)."
                    .to_string(),
            );
            self.bedrock_render_local();
            return;
        }
        let Some(config) = self.config.clone() else {
            self.bedrock.pull_error = Some("Codex is not running.".to_string());
            self.bedrock_render_local();
            return;
        };
        let id = self.bedrock.next_id();
        self.bedrock.pull_error = None;
        self.bedrock.pull_update = None;
        let task_model = model.clone();
        let task = self.backend.spawn(async move {
            let result = local::pull_ollama_model(config, task_model, move |update| {
                crate::ui_thread::post(move |app| app.bedrock_on_pull_progress(id, update));
            })
            .await;
            crate::ui_thread::post(move |app| app.bedrock_on_pull_done(id, result));
        });
        self.bedrock.pull = Some(PullJob { id, model, task });
        self.bedrock_render_local();
    }

    fn bedrock_on_pull_progress(&mut self, id: u64, update: PullUpdate) {
        if self.bedrock.pull.as_ref().is_some_and(|pull| pull.id == id) {
            self.bedrock.pull_update = Some(update);
            self.bedrock_render_pull();
        }
    }

    fn bedrock_on_pull_done(&mut self, id: u64, result: Result<(), String>) {
        let Some(pull) = self.bedrock.pull.take_if(|pull| pull.id == id) else {
            return;
        };
        self.bedrock.pull_update = None;
        match result {
            Ok(()) => {
                self.bedrock.pull_success = Some(format!("Pulled {}.", pull.model));
                self.window
                    .global::<BedrockState>()
                    .set_pull_name(SharedString::new());
                self.bedrock_probe_local(LocalKind::Ollama);
            }
            Err(err) => {
                self.bedrock.pull_error = Some(format!("Unable to pull {}: {err}", pull.model));
            }
        }
        self.bedrock_render_local();
    }

    fn bedrock_cancel_pull(&mut self) {
        if let Some(pull) = self.bedrock.pull.take() {
            pull.task.abort();
            self.bedrock.pull_update = None;
            self.bedrock.pull_error = None;
            self.bedrock.pull_success = None;
            self.bedrock_render_local();
        }
    }

    fn bedrock_open_url(&mut self, url: &str) {
        if let Err(err) = webbrowser::open(url) {
            self.toast(format!("Could not open {url}: {err}"));
        }
    }

    // ----- rendering --------------------------------------------------------

    fn bedrock_render_methods(&self) {
        let discovered = self.bedrock.discovered_or_empty();
        let selected_profile =
            flow::selected_profile_name(std::env::var("AWS_PROFILE").ok().as_deref());
        let items: Vec<BedrockMethodItem> = self
            .bedrock
            .methods
            .iter()
            .map(|method| {
                let view = flow::describe_method(*method, &discovered, &selected_profile);
                let badges: Vec<SharedString> = view
                    .badges
                    .iter()
                    .map(|badge| badge.as_str().into())
                    .collect();
                BedrockMethodItem {
                    title: view.title.into(),
                    detail: view.detail.into(),
                    badges: ModelRc::from(Rc::new(VecModel::from(badges))),
                    fields: view.fields.code(),
                }
            })
            .collect();
        self.window
            .global::<BedrockState>()
            .set_methods(ModelRc::from(Rc::new(VecModel::from(items))));
    }

    /// Pushes status, form, and progress state into `BedrockState`.
    fn bedrock_render(&self) {
        let state = self.window.global::<BedrockState>();
        let controller = &self.bedrock;
        let running = self.bedrock_running_provider();

        // Current provider.
        let (provider_label, model_label, model_is_default) = match controller.status.as_ref() {
            Some(status) => (
                flow::provider_label(&status.provider_id, status.provider_name.as_deref()),
                status
                    .model
                    .clone()
                    .unwrap_or_else(|| "Provider default".to_string()),
                status.model.is_none(),
            ),
            None => ("…".to_string(), String::new(), false),
        };
        state.set_provider_label(provider_label.into());
        state.set_model_label(model_label.into());
        state.set_model_is_default(model_is_default);
        let show_aws = controller
            .status
            .as_ref()
            .is_some_and(|status| flow::is_bedrock_provider(&status.provider_id));
        let aws = |value: Option<&String>| -> SharedString {
            value
                .filter(|_| show_aws)
                .map(|value| value.as_str().into())
                .unwrap_or_default()
        };
        state.set_aws_profile(aws(controller
            .status
            .as_ref()
            .and_then(|s| s.aws_profile.as_ref())));
        state.set_aws_region(aws(controller
            .status
            .as_ref()
            .and_then(|s| s.aws_region.as_ref())));
        state.set_account_label(
            controller
                .account
                .as_ref()
                .map(|account| {
                    let profile = controller
                        .status
                        .as_ref()
                        .and_then(|status| status.aws_profile.as_deref());
                    flow::account_label(account, profile)
                })
                .unwrap_or_default()
                .into(),
        );
        let origin_note = match controller.status.as_ref() {
            Some(status) if status.provider_locked => format!(
                "The provider is set by {}, which overrides changes made on this page.",
                status
                    .provider_origin
                    .as_deref()
                    .unwrap_or("another config layer")
            ),
            Some(status) => status
                .provider_origin
                .as_ref()
                .map(|origin| {
                    format!("The provider comes from {origin}. Changes here override it.")
                })
                .unwrap_or_default(),
            None => String::new(),
        };
        state.set_origin_note(origin_note.into());
        state.set_locked(
            controller
                .status
                .as_ref()
                .is_some_and(|status| status.provider_locked),
        );
        state.set_status_loading(controller.status_loading > 0);
        state.set_status_error(controller.status_error.clone().unwrap_or_default().into());
        let configured = controller.configured_provider();
        let restart_note = match (&running, controller.status.as_ref()) {
            (Some(running), Some(_)) if running != configured && controller.flow.is_none() => {
                let running_name = self
                    .config
                    .as_ref()
                    .map(|config| config.model_provider.name.as_str());
                let configured_name = controller
                    .status
                    .as_ref()
                    .and_then(|status| status.provider_name.as_deref());
                format!(
                    "Codex is still using {}. Restart it to switch to {}.",
                    flow::provider_label(running, running_name),
                    flow::provider_label(configured, configured_name)
                )
            }
            _ => String::new(),
        };
        state.set_restart_note(restart_note.into());
        let connection_note = if self.bedrock_remote() {
            format!(
                "Connected to {}. Changes here edit that server's config and take effect when it restarts.",
                self.connection_label
            )
        } else {
            String::new()
        };
        state.set_connection_note(connection_note.into());
        state.set_can_revert(controller.status.is_some() && configured != flow::OPENAI_PROVIDER_ID);
        state.set_bedrock_active(running.as_deref().is_some_and(flow::is_bedrock_provider));

        // Form.
        state.set_endpoint(match controller.endpoint {
            Endpoint::Mantle => 0,
            Endpoint::Runtime => 1,
        });
        let region = controller
            .region
            .clone()
            .unwrap_or_else(|| flow::DEFAULT_REGION.to_string());
        state.set_endpoint_note(controller.endpoint.description(&region).into());
        state.set_discovering(controller.discover_id.is_some());
        let discover_note = match (&controller.discover_error, &controller.discovered) {
            (Some(error), _) => error.clone(),
            (None, Some(discovered))
                if discovered.profiles.is_empty() && discovered.environment_credentials.is_empty() =>
            {
                "No AWS profiles or environment credentials were found. Enter a profile name or keys below, or configure the AWS CLI and scan again."
                    .to_string()
            }
            _ => String::new(),
        };
        state.set_discover_note(discover_note.into());
        let method_index = controller
            .method
            .and_then(|method| controller.methods.iter().position(|known| *known == method))
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1);
        state.set_method_index(method_index);
        state.set_region_index(
            flow::region_index(&region)
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
        state.set_region_note(controller.region_note.as_str().into());
        state.set_gov_cloud(flow::is_gov_cloud_region(&region));

        // Progress and notices.
        let busy = controller.busy();
        state.set_busy(busy);
        state.set_busy_area(
            controller
                .flow
                .as_ref()
                .map_or(Area::Bedrock, |flow| Area::of_flow(&flow.kind))
                .code(),
        );
        let current_step = controller.flow.as_ref().and_then(Flow::current);
        let busy_text = if controller.validation.is_some() {
            "Checking the AWS profile's credentials…"
        } else if controller.awaiting_confirmation {
            "Waiting for your confirmation…"
        } else {
            current_step.map_or("Working…", Step::progress_label)
        };
        state.set_busy_text(busy_text.into());
        state.set_busy_cancellable(
            controller.validation.is_some() || current_step.is_some_and(Step::is_cancellable),
        );
        match &controller.notice {
            Some(notice) => {
                state.set_notice_area(notice.area.code());
                state.set_notice_kind(notice.kind.code());
                state.set_notice_text(notice.text.as_str().into());
            }
            None => {
                state.set_notice_area(-1);
                state.set_notice_text(SharedString::new());
            }
        }
    }

    fn bedrock_render_models(&self) {
        let state = self.window.global::<BedrockState>();
        let controller = &self.bedrock;
        let labels: Vec<SharedString> = controller
            .models
            .iter()
            .map(|model| {
                if model.display_name.is_empty() || model.display_name == model.model {
                    model.model.as_str().into()
                } else {
                    format!("{} · {}", model.display_name, model.model).into()
                }
            })
            .collect();
        state.set_models(ModelRc::from(Rc::new(VecModel::from(labels))));
        let index = self.bedrock_effective_model_index();
        state.set_model_index(
            index
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
        state.set_models_loading(controller.models_loading);
        let selected = index.and_then(|index| controller.models.get(index));
        let configured = self.bedrock_configured_model();
        // Saving the provider default when nothing is configured changes
        // nothing, so only a different pick counts as an edit.
        state.set_model_dirty(selected.is_some_and(|model| match configured {
            Some(configured) => model.model != configured,
            None => !model.is_default,
        }));
        let note = if let Some(error) = &controller.models_error {
            error.clone()
        } else if controller.models_loading {
            "Loading models…".to_string()
        } else if configured.is_some_and(|configured| {
            !controller.models.is_empty()
                && !controller
                    .models
                    .iter()
                    .any(|model| model.model == configured)
        }) {
            format!(
                "The configured model {} is not in this provider's catalog. Save another model so new threads can start.",
                configured.unwrap_or_default()
            )
        } else if configured.is_none() {
            match selected {
                Some(model) => format!(
                    "No model saved; new threads use the provider default ({}).",
                    model.model
                ),
                None => String::new(),
            }
        } else {
            selected
                .map(|model| model.description.clone())
                .unwrap_or_default()
        };
        state.set_models_note(note.into());
    }

    /// Pushes the local server rows when they changed (rebuilding the rows
    /// would reset the pull input), then the pull progress.
    fn bedrock_render_local(&mut self) {
        let rows = self.bedrock_local_rows();
        if self.bedrock.local_rendered.as_ref() != Some(&rows) {
            let servers: Vec<LocalServerView> = rows
                .iter()
                .map(|row| LocalServerView {
                    id: row.kind.provider_id().into(),
                    title: row.kind.title().into(),
                    state: row.state,
                    status: row.status.as_str().into(),
                    message: row.message.as_str().into(),
                    can_use: row.can_use,
                    can_pull: row.kind == LocalKind::Ollama && row.can_use,
                    models: ModelRc::from(Rc::new(VecModel::from(
                        row.models
                            .iter()
                            .map(|(name, in_use)| LocalModelItem {
                                name: name.as_str().into(),
                                in_use: *in_use,
                            })
                            .collect::<Vec<_>>(),
                    ))),
                })
                .collect();
            self.window
                .global::<BedrockState>()
                .set_local_servers(ModelRc::from(Rc::new(VecModel::from(servers))));
            self.bedrock.local_rendered = Some(rows);
        }
        self.bedrock_render_pull();
    }

    fn bedrock_local_rows(&self) -> Vec<LocalRow> {
        let controller = &self.bedrock;
        let running_provider = self.bedrock_running_provider();
        let configured_model = self
            .bedrock_configured_model()
            .map(str::to_string)
            .or_else(|| self.config.as_ref().and_then(|config| config.model.clone()));
        LocalKind::ALL
            .into_iter()
            .map(|kind| {
                let in_use_provider = running_provider.as_deref() == Some(kind.provider_id())
                    && controller.configured_provider() == kind.provider_id();
                let mut row = LocalRow {
                    kind,
                    state: 0,
                    status: "Not checked".to_string(),
                    message: String::new(),
                    can_use: false,
                    models: Vec::new(),
                };
                match controller.local.get(&kind) {
                    None | Some(LocalState::Unknown) => {}
                    Some(LocalState::Checking) => {
                        row.state = 1;
                        row.status = "Checking…".to_string();
                    }
                    Some(LocalState::Unavailable(error)) => {
                        row.state = 3;
                        row.status = "Not running".to_string();
                        row.message.clone_from(error);
                    }
                    Some(LocalState::Running(probe)) => {
                        let mut parts = vec!["Running".to_string()];
                        if let Some(version) = &probe.version {
                            parts.push(format!("v{version}"));
                        }
                        if !probe.base_url.is_empty() {
                            parts.push(probe.base_url.clone());
                        }
                        row.state = 2;
                        row.status = parts.join(" · ");
                        row.message = probe.warning.clone().unwrap_or_default();
                        row.can_use = probe.warning.is_none();
                        row.models = probe
                            .models
                            .iter()
                            .map(|name| {
                                let in_use = in_use_provider
                                    && configured_model.as_deref() == Some(name.as_str());
                                (name.clone(), in_use)
                            })
                            .collect();
                    }
                }
                row
            })
            .collect()
    }

    fn bedrock_render_pull(&self) {
        let state = self.window.global::<BedrockState>();
        let controller = &self.bedrock;
        state.set_pulling(controller.pull.is_some());
        let update = controller.pull_update.as_ref();
        state.set_pull_status(
            update
                .map(|update| update.status.clone())
                .or_else(|| {
                    controller
                        .pull
                        .as_ref()
                        .map(|pull| format!("Starting to pull {}…", pull.model))
                })
                .unwrap_or_default()
                .into(),
        );
        state.set_pull_progress(update.and_then(|update| update.fraction).unwrap_or(-1.0));
        state.set_pull_error(controller.pull_error.clone().unwrap_or_default().into());
        state.set_pull_success(controller.pull_success.clone().unwrap_or_default().into());
    }
}

/// Notice for a finished switch back to OpenAI, given the provider status
/// read afterwards.
fn revert_outcome(status: &ProviderStatus, success: String) -> (NoticeKind, String) {
    if status.provider_id == flow::OPENAI_PROVIDER_ID {
        return (NoticeKind::Success, success);
    }
    let provider = flow::provider_label(&status.provider_id, status.provider_name.as_deref());
    let text = match &status.provider_origin {
        Some(origin) => format!(
            "Codex still uses {provider}: {origin} selects it. Change it there to use OpenAI."
        ),
        None => format!("Codex still uses {provider}. Check model_provider in config.toml."),
    };
    (NoticeKind::Error, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn status(provider_id: &str, origin: Option<&str>) -> ProviderStatus {
        ProviderStatus {
            provider_id: provider_id.to_string(),
            provider_origin: origin.map(str::to_string),
            ..ProviderStatus::default()
        }
    }

    #[test]
    fn a_revert_succeeds_only_when_openai_is_selected() {
        assert_eq!(
            revert_outcome(
                &status("openai", None),
                "Codex uses OpenAI again.".to_string()
            ),
            (NoticeKind::Success, "Codex uses OpenAI again.".to_string())
        );
        assert_eq!(
            revert_outcome(
                &status("amazon-bedrock", Some("system config /etc/codex/config.toml")),
                "Codex uses OpenAI again.".to_string()
            ),
            (
                NoticeKind::Error,
                "Codex still uses Amazon Bedrock (Mantle): system config /etc/codex/config.toml selects it. Change it there to use OpenAI.".to_string()
            )
        );
    }
}
