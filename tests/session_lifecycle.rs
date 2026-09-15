//! Integration tests driving `iot_ftp_upload_gateway::server::session::handle` directly against
//! an in-process mock backend (`tests/common`) -- no real Docker/FTP server needed, so these run
//! as plain `cargo test`. See PROJECT_TEST.ja.md for the fuller manual/docker-compose-based
//! test strategy this suite is a first slice of.

mod common;

use std::net::Ipv4Addr;

use iot_ftp_upload_gateway::backend::dns_cache::DnsCache;
use iot_ftp_upload_gateway::config::{
    BackendConfig, LimitsConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::server::session;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};

/// Starts a session exactly like `listener::run` does for one incoming connection: accepts a
/// single client connection on an ephemeral port and hands it to the real `session::handle`.
/// Returns the address the test should connect to as the "client".
async fn spawn_session(
    backend: &common::MockBackend,
    port_range: PortRange,
) -> std::net::SocketAddr {
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range,
    };
    let timeouts = TimeoutConfig::default();
    let port_manager = PortManager::new(port_range);

    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (stream, peer_addr) = gateway_listener.accept().await.unwrap();
        session::handle(
            stream,
            peer_addr,
            1,
            backend_config,
            passive_config,
            timeouts,
            LimitsConfig::default(),
            port_manager,
            iot_ftp_upload_gateway::metrics::Metrics::new(),
            DnsCache::new(),
        )
        .await
        .unwrap();
    });

    gateway_addr
}

#[tokio::test]
async fn normal_upload_reaches_backend() {
    let backend = common::spawn_mock_backend().await;
    let gateway_addr = spawn_session(
        &backend,
        PortRange {
            start: 19500,
            end: 19510,
        },
    )
    .await;

    let client = TcpStream::connect(gateway_addr).await.unwrap();
    let mut conn = BufReader::new(client);

    common::login(&mut conn).await;
    let completion_reply =
        common::perform_upload(&mut conn, "baseline.txt", b"hello from the baseline test").await;
    assert!(completion_reply.starts_with("226"), "{completion_reply}");

    let uploads = backend.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].0, "baseline.txt");
    assert_eq!(uploads[0].1, b"hello from the baseline test");
}
