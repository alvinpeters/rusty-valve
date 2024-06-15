use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct BindSettings {
    pub(crate) bind_ip_addrs: Arc<Vec<IpAddr>>,
    pub(crate) tls_rp_bind_port: Arc<Vec<u16>>,
}

#[derive(Copy, Clone)]
pub(crate) struct ConnectionSettings {
    pub(crate) timeout: Duration
}

#[derive(Copy, Clone)]
pub(crate) struct TlsConnectionSettings {
    pub(crate) connection_settings: ConnectionSettings,
    pub(crate) max_client_hello_size: usize,
}