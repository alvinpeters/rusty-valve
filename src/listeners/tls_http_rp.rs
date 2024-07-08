use std::collections::BTreeMap;
use std::io::{BufRead, Cursor};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::{debug, error, info, Level, span, trace, warn};
use rustls::server::Acceptor;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::Sender;
use tokio::task::{JoinError, JoinSet};
use tokio_rustls::LazyConfigAcceptor;
use tokio_util::time::FutureExt;
use crate::config::settings::{BindSettings, ConnectionSettings, TlsConnectionSettings};


use crate::connection::{Destination, ForwardedConnection, PossibleDestinations};

use crate::listeners::{ListenerHandler, BoundListenerHandler};
use crate::services::{InternalService, ServiceForwardTable};

pub(crate) struct TlsRpHandlerConfig {
    pub(crate) connection_settings: TlsConnectionSettings,
    pub(crate) destination_provider: BTreeMap<String, PossibleDestinations>,
}

pub(crate) struct TlsRpHandler {
    connection_settings: TlsConnectionSettings,
    destination_provider: Arc<BTreeMap<String, PossibleDestinations>>,
    forward_sender: ServiceForwardTable,
}

pub(crate) struct BoundTlsRpHandler {
    listeners: Vec<TcpListener>,
    connection_settings: TlsConnectionSettings,
    destination_provider: Arc<BTreeMap<String, PossibleDestinations>>,
    forward_sender: ServiceForwardTable,
}

impl ListenerHandler for TlsRpHandler {
    type Config = TlsRpHandlerConfig;
    type BoundListenerHandler = BoundTlsRpHandler;
    type ListenAddrProvider = BindSettings;
    type ForwardSender = ServiceForwardTable;
    type DestinationProvider = BTreeMap<String, PossibleDestinations>;

    fn new(
        config: Self::Config,
        forward_sender: Self::ForwardSender,
    ) -> Result<Self> where Self: Sized {
        let handler = Self {
            connection_settings: config.connection_settings,
            destination_provider: Arc::new(config.destination_provider),
            forward_sender,
        };
        Ok(handler)
    }

    async fn bind(self, mut socket_provider: Self::ListenAddrProvider) -> Result<Self::BoundListenerHandler> {
        trace!(listener="TLS/HTTP reverse proxy", "binding listener");
        let mut listeners = Vec::new();
        for port in socket_provider.tls_rp_bind_port.iter() {
            for ip_addr in socket_provider.bind_ip_addrs.iter() {
                let socket_addr = SocketAddr::new(*ip_addr, *port);
                let listener = TcpListener::bind(socket_addr).await?;
                listeners.push(listener);
            }
        }
        if listeners.is_empty() {
            error!("bruh empty");
            return Err(anyhow!("empty"))
        }
        let running_handler = BoundTlsRpHandler {
            listeners,
            connection_settings: self.connection_settings,
            destination_provider: self.destination_provider,
            forward_sender: self.forward_sender,
        };
        Ok(running_handler)
    }
}

impl BoundListenerHandler for BoundTlsRpHandler {
    type ListenerHandler = TlsRpHandler;

    async fn listen(mut self) -> Result<Self> {
        let mut listener_set = JoinSet::new();
        while let Some(listener) = self.listeners.pop() {
            let socket_addr = listener.local_addr().map(|a| a.to_string()).unwrap_or("N/A".to_string());
            let span = span!(Level::TRACE, "TLS/HTTP transparent reverse proxy", socket = socket_addr);
            listener_set.spawn(
                listen_tls(
                    listener,
                    self.destination_provider.clone(),
                    self.connection_settings,
                    self.forward_sender.clone(),
                )
            );
        }
        while let Some(res) = listener_set.join_next().await {
            let listener = match res {
                Ok(l) => l,
                Err(e) => {
                    error!("failed to retrieve a TCP listener! graceful halt failed!");
                    return Err(anyhow!("failed to retrieve a TCP listener: {}", e));
                }
            };
            self.listeners.push(listener);
        }
        Ok(self)
    }

    fn unbind(mut self) -> Self::ListenerHandler {
        // Will drop the sockets, hopefully.
        while let Some(listener) = self.listeners.pop() {
            drop(listener);
        }
        let handler = TlsRpHandler {
            connection_settings: self.connection_settings,
            destination_provider: self.destination_provider,
            forward_sender: self.forward_sender,
        };
        handler
    }
}

pub(crate) async fn listen_tls(
    listener: TcpListener,
    destination_provider: Arc<BTreeMap<String, PossibleDestinations>>,
    settings: TlsConnectionSettings,
    forward_sender: ServiceForwardTable,
) -> TcpListener {
    let mut handler_set = JoinSet::new();
    info!("now listening on {}", listener.local_addr().map(|s| s.to_string()).unwrap_or("N/A".to_string()));
    loop {
        let Ok((tcp_stream, remote)) = listener.accept().await else {
            debug!("Failed to accept stream");
            continue;
        };
        handler_set.spawn(
            get_backend_port(
                tcp_stream,
                remote,
                settings,
                destination_provider.clone(),
                forward_sender.clone(),
            )
        );
    }
    // return the listeners for reuse
    listener
}

