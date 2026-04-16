use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::{FutureExt, StreamExt, channel::oneshot, future, select};
use gpui::{AppContext as _, AsyncApp, BackgroundExecutor, Task};
use parking_lot::Mutex;
use postage::barrier;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, value::RawValue};
use slotmap::SlotMap;
use smol::channel;
use std::{
    fmt,
    path::PathBuf,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};
use util::{ResultExt, TryFutureExt};

use crate::{
    transport::{StdioTransport, Transport},
    types::{
        CancelledParams, ClientNotification, Notification as _, Request as _,
        notifications::Cancelled,
    },
};

const JSON_RPC_VERSION: &str = "2.0";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

// Standard JSON-RPC error codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

type ResponseHandler = Box<dyn Send + FnOnce(String)>;
type NotificationHandler = Box<dyn Send + FnMut(Value, AsyncApp)>;
type RequestHandler = Box<dyn Send + FnMut(RequestId, &RawValue, AsyncApp)>;

/// JSON-RPC error code for "Method not found", per spec.
const ERROR_CODE_METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC error code for "Invalid params", per spec.
const ERROR_CODE_INVALID_PARAMS: i32 = -32602;
/// JSON-RPC error code for "Internal error", per spec.
const ERROR_CODE_INTERNAL_ERROR: i32 = -32603;

fn send_ok_response<T: Serialize>(
    outbound_tx: &channel::Sender<String>,
    id: RequestId,
    value: &T,
) {
    let payload = serde_json::json!({
        "jsonrpc": JSON_RPC_VERSION,
        "id": id,
        "result": value,
    });
    match serde_json::to_string(&payload) {
        Ok(payload) => {
            if let Err(err) = outbound_tx.try_send(payload) {
                log::warn!("Failed to send response on outbound channel: {err}");
            }
        }
        Err(err) => {
            log::error!("Failed to serialize response: {err}");
        }
    }
}

fn send_error_response(
    outbound_tx: &channel::Sender<String>,
    id: RequestId,
    code: i32,
    message: String,
) {
    let payload = serde_json::json!({
        "jsonrpc": JSON_RPC_VERSION,
        "id": id,
        "error": { "code": code, "message": message },
    });
    match serde_json::to_string(&payload) {
        Ok(payload) => {
            if let Err(err) = outbound_tx.try_send(payload) {
                log::warn!("Failed to send error response on outbound channel: {err}");
            }
        }
        Err(err) => {
            log::error!("Failed to serialize error response: {err}");
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i32),
    Str(String),
}

pub(crate) struct Client {
    server_id: ContextServerId,
    next_id: AtomicI32,
    outbound_tx: channel::Sender<String>,
    name: Arc<str>,
    subscription_set: Arc<Mutex<NotificationSubscriptionSet>>,
    response_handlers: Arc<Mutex<Option<HashMap<RequestId, ResponseHandler>>>>,
    request_handlers: Arc<Mutex<HashMap<&'static str, RequestHandler>>>,
    #[allow(clippy::type_complexity)]
    #[allow(dead_code)]
    io_tasks: Mutex<Option<(Task<Option<()>>, Task<Option<()>>)>>,
    #[allow(dead_code)]
    output_done_rx: Mutex<Option<barrier::Receiver>>,
    executor: BackgroundExecutor,
    #[allow(dead_code)]
    transport: Arc<dyn Transport>,
    request_timeout: Option<Duration>,
    /// Single-slot side channel for the last transport-level error. When the
    /// output task encounters a send failure it stashes the error here and
    /// exits; the next request to observe cancellation `.take()`s it so it can
    /// propagate a typed error (e.g. `TransportError::AuthRequired`) instead
    /// of a generic "cancelled". This works because `initialize` is the sole
    /// in-flight request at startup, but would need rethinking if concurrent
    /// requests are ever issued during that phase.
    last_transport_error: Arc<Mutex<Option<anyhow::Error>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub(crate) struct ContextServerId(pub Arc<str>);

fn is_null_value<T: Serialize>(value: &T) -> bool {
    matches!(serde_json::to_value(value), Ok(Value::Null))
}

#[derive(Serialize, Deserialize)]
pub struct Request<'a, T> {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub method: &'a str,
    #[serde(skip_serializing_if = "is_null_value")]
    pub params: T,
}

#[derive(Serialize, Deserialize)]
pub struct AnyRequest<'a> {
    pub jsonrpc: &'a str,
    pub id: RequestId,
    pub method: &'a str,
    #[serde(skip_serializing_if = "is_null_value")]
    pub params: Option<&'a RawValue>,
}

