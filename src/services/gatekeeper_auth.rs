use tokio::sync::mpsc::Receiver;
use anyhow::Result;
use crate::connection::ForwardedConnection;
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

    async fn run(self) -> Result<()> {
        todo!()
    }
}