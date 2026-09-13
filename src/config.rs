use std::env::VarError;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Configuration is layered as "defaults -> YAML file -> environment variables",
/// with environment variables taking highest priority.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: ListenConfig,
    pub passive: PassiveConfig,
    pub backends: Vec<BackendConfig>,
    pub timeouts: TimeoutConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct ListenConfig {
    pub address: IpAddr,
    pub port: u16,
}

impl Default for ListenConfig {
    fn default() -> Self {
        ListenConfig {
            address: IpAddr::from([0, 0, 0, 0]),
            port: 21,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct PassiveConfig {
    /// IPv4 address advertised to clients in PASV replies (IPv4-only, matching the project
    /// scope). This is NOT the bind address for the data listener, which always binds
    /// `0.0.0.0` regardless of this value -- set this to whatever address clients can
    /// actually reach the gateway on (e.g. a NAT/load-balancer/container-published address).
    pub address: Ipv4Addr,
    pub port_range: PortRange,
}

impl Default for PassiveConfig {
    fn default() -> Self {
        PassiveConfig {
            address: Ipv4Addr::UNSPECIFIED,
            port_range: PortRange::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl Default for PortRange {
    fn default() -> Self {
        PortRange {
            start: 10000,
            end: 20000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BackendConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct TimeoutConfig {
    pub connection_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    /// How long to wait for the backend to reply to a forwarded control command.
    pub command_timeout_secs: u64,
    /// How long the data relay may go with zero bytes moved in either direction before it is
    /// considered stalled. This is inactivity-based, not a cap on total transfer time, so a
    /// slow-but-active upload over a mobile network is never penalized.
    pub data_idle_timeout_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        TimeoutConfig {
            connection_timeout_secs: 10,
            idle_timeout_secs: 300,
            command_timeout_secs: 30,
            data_idle_timeout_secs: 60,
        }
    }
}

impl Config {
    /// Loads the YAML file at `config_path` if given, then layers environment variables on top.
    pub fn load(config_path: Option<&Path>) -> Result<Config> {
        let mut config = match config_path {
            Some(path) => {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read config file: {}", path.display()))?;
                serde_yaml::from_str(&content)
                    .with_context(|| format!("failed to parse config file: {}", path.display()))?
            }
            None => Config::default(),
        };

        config.apply_env_overrides()?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env_overrides(&mut self) -> Result<()> {
        if let Some(v) = env_var("GATEWAY_LISTEN_ADDRESS")? {
            self.listen.address = v.parse().context("invalid GATEWAY_LISTEN_ADDRESS")?;
        }
        if let Some(v) = env_var("GATEWAY_LISTEN_PORT")? {
            self.listen.port = v.parse().context("invalid GATEWAY_LISTEN_PORT")?;
        }
        if let Some(v) = env_var("GATEWAY_PASSIVE_ADDRESS")? {
            self.passive.address = v.parse().context("invalid GATEWAY_PASSIVE_ADDRESS")?;
        }
        if let Some(v) = env_var("GATEWAY_PASSIVE_PORT_RANGE_START")? {
            self.passive.port_range.start = v
                .parse()
                .context("invalid GATEWAY_PASSIVE_PORT_RANGE_START")?;
        }
        if let Some(v) = env_var("GATEWAY_PASSIVE_PORT_RANGE_END")? {
            self.passive.port_range.end = v
                .parse()
                .context("invalid GATEWAY_PASSIVE_PORT_RANGE_END")?;
        }
        if let Some(v) = env_var("GATEWAY_BACKENDS")? {
            self.backends = parse_backends(&v)?;
        }
        if let Some(v) = env_var("GATEWAY_CONNECTION_TIMEOUT_SECS")? {
            self.timeouts.connection_timeout_secs = v
                .parse()
                .context("invalid GATEWAY_CONNECTION_TIMEOUT_SECS")?;
        }
        if let Some(v) = env_var("GATEWAY_IDLE_TIMEOUT_SECS")? {
            self.timeouts.idle_timeout_secs =
                v.parse().context("invalid GATEWAY_IDLE_TIMEOUT_SECS")?;
        }
        if let Some(v) = env_var("GATEWAY_COMMAND_TIMEOUT_SECS")? {
            self.timeouts.command_timeout_secs =
                v.parse().context("invalid GATEWAY_COMMAND_TIMEOUT_SECS")?;
        }
        if let Some(v) = env_var("GATEWAY_DATA_IDLE_TIMEOUT_SECS")? {
            self.timeouts.data_idle_timeout_secs = v
                .parse()
                .context("invalid GATEWAY_DATA_IDLE_TIMEOUT_SECS")?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.backends.is_empty() {
            bail!(
                "at least one backend must be configured (via config file's `backends` or GATEWAY_BACKENDS env var)"
            );
        }
        if self.passive.port_range.start >= self.passive.port_range.end {
            bail!("passive.port_range.start must be less than passive.port_range.end");
        }
        Ok(())
    }
}

fn env_var(key: &str) -> Result<Option<String>> {
    match std::env::var(key) {
        Ok(v) => Ok(Some(v)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => bail!("{key} is not valid unicode"),
    }
}

/// Parses the "host1:port1,host2:port2" format.
fn parse_backends(raw: &str) -> Result<Vec<BackendConfig>> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (host, port) = entry
                .rsplit_once(':')
                .with_context(|| format!("invalid backend entry '{entry}', expected host:port"))?;
            let port: u16 = port
                .parse()
                .with_context(|| format!("invalid port in backend entry '{entry}'"))?;
            Ok(BackendConfig {
                host: host.to_string(),
                port,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_backend() {
        let backends = parse_backends("ftp01:21").unwrap();
        assert_eq!(
            backends,
            vec![BackendConfig {
                host: "ftp01".to_string(),
                port: 21
            }]
        );
    }

    #[test]
    fn parses_multiple_backends_with_whitespace() {
        let backends = parse_backends("ftp01:21, ftp02:2121").unwrap();
        assert_eq!(
            backends,
            vec![
                BackendConfig {
                    host: "ftp01".to_string(),
                    port: 21
                },
                BackendConfig {
                    host: "ftp02".to_string(),
                    port: 2121
                },
            ]
        );
    }

    #[test]
    fn rejects_entry_without_port() {
        assert!(parse_backends("ftp01").is_err());
    }

    #[test]
    fn rejects_entry_with_invalid_port() {
        assert!(parse_backends("ftp01:notaport").is_err());
    }

    #[test]
    fn validate_rejects_empty_backends() {
        let config = Config::default();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_inverted_port_range() {
        let mut config = Config {
            backends: vec![BackendConfig {
                host: "ftp01".to_string(),
                port: 21,
            }],
            ..Config::default()
        };
        config.passive.port_range = PortRange {
            start: 20000,
            end: 10000,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_accepts_minimal_valid_config() {
        let config = Config {
            backends: vec![BackendConfig {
                host: "ftp01".to_string(),
                port: 21,
            }],
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }
}