#[derive(Serialize, Deserialize)]
struct AnyResponse<'a> {
    jsonrpc: &'a str,
    id: RequestId,
    #[serde(default)]
    error: Option<Error>,
    #[serde(borrow)]
    result: Option<&'a RawValue>,
}

#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
pub(crate) struct Response<T> {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    #[serde(flatten)]
    pub value: CspResult<T>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CspResult<T> {
    #[serde(rename = "result")]
    Ok(Option<T>),
    #[allow(dead_code)]
    Error(Option<Error>),
}

#[derive(Serialize, Deserialize)]
struct Notification<'a, T> {
    jsonrpc: &'static str,
    #[serde(borrow)]
    method: &'a str,
    params: T,
}

#[derive(Debug, Clone, Deserialize)]
struct AnyNotification<'a> {
    #[expect(
        unused,
        reason = "Part of the JSON-RPC protocol - we expect the field to be present in a valid JSON-RPC notification"
    )]
    jsonrpc: &'a str,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Error {
    pub message: String,
    pub code: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelContextServerBinary {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
    pub timeout: Option<u64>,
}

impl Client {
    /// Creates a new Client instance for a context server.
    ///
    /// This function initializes a new Client by spawning a child process for the context server,
    /// setting up communication channels, and initializing handlers for input/output operations.
    /// It takes a server ID, binary information, and an async app context as input.
    pub fn stdio(
        server_id: ContextServerId,
        binary: ModelContextServerBinary,
        working_directory: &Option<PathBuf>,
        cx: AsyncApp,
    ) -> Result<Self> {
        log::debug!(
            "starting context server (executable={:?}, args={:?})",
            binary.executable,
            &binary.args
        );

        let server_name = binary
            .executable
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(String::new);

        let timeout = binary.timeout.map(Duration::from_secs);
        let transport = Arc::new(StdioTransport::new(binary, working_directory, &cx)?);
        Self::new(server_id, server_name.into(), transport, timeout, cx)
    }

    /// Creates a new Client instance for a context server.
    pub fn new(
        server_id: ContextServerId,
        server_name: Arc<str>,
        transport: Arc<dyn Transport>,
        request_timeout: Option<Duration>,
        cx: AsyncApp,
    ) -> Result<Self> {
        let (outbound_tx, outbound_rx) = channel::unbounded::<String>();
        let (output_done_tx, output_done_rx) = barrier::channel();

        let subscription_set = Arc::new(Mutex::new(NotificationSubscriptionSet::default()));
        let response_handlers =
            Arc::new(Mutex::new(Some(HashMap::<_, ResponseHandler>::default())));
        let request_handlers = Arc::new(Mutex::new(HashMap::<_, RequestHandler>::default()));

        let receive_input_task = cx.spawn({
            let subscription_set = subscription_set.clone();
            let response_handlers = response_handlers.clone();
            let request_handlers = request_handlers.clone();
            let transport = transport.clone();
            let outbound_tx = outbound_tx.clone();
            async move |cx| {
                Self::handle_input(
                    transport,
                    subscription_set,
                    request_handlers,
                    response_handlers,
                    outbound_tx,
                    cx,
                )
                .log_err()
                .await
            }
        });
        let receive_err_task = cx.spawn({
            let transport = transport.clone();
            async move |_| Self::handle_err(transport).log_err().await
        });
        let input_task = cx.spawn(async move |_| {
            let (input, err) = futures::join!(receive_input_task, receive_err_task);
            input.or(err)
        });

        let last_transport_error: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
        let output_task = cx.background_spawn({
            let transport = transport.clone();
            let last_transport_error = last_transport_error.clone();
            Self::handle_output(
                transport,
                outbound_rx,
                output_done_tx,
                response_handlers.clone(),
                last_transport_error,
            )
            .log_err()
        });

        Ok(Self {
            server_id,
            subscription_set,
            response_handlers,
            request_handlers,
            name: server_name,
            next_id: Default::default(),
            outbound_tx,
            executor: cx.background_executor().clone(),
            io_tasks: Mutex::new(Some((input_task, output_task))),
            output_done_rx: Mutex::new(Some(output_done_rx)),
            transport,
            request_timeout,
            last_transport_error,
        })
    }

