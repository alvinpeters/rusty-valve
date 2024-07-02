mod bee_movie;

use std::io::BufRead;
use tokio::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Receiver;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use anyhow::Result;
use clap::ArgAction::Set;
use tokio::time::sleep;
use crate::connection::{ConnectionInner, ForwardedConnection};
use crate::services::{Service, ssh_tarpit};
use crate::utils::task_tracker::TaskTracker;

pub(crate) struct SshTarpitConfig {
    max_connections: usize,
    banner_repeat_time: Duration,
}

#[derive(Copy, Clone)]
struct Settings {
    max_connections: usize,
    banner_repeat_time: Duration,
}

pub(crate) struct SshTarpit {
    settings: Settings,
    custom_banners: Option<Arc<Vec<String>>>,
    forward_receiver: Receiver<ForwardedConnection>,
    conn_tracker: TaskTracker,
}

impl Service for SshTarpit {
    const CONFIG_NAME: &'static str = "ssh-tarpit";
    type Config = SshTarpitConfig;

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, task_tracker: TaskTracker) -> Result<Self> {
        let settings = Settings {
            max_connections: config.max_connections,
            banner_repeat_time: config.banner_repeat_time,
        };
        let ssh_tarpit = SshTarpit {
            settings,
            custom_banners: None,
            forward_receiver,
            conn_tracker: task_tracker,
        };
        Ok(ssh_tarpit)
    }

    async fn run(mut self) -> Result<()> {
        while let Some(conn) = self.forward_receiver.recv().await {
            let (ConnectionInner::Tcp(tcp_conn), remote_socket, b) = conn.into_inner() else {
                // Not the right type, no point handling it.
                continue;
            };
            info!("a victim from {} has fallen to the SSH tarpit", remote_socket);
            let mut stream = tcp_conn.tcp_stream;
            self.conn_tracker.spawn_with_tracker(move |tracker| async move {
                tarpit(tracker, stream, self.settings).await
            });
        }

        Ok(())
    }
}

async fn tarpit(conn_tracker: TaskTracker, mut tcp_stream: TcpStream, settings: Settings) {
    loop {
        if conn_tracker.current_task_count() >= settings.max_connections {
            return;
        }
        let buf = bee_movie::TRANSCRIPT.lines();
        // basically an infinite loop
        for line_res in buf {
            let Ok(line) = line_res else {
                return;
            };
            if let Err(e) = tcp_stream.writable().await {
                break;
            }
            if let Err(e) = tcp_stream.write(line.as_bytes()).await {
                break;
            }
            sleep(settings.banner_repeat_time).await;
        }
    }
}
