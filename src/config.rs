use std::env::VarError;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

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
    pub backend_tls: BackendTlsConfig,
    pub timeouts: TimeoutConfig,
    pub limits: LimitsConfig,
    pub metrics: MetricsConfig,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendTlsMode {
    /// Plain FTP to the backend (the gateway's original behavior).
    #[default]
    Off,
    /// Explicit FTPS (RFC 4217) toward the backend: `AUTH TLS`, `PBSZ 0`, `PROT P`, so both the
    /// control and data connections are encrypted. Clients still speak plain FTP to the gateway.
    Explicit,
}

impl BackendTlsMode {
    fn parse(raw: &str) -> Result<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "off" => Ok(BackendTlsMode::Off),
            "explicit" => Ok(BackendTlsMode::Explicit),
            other => bail!("invalid backend TLS mode '{other}', expected 'off' or 'explicit'"),
        }
    }
}

/// Highest TLS version the gateway will offer the backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum BackendTlsMaxVersion {
    #[serde(rename = "1.2")]
    V1_2,
    #[default]
    #[serde(rename = "1.3")]
    V1_3,
}

impl BackendTlsMaxVersion {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "1.2" => Ok(BackendTlsMaxVersion::V1_2),
            "1.3" => Ok(BackendTlsMaxVersion::V1_3),
            other => bail!("invalid backend TLS max version '{other}', expected '1.2' or '1.3'"),
        }
    }
}