    /// Handles input from the server's stdout.
    ///
    /// This function continuously reads lines from the provided stdout stream,
    /// parses them as JSON-RPC responses or notifications, and dispatches them
    /// to the appropriate handlers. It processes both responses (which are matched
    /// to pending requests) and notifications (which trigger registered handlers).
    async fn handle_input(
        transport: Arc<dyn Transport>,
        subscription_set: Arc<Mutex<NotificationSubscriptionSet>>,
        request_handlers: Arc<Mutex<HashMap<&'static str, RequestHandler>>>,
        response_handlers: Arc<Mutex<Option<HashMap<RequestId, ResponseHandler>>>>,
        outbound_tx: channel::Sender<String>,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<()> {
        let mut receiver = transport.receive();

        while let Some(message) = receiver.next().await {
            log::trace!("recv: {}", &message);
            // JSON-RPC batched messages were removed from MCP in the
            // 2025-06-18 revision. A single JSON-RPC message is always an
            // object, so a leading `[` (after whitespace) unambiguously
            // signals a batch. Reject it explicitly rather than silently
            // falling through to "Unhandled JSON" so the incompatibility is
            // diagnosable from the logs.
            if message.trim_start().starts_with('[') {
                log::error!(
                    "Rejecting JSON-RPC batch from context server; batching is \
                     not supported in MCP >= 2025-06-18: {}",
                    message
                );
                continue;
            }
            if let Ok(request) = serde_json::from_str::<AnyRequest>(&message) {
                let mut request_handlers = request_handlers.lock();
                if let Some(handler) = request_handlers.get_mut(request.method) {
                    handler(
                        request.id,
                        request.params.unwrap_or(RawValue::NULL),
                        cx.clone(),
                    );
                } else {
                    // Reply with JSON-RPC "Method not found" so the server
                    // does not hang waiting on our response. Without this,
                    // advertising a capability without installing its handler
                    // would silently strand the server-initiated request.
                    drop(request_handlers);
                    log::warn!(
                        "No request handler for {}; responding with method_not_found",
                        request.method
                    );
                    send_error_response(
                        &outbound_tx,
                        request.id,
                        ERROR_CODE_METHOD_NOT_FOUND,
                        format!("Method not found: {}", request.method),
                    );
                }
            } else if let Ok(response) = serde_json::from_str::<AnyResponse>(&message) {
                if let Some(handlers) = response_handlers.lock().as_mut()
                    && let Some(handler) = handlers.remove(&response.id)
                {
                    handler(message.to_string());
                }
            } else if let Ok(notification) = serde_json::from_str::<AnyNotification>(&message) {
                subscription_set.lock().notify(
                    &notification.method,
                    notification.params.unwrap_or(Value::Null),
                    cx,
                )
            } else {
                log::error!("Unhandled JSON from context_server: {}", message);
            }
        }

        smol::future::yield_now().await;

        Ok(())
    }

    /// Handles the stderr output from the context server.
    /// Continuously reads and logs any error messages from the server.
    async fn handle_err(transport: Arc<dyn Transport>) -> anyhow::Result<()> {
        while let Some(err) = transport.receive_err().next().await {
            log::debug!("context server stderr: {}", err.trim());
        }

        Ok(())
    }

    /// Handles the output to the context server's stdin.
    /// This function continuously receives messages from the outbound channel,
    /// writes them to the server's stdin, and manages the lifecycle of response handlers.
    async fn handle_output(
        transport: Arc<dyn Transport>,
        outbound_rx: channel::Receiver<String>,
        output_done_tx: barrier::Sender,
        response_handlers: Arc<Mutex<Option<HashMap<RequestId, ResponseHandler>>>>,
        last_transport_error: Arc<Mutex<Option<anyhow::Error>>>,
    ) -> anyhow::Result<()> {
        let _clear_response_handlers = util::defer({
            let response_handlers = response_handlers.clone();
            move || {
                response_handlers.lock().take();
            }
        });
        while let Ok(message) = outbound_rx.recv().await {
            log::trace!("outgoing message: {}", message);
            if let Err(err) = transport.send(message).await {
                log::debug!("transport send failed: {:#}", err);
                *last_transport_error.lock() = Some(err);
                return Ok(());
            }
        }
        drop(output_done_tx);
        Ok(())
    }

    /// Sends a JSON-RPC request to the context server and waits for a response.
    /// This function handles serialization, deserialization, timeout, and error handling.
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T> {
        self.request_with(
            method,
            params,
            None,
            self.request_timeout.or(Some(DEFAULT_REQUEST_TIMEOUT)),
        )
        .await
    }

    pub async fn request_with<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
        cancel_rx: Option<oneshot::Receiver<()>>,
        timeout: Option<Duration>,
    ) -> Result<T> {
        let id = self.next_id.fetch_add(1, SeqCst);
        let request = serde_json::to_string(&Request {
            jsonrpc: JSON_RPC_VERSION,
            id: RequestId::Int(id),
            method,
            params,
        })
        .unwrap();

        let (tx, rx) = oneshot::channel();
        let handle_response = self
            .response_handlers
            .lock()
            .as_mut()
            .context("server shut down")
            .map(|handlers| {
                handlers.insert(
                    RequestId::Int(id),
                    Box::new(move |result| {
                        let _ = tx.send(result);
                    }),
                );
            });

        let send = self
            .outbound_tx
            .try_send(request)
            .context("failed to write to context server's stdin");

        let executor = self.executor.clone();
        let started = Instant::now();
        handle_response?;
        send?;

        let mut timeout_fut = pin!(
            match timeout {
                Some(timeout) => future::Either::Left(executor.timer(timeout)),
                None => future::Either::Right(future::pending()),
            }
            .fuse()
        );
        let mut cancel_fut = pin!(
            match cancel_rx {
                Some(rx) => future::Either::Left(async {
                    rx.await.log_err();
                }),
                None => future::Either::Right(future::pending()),
            }
            .fuse()
        );

        select! {
            response = rx.fuse() => {
                let elapsed = started.elapsed();
                log::trace!("took {elapsed:?} to receive response to {method:?} id {id}");
                match response {
                    Ok(response) => {
                        let parsed: AnyResponse = serde_json::from_str(&response)?;
                        if let Some(error) = parsed.error {
                            Err(anyhow!(error.message))
                        } else if let Some(result) = parsed.result {
                            Ok(serde_json::from_str(result.get())?)
                        } else {
                            anyhow::bail!("Invalid response: no result or error");
                        }
                    }
                    Err(_canceled) => {
                        if let Some(err) = self.last_transport_error.lock().take() {
                            return Err(err);
                        }
                        anyhow::bail!("cancelled")
                    }
                }
            }
            _ = cancel_fut => {
                self.notify(
                    Cancelled::METHOD,
                    ClientNotification::Cancelled(CancelledParams {
                        request_id: RequestId::Int(id),
                        reason: None
                    })
                ).log_err();
                anyhow::bail!(RequestCanceled)
            }
            _ = timeout_fut => {
                log::error!("cancelled csp request task for {method:?} id {id} which took over {:?}", timeout.unwrap());
                anyhow::bail!("Context server request timeout");
            }
        }
    }

