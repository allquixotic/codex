//! Bridge between the Tokio side (embedded app-server) and the UI thread.
//!
//! One pump task owns the [`AppServerClient`]. It drains server
//! events, coalesces streaming deltas, and posts at most one batch per frame
//! to the UI thread. Requests never go through the pump: callers clone a
//! request handle and run them on their own tasks, so a slow request cannot
//! stall event delivery. Resolving server requests, restarting the server
//! after a provider change, and shutdown go through a small control channel
//! because only the client itself can do them.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_app_server_client::AppServerClient;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_client::TypedRequestError;
use codex_app_server_client::legacy_core::config::Config;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_arg0::Arg0DispatchPaths;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;

use crate::app::AppController;
use crate::connection::ConnectionTarget;
use crate::startup::ProcessServices;
use crate::startup::RestartRecipe;
use crate::startup::StartupOptions;
use crate::ui_thread;

/// Target time between UI batches (one frame at 60 Hz).
const FRAME: Duration = Duration::from_millis(16);
/// Flush early when a batch grows this large so one invoke stays cheap.
const MAX_BATCH_EVENTS: usize = 1024;
/// Total budget for unsubscribing threads during shutdown.
const UNSUBSCRIBE_BUDGET: Duration = Duration::from_secs(2);

/// Error returned by [`Backend::request`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum BackendError {
    #[error("{0}")]
    Request(#[from] TypedRequestError),
    #[error("the embedded app-server is not running")]
    Unavailable,
}

impl BackendError {
    /// Server-side JSON-RPC error, when the server rejected the request.
    pub(crate) fn server_error(&self) -> Option<&JSONRPCErrorError> {
        match self {
            Self::Request(TypedRequestError::Server { source, .. }) => Some(source),
            _ => None,
        }
    }

    /// Short message suitable for an inline error label.
    pub(crate) fn user_message(&self) -> String {
        match self.server_error() {
            Some(error) => error.message.clone(),
            None => self.to_string(),
        }
    }
}

enum Control {
    Resolve {
        request_id: RequestId,
        result: serde_json::Value,
    },
    Reject {
        request_id: RequestId,
        error: JSONRPCErrorError,
    },
    Restart,
    /// Stop the current server (if any) and connect to another target.
    Reconnect(ConnectionTarget),
    Shutdown {
        thread_ids: Vec<String>,
        done: oneshot::Sender<()>,
    },
}

/// Shown when the event stream ends without a `Disconnected` reason.
const CONNECTION_LOST: &str = "The app-server stopped or the connection was lost.";

#[derive(Clone)]
enum ServerState {
    Starting,
    Ready(AppServerRequestHandle),
    Stopped,
}

struct Shared {
    rt: tokio::runtime::Handle,
    control_tx: mpsc::UnboundedSender<Control>,
    state: watch::Receiver<ServerState>,
    next_request_id: AtomicU64,
    /// Where the server runs. Changes only through [`Backend::reconnect`];
    /// `None` when the configured address could not be parsed.
    connection: std::sync::RwLock<Option<ConnectionTarget>>,
}

/// Cheap, cloneable, `Send` handle to the embedded app-server.
#[derive(Clone)]
pub(crate) struct Backend {
    shared: Arc<Shared>,
}

impl Backend {
    /// Creates the handle and starts the embedded app-server in the background.
    ///
    /// Returns immediately; the UI is told about the outcome through
    /// [`AppController::on_server_ready`] or
    /// [`AppController::on_server_failed`].
    pub(crate) fn launch(
        rt: tokio::runtime::Handle,
        arg0_paths: Arg0DispatchPaths,
        options: StartupOptions,
    ) -> Self {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (state_tx, state_rx) = watch::channel(ServerState::Starting);
        let backend = Self {
            shared: Arc::new(Shared {
                rt: rt.clone(),
                control_tx,
                state: state_rx,
                next_request_id: AtomicU64::new(1),
                connection: std::sync::RwLock::new(options.connection.clone().ok()),
            }),
        };
        rt.spawn(Box::pin(run_backend(
            arg0_paths, options, state_tx, control_rx,
        )));
        backend
    }

