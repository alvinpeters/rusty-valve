mod ini_file;
pub(crate) mod settings;

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, TcpListener};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Parser;
use configparser::ini::{Ini, IniDefault};
use rustls::ServerConfig;

use crate::config::ini_file::{ConfigSection, get_parser, HasConfigSection, TryFromConfig};
use crate::config::settings::{BindSettings, ConnectionSettings, TlsConnectionSettings};
use crate::connection::{Destination, PossibleDestinations};
use crate::listeners::tcp_rp::SimpleTcpHandlerConfig;
use crate::listeners::transparent_rp::TlsRpHandlerConfig;
use crate::server::ServerSettings;
use crate::services::load_balancer::LoadBalancerConfig;
use crate::services::ssh_tarpit::SshTarpitConfig;
use crate::utils::logging::LoggerConfig;

// All configs are optional because eventually they will be propagated again and only changing
// settings that are Some()
pub(crate) struct Config {
    pub(crate) bind_settings: Option<BindSettings>,
    pub(crate) server_config: Option<ServerConfig>,
    pub(crate) server_settings: Option<ServerSettings>,
    pub(crate) tcp_rp_config: Option<SimpleTcpHandlerConfig>,
    pub(crate) tls_rp_config: Option<TlsRpHandlerConfig>,
    pub(crate) load_balancer_config: Option<LoadBalancerConfig>,
    pub(crate) ssh_tarpit_config: Option<SshTarpitConfig>,
}

pub(crate) struct ConfigBuilder {
    config_file_path: Option<PathBuf>,
    server_settings: ServerSettings,
    connection_settings: ConnectionSettings,
    bind_addresses: Vec<IpAddr>,
    tls_rp_dest_provider: BTreeMap<String, PossibleDestinations>,
    fallback_tls_destination_port: Destination,
    tls_rp_listen_port: Vec<u16>,
    conn_tls_ch_max: usize,
    tcp_rp_dest_provider: Vec<(u16, PossibleDestinations)>,
    // SSH tarpit config
    ssh_tarpit_max_connections: usize,
    ssh_tarpit_banner_repeat_time: Duration,
    load_balancer_addrs: Vec<IpAddr>,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self {
            config_file_path: None,
            server_settings: ServerSettings {
                forward_queue: 50,
            },
            connection_settings: ConnectionSettings {
                timeout: Duration::from_secs(5),
            },
            bind_addresses: vec![],
            tls_rp_dest_provider: BTreeMap::new(),
            fallback_tls_destination_port: Destination::PortOnly(4443),
            tls_rp_listen_port: vec![4080, 4443],
            conn_tls_ch_max: 16358,
            tcp_rp_dest_provider: Vec::new(),
            ssh_tarpit_max_connections: 0,
            ssh_tarpit_banner_repeat_time: Duration::from_secs(10),
            load_balancer_addrs: vec![],
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Options {
    /// Name of the person to greet
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Option to run as daemon
    #[arg(short, long, default_value_t = false)]
    daemonise: bool,
}

impl ConfigBuilder {
    /// Uses the default
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get_opts(mut self) -> Result<Self> {
        let args = Options::parse();
        #[cfg(debug_assertions)] {
            self.config_file_path = Some(PathBuf::from_str("resources/config/config.ini")?);
        }
        #[cfg(not(debug_assertions))] {
            self.config_file_path = args.config;
        }
        Ok(self)
    }

