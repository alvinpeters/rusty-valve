use std::collections::BTreeMap;
use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, trace, warn};
use rustls::server::Acceptor;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinSet;
use tokio_rustls::LazyConfigAcceptor;
use tokio_util::time::FutureExt;
use crate::config::settings::{BindSettings, ConnectionSettings};
use crate::connection::{Destination, ForwardedConnection, PossibleDestinations};

use crate::listeners::{ListenerHandler, BoundListenerHandler};
use crate::services::{InternalService, ServiceForwardTable};

pub(crate) struct SimpleTcpHandlerConfig {
    pub(crate) connection_settings: ConnectionSettings,
    pub(crate) destination_provider: Vec<(u16, PossibleDestinations)>,
}

pub(crate) struct SimpleTcpHandler {
    connection_settings: ConnectionSettings,
    destination_provider: Vec<(u16, PossibleDestinations)>,
    forward_sender: ServiceForwardTable,
}

pub(crate) struct BoundSimpleTcpHandler {
    listeners: Vec<(TcpListener, (u16, PossibleDestinations))>,
    connection_settings: ConnectionSettings,
    forward_sender: ServiceForwardTable,
}

impl ListenerHandler for SimpleTcpHandler {
    type Config = SimpleTcpHandlerConfig;
    type BoundListenerHandler = BoundSimpleTcpHandler;
    type ListenAddrProvider = BindSettings;
    type ForwardSender = ServiceForwardTable;
    type DestinationProvider = Vec<(u16, PossibleDestinations)>;

    fn new(
        config: Self::Config,
        forward_sender: Self::ForwardSender,
    ) -> Result<Self> where Self: Sized {
        let handler = Self {
            connection_settings: config.connection_settings,
            destination_provider: config.destination_provider,
            forward_sender,
        };
        Ok(handler)
    }

    async fn bind(mut self, mut socket_provider: Self::ListenAddrProvider) -> Result<Self::BoundListenerHandler> {
        trace!(listener="simple TCP reverse proxy", "binding listener");
        let mut ip_addrs = socket_provider.bind_ip_addrs.deref();
        let mut listeners = Vec::new();
        for ip_addr in ip_addrs {
            while let Some((src_port, dest)) = self.destination_provider.pop() {
                let socket_addr = SocketAddr::new(*ip_addr, src_port.to_owned());
                let listener = TcpListener::bind(socket_addr).await?;
                listeners.push((listener, (src_port, dest)));
            }
        }
        let running_handler = BoundSimpleTcpHandler {
            listeners,
            connection_settings: self.connection_settings,
            forward_sender: self.forward_sender,
        };
        Ok(running_handler)
    }
}

impl BoundListenerHandler for BoundSimpleTcpHandler {
    type ListenerHandler = SimpleTcpHandler;

    async fn listen(mut self) -> Result<()> {
        let mut listener_set = JoinSet::new();
        while let Some((listener, (src_port, dest))) = self.listeners.pop() {
            info!("now listening on {}", listener.local_addr().map(|a| a.to_string()).unwrap_or("N/A".to_string()));
            listener_set.spawn(
                listen_tcp(
                    listener,
                    src_port,
                    dest,
                    self.connection_settings,
                    self.forward_sender.clone(),
                )
            );
        }
        while let Some(res) = listener_set.join_next().await {
            let Ok((listener, src_port, dest)) = res else {
                continue;
            };
            self.listeners.push((listener, (src_port, dest)));
        }
        Ok(())
    }

    fn unbind(mut self) -> Self::ListenerHandler {
        // Will drop the sockets, hopefully.
        let destination_provider = self.listeners.into_iter()
            .map(|(listener, (src_port, dest))| {
                drop(listener);
                // Return
                (src_port, dest)
            }).collect();
        let handler = SimpleTcpHandler {
            connection_settings: self.connection_settings,
            destination_provider,
            forward_sender: self.forward_sender,
        };
        handler
    }
}

pub(crate) async fn listen_tcp(
    listener: TcpListener,
    src_port: u16,
    destinations: PossibleDestinations,
    settings: ConnectionSettings,
    forward_sender: ServiceForwardTable,
) -> (TcpListener, u16, PossibleDestinations) {
    loop {
        let Ok((tcp_stream, remote)) = listener.accept().await else {
            debug!("Failed to accept stream");
            continue;
        };
        // do gatekeeper stuff here
        let dest = if forward_sender.has_service(&InternalService::GatekeeperAuth) {
            // get gatekeeper stuff here, assume it returns true for now
            let approved = true;
            let Some(s) = destinations.try_get_inner(approved) else {
                error!("rejected source got nowhere to go!");
                continue;
            };
            s.clone()
        } else {
            destinations.get_only_destination().clone()
        };
        let conn = ForwardedConnection::new_tcp_conn(tcp_stream, remote, settings.clone(), dest);
        if let Err(e) = forward_sender.forward_conn(&InternalService::LoadBalancer, conn).await {
            debug!("failed to forward!");
        }
    }
    (listener, src_port, destinations)
}