    /// Where the server runs (embedded, local daemon, or remote). Known from
    /// the start, unlike readiness. An unparsable address reports embedded,
    /// which is what the user is offered to fall back to.
    pub(crate) fn connection(&self) -> ConnectionTarget {
        match self.shared.connection.read() {
            Ok(connection) => connection.clone().unwrap_or(ConnectionTarget::Embedded),
            Err(poisoned) => poisoned
                .into_inner()
                .clone()
                .unwrap_or(ConnectionTarget::Embedded),
        }
    }

    /// Whether the server's files live on another machine, so local paths
    /// (pasted images, file searches) must not be handed to it.
    pub(crate) fn uses_remote_workspace(&self) -> bool {
        self.connection().uses_remote_workspace()
    }

    /// Whether the GUI runs its own in-process server, as opposed to a
    /// daemon or remote app-server. Known even while the server is down.
    pub(crate) fn is_embedded(&self) -> bool {
        self.connection().is_embedded()
    }

    /// Tokio runtime handle for spawning background work.
    pub(crate) fn runtime(&self) -> &tokio::runtime::Handle {
        &self.shared.rt
    }

    /// Spawns `fut` on the runtime.
    pub(crate) fn spawn<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.shared.rt.spawn(fut)
    }

    /// Fresh request id, unique for this process.
    pub(crate) fn next_request_id(&self) -> RequestId {
        let id = self.shared.next_request_id.fetch_add(1, Ordering::Relaxed);
        RequestId::String(format!("gui-{id}"))
    }

    /// Sends a typed request, waiting for the server to finish starting first.
    pub(crate) async fn request<T>(&self, request: ClientRequest) -> Result<T, BackendError>
    where
        T: DeserializeOwned,
    {
        let handle = self.wait_ready().await?;
        Ok(handle.request_typed::<T>(request).await?)
    }

    /// Builds a request with a fresh id via `build`, sends it on a task, and
    /// delivers the result to `on_done` on the UI thread.
    pub(crate) fn call<T, B, D>(&self, build: B, on_done: D)
    where
        T: DeserializeOwned + Send + 'static,
        B: FnOnce(RequestId) -> ClientRequest,
        D: FnOnce(&mut AppController, Result<T, BackendError>) + Send + 'static,
    {
        let request = build(self.next_request_id());
        let backend = self.clone();
        self.spawn(async move {
            let result = backend.request::<T>(request).await;
            ui_thread::post(move |app| on_done(app, result));
        });
    }

    /// Like [`Backend::call`] but discards the response, logging failures.
    pub(crate) fn fire<T, B>(&self, build: B)
    where
        T: DeserializeOwned + Send + 'static,
        B: FnOnce(RequestId) -> ClientRequest,
    {
        let request = build(self.next_request_id());
        let method = request.method_name();
        let backend = self.clone();
        self.spawn(async move {
            if let Err(err) = backend.request::<T>(request).await {
                tracing::warn!(%err, method, "request failed");
            }
        });
    }

    /// Answers a server request (approval, user input, dynamic tool call).
    pub(crate) fn resolve(&self, request_id: RequestId, result: serde_json::Value) {
        let _ = self
            .shared
            .control_tx
            .send(Control::Resolve { request_id, result });
    }

    /// Answers a server request with a typed response.
    pub(crate) fn resolve_typed<T: serde::Serialize>(&self, request_id: RequestId, response: &T) {
        match serde_json::to_value(response) {
            Ok(value) => self.resolve(request_id, value),
            Err(err) => self.reject(
                request_id,
                JSONRPCErrorError {
                    code: -32603,
                    message: format!("codex-gui failed to encode response: {err}"),
                    data: None,
                },
            ),
        }
    }

    /// Rejects a server request.
    pub(crate) fn reject(&self, request_id: RequestId, error: JSONRPCErrorError) {
        let _ = self
            .shared
            .control_tx
            .send(Control::Reject { request_id, error });
    }

    /// Restarts the embedded server with config reloaded from disk.
    ///
    /// Needed after the model provider changes. The UI receives
    /// [`AppController::on_server_ready`] again when the new server is up.
    pub(crate) fn restart(&self) {
        let _ = self.shared.control_tx.send(Control::Restart);
    }

    /// Stops the current server, if any, and connects to `target` instead
    /// (for example the embedded server after a remote one failed). Open
    /// threads are resumed on the new server like after a restart.
    pub(crate) fn reconnect(&self, target: ConnectionTarget) {
        match self.shared.connection.write() {
            Ok(mut connection) => *connection = Some(target.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(target.clone()),
        }
        let _ = self.shared.control_tx.send(Control::Reconnect(target));
    }

    /// Unsubscribes `thread_ids`, stops the server, and flushes telemetry.
    pub(crate) async fn shutdown(&self, thread_ids: Vec<String>) {
        let (done, done_rx) = oneshot::channel();
        if self
            .shared
            .control_tx
            .send(Control::Shutdown { thread_ids, done })
            .is_ok()
        {
            let _ = done_rx.await;
        }
    }

    /// Whether the server finished starting and has not stopped.
    pub(crate) fn is_ready(&self) -> bool {
        matches!(*self.shared.state.borrow(), ServerState::Ready(_))
    }

    async fn wait_ready(&self) -> Result<AppServerRequestHandle, BackendError> {
        let mut state = self.shared.state.clone();
        let ready = state
            .wait_for(|state| !matches!(state, ServerState::Starting))
            .await
            .map_err(|_| BackendError::Unavailable)?;
        match &*ready {
            ServerState::Ready(handle) => Ok(handle.clone()),
            ServerState::Starting | ServerState::Stopped => Err(BackendError::Unavailable),
        }
    }
}

