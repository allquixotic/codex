//! Embedded app-server startup.
//!
//! Mirrors the Embedded path of `tui/src/startup_orchestration.rs`: config
//! bootstrap, cloud config bundle, environment manager, state DB, tracing, and
//! finally `InProcessAppServerClient::start`. Everything here must run on a
//! Tokio worker thread; the futures are large and the main thread belongs to
//! the UI toolkit.
//!
//! [`RestartRecipe`] keeps the inputs needed to rebuild `Config` and start a
//! fresh embedded server, which is required whenever the model provider
//! changes (the app-server fixes its provider and model catalog at startup).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Context;
use codex_app_server_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_app_server_client::EmbeddedNetworkPolicy;
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_client::legacy_core::config::Config;
use codex_app_server_client::legacy_core::config::ConfigBuilder;
use codex_app_server_client::legacy_core::config::ConfigOverrides;
use codex_app_server_client::legacy_core::config::bootstrap_auth_config;
use codex_app_server_client::legacy_core::config::load_config_toml_with_layer_stack;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_arg0::Arg0DispatchPaths;
use codex_cloud_config::cloud_config_bundle_loader_for_storage;
use codex_config::CloudConfigBundleLoader;
use codex_config::ConfigLoadError;
use codex_config::ConfigLoadOptions;
use codex_config::LoaderOverrides;
use codex_config::TomlValue;
use codex_config::format_config_error_with_source;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimeOptions;
use codex_feedback::CodexFeedback;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_login::enforce_login_restrictions;
use codex_login::is_workload_identity_selected;
use codex_protocol::protocol::SessionSource;
use codex_rollout::StateDbHandle;
use codex_rollout::state_db;
use codex_state::log_db;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::canonicalize_existing_preserving_symlinks;
use codex_utils_home_dir::find_codex_home;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Client name reported to the app-server. It becomes the HTTP originator.
pub(crate) const GUI_CLIENT_NAME: &str = "codex-gui";
const GUI_LOG_FILE_PREFIX: &str = "codex-gui";
const GUI_LOG_MAX_FILES: usize = 7;
const OTEL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// Inputs chosen by the user or command line before startup.
#[derive(Clone, Debug)]
pub(crate) struct StartupOptions {
    /// Embedded server (default) or a daemon/remote app-server; an error
    /// when the saved or requested connection could not be resolved, which
    /// the UI reports instead of starting.
    pub(crate) connection: Result<crate::connection::ConnectionTarget, String>,
    /// Folder used for the startup config load. Each tab still passes its
    /// own cwd to `thread/start`.
    pub(crate) cwd: Option<PathBuf>,
    /// Raw `-c key=value` overrides, already parsed.
    pub(crate) cli_overrides: Vec<(String, TomlValue)>,
}

