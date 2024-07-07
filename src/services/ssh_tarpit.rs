mod bee_movie;

use std::io::BufRead;
use tokio::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Receiver;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, Level, span, trace};
use anyhow::Result;
use clap::ArgAction::Set;
use tokio::time::sleep;
use crate::connection::{ConnectionInner, ForwardedConnection};
use crate::services::{Service, ssh_tarpit};
use crate::services::ssh_tarpit::bee_movie::TRANSCRIPT;
use crate::utils::conn_tracker::{ConnTracker, TaskTracker};

pub(crate) struct SshTarpitConfig {
    pub(crate) max_connections: usize,
    pub(crate) banner_repeat_time: Duration,
}

#[derive(Copy, Clone)]
struct Settings {
    max_connections: usize,
    banner_repeat_time: Duration,
}

pub(crate) struct SshTarpit {
    settings: Settings,
    forward_receiver: Receiver<ForwardedConnection>,
    conn_tracker: ConnTracker,
}

impl Service for SshTarpit {
    const SLUG_NAME: &'static str = "ssh-tarpit";
    type Config = SshTarpitConfig;

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, conn_tracker: ConnTracker) -> Result<Self> {
        let settings = Settings {
            max_connections: config.max_connections,
            banner_repeat_time: config.banner_repeat_time,
        };
        let ssh_tarpit = SshTarpit {
            settings,
            forward_receiver,
            conn_tracker,
        };
        Ok(ssh_tarpit)
    }

    async fn run(mut self) -> Result<()> {
        while let Some(conn) = self.forward_receiver.recv().await {
            let _span = span!(Level::TRACE, Self::SLUG_NAME, remote=conn.remote_socket).entered();
            let (ConnectionInner::Tcp(tcp_conn), remote_socket, b) = conn.into_inner() else {
                // Not the right type, no point handling it.
                continue;
            };

            let mut stream = tcp_conn.tcp_stream;
            self.conn_tracker.spawn_with_tracker(move |tracker| async move {
                tarpit(tracker, stream, self.settings).await
            });
        }

        Ok(())
    }
}

async fn tarpit(conn_tracker: TaskTracker, mut tcp_stream: TcpStream, settings: Settings) {
    if conn_tracker.current_task_count() >= settings.max_connections {
        return;
    }

    // basically an infinite loop
    for line in TRANSCRIPT.iter().cycle() {
        if let Err(e) = tcp_stream.writable().await {
            trace!("got an error waiting for the stream to be writable: {}", e);
            break;
        }
        if let Err(e) = tcp_stream.write(line.as_bytes()).await {
            trace!("got an error writing to the stream: {}", e);
        }
        sleep(settings.banner_repeat_time).await;
    }
}
