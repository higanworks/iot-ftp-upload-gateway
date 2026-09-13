//! PROJECT_TEST.ja.md 7 chapter "Port exhaustion": with the PASV pool sized to exactly one port,
//! a second concurrent session's `PASV` must be rejected with `425` rather than the gateway
//! blocking or panicking, and the first session must be completely unaffected.

mod common;

use std::net::Ipv4Addr;

use common::{await_session, spawn_session};
use iot_ftp_upload_gateway::config::{PassiveConfig, PortRange, TimeoutConfig};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[tokio::test]
async fn second_session_gets_425_while_pool_is_exhausted_then_succeeds_after_release() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = || iot_ftp_upload_gateway::config::BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19930,
            end: 19930,
        },
    };
    let timeouts = TimeoutConfig::default();
    // A single shared pool of exactly one port, used by both sessions below -- this is what
    // makes the exhaustion happen.
    let port_manager = PortManager::new(passive_config.port_range);

    // Session A takes the pool's only port and holds it (never sends STOR, so the port stays
    // allocated until the connection closes).
    let (addr_a, session_a) = spawn_session(
        backend_config(),
        passive_config,
        timeouts,
        port_manager.clone(),
    )
    .await;
    let client_a = TcpStream::connect(addr_a).await.unwrap();
    let mut conn_a = BufReader::new(client_a);
    common::login(&mut conn_a).await;
    conn_a.write_all(b"PASV\r\n").await.unwrap();
    let reply_a = common::read_reply(&mut conn_a).await;
    assert!(reply_a.starts_with("227"), "{reply_a}");

    // Session B, sharing the same (now-exhausted) pool, must be told 425 rather than hang.
    let (addr_b, session_b) = spawn_session(
        backend_config(),
        passive_config,
        timeouts,
        port_manager.clone(),
    )
    .await;
    let client_b = common::connect_with_retry(addr_b).await;
    let mut conn_b = BufReader::new(client_b);
    common::login(&mut conn_b).await;
    conn_b.write_all(b"PASV\r\n").await.unwrap();
    let reply_b = common::read_reply(&mut conn_b).await;
    assert!(reply_b.starts_with("425"), "{reply_b}");

    // Session A is entirely unaffected by B's exhausted request: it can still complete an
    // upload using the port it already holds.
    let upload_reply = common::perform_upload(&mut conn_a, "a.txt", b"session A's data").await;
    assert!(upload_reply.starts_with("226"), "{upload_reply}");
    conn_a.write_all(b"QUIT\r\n").await.unwrap();
    await_session(session_a)
        .await
        .expect("session A should end cleanly");

    // Now that A released the port, B can successfully retry PASV and complete its own upload.
    conn_b.write_all(b"PASV\r\n").await.unwrap();
    let retry_reply = common::read_reply(&mut conn_b).await;
    assert!(retry_reply.starts_with("227"), "{retry_reply}");

    conn_b.write_all(b"TYPE I\r\n").await.unwrap();
    common::read_reply(&mut conn_b).await;

    let (_, port) =
        iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply(&retry_reply).unwrap();
    let mut data_conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    conn_b.write_all(b"STOR b.txt\r\n").await.unwrap();
    let stor_reply = common::read_reply(&mut conn_b).await;
    assert!(stor_reply.starts_with("150"), "{stor_reply}");
    data_conn.write_all(b"session B's data").await.unwrap();
    data_conn.shutdown().await.unwrap();
    let completion = common::read_reply(&mut conn_b).await;
    assert!(completion.starts_with("226"), "{completion}");

    conn_b.write_all(b"QUIT\r\n").await.unwrap();
    await_session(session_b)
        .await
        .expect("session B should end cleanly");

    let uploads = backend.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 2);
    assert!(uploads.contains(&("a.txt".to_string(), b"session A's data".to_vec())));
    assert!(uploads.contains(&("b.txt".to_string(), b"session B's data".to_vec())));
}
