pub mod client;
pub mod listener;
pub mod oauth;
pub mod protocol;
#[cfg(any(test, feature = "test-support"))]
pub mod test;
pub mod transport;
pub mod types;

use collections::HashMap;
use http_client::HttpClient;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::{fmt::Display, path::PathBuf};

use anyhow::Result;
use client::Client;
use gpui::AsyncApp;
use parking_lot::RwLock;
pub use settings::ContextServerCommand;
use url::Url;

use crate::transport::HttpTransport;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextServerId(pub Arc<str>);

impl Display for ContextServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

enum ContextServerTransport {
    Stdio(ContextServerCommand, Option<PathBuf>),
    Custom(Arc<dyn crate::transport::Transport>),
}

pub struct ContextServer {
    id: ContextServerId,
    client: RwLock<Option<Arc<crate::protocol::InitializedContextServerProtocol>>>,
    configuration: ContextServerTransport,
    request_timeout: Option<Duration>,
    elicitation_handler: RwLock<Option<ElicitationHandler>>,
}

/// Handler invoked when a context server sends an `elicitation/create`
/// request. The handler runs in the GPUI async context so it can show UI
/// and await the user's response. It returns the full typed response
/// (including the user's `accept`/`decline`/`cancel` action) or an error
/// if the elicitation could not be completed.
pub type ElicitationHandler = Arc<
    dyn Send
        + Sync
        + Fn(
            types::ElicitationCreateParams,
            AsyncApp,
        ) -> gpui::Task<Result<types::ElicitationCreateResponse>>,
>;

impl ContextServer {
    pub fn stdio(
        id: ContextServerId,
        command: ContextServerCommand,
        working_directory: Option<Arc<Path>>,
    ) -> Self {
        Self {
            id,
            client: RwLock::new(None),
            configuration: ContextServerTransport::Stdio(
                command,
                working_directory.map(|directory| directory.to_path_buf()),
            ),
            request_timeout: None,
            elicitation_handler: RwLock::new(None),
        }
    }

    pub fn http(
        id: ContextServerId,
        endpoint: &Url,
        headers: HashMap<String, String>,
        http_client: Arc<dyn HttpClient>,
        executor: gpui::BackgroundExecutor,
        request_timeout: Option<Duration>,
    ) -> Result<Self> {
        let transport = match endpoint.scheme() {
            "http" | "https" => {
                log::info!("Using HTTP transport for {}", endpoint);
                let transport =
                    HttpTransport::new(http_client, endpoint.to_string(), headers, executor);
                Arc::new(transport) as _
            }
            _ => anyhow::bail!("unsupported MCP url scheme {}", endpoint.scheme()),
        };
        Ok(Self::new_with_timeout(id, transport, request_timeout))
    }

    pub fn new(id: ContextServerId, transport: Arc<dyn crate::transport::Transport>) -> Self {
        Self::new_with_timeout(id, transport, None)
    }

    pub fn new_with_timeout(
        id: ContextServerId,
        transport: Arc<dyn crate::transport::Transport>,
        request_timeout: Option<Duration>,
    ) -> Self {
        Self {
            id,
            client: RwLock::new(None),
            configuration: ContextServerTransport::Custom(transport),
            request_timeout,
            elicitation_handler: RwLock::new(None),
        }
    }

    pub fn id(&self) -> ContextServerId {
        self.id.clone()
    }

    pub fn client(&self) -> Option<Arc<crate::protocol::InitializedContextServerProtocol>> {
        self.client.read().clone()
    }

    pub async fn start(&self, cx: &AsyncApp) -> Result<()> {
        self.initialize(self.new_client(cx)?).await
    }

    /// Register a handler for server-initiated `elicitation/create` requests.
    /// Must be called before `start()` so the handler is in place before the
    /// first server traffic arrives.
    pub fn set_elicitation_handler(&self, handler: ElicitationHandler) {
        *self.elicitation_handler.write() = Some(handler);
    }

    fn new_client(&self, cx: &AsyncApp) -> Result<Client> {
        let client = match &self.configuration {
            ContextServerTransport::Stdio(command, working_directory) => Client::stdio(
                client::ContextServerId(self.id.0.clone()),
                client::ModelContextServerBinary {
                    executable: Path::new(&command.path).to_path_buf(),
                    args: command.args.clone(),
                    env: command.env.clone(),
                    timeout: command.timeout,
                },
                working_directory,
                cx.clone(),
            )?,
            ContextServerTransport::Custom(transport) => Client::new(
                client::ContextServerId(self.id.0.clone()),
                self.id().0,
                transport.clone(),
                self.request_timeout,
                cx.clone(),
            )?,
        };
        if let Some(handler) = self.elicitation_handler.read().clone() {
            client.install_elicitation_handler(handler);
        }
        Ok(client)
    }

    async fn initialize(&self, client: Client) -> Result<()> {
        log::debug!("starting context server {}", self.id);
        let protocol = crate::protocol::ModelContextProtocol::new(client);
        let client_info = types::Implementation {
            name: "Zed".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: None,
            icons: None,
        };
        let initialized_protocol = protocol.initialize(client_info).await?;

        log::debug!(
            "context server {} initialized: {:?}",
            self.id,
            initialized_protocol.initialize,
        );

        *self.client.write() = Some(Arc::new(initialized_protocol));
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let mut client = self.client.write();
        if let Some(protocol) = client.take() {
            drop(protocol);
        }
        Ok(())
    }
}