/// Process-wide services created once and kept until exit.
pub(crate) struct ProcessServices {
    pub(crate) otel: Option<codex_otel::OtelProvider>,
    _log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl ProcessServices {
    pub(crate) async fn shutdown(self) {
        if let Some(otel) = self.otel
            && let Err(err) = otel.shutdown_with_timeout(OTEL_SHUTDOWN_TIMEOUT).await
        {
            tracing::warn!(%err, "failed to finish telemetry shutdown");
        }
    }
}

/// Everything needed to rebuild `Config` and start another embedded server.
#[derive(Clone)]
pub(crate) struct RestartRecipe {
    start_args: InProcessClientStartArgs,
    harness_overrides: ConfigOverrides,
}

impl RestartRecipe {
    /// Reloads `Config` from disk and starts a new embedded app-server.
    ///
    /// The caller must have shut the previous client down first so the two
    /// servers never write the same threads concurrently.
    pub(crate) async fn restart(&mut self) -> anyhow::Result<(InProcessAppServerClient, Config)> {
        let mut config = Box::pin(
            ConfigBuilder::default()
                .cli_overrides(self.start_args.cli_overrides.clone())
                .harness_overrides(self.harness_overrides.clone())
                .loader_overrides(self.start_args.loader_overrides.clone())
                .strict_config(self.start_args.strict_config)
                .cloud_config_bundle(self.start_args.cloud_config_bundle.clone())
                .build(),
        )
        .await
        .map_err(describe_config_error)?;
        self.start_args
            .embedded_network_policy
            .bind_config(&mut config);
        if !is_workload_identity_selected() {
            // A login may have changed the identity used to fetch managed config.
            self.start_args.cloud_config_bundle = cloud_config_bundle_loader_for_storage(
                self.start_args
                    .embedded_network_policy
                    .bind_bootstrap_auth(config.auth_config()),
                /*enable_codex_api_key_env*/ false,
            )
            .await
            .context("failed to prepare cloud config bundle")?;
        }
        self.start_args.config = Arc::new(config.clone());
        self.start_args.config_warnings = config_warnings(&config, &[]);
        let client = InProcessAppServerClient::start(self.start_args.clone())
            .await
            .context("failed to restart embedded app server")?;
        Ok((client, config))
    }
}

/// Result of a successful first start.
pub(crate) struct Startup {
    pub(crate) client: InProcessAppServerClient,
    pub(crate) config: Config,
    pub(crate) restart: RestartRecipe,
    pub(crate) services: ProcessServices,
}

/// Builds config and starts the embedded app-server.
///
/// Must run on a Tokio worker; never poll it on the UI thread.
pub(crate) async fn start_embedded(
    arg0_paths: Arg0DispatchPaths,
    options: StartupOptions,
) -> anyhow::Result<Startup> {
    let codex_home = find_codex_home().context("failed to resolve CODEX_HOME")?;
    let cli_overrides = options.cli_overrides;
    let loader_overrides = LoaderOverrides::default();
    let strict_config = false;
    let workload_identity = is_workload_identity_selected();

    let embedded_network_policy = EmbeddedNetworkPolicy::load(&loader_overrides).await;

    let prepared_environment_manager =
        EnvironmentManager::prepare_from_codex_home(codex_home.as_path())
            .await
            .map_err(std::io::Error::other)
            .context("failed to prepare execution environments")?;

    let config_cwd: Option<AbsolutePathBuf> =
        if prepared_environment_manager.default_environment_is_remote() {
            None
        } else {
            let cwd = match options.cwd {
                Some(path) => path,
                None => startup_fallback_cwd(),
            };
            Some(AbsolutePathBuf::from_absolute_path(
                canonicalize_existing_preserving_symlinks(&cwd)
                    .with_context(|| format!("folder {} is not accessible", cwd.display()))?,
            )?)
        };

    let bootstrap = Box::pin(load_config_toml_with_layer_stack(
        codex_home.as_path(),
        config_cwd.as_ref(),
        cli_overrides.clone(),
        ConfigLoadOptions {
            loader_overrides: loader_overrides.clone(),
            strict_config,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
        },
    ))
    .await
    .map_err(describe_config_error)?;

    let bootstrap_cloud_bundle = cloud_config_bundle_loader_for_storage(
        embedded_network_policy
            .bind_bootstrap_auth(bootstrap_auth_config(codex_home.as_path(), &bootstrap)?),
        /*enable_codex_api_key_env*/ false,
    )
    .await
    .context("failed to prepare cloud config bundle")?;

    let harness_overrides = ConfigOverrides {
        cwd: config_cwd.clone().map(AbsolutePathBuf::into_path_buf),
        codex_self_exe: arg0_paths.codex_self_exe.clone(),
        codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe.clone(),
        main_execve_wrapper_exe: arg0_paths.main_execve_wrapper_exe.clone(),
        ..Default::default()
    };
    let mut config = Box::pin(
        ConfigBuilder::default()
            .cli_overrides(cli_overrides.clone())
            .harness_overrides(harness_overrides.clone())
            .loader_overrides(loader_overrides.clone())
            .strict_config(strict_config)
            .cloud_config_bundle(bootstrap_cloud_bundle.clone())
            .build(),
    )
    .await
    .map_err(describe_config_error)?;
    embedded_network_policy.activate(&mut config);

    let cloud_config_bundle = if workload_identity {
        bootstrap_cloud_bundle
    } else {
        cloud_config_bundle_loader_for_storage(
            embedded_network_policy.bind_bootstrap_auth(config.auth_config()),
            /*enable_codex_api_key_env*/ false,
        )
        .await
        .context("failed to prepare cloud config bundle")?
    };

    let local_runtime_paths = ExecServerRuntimeOptions::from_optional_paths(
        arg0_paths.codex_self_exe.clone(),
        arg0_paths.codex_linux_sandbox_exe.clone(),
    )?;
    #[cfg(target_os = "macos")]
    let local_runtime_paths = local_runtime_paths.with_allowed_symlinked_codex_home(
        codex_config::allowed_symlinked_codex_home(&config.config_layer_stack, &config.codex_home),
    );
    let environment_manager = Arc::new(
        prepared_environment_manager
            .build(
                Some(local_runtime_paths),
                embedded_network_policy.bind(config.http_client_factory()),
            )
            .map_err(std::io::Error::other)
            .context("failed to build environment manager")?,
    );

    let mut extra_warnings = Vec::new();
    let otel = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        codex_app_server_client::build_otel_provider(
            &config,
            env!("CARGO_PKG_VERSION"),
            /*service_name_override*/ None,
            /*default_analytics_enabled*/ true,
        )
    })) {
        Ok(Ok(otel)) => otel,
        Ok(Err(err)) => {
            extra_warnings.push(format!("Could not create otel exporter: {err}"));
            None
        }
        Err(_) => {
            extra_warnings
                .push("Could not create otel exporter: panicked during initialization".to_string());
            None
        }
    };
    if let Some(metrics) = otel.as_ref().and_then(codex_otel::OtelProvider::metrics) {
        let _ = codex_otel::record_process_start_once(metrics, GUI_CLIENT_NAME);
        let _ = codex_state::install_process_db_telemetry(
            codex_rollout::sqlite_telemetry_recorder(metrics.clone(), GUI_CLIENT_NAME),
        );
    }

    let state_db: StateDbHandle = state_db::try_init(&config).await.map_err(|err| {
        let path = codex_state::runtime_db_path_for_corruption_error(&err)
            .unwrap_or_else(|| config.sqlite_config().state_db_path());
        anyhow::anyhow!(
            "failed to open the local Codex database at {}: {err:#}",
            path.display()
        )
    })?;

    set_default_client_residency_requirement(config.enforce_residency.value());
    if !workload_identity {
        enforce_login_restrictions(&config.auth_config())
            .await
            .context("login restrictions")?;
    }

    let feedback = CodexFeedback::new();
    let log_db = log_db::start(state_db.clone(), Arc::new(feedback.clone()));
    let log_guard = init_tracing(&config, &feedback, Some(log_db.clone()), otel.as_ref());

    let start_args = InProcessClientStartArgs {
        arg0_paths,
        config: Arc::new(config.clone()),
        cli_overrides,
        loader_overrides,
        strict_config,
        cloud_config_bundle,
        embedded_network_policy,
        feedback: feedback.clone(),
        log_db: Some(log_db),
        state_db: Some(state_db),
        environment_manager,
        config_warnings: config_warnings(&config, &extra_warnings),
        // `Cli` keeps GUI threads visible to `thread/list` and `codex resume`.
        session_source: SessionSource::Cli,
        enable_codex_api_key_env: false,
        client_name: GUI_CLIENT_NAME.to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY.max(512),
    };
    let client = InProcessAppServerClient::start(start_args.clone())
        .await
        .context("failed to start embedded app server")?;

    Ok(Startup {
        client,
        config,
        restart: RestartRecipe {
            start_args,
            harness_overrides,
        },
        services: ProcessServices {
            otel,
            _log_guard: log_guard,
        },
    })
}

