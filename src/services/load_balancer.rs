use std::cmp::Ordering;
use std::cmp::Ordering::{Equal, Greater, Less};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::ops::Deref;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::Ordering::{Acquire, Relaxed, SeqCst};
use std::time::{Duration, Instant};
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tokio_util::time::FutureExt;
use tracing::{debug, debug_span, error, Instrument, Level, span, trace, warn};
use crate::connection::{ConnectionInner, Destination, ForwardedConnection};
use crate::services::Service;
use crate::utils::conn_tracker::{ConnTracker};
use anyhow::{anyhow, Result};
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

pub(crate) struct LoadBalancerConfig {
    pub(crate) addresses: Vec<IpAddr>
}

/// Handles connections
pub(crate) struct LoadBalancer {
    conn_receiver: Receiver<ForwardedConnection>,
    machines: Arc<BTreeMap<IpAddr, DestinationMachine>>,
    conn_tracker: ConnTracker,
}

impl Service for LoadBalancer {
    const SLUG_NAME: &'static str = "load-balancer";
    type Config = LoadBalancerConfig;

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, conn_tracker: ConnTracker) -> Result<Self> {
        let mut machines = BTreeMap::new();
        for ip_addr in config.addresses {
            let machine = DestinationMachine::new(1);
            machines.insert(ip_addr, machine);
        }
        let load_balancer = Self {
            conn_receiver: forward_receiver,
            machines: Arc::new(machines),
            conn_tracker,
        };
        Ok(load_balancer)
    }

    async fn run(mut self) -> Result<Self> {
        while let Some(conn) = self.conn_receiver.recv().await {
            let machines = self.machines.clone();
            self.conn_tracker.spawn(async move {
                let span = span!(Level::TRACE, "forwarding", remote_connection = &conn.remote_socket.to_string());
                trace!("received");
                handle_connection(conn, machines).instrument(span).await;
            });
        }
        Ok(self)
    }
}

async fn handle_connection(mut conn: ForwardedConnection, addresses: Arc<BTreeMap<IpAddr, DestinationMachine>>) {
    match conn.destination {
        Destination::PortOnly(port) => {
            trace!("got a port-only destination. attempting to find the best machine");
            let mut sorted_machines = pick_best_ip(None, &addresses);
            sorted_machines.reverse();
            if let Err(e) = attempt_connections(conn.inner, port, sorted_machines).await {
                debug!("failed to connect to the backend: {}", e);
                return;
            }
        },
        Destination::Socket(socket_addr) => {
            let Some(dest_machine) = addresses.get(&socket_addr.ip()) else {
                error!("requested destination socket {} IP is not one of the machines!", socket_addr);
                return;
            };
            if let Some(_returned_conn_inner) = dest_machine.connect(conn.inner, socket_addr).await {
                debug!("failed to connect to the only destination provided: {}", socket_addr);
                return;
            }
        },
        Destination::PortAndAddrs(port, addrs) => {
            let mut sorted_machines = pick_best_ip(Some(addrs), &addresses);
            sorted_machines.reverse();
            if let Err(e) = attempt_connections(conn.inner, port, sorted_machines).await {
                debug!("failed to connect to the backend: {}", e);
                return;
            }
        }
        _ => {
            //error!("received connection from {} but the load balancer cannot send it to {}", co.remote_socket, conn.destination);
        }
    }
}

/// Recursive functon, keeps attempting until the list is exhausted
async fn attempt_connections(conn_inner: ConnectionInner, port: u16, mut backend_list: Vec<(&IpAddr, &DestinationMachine)>) -> Result<()>
{
    // TODO: Optimise this bruh
    let mut conn_inner_opt = Some(conn_inner);
    while let Some((dest_ip, dest_machine)) = backend_list.pop() {
        let Some(conn_inner) = conn_inner_opt.take() else {
            error!("this isn't supposed to happen");
            return Err(anyhow!("bruh"));
        };
        let socket_addr = SocketAddr::new(dest_ip.to_owned(), port);
        conn_inner_opt = dest_machine.connect(conn_inner, socket_addr).await;
        if conn_inner_opt.is_none() {
            return Ok(())
        }
    }
    return Err(anyhow!("ran out of available connections"))
}

#[derive(Ord, PartialOrd, Eq, PartialEq)]
enum PortProtocol {
    Tcp,
    Quic
}

