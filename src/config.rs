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
    pub backend_source: BackendSourceConfig,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    /// Number of ports in the range (both ends inclusive).
    pub fn len(&self) -> usize {
        usize::from(self.end) - usize::from(self.start) + 1
    }

    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }

    /// Parses the `first:last` form, e.g. `8192:8200`. A single port is `8192:8192`.
    fn parse_first_last(raw: &str) -> Result<PortRange> {
        let (first, last) = raw
            .split_once(':')
            .with_context(|| format!("invalid port range '{raw}', expected first:last"))?;
        let start: u16 = first
            .trim()
            .parse()
            .with_context(|| format!("invalid first port in range '{raw}'"))?;
        let end: u16 = last
            .trim()
            .parse()
            .with_context(|| format!("invalid last port in range '{raw}'"))?;
        if start > end {
            bail!("invalid port range '{raw}': first port is greater than last port");
        }
        Ok(PortRange { start, end })
    }
}

impl Default for PortRange {
    fn default() -> Self {
        PortRange {
            start: 10000,
            end: 20000,
        }
    }
}

/// Data-connection port range AWS Transfer Family uses for FTP/FTPS; the default because that is
/// the backend this setting exists for.
pub const DEFAULT_BACKEND_PASSIVE_PORTS: PortRange = PortRange {
    start: 8192,
    end: 8200,
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BackendConfig {
    pub host: String,
    pub port: u16,
    /// The range of data-connection (PASV) ports this backend serves. Its size is how many
    /// data connections one source address can have open to this backend at once, which
    /// `backend_source` rotation uses as a capacity. It is not enforced as a filter: a PASV
    /// reply naming a port outside it is still honored, since the default applies to every
    /// backend whether or not it actually uses that range.
    #[serde(
        default = "default_backend_passive_ports",
        deserialize_with = "deserialize_port_range"
    )]
    pub passive_ports: PortRange,
}

impl Default for BackendConfig {
    /// For tests and struct-update syntax; a real `host` always comes from configuration.
    fn default() -> Self {
        BackendConfig {
            host: String::new(),
            port: 21,
            passive_ports: DEFAULT_BACKEND_PASSIVE_PORTS,
        }
    }
}

fn default_backend_passive_ports() -> PortRange {
    DEFAULT_BACKEND_PASSIVE_PORTS
}

/// Accepts either the `"first:last"` string form or the `{start, end}` map form.
fn deserialize_port_range<'de, D>(deserializer: D) -> std::result::Result<PortRange, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Form {
        Text(String),
        Map { start: u16, end: u16 },
    }

    let range = match Form::deserialize(deserializer)? {
        Form::Text(text) => PortRange::parse_first_last(&text).map_err(serde::de::Error::custom)?,
        Form::Map { start, end } => PortRange { start, end },
    };
    if range.is_empty() {
        return Err(serde::de::Error::custom(
            "passive_ports: first port is greater than last port",
        ));
    }
    Ok(range)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendSourceMode {
    /// The OS picks the source address for backend connections, with no cap on concurrent data
    /// transfers (the gateway's original behavior).
    #[default]
    Off,
    /// Rotate through the host's source addresses for backend connections, and cap concurrent
    /// data transfers per source address at the backend's `passive_ports` size.
    Rotate,
}

impl BackendSourceMode {
    fn parse(raw: &str) -> Result<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "off" => Ok(BackendSourceMode::Off),
            "rotate" => Ok(BackendSourceMode::Rotate),
            other => bail!("invalid backend source mode '{other}', expected 'off' or 'rotate'"),
        }
    }
}

/// Which local addresses the gateway uses as the source of its backend connections.
///
/// With `include_interfaces` set, only those interfaces are used. Otherwise every usable IPv4
/// address on the host is, except those on built-in virtual interfaces (`lo`, `docker*`, `veth*`,
/// ...) and on `exclude_interfaces`. Interface names accept `*` and `?` wildcards.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BackendSourceConfig {
    pub mode: BackendSourceMode,
    pub include_interfaces: Vec<String>,
    pub exclude_interfaces: Vec<String>,
    /// How often the host's interfaces are listed again, so an interface attached later (an EC2
    /// ENI, say) is picked up without a restart.
    pub refresh_secs: u64,
}

