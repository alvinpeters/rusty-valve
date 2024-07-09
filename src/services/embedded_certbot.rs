use tokio::sync::mpsc::Receiver;

use anyhow::Result;

use crate::connection::ForwardedConnection;
use crate::services::Service;
use crate::utils::conn_tracker::{ConnTracker};

struct EmbeddedCertbot {

}

impl Service for EmbeddedCertbot {
    const SLUG_NAME: &'static str = "certbot";
    type Config = ();

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, task_tracker: ConnTracker) -> Result<Self> {
        todo!()
    }

    async fn run(self) -> Result<Self> {
        todo!()
    }
}