pub(crate) async fn get_backend_port(
    mut tcp_stream: TcpStream,
    remote_addr: SocketAddr,
    settings: TlsConnectionSettings,
    destination_provider: Arc<BTreeMap<String, PossibleDestinations>>,
    forward_sender: ServiceForwardTable,
) {
    let timeout_duration = settings.connection_settings.timeout;
    // Peek the record header first, which is the first 5 bytes
    let mut record_header = [0; 5];
    if let Err(e) = peek_timeout(&tcp_stream, &mut record_header, timeout_duration).await {
        debug!("Error peeking record header from {}: {}", remote_addr, e);
        return;
    }
    // Check for TLS handshake record
    let name_check = if record_header[0] == 0x16 {
        extract_sni_from_tls_ch(
            record_header,
            &tcp_stream,
            &settings
        ).await
    } else {
        trace!("packet header ({:#04x} did not match expected handshake record ({:#04x}). trying to get Host name from HTTP",
            record_header[0],
            0x16,
        );
        extract_host_from_http_header(
            &remote_addr,
            &tcp_stream,
            &settings
        ).await
    };
    let Some(name) = name_check else {
        debug!("didn't get the host name from {}", remote_addr);
        return;
    };

    let Some(res) = destination_provider.get(&name) else {
        debug!("remote {} provided '{}' which is not on the list", remote_addr, name);
        return;
    };
    let dest =  res.get_only_destination().clone();
    let conn = ForwardedConnection::new_tcp_conn(tcp_stream, remote_addr, settings.connection_settings, dest);
    // Send it to the server thread for forwarding
    trace!("attempting to send {} to the load balancer", remote_addr);
    if let Err(e) = forward_sender.forward_conn(&InternalService::LoadBalancer, conn).await {
        debug!("Failed to send the connection from {}: {}", remote_addr, e);
    }
    trace!("connection has been sent to the load balancer {}", remote_addr);
}


async fn extract_sni_from_tls_ch(
    record_header: [u8; 5],
    tcp_stream: &TcpStream,
    settings: &TlsConnectionSettings
) -> Option<String> {
    // TLS version byte 1
    // SSL 3.0 and above only
    if record_header[1] != 0x03 {
        debug!("dropped connection. expected SSL 3.0 byte ({:#04x}), found ({:#04x})",
            0x03,
            record_header[1]
        );
        return None;
    }

    // TLS version byte 2
    // TLS 1.0 (for compatibility, not supported by rustls) TLS 1.2, TLS 1.3
    if ! [0x01, 0x03, 0x04].contains(&record_header[2]) {
        debug!("dropped connection. expected TLS 1.0 compatibility ({:#04x}), TLS 1.2 byte ({:#04x}), or TLS 1.3 ({:#04x}) byte, found ({:#04x})",
            0x01,
            0x03,
            0x04,
            record_header[1]
        );
        return None;
    }
    // Client Hello length. Let's limit this to 16,384 bytes
    let ch_size = u16::from_be_bytes([record_header[3], record_header[4]]) as usize;
    if ch_size > settings.max_client_hello_size { return None; };
    // HOLY SHIT https://tls13.xargs.org/#client-hello/annotated
    // https://security.stackexchange.com/questions/56338/identifying-ssl-traffic

    let mut client_hello_vec = vec![0; (ch_size) + 5];

    if let Err(e) = peek_timeout(&tcp_stream, &mut client_hello_vec, settings.connection_settings.timeout).await {
        debug!("Error peeking client hello: {}", e);
        return None;
    }
    let mut acceptor = Acceptor::default();
    let mut cursor = Cursor::new(client_hello_vec);
    if let Err(e) = acceptor.read_tls(&mut cursor) {
        debug!("failed to accept");
        return None;
    }
    let Ok(accepted) = acceptor.accept() else {
        debug!("failed to be accepted");
        return None;
    };
    let Some(accepted) = accepted else {
        debug!("failed to be accepted2");
        return None;
    };
    let client_hello = accepted.client_hello();
    if let Some(name) = client_hello.server_name() {
        trace!("found host from TLS SNI: {}", name);
        Some(name.to_owned())
    } else {
        None
    }
}

async fn extract_host_from_http_header(
    remote_addr: &SocketAddr,
    tcp_stream: &TcpStream,
    settings: &TlsConnectionSettings
) -> Option<String> {
    let mut buf = vec![0; 512];

    if let Err(e) = peek_timeout(tcp_stream, &mut buf, settings.connection_settings.timeout).await {
        return None;
    }
    let mut http_lines = buf.lines();
    while let Some(out) = http_lines.next() {
        let Ok(header_field) = out else {
            println!("broke here");
            // Error on parsing, break
            break;
        };
        // Header part ends
        if header_field.starts_with("\r\n") {
            break;
        }
        let Some((field, value)) = header_field.split_once(':') else {
            continue;
        };
        if ! field.eq_ignore_ascii_case("host") {
            continue;
        }
        let (name, _port) = value.trim().rsplit_once(':').unwrap_or((value, ""));
        // matched here
        trace!("found host from HTTP header: {}", name);
        return Some(name.to_owned());
    }
    None
}

async fn peek_timeout(tcp_stream: &TcpStream, buf: &mut [u8], timeout_duration: Duration) -> Result<()> {
    return match tcp_stream.peek(buf).timeout(timeout_duration).await {
        Ok(result) => {
            if let Err(peek_err) = result {
                Err(anyhow!(peek_err))
            } else {
                Ok(())
            }
        }
        Err(_e) => {
            Err(anyhow!("timed out after {:.1} seconds", timeout_duration.as_secs_f32()))
        }
    }
}