impl Default for BackendSourceConfig {
    fn default() -> Self {
        BackendSourceConfig {
            mode: BackendSourceMode::Off,
            include_interfaces: Vec::new(),
            exclude_interfaces: Vec::new(),
            refresh_secs: 30,
        }
    }
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
    /// Only accept a client's data connection (to the PASV port the gateway announced) from the
    /// same IP address as that client's control connection (default `true`). The PASV port is
    /// open to the whole network from the moment it is announced, so without this a host that
    /// connects to it first would have its bytes uploaded under the real client's filename.
    /// Set to `false` only if devices legitimately open their data connections from a different
    /// address than their control connections (some carrier-grade NAT pools do).
    pub require_data_ip_match: bool,
    /// When a backend's `PASV` reply names the unspecified address (`0.0.0.0`, which vsftpd
    /// sends when it cannot work out its own address, e.g. under `listen_ipv6=YES`), connect to
    /// the data port at the address the control connection goes to instead (default `true`).
    /// Connecting to `0.0.0.0` would reach the gateway's own host, so such a reply cannot work
    /// otherwise; most FTP clients make the same substitution. Set to `false` to treat the
    /// reply literally.
    pub backend_pasv_fallback_to_control_ip: bool,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            max_command_line_bytes: 4096,
            max_connections_per_ip: 10,
            require_data_ip_match: true,
            backend_pasv_fallback_to_control_ip: true,
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
        if let Some(v) = env_var("GATEWAY_BACKEND_SOURCE")? {
            self.backend_source.mode = BackendSourceMode::parse(&v)?;
        }
        if let Some(v) = env_var("GATEWAY_BACKEND_SOURCE_INCLUDE")? {
            self.backend_source.include_interfaces = parse_name_list(&v);
        }
        if let Some(v) = env_var("GATEWAY_BACKEND_SOURCE_EXCLUDE")? {
            self.backend_source.exclude_interfaces = parse_name_list(&v);
        }
        if let Some(v) = env_var("GATEWAY_BACKEND_SOURCE_REFRESH_SECS")? {
            self.backend_source.refresh_secs = v
                .parse()
                .context("invalid GATEWAY_BACKEND_SOURCE_REFRESH_SECS")?;
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
        if let Some(v) = env_var("GATEWAY_REQUIRE_DATA_IP_MATCH")? {
            self.limits.require_data_ip_match =
                parse_bool(&v).context("invalid GATEWAY_REQUIRE_DATA_IP_MATCH")?;
        }
        if let Some(v) = env_var("GATEWAY_BACKEND_PASV_FALLBACK_TO_CONTROL_IP")? {
            self.limits.backend_pasv_fallback_to_control_ip =
                parse_bool(&v).context("invalid GATEWAY_BACKEND_PASV_FALLBACK_TO_CONTROL_IP")?;
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
        if self.backend_source.mode == BackendSourceMode::Rotate {
            if self.backend_source.refresh_secs == 0 {
                bail!("backend_source.refresh_secs must be greater than 0");
            }
            let names = self
                .backend_source
                .include_interfaces
                .iter()
                .chain(&self.backend_source.exclude_interfaces);
            if names.into_iter().any(|name| name.trim().is_empty()) {
                bail!("backend_source interface names must not be empty");
            }
        }
        Ok(())
    }
}

/// Parses `true`/`false` (case-insensitive) or `1`/`0`.
fn parse_bool(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => bail!("'{other}' is not a boolean (expected true/false or 1/0)"),
    }
}

/// Parses a comma-separated list of names, ignoring blanks (`"eth0, eth1"` -> `["eth0", "eth1"]`).
fn parse_name_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

