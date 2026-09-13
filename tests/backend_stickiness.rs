//! PROJECT_TEST.ja.md 4章 calls session stickiness -- a session's control connection and its
//! PASV data connection always landing on the same backend -- "the important integration test"
//! for this project. This drives the real accept loop (`iot_ftp_upload_gateway::run`, the same
//! entry point `main.rs` uses) with two mock backends, so it exercises round-robin selection
//! (`backend::selector::BackendSelector`) together with the per-session backend pinning in
//! `server::session::handle`, rather than just the latter in isolation.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use iot_ftp_upload_gateway::config::{
    BackendConfig, Config, ListenConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use tokio::io::BufReader;

#[tokio::test]
async fn control_and_data_stay_on_the_backend_assigned_at_session_start() {
    let backend_a = common::spawn_mock_backend().await;
    let backend_b = common::spawn_mock_backend().await;

    let config = Config {
        listen: ListenConfig {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 19700,
        },
        passive: PassiveConfig {
            address: Ipv4Addr::LOCALHOST,
            port_range: PortRange {
                start: 19710,
                end: 19730,
            },
        },
        backends: vec![
            BackendConfig {
                host: backend_a.addr.ip().to_string(),
                port: backend_a.addr.port(),
            },
            BackendConfig {
                host: backend_b.addr.ip().to_string(),
                port: backend_b.addr.port(),
            },
        ],
        timeouts: TimeoutConfig::default(),
    };
    let gateway_addr = SocketAddr::new(config.listen.address, config.listen.port);

    tokio::spawn(async move {
        iot_ftp_upload_gateway::run(config).await.unwrap();
    });

    // Backend selection is round-robin starting at index 0, so the first session accepted gets
    // the first configured backend and the second session gets the second -- deterministic as
    // long as each session fully completes before the next one connects.
    let client_a = common::connect_with_retry(gateway_addr).await;
    let mut conn_a = BufReader::new(client_a);
    common::login(&mut conn_a).await;
    let reply_a = common::perform_upload(&mut conn_a, "from_a.txt", b"payload from client A").await;
    assert!(reply_a.starts_with("226"), "{reply_a}");

    let client_b = common::connect_with_retry(gateway_addr).await;
    let mut conn_b = BufReader::new(client_b);
    common::login(&mut conn_b).await;
    let reply_b = common::perform_upload(&mut conn_b, "from_b.txt", b"payload from client B").await;
    assert!(reply_b.starts_with("226"), "{reply_b}");

    let uploads_a = backend_a.uploads.lock().unwrap();
    let uploads_b = backend_b.uploads.lock().unwrap();

    assert_eq!(
        *uploads_a,
        vec![("from_a.txt".to_string(), b"payload from client A".to_vec())],
        "backend A should have received exactly client A's upload, and nothing from client B"
    );
    assert_eq!(
        *uploads_b,
        vec![("from_b.txt".to_string(), b"payload from client B".to_vec())],
        "backend B should have received exactly client B's upload, and nothing from client A"
    );
}