/// TLS settings applied to every backend (not per-backend). There is deliberately no option to
/// disable certificate verification: a backend with a private/self-signed CA is supported via
/// `ca_file` instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BackendTlsConfig {
    pub mode: BackendTlsMode,
    /// PEM file of CA certificate(s) trusted for backend server certificates. When unset, the
    /// bundled public roots (webpki-roots) are used.
    pub ca_file: Option<PathBuf>,
    /// Name used for SNI and certificate verification. Defaults to each backend's `host`; set
    /// this when the backend is reached via a name its certificate does not cover.
    pub server_name: Option<String>,
    /// Highest TLS version offered. Set to `1.2` for backends whose TLS 1.3 session resumption
    /// does not interoperate with data-connection session reuse.
    pub max_version: BackendTlsMaxVersion,
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

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Upper bound on a single control-line read (client commands and backend replies alike).
    /// Without this, a peer that never sends `\n` could make the gateway buffer an unbounded
    /// amount of memory for one line (PROJECT_SECURITY.md section 4/6); exceeding it ends the
    /// session rather than growing the line buffer further.
    pub max_command_line_bytes: usize,
    /// Maximum number of concurrent connections accepted from a single client IP (default `10`).
    /// `0` disables the check entirely -- unlimited connections per IP, the gateway's original
    /// behavior. Without this, a single misbehaving or malicious source could open unlimited
    /// connections and exhaust file descriptors/memory on its own (PROJECT_SECURITY.md section
    /// 5, "Connection Exhaustion"). Operators behind carrier-grade NAT -- where many IoT devices
    /// can share one public IP -- should raise this or disable it.
    pub max_connections_per_ip: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            max_command_line_bytes: 4096,
            max_connections_per_ip: 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Address the `/metrics` HTTP endpoint binds to. Defaults to loopback-only, not
    /// `listen.address`'s `0.0.0.0` default -- this endpoint carries no per-client secrets, but
    /// it isn't meant to be reachable straight from the Internet either; an operator who wants
    /// it reachable from outside the host (e.g. a Prometheus server on another machine) must
    /// opt in explicitly via `GATEWAY_METRICS_ADDRESS`.
    pub address: IpAddr,
    /// TCP port the `/metrics` HTTP endpoint listens on. Distinct from `listen.port` (which
    /// speaks only FTP, not HTTP) since the two protocols can't share one port. `None` (the
    /// default) disables the metrics endpoint entirely -- opt-in, matching this gateway's
    /// general posture of not exposing anything not explicitly configured.
    pub port: Option<u16>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        MetricsConfig {
            address: IpAddr::from([127, 0, 0, 1]),
            port: None,
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
        if let Some(v) = env_var("GATEWAY_BACKEND_TLS")? {
            self.backend_tls.mode = BackendTlsMode::parse(&v)?;
        }
        if let Some(v) = env_var("GATEWAY_BACKEND_TLS_CA_FILE")? {
            self.backend_tls.ca_file = Some(PathBuf::from(v));
        }
        if let Some(v) = env_var("GATEWAY_BACKEND_TLS_SERVER_NAME")? {
            self.backend_tls.server_name = Some(v);
        }
        if let Some(v) = env_var("GATEWAY_BACKEND_TLS_MAX_VERSION")? {
            self.backend_tls.max_version = BackendTlsMaxVersion::parse(&v)?;
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
        if let Some(v) = env_var("GATEWAY_MAX_COMMAND_LINE_BYTES")? {
            self.limits.max_command_line_bytes = v
                .parse()
                .context("invalid GATEWAY_MAX_COMMAND_LINE_BYTES")?;
        }
        if let Some(v) = env_var("GATEWAY_MAX_CONNECTIONS_PER_IP")? {
            self.limits.max_connections_per_ip = v
                .parse()
                .context("invalid GATEWAY_MAX_CONNECTIONS_PER_IP")?;
        }
        if let Some(v) = env_var("GATEWAY_METRICS_ADDRESS")? {
            self.metrics.address = v.parse().context("invalid GATEWAY_METRICS_ADDRESS")?;
        }
        if let Some(v) = env_var("GATEWAY_METRICS_PORT")? {
            self.metrics.port = Some(v.parse().context("invalid GATEWAY_METRICS_PORT")?);
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
        if self.backend_tls.mode == BackendTlsMode::Explicit {
            if let Some(ca_file) = &self.backend_tls.ca_file
                && !ca_file.is_file()
            {
                bail!(
                    "backend_tls.ca_file '{}' does not exist or is not a file",
                    ca_file.display()
                );
            }
            if self.backend_tls.server_name.as_deref() == Some("") {
                bail!("backend_tls.server_name must not be empty");
            }
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

    fn config_with_backend() -> Config {
        Config {
            backends: vec![BackendConfig {
                host: "ftp01".to_string(),
                port: 21,
            }],
            ..Config::default()
        }
    }

    #[test]
    fn example_config_parses_with_backend_tls_off_and_when_enabled() {
        let example = include_str!("../config.example.yaml");
        let as_shipped: Config = serde_yaml::from_str(example).unwrap();
        assert_eq!(as_shipped.backend_tls.mode, BackendTlsMode::Off);

        // Uncomment only the `backend_tls` block, not the header's env var list.
        let enabled = [
            "backend_tls:",
            "  mode:",
            "  ca_file:",
            "  server_name:",
            "  max_version:",
        ]
        .iter()
        .fold(example.to_string(), |text, line| {
            text.replace(&format!("#{line}"), line)
        });
        let enabled: Config = serde_yaml::from_str(&enabled).unwrap();
        assert_eq!(enabled.backend_tls.mode, BackendTlsMode::Explicit);
        assert_eq!(
            enabled.backend_tls.server_name.as_deref(),
            Some("ftp.example.com")
        );
        assert_eq!(enabled.backend_tls.max_version, BackendTlsMaxVersion::V1_3);
    }

    #[test]
    fn backend_tls_defaults_to_off() {
        assert_eq!(Config::default().backend_tls.mode, BackendTlsMode::Off);
        assert_eq!(
            Config::default().backend_tls.max_version,
            BackendTlsMaxVersion::V1_3
        );
    }

    #[test]
    fn parses_backend_tls_from_yaml() {
        let config: Config = serde_yaml::from_str(
            "backend_tls:\n  mode: explicit\n  server_name: ftp.example.com\n  max_version: \"1.2\"\n",
        )
        .unwrap();
        assert_eq!(config.backend_tls.mode, BackendTlsMode::Explicit);
        assert_eq!(
            config.backend_tls.server_name.as_deref(),
            Some("ftp.example.com")
        );
        assert_eq!(config.backend_tls.max_version, BackendTlsMaxVersion::V1_2);
    }

    #[test]
    fn rejects_unknown_backend_tls_mode_and_version() {
        assert!(BackendTlsMode::parse("implicit").is_err());
        assert!(BackendTlsMaxVersion::parse("1.1").is_err());
        assert_eq!(
            BackendTlsMode::parse("Explicit").unwrap(),
            BackendTlsMode::Explicit
        );
    }

    #[test]
    fn validate_rejects_missing_backend_tls_ca_file() {
        let mut config = config_with_backend();
        config.backend_tls.mode = BackendTlsMode::Explicit;
        config.backend_tls.ca_file = Some(PathBuf::from("/nonexistent/ca.pem"));
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_ignores_backend_tls_ca_file_when_off() {
        let mut config = config_with_backend();
        config.backend_tls.ca_file = Some(PathBuf::from("/nonexistent/ca.pem"));
        assert!(config.validate().is_ok());
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