/// Summary of the running server passed to the UI on (re)start.
pub(crate) struct ServerReady {
    /// Local config of the embedded server; `None` for daemon/remote servers.
    pub(crate) config: Option<Arc<Config>>,
    /// Human-readable connection description.
    pub(crate) connection: String,
    pub(crate) restarted: bool,
}

/// How to bring the server back after `Backend::restart`.
enum Relaunch {
    Embedded(Box<RestartRecipe>),
    Remote(RemoteAppServerConnectArgs),
}

/// A connected server, ready for the pump.
struct Started {
    client: AppServerClient,
    config: Option<Arc<Config>>,
    relaunch: Relaunch,
    services: Option<ProcessServices>,
}

/// Why [`Pump::run`] returned.
enum PumpExit {
    /// Shut down for good (or every `Backend` handle is gone).
    Stopped,
    /// The UI asked for another connection target.
    Reconnect(ConnectionTarget),
}

async fn run_backend(
    arg0_paths: Arg0DispatchPaths,
    options: StartupOptions,
    state_tx: watch::Sender<ServerState>,
    mut control_rx: mpsc::UnboundedReceiver<Control>,
) {
    let mut connection = options.connection.clone();
    // Services of replaced servers: their log writers must outlive every
    // log call, so they are shut down only when the process stops.
    let mut retired_services: Vec<ProcessServices> = Vec::new();
    let mut was_ready = false;
    loop {
        let label = connection
            .as_ref()
            .map(ConnectionTarget::label)
            .unwrap_or_default();
        match start_server(&arg0_paths, &options, &connection).await {
            Ok(started) => {
                let pump = Pump {
                    client: Some(started.client),
                    relaunch: started.relaunch,
                    connection: label,
                    services: started.services,
                    state_tx: &state_tx,
                    batch: Batch::default(),
                    disconnect_reported: false,
                };
                let restarted = was_ready;
                was_ready = true;
                let (exit, services) = pump.run(started.config, restarted, &mut control_rx).await;
                retired_services.extend(services);
                match exit {
                    PumpExit::Stopped => break,
                    PumpExit::Reconnect(target) => connection = Ok(target),
                }
            }
            Err(message) => {
                crate::startup::log_startup_error(&message);
                let _ = state_tx.send(ServerState::Stopped);
                ui_thread::post(move |app| app.on_server_failed(message));
                // Wait for a retry (for example after the user fixed
                // config.toml) and keep answering shutdown so it cannot hang.
                match wait_for_retry(&mut control_rx).await {
                    Some(Retry::Same) => {}
                    Some(Retry::Reconnect(target)) => connection = Ok(target),
                    None => break,
                }
                let _ = state_tx.send(ServerState::Starting);
            }
        }
    }
    let _ = state_tx.send(ServerState::Stopped);
    for services in retired_services {
        services.shutdown().await;
    }
}

