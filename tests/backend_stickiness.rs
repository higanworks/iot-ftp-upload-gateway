//! PROJECT_TEST.ja.md 4章 calls session stickiness -- a session's control connection and its
//! PASV data connection always landing on the same backend -- "the important integration test"
//! for this project, and 2章 frames the integration environment around 3 backends. This drives
//! the real accept loop (`iot_ftp_upload_gateway::run`, the same entry point `main.rs` uses)
//! with 3 mock backends, so it exercises round-robin selection
//! (`backend::selector::BackendSelector`) together with the per-session backend pinning in
//! `server::session::handle`, rather than just the latter in isolation.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use iot_ftp_upload_gateway::config::{
    BackendConfig, Config, LimitsConfig, ListenConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use tokio::io::BufReader;

#[tokio::test]
async fn control_and_data_stay_on_the_backend_assigned_at_session_start() {
    let backends = [
        common::spawn_mock_backend().await,
        common::spawn_mock_backend().await,
        common::spawn_mock_backend().await,
    ];

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
        backends: backends
            .iter()
            .map(|b| BackendConfig {
                host: b.addr.ip().to_string(),
                port: b.addr.port(),
            })
            .collect(),
        timeouts: TimeoutConfig::default(),
        limits: LimitsConfig::default(),
    };
    let gateway_addr = SocketAddr::new(config.listen.address, config.listen.port);

    tokio::spawn(async move {
        iot_ftp_upload_gateway::run(config).await.unwrap();
    });

    // Backend selection is round-robin starting at index 0, so session N (0-based) deterministically
    // gets `backends[N % backends.len()]` -- as long as each session fully completes before the
    // next one connects. Drive one more client than there are backends, to prove the assignment
    // actually wraps around rather than just walking through the list once.
    let client_count = backends.len() + 1;
    for i in 0..client_count {
        let client = common::connect_with_retry(gateway_addr).await;
        let mut conn = BufReader::new(client);
        common::login(&mut conn).await;
        let filename = format!("from_client_{i}.txt");
        let payload = format!("payload from client {i}").into_bytes();
        let reply = common::perform_upload(&mut conn, &filename, &payload).await;
        assert!(reply.starts_with("226"), "{reply}");
    }

    // Each backend should have received exactly the uploads from the clients assigned to it by
    // round-robin (including the one that wrapped around), and nothing from any other client --
    // proving both the round-robin assignment and that each session's control+data stuck to a
    // single backend.
    for (backend_index, backend) in backends.iter().enumerate() {
        let expected: Vec<(String, Vec<u8>)> = (backend_index..client_count)
            .step_by(backends.len())
            .map(|i| {
                (
                    format!("from_client_{i}.txt"),
                    format!("payload from client {i}").into_bytes(),
                )
            })
            .collect();
        let uploads = backend.uploads.lock().unwrap();
        assert_eq!(
            *uploads, expected,
            "backend {backend_index} should have received exactly the uploads round-robin assigned to it"
        );
    }
}
