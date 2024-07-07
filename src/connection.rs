use std::collections::BTreeSet;
use std::fmt::{Debug, Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use anyhow::anyhow;
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::time::FutureExt;
use crate::config::settings::ConnectionSettings;
use crate::connection::ConnectionInner::Tcp;
use crate::services::gatekeeper_auth::GatekeeperAuth;
use crate::services::load_balancer::LoadBalancer;
use crate::services::{InternalService, Service};
use crate::services::ssh_tarpit::SshTarpit;

pub(crate) struct QuicConnection {
    source_id: SocketAddr,
}

pub(crate) enum ConnectionInner {
    Tcp(TcpStream),
    Quic,
}

pub(crate) struct ForwardedConnection {
    pub(crate) inner: ConnectionInner,
    settings: ConnectionSettings,
    pub(crate) destination: Destination,
    pub(crate) remote_socket: SocketAddr,
}

impl ForwardedConnection {
    /// Wraps TCP stream to be handled later
    pub(crate) fn new_tcp_conn(tcp_stream: TcpStream, remote_socket: SocketAddr, settings: ConnectionSettings, destination: Destination) -> Self {
        Self::new_conn_with_inner(Tcp(tcp_stream), remote_socket, settings, destination)
    }

    pub(crate) fn new_quic_conn(tcp_stream: TcpStream, remote_socket: SocketAddr, settings: ConnectionSettings, destination: Destination) -> Self {
        unimplemented!("QUIC not implemented yet")
    }

    fn new_conn_with_inner(inner: ConnectionInner, remote_socket: SocketAddr, settings: ConnectionSettings, destination: Destination) -> Self {
        Self {
            inner,
            settings,
            destination,
            remote_socket,
        }
    }

    pub(crate) fn into_inner(self) -> (ConnectionInner, SocketAddr, ConnectionSettings) {
        (self.inner, self.remote_socket, self.settings)
    }
}

#[derive(PartialEq, Debug, Clone)]
pub(crate) struct PossibleDestinations {
    destination: Destination,
    else_destination: Option<Destination>,
}

impl PossibleDestinations {
    pub(crate) fn without_else(destination: Destination) -> Self {
        Self {
            destination,
            else_destination: None,
        }
    }

    pub(crate) fn with_else(destination: Destination, else_destionation: Destination) -> Self {
        Self {
            destination,
            else_destination: Option::from(else_destionation),
        }
    }

    pub(crate) fn try_into_inner(self, condition: bool) -> Option<Destination> {
        if condition {
            Option::from(self.destination)
        } else {
            self.else_destination
        }
    }

    pub(crate) fn try_get_inner(&self, condition: bool) -> Option<&Destination> {
        if condition {
            Option::from(&self.destination)
        } else {
            self.else_destination.as_ref()
        }
    }

    pub(crate) fn get_only_destination(&self) -> &Destination {
        &self.destination
    }
}

#[derive(PartialOrd, PartialEq, Clone)]
pub(crate) enum Destination {
    PortOnly(u16), // Only the port provided. Up to the load balancer to pick the destination.
    Socket(SocketAddr), // Not supposed to be taken
    PortAndAddrs(u16, Arc<Vec<IpAddr>>),
    InternalService(InternalService), // Should be sent already to a service that consumes it
}

impl Display for Destination {
    /// Meant to be displayed as a dative phrase (e.g. to 'port 8080')
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Destination::PortOnly(port) => write!(f, "port {}", port),
            Destination::Socket(socket) => write!(f, "socket {}", socket),
            Destination::PortAndAddrs(port, ip_addrs) => write!(f, "port {} with addresses: {:?}", port, ip_addrs),
            Destination::InternalService(service) => write!(f, "{}", service),
        }
    }
}

impl Debug for Destination {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Destination::PortOnly(port) => write!(f, "[port: {}]", port),
            Destination::Socket(socket) => write!(f, "[socket: {:?}]", socket),
            Destination::PortAndAddrs(port, ip_addrs) => write!(f, "[port: {}, addresses: {:?}]", port, ip_addrs),
            Destination::InternalService(service) => write!(f, "[service: {}]", service),
        }
    }
}

impl Destination {
    pub(crate) fn with_port(port: u16) -> Self {
        Self::PortOnly(port)
    }