/// Starts the embedded server or connects to a daemon/remote one.
async fn start_server(
    arg0_paths: &Arg0DispatchPaths,
    options: &StartupOptions,
    connection: &Result<ConnectionTarget, String>,
) -> Result<Started, String> {
    let target = connection.clone()?;
    let started = match target {
        ConnectionTarget::Embedded => Box::pin(crate::startup::start_embedded(
            arg0_paths.clone(),
            options.clone(),
        ))
        .await
        .map(|startup| {
            let crate::startup::Startup {
                client,
                config,
                restart,
                services,
            } = startup;
            Started {
                client: AppServerClient::InProcess(client),
                config: Some(Arc::new(config)),
                relaunch: Relaunch::Embedded(Box::new(restart)),
                services: Some(services),
            }
        }),
        ConnectionTarget::Remote(endpoint) => {
            crate::startup::init_remote_tracing();
            let args = remote_connect_args(endpoint);
            connect_remote(args.clone()).await.map(|client| Started {
                client,
                config: None,
                relaunch: Relaunch::Remote(args),
                services: None,
            })
        }
    };
    started.map_err(|err| format!("{err:#}"))
}

enum Retry {
    Same,
    Reconnect(ConnectionTarget),
}

/// Parks until the UI asks to retry; `None` means shut down.
async fn wait_for_retry(control_rx: &mut mpsc::UnboundedReceiver<Control>) -> Option<Retry> {
    loop {
        match control_rx.recv().await? {
            Control::Restart => return Some(Retry::Same),
            Control::Reconnect(target) => return Some(Retry::Reconnect(target)),
            Control::Shutdown { done, .. } => {
                let _ = done.send(());
                return None;
            }
            // No server to answer; its requests died with it.
            Control::Resolve { .. } | Control::Reject { .. } => {}
        }
    }
}

fn remote_connect_args(endpoint: RemoteAppServerEndpoint) -> RemoteAppServerConnectArgs {
    RemoteAppServerConnectArgs {
        endpoint,
        client_name: crate::startup::GUI_CLIENT_NAME.to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY.max(512),
    }
}

async fn connect_remote(args: RemoteAppServerConnectArgs) -> anyhow::Result<AppServerClient> {
    let client = RemoteAppServerClient::connect(args)
        .await
        .map_err(|err| anyhow::anyhow!("failed to connect to the app-server: {err}"))?;
    Ok(AppServerClient::Remote(client))
}

/// Message for the end of the event stream. The generic text is only used
/// when the client did not deliver a `Disconnected` reason, which must stay
/// visible instead.
fn connection_lost_message(disconnect_reported: bool) -> Option<String> {
    (!disconnect_reported).then(|| CONNECTION_LOST.to_string())
}

struct Pump<'a> {
    client: Option<AppServerClient>,
    relaunch: Relaunch,
    connection: String,
    services: Option<ProcessServices>,
    state_tx: &'a watch::Sender<ServerState>,
    batch: Batch,
    /// A `Disconnected` event with the specific reason was queued for the
    /// UI since the server last (re)started.
    disconnect_reported: bool,
}

