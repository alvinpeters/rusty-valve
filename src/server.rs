use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinSet;
use tracing::{error, info, trace};

use crate::config::Config;
use crate::config::settings::BindSettings;
use crate::connection::ForwardedConnection;
use crate::listeners::{BoundListenerHandler, ListenerHandler};
use crate::listeners::tcp_rp::{BoundSimpleTcpHandler, SimpleTcpHandler};
use crate::listeners::tls_http_rp::{BoundTlsRpHandler, TlsRpHandler};
use crate::services::{InternalService, Service, ServiceForwardTable, ServiceForwardTableBuilder};
use crate::services::load_balancer::LoadBalancer;
use crate::utils::task_tracker::{TaskTracker, TaskWaiter};

#[derive(Copy, Clone)]
pub(crate) struct ServerSettings {
    pub(crate) forward_queue: usize
}

struct UnboundListeners {
    tls_sni_handler: Option<TlsRpHandler>,
    simple_tcp_handler: Option<SimpleTcpHandler>,
}

struct BoundListeners {
    tls_sni_handler: Option<BoundTlsRpHandler>,
    simple_tcp_handler: Option<BoundSimpleTcpHandler>,
}

enum ListenerInner {
    Unbound(UnboundListeners),
    Bound(BoundListeners),
}

impl ListenerInner {
    fn new(config: &mut Config, forward_table: &ServiceForwardTable) -> Result<Self> {
        let tls_sni_handler = match config.tls_rp_config.take() {
            None => None,
            Some(c) => Some(TlsRpHandler::new(c, forward_table.clone())?)
        };
        let simple_tcp_handler = match config.tcp_rp_config.take() {
            None => None,
            Some(c) => Some(SimpleTcpHandler::new(c, forward_table.clone())?)
        };
        if tls_sni_handler.is_none() && simple_tcp_handler.is_none() {
            error!("you can't just run a server without a single listener");
            return Err(anyhow!("nothing to listen on"));
        };
        let unbound_listeners = UnboundListeners {
            tls_sni_handler,
            simple_tcp_handler: None,
        };
        Ok(Self::Unbound(unbound_listeners))
    }

    async fn bind(self, bind_settings: BindSettings) -> Result<Self> {
        trace!("binding listeners");
        let ListenerInner::Unbound(mut unbound_listeners) = self else {
            error!("binding error");
            return Err(anyhow!("listeners are currently bound"));
        };

        let tls_sni_handler = match unbound_listeners.tls_sni_handler {
            None => None,
            Some(h) => {Some(h.bind(bind_settings.clone()).await?)}
        };
        let bound_listeners = BoundListeners {
            tls_sni_handler,
            simple_tcp_handler: None,
        };
        Ok(Self::Bound(bound_listeners))
    }

    fn unbind(self) -> Result<Self> {
        trace!("binding listeners");
        let ListenerInner::Bound(mut bound_listeners) = self else {
            error!("binding error");
            return Err(anyhow!("listeners are currently not bound"));
        };

        let tls_sni_handler = match bound_listeners.tls_sni_handler {
            None => None,
            Some(h) => {Some(h.unbind())}
        };
        let unbound_listeners = UnboundListeners {
            tls_sni_handler,
            simple_tcp_handler: None,
        };
        Ok(Self::Unbound(unbound_listeners))
    }
}

pub(crate) struct Server {
    settings: ServerSettings,
    lb_ip_addrs: Vec<IpAddr>,
    tls_sni_handler: Option<TlsRpHandler>,
    simple_tcp_handler: Option<SimpleTcpHandler>,
    forward_table: ServiceForwardTable,
    load_balancer: Option<LoadBalancer>,
    conn_waiter: TaskWaiter,
}

pub(crate) struct BoundServer {
    tls_sni_handler: Option<BoundTlsRpHandler>,
    simple_tcp_handler: Option<BoundSimpleTcpHandler>,
    forward_table: ServiceForwardTable,
    load_balancer: Option<LoadBalancer>,
    conn_waiter: TaskWaiter,
}

impl Server {
    pub(crate) fn new(config: &mut Config) -> Result<Self> {
        let conn_waiter = TaskWaiter::new(Some(5000));
        let mut forward_table_builder = ServiceForwardTableBuilder::new();
        let mut tls_sni_handler = None;
        
        let tls_lb_config = config.tls_rp_config.take();
        let tcp_lb_config = config.tcp_rp_config.take();
        let load_balancer_config = config.load_balancer_config.take();

        let (lb_sender, lb_receiver) = mpsc::channel(50);
        let load_balancer = match load_balancer_config {
            None => None,
            Some(c) => Some(LoadBalancer::new(c, lb_receiver, conn_waiter.create_tracker())?)
        };
        forward_table_builder.add(InternalService::LoadBalancer, lb_sender)?;
        // Finish services here
        let forward_table = forward_table_builder.build();
        tls_sni_handler = match tls_lb_config {
            None => None,
            Some(c) => Some(TlsRpHandler::new(c, forward_table.clone())?)
        };

        let server = Server {
            settings: config.server_settings.take().ok_or(anyhow!("server settings missing from config"))?,
            lb_ip_addrs: vec![],
            tls_sni_handler,
            simple_tcp_handler: None,
            forward_table,
            load_balancer,
            conn_waiter,
        };

        if server.tls_sni_handler.is_none() && server.simple_tcp_handler.is_none() && server.load_balancer.is_none() {
            error!("you can't just run a server without a service");
            return Err(anyhow!("nothing to run"));
        };

        Ok(server)
    }

    /// Binds to sockets
    pub(super) async fn bind(mut self, bind_settings: BindSettings) -> Result<BoundServer> {
        trace!("binding listeners");
        let tls_sni_handler = match self.tls_sni_handler {
            None => None,
            Some(h) => {Some(h.bind(bind_settings.clone()).await?)}
        };
        Ok(
            BoundServer {
                tls_sni_handler,
                simple_tcp_handler: None,
                forward_table: self.forward_table,
                load_balancer: self.load_balancer,
                conn_waiter: self.conn_waiter,
            }
        )
    }

}

impl BoundServer {
    pub(crate) async fn run(mut self) -> Result<()> {
        let mut listener_set = JoinSet::new();
        //let mut conn_set = JoinSet::new();
        if let Some(lb) = self.load_balancer {
            listener_set.spawn(lb.run());
        }
        if let Some(t) = self.tls_sni_handler {
            listener_set.spawn(t.listen());
        }
        while let Some(res) = listener_set.join_next().await {

        }
        Ok(())
    }
}