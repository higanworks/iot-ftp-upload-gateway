//! PROJECT_SECURITY.md section 5, "Connection Exhaustion": a single source shouldn't be able to
//! hold unlimited connections open. Drives the real accept loop
//! (`iot_ftp_upload_gateway::run`, the same entry point `main.rs` uses; see also
//! `tests/backend_stickiness.rs`) with a small `max_connections_per_ip`, so this exercises the
//! real `IpConnectionLimiter` wiring in `server::listener`, not just its unit tests.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use iot_ftp_upload_gateway::config::{
    BackendConfig, Config, LimitsConfig, ListenConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn extra_connection_beyond_the_per_ip_limit_is_rejected() {
    let backend = common::spawn_mock_backend().await;

    let config = Config {
        listen: ListenConfig {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 19800,
        },
        passive: PassiveConfig {
            address: Ipv4Addr::LOCALHOST,
            port_range: PortRange {
                start: 19810,
                end: 19820,
            },
        },
        backends: vec![BackendConfig {
            host: backend.addr.ip().to_string(),
            port: backend.addr.port(),
        }],
        timeouts: TimeoutConfig::default(),
        limits: LimitsConfig {
            max_connections_per_ip: 2,
            ..LimitsConfig::default()
        },
        metrics: Default::default(),
    };
    let gateway_addr = SocketAddr::new(config.listen.address, config.listen.port);

    tokio::spawn(async move {
        iot_ftp_upload_gateway::run(config).await.unwrap();
    });

    // Open (and keep open) as many connections as the limit allows -- each should get the
    // normal banner.
    let mut held_connections = Vec::new();
    for _ in 0..2 {
        let client = common::connect_with_retry(gateway_addr).await;
        let mut conn = BufReader::new(client);
        let mut banner = String::new();
        conn.read_line(&mut banner).await.unwrap();
        assert!(banner.starts_with("220"), "{banner}");
        held_connections.push(conn);
    }

    // A third connection from the same IP, while the first two are still open, must be
    // rejected -- no banner, connection closed rather than the gateway hanging.
    let extra_client = common::connect_with_retry(gateway_addr).await;
    let mut extra_conn = BufReader::new(extra_client);
    let mut extra_banner = String::new();
    let read_result = tokio::time::timeout(
        Duration::from_secs(2),
        extra_conn.read_line(&mut extra_banner),
    )
    .await
    .expect("gateway should close the rejected connection promptly, not hang");
    let bytes_read = read_result.unwrap_or(0);
    assert_eq!(
        bytes_read, 0,
        "expected the over-limit connection to be closed with no banner, got: {extra_banner:?}"
    );

    // Freeing one slot should let a new connection through again.
    drop(held_connections.remove(0));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = common::connect_with_retry(gateway_addr).await;
    let mut conn = BufReader::new(client);
    let mut banner = String::new();
    conn.read_line(&mut banner).await.unwrap();
    assert!(banner.starts_with("220"), "{banner}");
}
