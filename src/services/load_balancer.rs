use std::cmp::Ordering;
use std::cmp::Ordering::{Equal, Greater, Less};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::time::{Duration, Instant};
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tokio_util::time::FutureExt;
use tracing::{debug, error, Level, span, trace, warn};
use crate::connection::{ConnectionInner, Destination, ForwardedConnection};
use crate::services::Service;
use crate::utils::task_tracker::TaskTracker;
use anyhow::Result;

pub(crate) struct LoadBalancerConfig {
    pub(crate) addresses: Vec<IpAddr>
}

/// Handles connections
pub(crate) struct LoadBalancer {
    conn_receiver: Receiver<ForwardedConnection>,
    machines: Arc<BTreeMap<IpAddr, DestinationMachine>>,
    conn_tracker: TaskTracker,
}

impl Service for LoadBalancer {
    const CONFIG_NAME: &'static str = "load-balancer";
    type Config = LoadBalancerConfig;

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, task_tracker: TaskTracker) -> Result<Self> {
        let mut machines = BTreeMap::new();
        for ip_addr in config.addresses {
            let machine = DestinationMachine::new(1);
            machines.insert(ip_addr, machine);
        }
        let load_balancer = Self {
            conn_receiver: forward_receiver,
            machines: Arc::new(machines),
            conn_tracker: task_tracker,
        };
        Ok(load_balancer)
    }

    async fn run(mut self) -> Result<()> {
        while let Some(conn) = self.conn_receiver.recv().await {
            let machines = self.machines.clone();
            self.conn_tracker.spawn_with_tracker(|tracker| async move {
                let span = span!(Level::TRACE, "load balancer connection", remote_connection = &conn.remote_socket.to_string());
                let _span_guard = span.enter();
                handle_connection(conn, machines).await;
                // TODO: Figure out a way to keep the tracker from dropping without a guard
                let _tracker_guard = tracker;
            });
        }
        Ok(())
    }
}

async fn handle_connection(conn: ForwardedConnection, addresses: Arc<BTreeMap<IpAddr, DestinationMachine>>) {
    debug!("bruhawtwats");
    let Some(dest_conn) = DestinationConnection::negotiate_destination(conn.destination, addresses).await else {
        return;
    };
    debug!("bruhs");
    dest_conn.connect_to_machine(conn.inner, conn.remote_socket).await;
}

struct DestinationMachineInner {
    active_connections: AtomicUsize,
    reachable: AtomicBool,
    weight: usize,
}

#[derive(Clone)]
pub(crate) struct DestinationMachine {
    inner: Arc<DestinationMachineInner>
}

impl DestinationMachine {
    pub(crate) fn new(weight: usize) -> Self {
        let inner = DestinationMachineInner {
            active_connections: Default::default(),
            reachable: Default::default(),
            weight,
        };
        Self {
            inner: Arc::new(inner)
        }
    }

    pub(crate) fn available(&self) -> bool {
        self.inner.reachable.load(SeqCst)
    }

    pub(crate) fn weighted_load(&self) -> usize {
        self.inner.active_connections.load(SeqCst) * self.inner.weight
    }

    pub(crate) fn add_conn(&self) {
        self.inner.active_connections.fetch_add(1, Relaxed);
    }

    pub(crate) fn sub_conn(&self) {
        self.inner.active_connections.fetch_sub(1, Relaxed);
    }

}

pub(crate) enum DestinationConnectionInner {
    Tcp(TcpStream),
    Quic // not done
}

pub(crate) struct DestinationConnection {
    inner: DestinationConnectionInner,
    machine: DestinationMachine,
    socket_addr: SocketAddr,
}

impl DestinationConnection {
    fn new(inner: DestinationConnectionInner, machine: DestinationMachine, socket_addr: SocketAddr) -> Self {
        Self {
            inner,
            machine,
            socket_addr,
        }
    }
    
    async fn negotiate_destination(destination: Destination, dest_addrs: Arc<BTreeMap<IpAddr, DestinationMachine>>) -> Option<Self> {
        match destination {
            Destination::PortOnly(port) => {
                trace!("got a port-only destination. attempting to find the best machine");
                let sorted_machines = pick_best_ip(None, &dest_addrs);
                return Self::attempt_tcp_connections(sorted_machines, port).await;
            },
            Destination::Socket(socket) => {
                let Some(machine) = dest_addrs.get(&socket.ip()) else {
                    warn!("requested destination socket {} IP is not one of the machines!", socket);
                    return None;
                };
                if let Some(success) = Self::attempt_tcp_connection(&socket).await {
                    let dest = Self {
                        inner: success,
                        machine: machine.clone(),
                        socket_addr: socket,
                    };
                    return Some(dest);
                };
                debug!("failed to connect");
            },
            Destination::PortAndAddrs(port, addrs) => {
                let sorted_machines = pick_best_ip(Some(addrs), &dest_addrs);
                return Self::attempt_tcp_connections(sorted_machines, port).await;
            }
            _ => {
                //error!("received connection from {} but the load balancer cannot send it to {}", co.remote_socket, conn.destination);
                return None;
            }
        }
        None
    }

    async fn attempt_tcp_connections(machines: Vec<(&IpAddr, &DestinationMachine)>, port: u16) -> Option<Self> {
        for (ip, machine) in machines {
            let socket_addr = SocketAddr::new(ip.clone(), port);
            if let Some(success) = Self::attempt_tcp_connection(&socket_addr).await {
                let dest_conn = Self::new(success, machine.clone(), socket_addr);
                return Some(dest_conn);
            };
            continue;
        }
        None
    }

    async fn attempt_tcp_connection(socket_addr: &SocketAddr) -> Option<DestinationConnectionInner> {
        trace!("attempting connection to {}", socket_addr);
        let Ok(tcp_conn_timeout) = TcpStream::connect(socket_addr).timeout(Duration::from_secs(5)).await else {
            debug!("timed out making a TCP connection to {}", socket_addr);
            return None
        };
        let Ok(tcp_stream) = tcp_conn_timeout else {
            debug!("failed to make TCP connection to {}", socket_addr);
            return None
        };
        Some(DestinationConnectionInner::Tcp(tcp_stream))
    }

    async fn connect_to_machine(self, destination_connection: ConnectionInner, remote_socket: SocketAddr) {

        // TODO: Allow others
        let ConnectionInner::Tcp(mut src_conn) = destination_connection else {
            return;
        };
        let DestinationConnectionInner::Tcp(mut dest_stream) = self.inner else {
            return;
        };
        trace!("connecting remote {} and backend {} via TCP", remote_socket, self.socket_addr);
        let conn_time = Instant::now();
        self.machine.add_conn();
        if let Err(_e) = copy_bidirectional(&mut dest_stream, &mut src_conn.tcp_stream).await {
            // TODO: Warn: Connection broken: e
            self.machine.sub_conn();
            return;
        };
        self.machine.sub_conn();
        trace!("connection ended gracefully between {} and {}. Took {:.2}", remote_socket, self.socket_addr, conn_time.elapsed().as_secs_f32())
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


fn create_conn_list() {}