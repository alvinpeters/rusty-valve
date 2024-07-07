use std::fs::File;
use std::process;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use std::time::Duration;
use tokio::signal;
use tokio::{select, task};
use tokio::signal::unix::SignalKind;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, Subscriber, trace, warn};
use tracing_subscriber::{filter, Layer, Registry};
use tracing_subscriber::fmt::format::{Format, Pretty};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::ConfigBuilder;
use crate::server::Server;
use crate::utils::logging::init_logger;

pub(crate) mod config;
pub(crate) mod connection;
mod utils;
pub(crate) mod listeners;
pub(crate) mod services;
mod server;

// Settings for this program. Will never be distributed
pub (crate) struct ServerConfig {
    daemonise: bool,
    interrupt_count: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let (mut config_builder, logger_config)
        = ConfigBuilder::new().get_opts()?.get_ini_config()?;
    // Initialise full-blown tracing logs with the settings
    let _guard = init_logger(logger_config)?;
    // Test log levels here
    #[cfg(debug_assertions)]
    {
        trace!("I'm already Tracer");
        debug!("I'm debugging");
        info!("so informative");
        warn!("check warnings");
        error!("error! run!");
    }
    // Then build config
    let mut config = config_builder.build()?;

    let task_tracker = TaskTracker::new();

    let mut server = match Server::new(&mut config) {
        Ok(s) => s,
        Err(e) => {
            error!("failed to start server: {}", e);
            wait_then_stop().await;
            return Err(e)
        }
    };
    let bound_server = server.bind(config.bind_settings.unwrap()).await?;

    // This event will *only* be recorded by the metrics layer.
    let _shutdown_token = CancellationToken::new();
    //tokio::task::spawn(handle_interrupt(5, shutdown_token));
    bound_server.run().await?;

    Ok(())
}

async fn wait_then_stop() {
    tokio::time::sleep(Duration::from_secs(5)).await;
}

async fn handle_interrupt(interrupt_count: usize, shutdown_token: CancellationToken) -> Result<()> {
    let mut sigterm = signal::unix::signal(SignalKind::terminate())?;

    for count in 0..interrupt_count {
        select! {
            c = signal::ctrl_c() => {
                if let Err(e) = c {
                    error!("HHHH");
                }
            },
            _ = sigterm.recv() => {
                break;
            }
        }
        if count < interrupt_count {
            println!("Interrupted {count}/{interrupt_count} times.");
            shutdown_token.cancel();
        } else {
            panic!("AAAAAA");
        }
    }
    Ok(())
}