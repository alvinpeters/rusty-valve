mod webpage;

use std::net::{IpAddr, TcpStream};
use tokio::sync::mpsc::Receiver;
use anyhow::Result;
use tracing::error;
use crate::connection::{ConnectionInner, ForwardedConnection};
use crate::services::Service;
use crate::utils::conn_tracker::ConnTracker;

pub(crate) struct GatekeeperAuth {
    forward_receiver: Receiver<ForwardedConnection>,
    task_tracker: ConnTracker,
}

impl Service for GatekeeperAuth {
    const SLUG_NAME: &'static str = "gatekeeper-auth";
    type Config = ();

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, task_tracker: ConnTracker) -> Result<Self> {
        Ok(Self {
            forward_receiver,
            task_tracker,
        })
    }

    async fn run(mut self) -> Result<Self> {
        while let Some(conn) = self.forward_receiver.recv().await {
            let ConnectionInner::Tcp(stream) = conn.inner else {
                error!("not tcp");
                continue;
            };

        }
        Ok(self)
    }
}

async fn handle_connection(tcp_stream: TcpStream, ip_addr: IpAddr) {

}