use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use anyhow::{anyhow, Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::trace;
use crate::connection::ForwardedConnection;
use crate::services::gatekeeper_auth::GatekeeperAuth;
use crate::services::load_balancer::LoadBalancer;
use crate::services::ssh_tarpit::SshTarpit;
use crate::utils::conn_tracker::{ConnTracker, TaskTracker};

pub(crate) mod ssh_tarpit;
pub(crate) mod gatekeeper_auth;
pub(crate) mod embedded_certbot;
pub(crate) mod load_balancer;

#[derive(PartialEq, PartialOrd, Ord, Clone, Eq)]
pub(crate) enum InternalService {
    SshTarpit,
    LoadBalancer,
    GatekeeperAuth,
    #[cfg(test)]
    TestService,
}

impl FromStr for InternalService {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            SshTarpit::SLUG_NAME => Ok(Self::SshTarpit),
            LoadBalancer::SLUG_NAME => Ok(Self::LoadBalancer),
            GatekeeperAuth::SLUG_NAME => Ok(Self::GatekeeperAuth),
            #[cfg(test)]
            "test-service" => Ok(Self::TestService),
            &_ => Err(anyhow!("{} does not match any existing services", s)),
        }
    }
}

impl Display for InternalService {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SshTarpit => write!(f, "{}", SshTarpit::SLUG_NAME),
            Self::LoadBalancer => write!(f, "{}", LoadBalancer::SLUG_NAME),
            Self::GatekeeperAuth => write!(f, "{}", GatekeeperAuth::SLUG_NAME),
            #[cfg(test)]
            Self::TestService => write!(f, "test-service"),
        }
    }
}

pub(crate) trait Service: Sized {
    const SLUG_NAME: &'static str;
    type Config;

    fn new(config: Self::Config, forward_receiver: Receiver<ForwardedConnection>, task_tracker: ConnTracker)
           -> Result<Self>;
    async fn run(self) -> Result<()>
    ;
}

#[derive(Clone)]
pub(crate) struct ServiceForwardTable {
    inner: Arc<BTreeMap<InternalService, Sender<ForwardedConnection>>>
}

impl ServiceForwardTable {
    pub(crate) fn has_service(&self, service_type: &InternalService) -> bool {
        self.inner.contains_key(service_type)
    }

    pub(crate) async fn forward_conn(&self, service_type: &InternalService, conn: ForwardedConnection) -> Result<()> {
        let Some(forwarder) = self.inner.get(service_type) else {
            return Err(anyhow!("forwarder for {} does not exist", service_type));
        };
        if let Err(e) = forwarder.send(conn).await {
            return Err(anyhow!("failed to foward connection to {}: {}", service_type, e));
        }
        Ok(())
    }
}

pub(crate) struct ServiceForwardTableBuilder {
    forwarders: BTreeMap<InternalService, Sender<ForwardedConnection>>
}

impl ServiceForwardTableBuilder {
    pub(crate) fn new() -> Self {
        Self {
            forwarders: BTreeMap::new()
        }
    }

    pub(crate) fn add(&mut self, service_type: InternalService, sender: Sender<ForwardedConnection>) -> Result<()> {
        if self.forwarders.contains_key(&service_type) {
            return Err(anyhow!("service already entered"));
        }
        self.forwarders.insert(service_type, sender);
        Ok(())
    }

    pub(crate) fn build(self) -> ServiceForwardTable {
        ServiceForwardTable {
            inner: Arc::new(self.forwarders)
        }
    }
}