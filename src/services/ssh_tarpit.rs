use tokio::net::TcpStream;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Receiver;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use anyhow::Result;
use crate::connection::{ConnectionInner, ForwardedConnection};
use crate::services::{Service, ssh_tarpit};
use crate::utils::task_tracker::TaskTracker;

pub(crate) struct SshTarpitConfig {
    max_connections: usize,
    custom_banners: Option<Vec<String>>,
}

pub(crate) struct SshTarpit {
    custom_banners: Option<Arc<Vec<String>>>,
    forward_receiver: Receiver<ForwardedConnection>,
}

impl Service for SshTarpit {
    const CONFIG_NAME: &'static str = "ssh-tarpit";
    type Config = SshTarpitConfig;

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, task_tracker: TaskTracker) -> Result<Self> {
        let ssh_tarpit = SshTarpit {
            custom_banners: None,
            forward_receiver,
        };
        todo!()
    }

    async fn run(mut self) -> Result<()> {
        let (conn_tracker, conn_waiter) = TaskTracker::new(None);
        while let Some(conn) = self.forward_receiver.recv().await {
            let (ConnectionInner::Tcp(tcp_conn), remote_socket, b) = conn.into_inner() else {
                // Not the right type, no point handling it.
                continue;
            };
            info!("a victim from {} has fallen to the SSH tarpit", remote_socket);
            let mut stream = tcp_conn.tcp_stream;
            conn_tracker.spawn(tarpit(stream));
        }

        Ok(())
    }
}

async fn tarpit(mut tcp_stream: TcpStream) {
    loop {
        if let Err(e) = tcp_stream.writable().await {
            break;
        }
        if let Err(e) = tcp_stream.write(b"You fell off!\r\n").await {
            break;
        }
    }
}