/// Startup folder when none was given: launching from Finder, the Dock, or
/// the Start menu leaves an arbitrary process cwd (often `/`).
fn startup_fallback_cwd() -> PathBuf {
    match std::env::current_dir() {
        Ok(cwd) if cwd.parent().is_some() => cwd,
        _ => dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
    }
}

/// File-only logging into `$CODEX_HOME/log`, used before (or without) a
/// local `Config`. Built once per process: the writer thread and its guard
/// live until exit, so retries cannot leak them.
fn file_log_dispatch() -> Option<&'static tracing::Dispatch> {
    static DISPATCH: OnceLock<Option<tracing::Dispatch>> = OnceLock::new();
    DISPATCH
        .get_or_init(|| {
            let log_dir = find_codex_home().ok()?.join("log");
            let _ = std::fs::create_dir_all(&log_dir);
            let appender = tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix(GUI_LOG_FILE_PREFIX)
                .filename_suffix("log")
                .max_log_files(GUI_LOG_MAX_FILES)
                .build(log_dir.as_path())
                .ok()?;
            // The writer must outlive every log call through this dispatch.
            let (writer, guard) = tracing_appender::non_blocking(appender);
            std::mem::forget(guard);
            let filter = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("codex_gui=info,codex_app_server_client=info"));
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_target(true)
                    .with_ansi(false)
                    .with_filter(filter),
            );
            Some(tracing::Dispatch::new(subscriber))
        })
        .as_ref()
}