    pub(crate) fn with_socket_addr(socket_addr: SocketAddr) -> Self {
        Self::Socket(socket_addr)
    }

    pub(crate) fn with_port_and_pref_addrs(port: u16, addrs: Vec<IpAddr>) -> Self {
        Self::PortAndAddrs(port, Arc::new(addrs))
    }

    pub(crate) fn with_internal_service(service: InternalService) -> Self {
        Self::InternalService(service)
    }
}

impl FromStr for Destination {
    type Err = anyhow::Error;


    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // If successful, just return this one
        if let Ok(internal_service) = InternalService::from_str(s) {
            return Ok(Self::InternalService(internal_service))
        }
        // Do a reverse split once only so IPv6 addresses don't get split
        let (addr_str_vec_opt, port_str) = s.rsplit_once(':')
            .map(|(addrs_str, port_str)| {
                let addrs_str = addrs_str.trim();
                let vec_opt: Option<Vec<&str>> = if addrs_str.is_empty() {
                    None
                } else {
                    Some(addrs_str.trim().split(',').collect())
                };
                (vec_opt, port_str)
            })
            .unwrap_or((None, s));
        // Port MUST exist and be parsable
        let Ok(port) = port_str.trim().parse() else {
            return Err(anyhow!("failed to parse port from assumed port string {} in {}", port_str, s));
        };
        let Some(addr_str_vec) = addr_str_vec_opt else {
            return Ok(Self::with_port(port))
        };
        let mut addr_vec = Vec::new();
        for addr_str in addr_str_vec {
            // Trim any whitespace then remove square brackets
            let addr_str = addr_str.trim().replace("[", "").replace("]", "");
            if addr_str.is_empty() { continue; }
            addr_vec.push(addr_str.parse()?);
        }
        // All of that effort and address vector is still empty, just return port
        if addr_vec.is_empty() {
            Ok(Self::with_port(port))
        } else if addr_vec.len() > 1 {
            Ok(Self::with_port_and_pref_addrs(port, addr_vec))
        } else {
            // Only 1 IP address. Might as well convert to socket
            let socket_addr = SocketAddr::new(addr_vec[0], port);
            Ok(Self::with_socket_addr(socket_addr))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use crate::connection::Destination;

    #[test]
    fn parse_destination() {
        let pass_port_only = "4000";
        let pass_socket = "127.0.0.1:10";
        let pass_port_and_addrs = "127.0.0.1,[::1]:4000"; // Port should be same as pass_port_only
        let pass_service = "test-service";
        let fail_two_ports = "127.0.0.1:4000:4000";
        let fail_invalid_ip = "12222.1521.2161:10";
        let fail_invalid_port = "four20";

        let ip_addrs = vec![
            "127.0.0.1".parse().expect("parsable IPv4 address string in ip_addrs vector"),
            "::1".parse().expect("parsable IPv6 address string in ip_addrs vector (remove brackets)")
        ];
        let port = pass_port_only.parse().expect("parsable pass_port_only");
        let comp_pass_port_only = Destination::with_port(port);
        let comp_pass_socket = Destination::with_socket_addr(pass_socket.parse().expect("parsable pass_socket"));
        let comp_pass_port_and_addrs = Destination::with_port_and_pref_addrs(port, ip_addrs);
        let comp_pass_service = Destination::with_internal_service(pass_service.parse().expect("parsable pass_service"));
        assert_eq!(comp_pass_port_only, pass_port_only.parse().expect("parsable port-only destination"));
        assert_eq!(comp_pass_socket, pass_socket.parse().expect("parsable socket destination"));
        assert_eq!(comp_pass_port_and_addrs, pass_port_and_addrs.parse().expect("parsable socket destination"));
        assert_eq!(comp_pass_service, pass_service.parse().expect("parsable service destination"));
        assert!(Destination::from_str(fail_two_ports).is_err(), "should fail to parse: {}", fail_two_ports);
        assert!(Destination::from_str(fail_invalid_ip).is_err(), "should fail to parse: {}", fail_invalid_ip);
        assert!(Destination::from_str(fail_invalid_port).is_err(), "should fail to parse: {}", fail_invalid_port);
    }
}