fn env_var(key: &str) -> Result<Option<String>> {
    match std::env::var(key) {
        Ok(v) => Ok(Some(v)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => bail!("{key} is not valid unicode"),
    }
}

/// Parses the "host1:port1,host2:port2@first:last" format. The optional `@first:last` suffix is
/// that backend's data-connection port range (see `BackendConfig::passive_ports`).
fn parse_backends(raw: &str) -> Result<Vec<BackendConfig>> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (address, passive_ports) = match entry.split_once('@') {
                Some((address, ports)) => (
                    address,
                    PortRange::parse_first_last(ports)
                        .with_context(|| format!("invalid backend entry '{entry}'"))?,
                ),
                None => (entry, DEFAULT_BACKEND_PASSIVE_PORTS),
            };
            let (host, port) = address.rsplit_once(':').with_context(|| {
                format!("invalid backend entry '{entry}', expected host:port[@first:last]")
            })?;
            let port: u16 = port
                .parse()
                .with_context(|| format!("invalid port in backend entry '{entry}'"))?;
            Ok(BackendConfig {
                host: host.to_string(),
                port,
                passive_ports,
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
                port: 21,
                ..Default::default()
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
                    port: 21,
                    ..Default::default()
                },
                BackendConfig {
                    host: "ftp02".to_string(),
                    port: 2121,
                    ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
            }],
            ..Config::default()
        }
    }

    #[test]
    fn example_config_parses_as_shipped_and_with_the_optional_blocks_enabled() {
        let example = include_str!("../config.example.yaml");
        let as_shipped: Config = serde_yaml::from_str(example).unwrap();
        assert_eq!(as_shipped.backend_tls.mode, BackendTlsMode::Off);
        assert_eq!(as_shipped.backend_source.mode, BackendSourceMode::Off);
        assert_eq!(as_shipped.backends[0].passive_ports.len(), 9);

        // Uncomment only the `backend_tls` / `backend_source` blocks, not the header's env var
        // list.
        let enabled = [
            "backend_tls:",
            "  mode:",
            "  ca_file:",
            "  server_name:",
            "  max_version:",
            "backend_source:",
            "  include_interfaces:",
            "  exclude_interfaces:",
            "  refresh_secs:",
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
        assert_eq!(enabled.backend_source.mode, BackendSourceMode::Rotate);
        assert_eq!(enabled.backend_source.include_interfaces, ["ens5", "ens6"]);
        assert_eq!(enabled.backend_source.exclude_interfaces, ["ens7"]);
        assert_eq!(enabled.backend_source.refresh_secs, 30);
    }

    #[test]
    fn backend_passive_ports_default_to_the_aws_transfer_family_range() {
        let backends = parse_backends("ftp01:21").unwrap();
        assert_eq!(
            backends[0].passive_ports,
            PortRange {
                start: 8192,
                end: 8200
            }
        );
        assert_eq!(backends[0].passive_ports.len(), 9);
    }

    #[test]
    fn parses_per_backend_passive_ports_from_env_syntax() {
        let backends =
            parse_backends("ftp01:21@8192:8200, ftp02:2121@30000:30009,ftp03:21").unwrap();
        assert_eq!(backends[0].passive_ports.len(), 9);
        assert_eq!(
            backends[1].passive_ports,
            PortRange {
                start: 30000,
                end: 30009
            }
        );
        assert_eq!(backends[1].port, 2121);
        assert_eq!(backends[2].passive_ports, DEFAULT_BACKEND_PASSIVE_PORTS);
    }

    #[test]
    fn single_port_range_is_allowed() {
        let backends = parse_backends("ftp01:21@8192:8192").unwrap();
        assert_eq!(backends[0].passive_ports.len(), 1);
    }

    #[test]
    fn rejects_malformed_backend_passive_ports() {
        assert!(parse_backends("ftp01:21@8200:8192").is_err());
        assert!(parse_backends("ftp01:21@8192").is_err());
        assert!(parse_backends("ftp01:21@a:b").is_err());
        assert!(parse_backends("ftp01:21@8192:70000").is_err());
    }

    #[test]
    fn parses_backend_passive_ports_from_yaml_in_both_forms() {
        let config: Config = serde_yaml::from_str(
            "backends:\n  - host: a\n    port: 21\n    passive_ports: \"9000:9004\"\n  - host: b\n    port: 21\n    passive_ports: {start: 9100, end: 9102}\n  - host: c\n    port: 21\n",
        )
        .unwrap();
        assert_eq!(config.backends[0].passive_ports.len(), 5);
        assert_eq!(config.backends[1].passive_ports.len(), 3);
        assert_eq!(
            config.backends[2].passive_ports,
            DEFAULT_BACKEND_PASSIVE_PORTS
        );
    }

    #[test]
    fn rejects_inverted_backend_passive_ports_in_yaml() {
        assert!(
            serde_yaml::from_str::<Config>(
                "backends:\n  - host: a\n    port: 21\n    passive_ports: \"9004:9000\"\n"
            )
            .is_err()
        );
        assert!(
            serde_yaml::from_str::<Config>(
                "backends:\n  - host: a\n    port: 21\n    passive_ports: {start: 9004, end: 9000}\n"
            )
            .is_err()
        );
    }

    #[test]
    fn pasv_control_ip_fallback_defaults_to_on_and_can_be_switched_off() {
        assert!(Config::default().limits.backend_pasv_fallback_to_control_ip);
        let config: Config =
            serde_yaml::from_str("limits:\n  backend_pasv_fallback_to_control_ip: false\n")
                .unwrap();
        assert!(!config.limits.backend_pasv_fallback_to_control_ip);
        assert!(config.limits.require_data_ip_match);
    }

    #[test]
    fn data_ip_match_defaults_to_required_and_can_be_switched_off() {
        assert!(Config::default().limits.require_data_ip_match);
        let config: Config =
            serde_yaml::from_str("limits:\n  require_data_ip_match: false\n").unwrap();
        assert!(!config.limits.require_data_ip_match);
        // Other limits keep their defaults when only this one is given.
        assert_eq!(config.limits.max_connections_per_ip, 10);
    }

    #[test]
    fn parses_booleans_from_env_style_text() {
        for truthy in ["true", "TRUE", "1", " true "] {
            assert!(parse_bool(truthy).unwrap(), "{truthy}");
        }
        for falsy in ["false", "False", "0"] {
            assert!(!parse_bool(falsy).unwrap(), "{falsy}");
        }
        assert!(parse_bool("yes").is_err());
        assert!(parse_bool("").is_err());
    }

    #[test]
    fn backend_source_defaults_to_off() {
        let source = Config::default().backend_source;
        assert_eq!(source.mode, BackendSourceMode::Off);
        assert!(source.include_interfaces.is_empty());
        assert!(source.exclude_interfaces.is_empty());
        assert_eq!(source.refresh_secs, 30);
    }

    #[test]
    fn parses_backend_source_from_yaml() {
        let config: Config = serde_yaml::from_str(
            "backend_source:\n  mode: rotate\n  include_interfaces: [eth0, \"ens*\"]\n  exclude_interfaces: [eth9]\n  refresh_secs: 10\n",
        )
        .unwrap();
        assert_eq!(config.backend_source.mode, BackendSourceMode::Rotate);
        assert_eq!(config.backend_source.include_interfaces, ["eth0", "ens*"]);
        assert_eq!(config.backend_source.exclude_interfaces, ["eth9"]);
        assert_eq!(config.backend_source.refresh_secs, 10);
    }

    #[test]
    fn parses_backend_source_mode_and_name_lists() {
        assert_eq!(
            BackendSourceMode::parse("Rotate").unwrap(),
            BackendSourceMode::Rotate
        );
        assert_eq!(
            BackendSourceMode::parse("off").unwrap(),
            BackendSourceMode::Off
        );
        assert!(BackendSourceMode::parse("round-robin").is_err());
        assert_eq!(parse_name_list(" eth0, ,ens*,"), ["eth0", "ens*"]);
        assert!(parse_name_list("").is_empty());
    }

    #[test]
    fn validate_checks_backend_source_only_when_rotating() {
        let mut config = config_with_backend();
        config.backend_source.refresh_secs = 0;
        config.backend_source.include_interfaces = vec![" ".to_string()];
        assert!(config.validate().is_ok());

        config.backend_source.mode = BackendSourceMode::Rotate;
        assert!(config.validate().is_err());

        config.backend_source.refresh_secs = 30;
        assert!(config.validate().is_err(), "blank interface name");

        config.backend_source.include_interfaces = vec!["eth0".to_string()];
        assert!(config.validate().is_ok());
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
                ..Default::default()
            }],
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }
}