/// File logging for daemon/remote mode, where no local `Config` is built.
/// Safe to call on every connection attempt.
pub(crate) fn init_remote_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Some(dispatch) = file_log_dispatch() {
            let _ = tracing::dispatcher::set_global_default(dispatch.clone());
        }
    });
}

/// Logs a startup failure. Failures can happen before [`init_tracing`]
/// installed the global subscriber (config, state DB, login restrictions),
/// so they go straight to the log file then.
pub(crate) fn log_startup_error(message: &str) {
    if tracing::dispatcher::has_been_set() {
        tracing::error!(error = %message, "codex-gui failed to start");
    } else if let Some(dispatch) = file_log_dispatch() {
        tracing::dispatcher::with_default(dispatch, || {
            tracing::error!(error = %message, "codex-gui failed to start");
        });
    }
}

fn config_warnings(config: &Config, extra: &[String]) -> Vec<ConfigWarningNotification> {
    config
        .startup_warnings
        .iter()
        .chain(extra.iter())
        .map(|summary| ConfigWarningNotification {
            summary: summary.clone(),
            details: None,
            path: None,
            range: None,
        })
        .collect()
}

fn describe_config_error(err: std::io::Error) -> anyhow::Error {
    match err
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ConfigLoadError>())
    {
        Some(config_error) => anyhow::anyhow!(
            "Error loading config.toml:\n{}",
            format_config_error_with_source(config_error.config_error())
        ),
        None => anyhow::Error::new(err).context("Error loading configuration"),
    }
}

/// Installs the global tracing subscriber.
///
/// The GUI has no stderr on Windows, so unlike the TUI it always keeps a
/// file log, rotated daily and capped at [`GUI_LOG_MAX_FILES`] files.
fn init_tracing(
    config: &Config,
    feedback: &CodexFeedback,
    log_db: Option<log_db::LogDbLayer>,
    otel: Option<&codex_otel::OtelProvider>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    // The appender prunes old files at startup and complains when the
    // directory does not exist yet.
    let _ = std::fs::create_dir_all(&config.log_dir);
    let (file_layer, guard) = match tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(GUI_LOG_FILE_PREFIX)
        .filename_suffix("log")
        .max_log_files(GUI_LOG_MAX_FILES)
        .build(&config.log_dir)
    {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                EnvFilter::new("codex_core=info,codex_gui=info,codex_rmcp_client=info")
            });
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_target(true)
                .with_ansi(false)
                .with_filter(filter);
            (Some(layer), Some(guard))
        }
        Err(_) => (None, None),
    };
    let log_db_layer = log_db.map(|layer| layer.with_filter(log_db::default_filter()));
    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(feedback.logger_layer())
        .with(feedback.metadata_layer())
        .with(log_db_layer)
        .with(otel.and_then(codex_otel::OtelProvider::logger_layer))
        .with(otel.and_then(codex_otel::OtelProvider::tracing_layer))
        .try_init();
    guard
}
