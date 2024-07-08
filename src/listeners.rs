use std::net::SocketAddr;

use anyhow::Result;
use tokio::sync::mpsc::Sender;

pub(crate) mod tcp_rp;
pub(crate) mod tls_http_rp;
mod quic_rp;

pub(crate) trait ListenerHandler {
    type Config;
    type BoundListenerHandler: BoundListenerHandler;
    type ListenAddrProvider;
    type ForwardSender;
    type DestinationProvider;

    fn new(
        connection_settings: Self::Config,
        forward_sender: Self::ForwardSender,
    ) -> Result<Self> where Self: Sized;
    async fn bind(self, socket_provider: Self::ListenAddrProvider)
        -> Result<Self::BoundListenerHandler>;
}

pub(crate) trait BoundListenerHandler: Sized {
    type ListenerHandler: ListenerHandler;

    async fn listen(self) -> Result<Self>;
    fn unbind(self) -> Self::ListenerHandler;
}