    pub(crate) fn set_negotiated_protocol_version(&self, version: &'static str) {
        self.transport.set_negotiated_protocol_version(version);
    }

    /// Install a handler for server-initiated `elicitation/create` requests.
    /// The handler produces a typed response (including the user's accept/
    /// decline/cancel choice) or an error; errors are returned to the server
    /// as a JSON-RPC internal error.
    pub(crate) fn install_elicitation_handler(&self, handler: crate::ElicitationHandler) {
        use crate::types::requests::ElicitationCreate;
        let method = ElicitationCreate::METHOD;
        let outbound_tx = self.outbound_tx.clone();
        let server_id = self.server_id.clone();
        let request_handler: RequestHandler =
            Box::new(move |id, params, cx| {
                let outbound_tx = outbound_tx.clone();
                let handler = handler.clone();
                let params: crate::types::ElicitationCreateParams =
                    match serde_json::from_str(params.get()) {
                        Ok(params) => params,
                        Err(err) => {
                            log::error!(
                                "{} failed to parse elicitation/create params: {err}",
                                server_id.0
                            );
                            send_error_response(
                                &outbound_tx,
                                id,
                                ERROR_CODE_INVALID_PARAMS,
                                format!("Invalid elicitation/create params: {err}"),
                            );
                            return;
                        }
                    };
                let server_id = server_id.clone();
                cx.spawn(async move |cx| {
                    let result = handler(params, cx.clone()).await;
                    match result {
                        Ok(response) => send_ok_response(&outbound_tx, id, &response),
                        Err(err) => {
                            log::warn!(
                                "{} elicitation handler failed: {err:#}",
                                server_id.0
                            );
                            send_error_response(
                                    &outbound_tx,
                                    id,
                                    ERROR_CODE_INTERNAL_ERROR,
                                    format!("Elicitation handler failed: {err}"),
                                );
                            }
                        }
                    })
                    .detach();
            });
        self.request_handlers.lock().insert(method, request_handler);
    }

