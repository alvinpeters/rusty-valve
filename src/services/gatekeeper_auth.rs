use tokio::sync::mpsc::Receiver;
use anyhow::Result;
use crate::connection::ForwardedConnection;
use crate::services::Service;
use crate::utils::task_tracker::TaskTracker;

pub(crate) struct GatekeeperAuth {

}

impl Service for GatekeeperAuth {
    const CONFIG_NAME: &'static str = "gatekeeper-auth";
    type Config = ();

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, task_tracker: TaskTracker) -> anyhow::Result<Self> {
        todo!()
    }

    async fn run(self) -> Result<()> {
        todo!()
    }
}