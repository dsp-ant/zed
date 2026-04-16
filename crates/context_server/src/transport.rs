pub mod http;
mod stdio_transport;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

pub use http::*;
pub use stdio_transport::*;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, message: String) -> Result<()>;
    fn receive(&self) -> Pin<Box<dyn Stream<Item = String> + Send>>;
    fn receive_err(&self) -> Pin<Box<dyn Stream<Item = String> + Send>>;

    /// Called once after the `initialize` handshake completes. Transports that
    /// need to advertise the negotiated MCP protocol version on subsequent
    /// messages (e.g. HTTP via the `MCP-Protocol-Version` header) override
    /// this; the default is a no-op.
    fn set_negotiated_protocol_version(&self, _version: &'static str) {}
}