impl Pump<'_> {
    /// Runs until shutdown or a reconnect request. Hands back the process
    /// services that are still alive so the caller can stop them last.
    async fn run(
        mut self,
        config: Option<Arc<Config>>,
        restarted: bool,
        control_rx: &mut mpsc::UnboundedReceiver<Control>,
    ) -> (PumpExit, Option<ProcessServices>) {
        self.announce_ready(config, restarted);
        let mut flush_at: Option<tokio::time::Instant> = None;
        loop {
            let flush_deadline = flush_at;
            tokio::select! {
                biased;
                control = control_rx.recv() => {
                    let Some(control) = control else {
                        // Every Backend handle is gone; nothing can observe us.
                        self.stop(Vec::new()).await;
                        return (PumpExit::Stopped, self.services.take());
                    };
                    match control {
                        Control::Resolve { request_id, result } => {
                            if let Some(client) = self.client.as_ref()
                                && let Err(err) = client.resolve_server_request(request_id, result).await
                            {
                                tracing::warn!(%err, "failed to resolve server request");
                            }
                        }
                        Control::Reject { request_id, error } => {
                            if let Some(client) = self.client.as_ref()
                                && let Err(err) = client.reject_server_request(request_id, error).await
                            {
                                tracing::warn!(%err, "failed to reject server request");
                            }
                        }
                        Control::Restart => {
                            self.flush();
                            flush_at = None;
                            self.restart_server().await;
                        }
                        Control::Reconnect(target) => {
                            self.flush();
                            self.stop(Vec::new()).await;
                            let _ = self.state_tx.send(ServerState::Starting);
                            return (PumpExit::Reconnect(target), self.services.take());
                        }
                        Control::Shutdown { thread_ids, done } => {
                            self.flush();
                            self.stop(thread_ids).await;
                            if let Some(services) = self.services.take() {
                                services.shutdown().await;
                            }
                            let _ = done.send(());
                            return (PumpExit::Stopped, None);
                        }
                    }
                }
                event = next_event(self.client.as_mut()) => {
                    match event {
                        Some(event) => {
                            if matches!(event, AppServerEvent::Disconnected { .. }) {
                                self.disconnect_reported = true;
                            }
                            self.batch.push(event);
                            if self.batch.len() >= MAX_BATCH_EVENTS {
                                self.flush();
                                flush_at = None;
                            } else if flush_at.is_none() {
                                flush_at = Some(tokio::time::Instant::now() + FRAME);
                            }
                        }
                        None => {
                            // The server exited or the connection dropped.
                            self.flush();
                            flush_at = None;
                            self.client = None;
                            let _ = self.state_tx.send(ServerState::Stopped);
                            if let Some(message) = connection_lost_message(self.disconnect_reported) {
                                ui_thread::post(move |app| app.on_server_failed(message));
                            }
                        }
                    }
                }
                () = sleep_until(flush_deadline) => {
                    self.flush();
                    flush_at = None;
                }
            }
        }
    }

    fn announce_ready(&self, config: Option<Arc<Config>>, restarted: bool) {
        if let Some(client) = self.client.as_ref() {
            let _ = self
                .state_tx
                .send(ServerState::Ready(client.request_handle()));
        }
        let ready = ServerReady {
            config,
            connection: self.connection.clone(),
            restarted,
        };
        ui_thread::post(move |app| app.on_server_ready(ready));
    }

    fn flush(&mut self) {
        let events = self.batch.take();
        if !events.is_empty() {
            ui_thread::post(move |app| app.handle_server_events(events));
        }
    }

    async fn restart_server(&mut self) {
        let _ = self.state_tx.send(ServerState::Starting);
        if let Some(client) = self.client.take()
            && let Err(err) = client.shutdown().await
        {
            tracing::warn!(%err, "failed to stop embedded app-server for restart");
        }
        let restarted = match &mut self.relaunch {
            Relaunch::Embedded(recipe) => {
                Box::pin(recipe.restart()).await.map(|(client, config)| {
                    (AppServerClient::InProcess(client), Some(Arc::new(config)))
                })
            }
            Relaunch::Remote(args) => connect_remote(args.clone())
                .await
                .map(|client| (client, None)),
        };
        match restarted {
            Ok((client, config)) => {
                self.client = Some(client);
                self.disconnect_reported = false;
                self.announce_ready(config, /*restarted*/ true);
            }
            Err(err) => {
                let _ = self.state_tx.send(ServerState::Stopped);
                let message = format!("{err:#}");
                tracing::error!(error = %message, "embedded app-server failed to restart");
                ui_thread::post(move |app| app.on_server_failed(message));
            }
        }
    }

    /// Unsubscribes `thread_ids` and stops the client. Process services
    /// (telemetry and the log writer) are left to the caller.
    async fn stop(&mut self, thread_ids: Vec<String>) {
        if let Some(client) = self.client.take() {
            let handle = client.request_handle();
            let unsubscribe_all = async {
                for (index, thread_id) in thread_ids.into_iter().enumerate() {
                    let result = handle
                        .request_typed::<ThreadUnsubscribeResponse>(
                            ClientRequest::ThreadUnsubscribe {
                                request_id: RequestId::String(format!("gui-shutdown-{index}")),
                                params: ThreadUnsubscribeParams { thread_id },
                            },
                        )
                        .await;
                    if let Err(err) = result {
                        tracing::warn!(%err, "thread/unsubscribe failed during shutdown");
                    }
                }
            };
            if tokio::time::timeout(UNSUBSCRIBE_BUDGET, unsubscribe_all)
                .await
                .is_err()
            {
                tracing::warn!("timed out unsubscribing threads during shutdown");
            }
            if let Err(err) = client.shutdown().await {
                tracing::warn!(%err, "failed to shut down embedded app-server");
            }
        }
        let _ = self.state_tx.send(ServerState::Stopped);
    }
}

