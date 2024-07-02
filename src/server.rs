use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use anyhow::{anyhow, Result};
use tokio::join;
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
        if tls_sni_handler.is_none() || simple_tcp_handler.is_none() {
            error!("you can't just run a server without a single listener");
            return Err(anyhow!("nothing to listen on"));
        };
        let unbound_listeners = UnboundListeners {
            tls_sni_handler,
            simple_tcp_handler,
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
        let simple_tcp_handler = match unbound_listeners.simple_tcp_handler {
            None => None,
            Some(h) => {Some(h.bind(bind_settings.clone()).await?)}
        };
        let bound_listeners = BoundListeners {
            tls_sni_handler,
            simple_tcp_handler,
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
        let simple_tcp_handler = match bound_listeners.simple_tcp_handler {
            None => None,
            Some(h) => {Some(h.unbind())}
        };
        let unbound_listeners = UnboundListeners {
            tls_sni_handler,
            simple_tcp_handler,
        };
        Ok(Self::Unbound(unbound_listeners))
    }

    async fn listen(self) -> Result<Self> {
        let ListenerInner::Bound(mut listeners) = self else {
            error!("binding error");
            return Err(anyhow!("listeners are currently not bound"));
        };
        let tls_http_rp = {
            if let Some(tls_http) = listeners.tls_sni_handler {
                tokio::task::spawn(tls_http.listen())
            } else {
                panic!("aaaa")
            }
        };
        let tcp_rp = if let Some(tcp) = listeners.simple_tcp_handler {
            tokio::task::spawn(tcp.listen())
        } else {
            panic!("aaaa")
        };
        let (res2, res1) = join!(tls_http_rp, tcp_rp);
        let bound_listeners = BoundListeners {
            tls_sni_handler: None,
            simple_tcp_handler: None,
        };
        Ok(Self::Bound(bound_listeners))
    }
}

pub(crate) struct Server {
    listeners: ListenerInner,
    settings: ServerSettings,
    lb_ip_addrs: Vec<IpAddr>,
    forward_table: ServiceForwardTable,
    load_balancer: Option<LoadBalancer>,
    conn_waiter: TaskWaiter,
}

pub(crate) struct BoundServer {
    forward_table: ServiceForwardTable,
    load_balancer: Option<LoadBalancer>,
    conn_waiter: TaskWaiter,
}

impl Server {
    pub(crate) fn new(config: &mut Config) -> Result<Self> {
        let conn_waiter = TaskWaiter::new(Some(5000));
        let mut forward_table_builder = ServiceForwardTableBuilder::new();

        let load_balancer_config = config.load_balancer_config.take();

        let (lb_sender, lb_receiver) = mpsc::channel(50);
        let load_balancer = match load_balancer_config {
            None => None,
            Some(c) => Some(LoadBalancer::new(c, lb_receiver, conn_waiter.create_tracker())?)
        };
        forward_table_builder.add(InternalService::LoadBalancer, lb_sender)?;
        // Finish services here
        let forward_table = forward_table_builder.build();
        let listeners = ListenerInner::new(config, &forward_table)?;

        let server = Server {
            listeners,
            settings: config.server_settings.take().ok_or(anyhow!("server settings missing from config"))?,
            lb_ip_addrs: vec![],
            forward_table,
            load_balancer,
            conn_waiter,
        };

        Ok(server)
    }

    /// Binds to sockets
    pub(super) async fn bind(mut self, bind_settings: BindSettings) -> Result<Self> {
        trace!("binding listeners");
        self.listeners = self.listeners.bind(bind_settings).await?;
        Ok(self)
    }

    pub(crate) async fn run(mut self) -> Result<()> {
        //let mut conn_set = JoinSet::new();
        let listeners =  self.listeners.listen();
        let lb = self.load_balancer.unwrap().run();
        let (_res1, _res2) = join!(listeners, lb);
        Ok(())
    }

}

