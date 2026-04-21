//! This module implements parts of the Model Context Protocol.
//!
//! It handles the lifecycle messages, and provides a general interface to
//! interacting with an MCP server. It uses the generic JSON-RPC client to
//! read/write messages and the types from types.rs for serialization/deserialization
//! of messages.

use std::time::Duration;

use anyhow::Result;
use futures::channel::oneshot;
use gpui::AsyncApp;
use serde_json::Value;

use crate::client::{Client, NotificationSubscription};
use crate::types::{self, Notification, Request};

pub struct ModelContextProtocol {
    inner: Client,
}

impl ModelContextProtocol {
    pub(crate) fn new(inner: Client) -> Self {
        Self { inner }
    }

    pub async fn initialize(
        self,
        client_info: types::Implementation,
    ) -> Result<InitializedContextServerProtocol> {
        let params = types::InitializeParams {
            protocol_version: types::ProtocolVersion(types::LATEST_PROTOCOL_VERSION.to_string()),
            capabilities: types::client_capabilities_for_version(
                types::LATEST_PROTOCOL_VERSION,
            ),
            meta: None,
            client_info,
        };

        let response: types::InitializeResponse = self
            .inner
            .request(types::requests::Initialize::METHOD, params)
            .await?;

        let negotiated_version = types::SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .find(|supported| **supported == response.protocol_version.0.as_str())
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Unsupported protocol version: {:?}",
                    response.protocol_version
                )
            })?;

        self.inner.set_negotiated_protocol_version(negotiated_version);

        log::trace!(
            "mcp server info {:?} (negotiated protocol {})",
            response.server_info,
            negotiated_version
        );

        let initialized_protocol = InitializedContextServerProtocol {
            inner: self.inner,
            initialize: response,
            negotiated_protocol_version: negotiated_version,
        };

        initialized_protocol.notify::<types::notifications::Initialized>(())?;

        Ok(initialized_protocol)
    }
}

pub struct InitializedContextServerProtocol {
    inner: Client,
    pub initialize: types::InitializeResponse,
    negotiated_protocol_version: &'static str,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ServerCapability {
    Experimental,
    Logging,
    Prompts,
    Resources,
    Tools,
}

impl InitializedContextServerProtocol {
    pub fn negotiated_protocol_version(&self) -> &'static str {
        self.negotiated_protocol_version
    }

    /// Check if the server supports a specific capability
    pub fn capable(&self, capability: ServerCapability) -> bool {
        match capability {
            ServerCapability::Experimental => self.initialize.capabilities.experimental.is_some(),
            ServerCapability::Logging => self.initialize.capabilities.logging.is_some(),
            ServerCapability::Prompts => self.initialize.capabilities.prompts.is_some(),
            ServerCapability::Resources => self.initialize.capabilities.resources.is_some(),
            ServerCapability::Tools => self.initialize.capabilities.tools.is_some(),
        }
    }

    pub async fn request<T: Request>(&self, params: T::Params) -> Result<T::Response> {
        self.inner.request(T::METHOD, params).await
    }

    pub async fn request_with<T: Request>(
        &self,
        params: T::Params,
        cancel_rx: Option<oneshot::Receiver<()>>,
        timeout: Option<Duration>,
    ) -> Result<T::Response> {
        self.inner
            .request_with(T::METHOD, params, cancel_rx, timeout)
            .await
    }

    pub fn notify<T: Notification>(&self, params: T::Params) -> Result<()> {
        self.inner.notify(T::METHOD, params)
    }

    pub fn on_notification(
        &self,
        method: &'static str,
        f: Box<dyn 'static + Send + FnMut(Value, AsyncApp)>,
    ) -> NotificationSubscription {
        self.inner.on_notification(method, f)
    }
}