async fn next_event(client: Option<&mut AppServerClient>) -> Option<AppServerEvent> {
    match client {
        Some(client) => client.next_event().await,
        // No server: park this branch until a control message changes state.
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Key identifying one streaming text channel of one item.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum DeltaKey {
    AgentMessage(String, String),
    Plan(String, String),
    ReasoningSummary(String, String, i64),
    ReasoningText(String, String, i64),
    CommandOutput(String, String),
}

/// Events collected for one frame, with streaming deltas concatenated.
#[derive(Default)]
pub(crate) struct Batch {
    events: Vec<AppServerEvent>,
    /// Index into `events` of the latest delta for each channel. Cleared on
    /// any non-delta event so ordering relative to item lifecycle and
    /// approvals is preserved exactly.
    open_deltas: HashMap<DeltaKey, usize>,
}

impl Batch {
    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    pub(crate) fn take(&mut self) -> Vec<AppServerEvent> {
        self.open_deltas.clear();
        std::mem::take(&mut self.events)
    }

    pub(crate) fn push(&mut self, event: AppServerEvent) {
        let key = match &event {
            AppServerEvent::ServerNotification(notification) => delta_key(notification),
            _ => None,
        };
        let Some(key) = key else {
            self.open_deltas.clear();
            self.events.push(event);
            return;
        };
        if let Some(&index) = self.open_deltas.get(&key)
            && let Some(AppServerEvent::ServerNotification(existing)) = self.events.get_mut(index)
            && let AppServerEvent::ServerNotification(incoming) = &event
            && append_delta(existing, incoming)
        {
            return;
        }
        self.open_deltas.insert(key, self.events.len());
        self.events.push(event);
    }
}

fn delta_key(notification: &ServerNotification) -> Option<DeltaKey> {
    match notification {
        ServerNotification::AgentMessageDelta(n) => Some(DeltaKey::AgentMessage(
            n.thread_id.clone(),
            n.item_id.clone(),
        )),
        ServerNotification::PlanDelta(n) => {
            Some(DeltaKey::Plan(n.thread_id.clone(), n.item_id.clone()))
        }
        ServerNotification::ReasoningSummaryTextDelta(n) => Some(DeltaKey::ReasoningSummary(
            n.thread_id.clone(),
            n.item_id.clone(),
            n.summary_index,
        )),
        ServerNotification::ReasoningTextDelta(n) => Some(DeltaKey::ReasoningText(
            n.thread_id.clone(),
            n.item_id.clone(),
            n.content_index,
        )),
        ServerNotification::CommandExecutionOutputDelta(n) => Some(DeltaKey::CommandOutput(
            n.thread_id.clone(),
            n.item_id.clone(),
        )),
        _ => None,
    }
}

/// Appends `incoming`'s delta text to `existing` when both are the same kind.
fn append_delta(existing: &mut ServerNotification, incoming: &ServerNotification) -> bool {
    match (existing, incoming) {
        (ServerNotification::AgentMessageDelta(a), ServerNotification::AgentMessageDelta(b)) => {
            a.delta.push_str(&b.delta);
            true
        }
        (ServerNotification::PlanDelta(a), ServerNotification::PlanDelta(b)) => {
            a.delta.push_str(&b.delta);
            true
        }
        (
            ServerNotification::ReasoningSummaryTextDelta(a),
            ServerNotification::ReasoningSummaryTextDelta(b),
        ) => {
            a.delta.push_str(&b.delta);
            true
        }
        (ServerNotification::ReasoningTextDelta(a), ServerNotification::ReasoningTextDelta(b)) => {
            a.delta.push_str(&b.delta);
            true
        }
        (
            ServerNotification::CommandExecutionOutputDelta(a),
            ServerNotification::CommandExecutionOutputDelta(b),
        ) => {
            a.delta.push_str(&b.delta);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::AgentMessageDeltaNotification;
    use codex_app_server_protocol::CommandExecutionOutputDeltaNotification;
    use codex_app_server_protocol::ThreadClosedNotification;
    use pretty_assertions::assert_eq;

    fn agent_delta(item: &str, delta: &str) -> AppServerEvent {
        AppServerEvent::ServerNotification(Box::new(ServerNotification::AgentMessageDelta(
            AgentMessageDeltaNotification {
                thread_id: "t".to_string(),
                turn_id: "u".to_string(),
                item_id: item.to_string(),
                delta: delta.to_string(),
            },
        )))
    }

    fn exec_delta(item: &str, delta: &str) -> AppServerEvent {
        AppServerEvent::ServerNotification(Box::new(
            ServerNotification::CommandExecutionOutputDelta(
                CommandExecutionOutputDeltaNotification {
                    thread_id: "t".to_string(),
                    turn_id: "u".to_string(),
                    item_id: item.to_string(),
                    delta: delta.to_string(),
                },
            ),
        ))
    }

    fn barrier() -> AppServerEvent {
        AppServerEvent::ServerNotification(Box::new(ServerNotification::ThreadClosed(
            ThreadClosedNotification {
                thread_id: "t".to_string(),
            },
        )))
    }

    fn describe(events: &[AppServerEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                AppServerEvent::ServerNotification(n) => match n.as_ref() {
                    ServerNotification::AgentMessageDelta(d) => {
                        format!("agent:{}:{}", d.item_id, d.delta)
                    }
                    ServerNotification::CommandExecutionOutputDelta(d) => {
                        format!("exec:{}:{}", d.item_id, d.delta)
                    }
                    other => other.to_string(),
                },
                _ => "other".to_string(),
            })
            .collect()
    }

    #[test]
    fn coalesces_interleaved_deltas_per_item() {
        let mut batch = Batch::default();
        batch.push(agent_delta("a", "Hel"));
        batch.push(exec_delta("x", "out1\n"));
        batch.push(agent_delta("a", "lo"));
        batch.push(exec_delta("x", "out2\n"));
        assert_eq!(
            describe(&batch.take()),
            vec![
                "agent:a:Hello".to_string(),
                "exec:x:out1\nout2\n".to_string()
            ]
        );
    }

    #[test]
    fn non_delta_event_is_a_coalescing_barrier() {
        let mut batch = Batch::default();
        batch.push(agent_delta("a", "one"));
        batch.push(barrier());
        batch.push(agent_delta("a", "two"));
        assert_eq!(
            describe(&batch.take()),
            vec![
                "agent:a:one".to_string(),
                "thread/closed".to_string(),
                "agent:a:two".to_string(),
            ]
        );
    }

    #[test]
    fn disconnect_reason_is_not_overwritten_by_the_generic_message() {
        assert_eq!(connection_lost_message(/*disconnect_reported*/ true), None);
        assert_eq!(
            connection_lost_message(/*disconnect_reported*/ false),
            Some(CONNECTION_LOST.to_string())
        );
    }

    #[test]
    fn different_items_do_not_merge() {
        let mut batch = Batch::default();
        batch.push(agent_delta("a", "1"));
        batch.push(agent_delta("b", "2"));
        batch.push(agent_delta("a", "3"));
        assert_eq!(
            describe(&batch.take()),
            vec!["agent:a:13".to_string(), "agent:b:2".to_string()]
        );
        assert_eq!(batch.len(), 0);
    }
}