    // Also returns LoggerConfig to initialise logger
    pub(crate) fn get_ini_config(mut self) -> Result<(Self, LoggerConfig)> {
        let Some(config_file_path) = &self.config_file_path else {
            return Err(anyhow!("ConfigBuilder.config_file_path is not set"))
        };

        // Set up config parser
        let default_section = "rusty-valve";
        let tls_rp_section = "tls-rp-ports";
        let tcp_rp_section = "tcp-rp-ports";
        let ssh_tarpit_section = "ssh-tarpit";
        let logger_section = "logger";

        // You can easily load a file to get a clone of the map:
        let mut config_map = get_parser().load(config_file_path)
            .map_err(|e| {anyhow!(e)})?;
        // Take main section
        let mut default_map = config_map.try_take_section(default_section)?;
        //println!("{:?}", config_map["rusty-valve"]);
        self.bind_addresses = default_map.take_multi_value("bind-addresses")?;
        self.load_balancer_addrs = default_map.take_multi_value("destination-addresses")?;
        // Take then iterate over it
        if let Some(mut tls_rp_map) = config_map.remove(tls_rp_section) {
            if let Some(v) = default_map.take_opt_value("fallback-tls-destination-port") {self.fallback_tls_destination_port = v}
            if let Ok(v) = default_map.take_multi_value("tls-rp-listen-port") {self.tls_rp_listen_port = v}
            for (host, port_string) in tls_rp_map {
                let possible_destination = port_string.and_then(|s| PossibleDestinations::try_from_config(s).ok())
                    .unwrap_or(PossibleDestinations::without_else(self.fallback_tls_destination_port.clone()));
                self.tls_rp_dest_provider.insert(host, possible_destination);
            }
        }
        if let Some(tcp_rp_map) = config_map.remove(tcp_rp_section) {
            for (src_port_string, dest_port_string) in tcp_rp_map {
                let src_port = src_port_string.parse()?;
                let dest_port = dest_port_string.and_then(|s| PossibleDestinations::try_from_config(s).ok())
                    .unwrap_or(PossibleDestinations::without_else(Destination::PortOnly(src_port)));
                self.tcp_rp_dest_provider.push((src_port, dest_port));
            }
        }
        // SSH TARPIT
        if let Some(mut ssh_tarpit_map) = config_map.remove(ssh_tarpit_section) {
            self.ssh_tarpit_max_connections = ssh_tarpit_map.take_opt_value("max-connections").unwrap_or(0);
            self.ssh_tarpit_banner_repeat_time = ssh_tarpit_map.take_opt_value("banner-repeat-time").unwrap_or(self.ssh_tarpit_banner_repeat_time);
        }
        // Get default log config, change fields if entry exists
        // Gotta deal with the matryoshka options
        let mut log_config = LoggerConfig::default();
        if let Some(mut log_map) = config_map.take_section(logger_section) {
            log_config.log_path = log_map.take_opt_value("log-file");
            log_config.stdout_log = log_map.take_opt_value("print-log");
            if let Some(log_level) = log_map.take_opt_value("print-level") {
                log_config.print_level = log_level;
            }
            if let Some(log_level) = log_map.take_opt_value("logfile-level") {
                log_config.logfile_level = log_level;
            }
        };
        Ok((self, log_config))
    }

    pub(crate) fn set_defaults(mut self) -> Self {
        self
    }

    pub(crate) fn build(self) -> Result<Config> {
        let mut config = Config {
            bind_settings: None,
            server_config: None,
            server_settings: None,
            tcp_rp_config: None,
            tls_rp_config: None,
            load_balancer_config: None,
            ssh_tarpit_config: None,
        };
        config.bind_settings = Some(BindSettings {
            bind_ip_addrs: Arc::new(self.bind_addresses),
            tls_rp_bind_port: Arc::new(self.tls_rp_listen_port),
        });
        config.server_settings = Option::from(self.server_settings);
        // add load balancer config if not empty
        config.load_balancer_config = (!self.load_balancer_addrs.is_empty()).then(|| {
            LoadBalancerConfig {
                addresses: self.load_balancer_addrs,
            }
        });
        // add if not empty
        config.tls_rp_config = if ! self.tls_rp_dest_provider.is_empty() {
            Some(TlsRpHandlerConfig {
                connection_settings: TlsConnectionSettings {
                    connection_settings: self.connection_settings,
                    max_client_hello_size: self.conn_tls_ch_max,
                },
                destination_provider: self.tls_rp_dest_provider,
            })
        } else {
            None
        };
        // add if not empty
        config.tcp_rp_config = (!self.tcp_rp_dest_provider.is_empty()).then(|| {
            SimpleTcpHandlerConfig {
                connection_settings: self.connection_settings,
                destination_provider: self.tcp_rp_dest_provider,
            }
        });
        // Add if max connection isn't 0
        config.ssh_tarpit_config = (!self.ssh_tarpit_max_connections == 0).then(|| {
            SshTarpitConfig {
                max_connections: self.ssh_tarpit_max_connections,
                banner_repeat_time: self.ssh_tarpit_banner_repeat_time,
            }
        });


        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn load_config_file() {

    }
}