impl FromStr for PortProtocol {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("tcp") {
            Ok(Self::Tcp)
        } else if s.eq_ignore_ascii_case("quic") {
            Ok(Self::Quic)
        } else {
            Err(anyhow!("didn't match any port protocols!"))
        }
    }
}

#[derive(Ord, PartialOrd, Eq, PartialEq)]
struct LoadBalancerService {
    port: u16,
    port_protocol: PortProtocol
}

impl FromStr for LoadBalancerService {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let Some((port_str, proto_str)) = s.split_once('/') else {
            return Err(anyhow!("failed to spit: {}", s))
        };
        let Ok(port) = port_str.parse() else {
            return Err(anyhow!("failed to parse port string: {}", port_str))
        };
        let Ok(port_protocol) = proto_str.parse() else {
            return Err(anyhow!("failed to parse port string: {}", proto_str))
        };
        Ok(Self {
            port,
            port_protocol,
        })
    }
}

struct DestinationMachineInner {
    active_connections_by_peers: AtomicUsize,
    //services_offered: BTreeMap<u8, AtomicBool>
    reachable: AtomicBool,
    weight: usize,
    task_tracker: TaskTracker,
}

#[derive(Clone)]
pub(crate) struct DestinationMachine {
    inner: Arc<DestinationMachineInner>
}

impl DestinationMachine {
    pub(crate) fn new(weight: usize) -> Self {
        let inner = DestinationMachineInner {
            active_connections_by_peers: Default::default(),
            reachable: Default::default(),
            weight,
            task_tracker: TaskTracker::new()
        };
        Self {
            inner: Arc::new(inner)
        }
    }

    pub(crate) fn available(&self) -> bool {
        self.inner.reachable.load(SeqCst)
    }

    pub(crate) fn weighted_load(&self) -> usize {
        if self.available() {
            (self.inner.task_tracker.len() + self.inner.active_connections_by_peers.load(Acquire)) * self.inner.weight
        } else {
            usize::MAX
        }
    }

    /// Returns connection inner if failed.
    async fn connect(&self, conn_inner: ConnectionInner, socket_addr: SocketAddr) -> Option<ConnectionInner> {
        return match conn_inner {
            ConnectionInner::Tcp(mut remote_stream) => {
                let Ok(tcp_conn_timeout) = TcpStream::connect(socket_addr).timeout(Duration::from_secs(5)).await else {
                    debug!("timed out making a TCP connection to {}", socket_addr);
                    // Give it back
                    return Some(ConnectionInner::Tcp(remote_stream))
                };
                let Ok(mut dest_stream) = tcp_conn_timeout else {
                    debug!("failed to make TCP connection to {}", socket_addr);
                    return Some(ConnectionInner::Tcp(remote_stream))
                };
                self.inner.task_tracker.spawn(async move {
                    let dest_span = debug_span!(LoadBalancer::SLUG_NAME, "Backend socket"=socket_addr.to_string());
                    if let Err(e) = copy_bidirectional(&mut dest_stream, &mut remote_stream).instrument(dest_span).await {
                        debug!("connection unceremoniously ended: {}", e);
                    } else {
                        trace!("connection gracefully ended");
                    };
                });
                None
            },
            ConnectionInner::Quic => {
                error!("QUIC not supported yet!");
                Some(ConnectionInner::Quic)
            }
        }
    }

    pub(crate) fn add_conn(&self) {
        self.inner.active_connections_by_peers.fetch_add(1, Relaxed);
    }

    pub(crate) fn sub_conn(&self) {
        self.inner.active_connections_by_peers.fetch_sub(1, Relaxed);
    }

}

// TODO: Looks expensive
fn pick_best_ip(provided_ip: Option<Arc<Vec<IpAddr>>>, dest_addrs: &Arc<BTreeMap<IpAddr, DestinationMachine>>) -> Vec<(&IpAddr, &DestinationMachine)> {
    let mut vec = Vec::new();
    for (a, b) in dest_addrs.iter() {
        vec.push((a, b));
    }
    if let Some(ip_addrs) = provided_ip {
        vec.retain(|(ip_addr, machine)| ip_addrs.contains(ip_addr));
    }
    vec.sort_unstable_by(|(_ip_a, machine_a), (_ip_b, machine_b)| machine_a.weighted_load().cmp(&machine_b.weighted_load()));
    vec
}