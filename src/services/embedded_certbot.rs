use tokio::sync::mpsc::Receiver;

use anyhow::Result;

use crate::connection::ForwardedConnection;
use crate::services::Service;
use crate::utils::task_tracker::TaskTracker;

struct EmbeddedCertbot {

}

impl Service for EmbeddedCertbot {
    const CONFIG_NAME: &'static str = "certbot";
    type Config = ();

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, task_tracler: TaskTracker) -> Result<Self> {
        todo!()
    }

    async fn run(self) -> Result<()> {
        todo!()
    }
}