    /// Sends a notification to the context server without expecting a response.
    /// This function serializes the notification and sends it through the outbound channel.
    pub fn notify(&self, method: &str, params: impl Serialize) -> Result<()> {
        let notification = serde_json::to_string(&Notification {
            jsonrpc: JSON_RPC_VERSION,
            method,
            params,
        })
        .unwrap();
        self.outbound_tx.try_send(notification)?;
        Ok(())
    }

    #[must_use]
    pub fn on_notification(
        &self,
        method: &'static str,
        f: Box<dyn 'static + Send + FnMut(Value, AsyncApp)>,
    ) -> NotificationSubscription {
        let mut notification_subscriptions = self.subscription_set.lock();

        NotificationSubscription {
            id: notification_subscriptions.add_handler(method, f),
            set: self.subscription_set.clone(),
        }
    }
}

#[derive(Debug)]
pub struct RequestCanceled;

impl std::error::Error for RequestCanceled {}

impl std::fmt::Display for RequestCanceled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Context server request was canceled")
    }
}

impl fmt::Display for ContextServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context Server Client")
            .field("id", &self.server_id.0)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

slotmap::new_key_type! {
    struct NotificationSubscriptionId;
}

#[derive(Default)]
pub struct NotificationSubscriptionSet {
    // we have very few subscriptions at the moment
    methods: Vec<(&'static str, Vec<NotificationSubscriptionId>)>,
    handlers: SlotMap<NotificationSubscriptionId, NotificationHandler>,
}

impl NotificationSubscriptionSet {
    #[must_use]
    fn add_handler(
        &mut self,
        method: &'static str,
        handler: NotificationHandler,
    ) -> NotificationSubscriptionId {
        let id = self.handlers.insert(handler);
        if let Some((_, handler_ids)) = self
            .methods
            .iter_mut()
            .find(|(probe_method, _)| method == *probe_method)
        {
            debug_assert!(
                handler_ids.len() < 20,
                "Too many MCP handlers for {}. Consider using a different data structure.",
                method
            );

            handler_ids.push(id);
        } else {
            self.methods.push((method, vec![id]));
        };
        id
    }

    fn notify(&mut self, method: &str, payload: Value, cx: &mut AsyncApp) {
        let Some((_, handler_ids)) = self
            .methods
            .iter_mut()
            .find(|(probe_method, _)| method == *probe_method)
        else {
            return;
        };

        for handler_id in handler_ids {
            if let Some(handler) = self.handlers.get_mut(*handler_id) {
                handler(payload.clone(), cx.clone());
            }
        }
    }
}

pub struct NotificationSubscription {
    id: NotificationSubscriptionId,
    set: Arc<Mutex<NotificationSubscriptionSet>>,
}

impl Drop for NotificationSubscription {
    fn drop(&mut self) {
        let mut set = self.set.lock();
        set.handlers.remove(self.id);
        set.methods.retain_mut(|(_, handler_ids)| {
            handler_ids.retain(|id| *id != self.id);
            !handler_ids.is_empty()
        });
    }
}
