//! PROJECT_TEST.ja.md 5-6 chapters "Backend disconnect": the gateway must not panic when its
//! own backend is unreachable or drops mid-session, must give the client a sensible error, and
//! must release resources so it keeps serving other sessions.

mod common;

use std::net::Ipv4Addr;

use common::{await_session, spawn_session};
use iot_ftp_upload_gateway::config::{BackendConfig, PassiveConfig, PortRange, TimeoutConfig};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[tokio::test]
async fn backend_refuses_connection_returns_421() {
    let unreachable = common::unused_addr().await;
    let backend_config = BackendConfig {
        host: unreachable.ip().to_string(),
        port: unreachable.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19900,
            end: 19900,
        },
    };
    let timeouts = TimeoutConfig::default();
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) =
        spawn_session(backend_config, passive_config, timeouts, port_manager).await;

    let client = TcpStream::connect(gateway_addr).await.unwrap();
    let mut conn = BufReader::new(client);
    let mut reply = String::new();
    conn.read_line(&mut reply).await.unwrap();
    assert!(reply.starts_with("421"), "{reply}");

    let result = await_session(session_task).await;
    assert!(
        result.is_ok(),
        "session::handle returned an error: {result:?}"
    );
}

#[tokio::test]
async fn backend_closes_before_banner_returns_421() {
    let backend_addr = common::spawn_mock_backend_closing_immediately().await;
    let backend_config = BackendConfig {
        host: backend_addr.ip().to_string(),
        port: backend_addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19910,
            end: 19910,
        },
    };
    let timeouts = TimeoutConfig::default();
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) =
        spawn_session(backend_config, passive_config, timeouts, port_manager).await;

    let client = TcpStream::connect(gateway_addr).await.unwrap();
    let mut conn = BufReader::new(client);
    let mut reply = String::new();
    conn.read_line(&mut reply).await.unwrap();
    assert!(reply.starts_with("421"), "{reply}");

    let result = await_session(session_task).await;
    assert!(
        result.is_ok(),
        "session::handle returned an error: {result:?}"
    );
}

#[tokio::test]
async fn backend_disconnect_before_data_connection_returns_425_and_releases_port() {
    let backend_addr = common::spawn_mock_backend_disconnecting_after_login().await;
    let backend_config = BackendConfig {
        host: backend_addr.ip().to_string(),
        port: backend_addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19920,
            end: 19920,
        },
    };
    let timeouts = TimeoutConfig::default();
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) = spawn_session(
        backend_config,
        passive_config,
        timeouts,
        port_manager.clone(),
    )
    .await;

    let client = TcpStream::connect(gateway_addr).await.unwrap();
    let mut conn = BufReader::new(client);
    common::login(&mut conn).await;

    // The backend is already gone by this point (it disconnects right after PASS). PASV is
    // handled entirely gateway-side, so it still succeeds; the backend is only contacted when
    // the client's data connection arrives and the gateway tries to open its own PASV data
    // connection to the backend, which is where this should fail.
    conn.write_all(b"PASV\r\n").await.unwrap();
    let pasv_reply = common::read_reply(&mut conn).await;
    assert!(pasv_reply.starts_with("227"), "{pasv_reply}");
    let (_, port) = iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply(&pasv_reply).unwrap();
    let _data_conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    conn.write_all(b"STOR whatever.txt\r\n").await.unwrap();
    let stor_reply = common::read_reply(&mut conn).await;
    assert!(stor_reply.starts_with("425"), "{stor_reply}");

    // The port must be available again even though the backend (not the client) is what failed.
    let guard = port_manager.allocate().await;
    assert!(
        guard.is_some(),
        "PASV port was not released after a backend failure"
    );
    drop(guard);

    // The control connection is still nominally open from the client's side, but every further
    // command needs the (now-dead) backend connection, so the session should end the next time
    // one is sent.
    conn.write_all(b"QUIT\r\n").await.unwrap();
    let result = await_session(session_task).await;
    assert!(
        result.is_ok(),
        "session::handle returned an error: {result:?}"
    );
}

#[tokio::test]
async fn backend_disconnect_during_transfer_returns_426_and_releases_port() {
    let backend_addr = common::spawn_mock_backend_disconnecting_during_transfer().await;
    let backend_config = BackendConfig {
        host: backend_addr.ip().to_string(),
        port: backend_addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19940,
            end: 19940,
        },
    };
    let timeouts = TimeoutConfig::default();
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) = spawn_session(
        backend_config,
        passive_config,
        timeouts,
        port_manager.clone(),
    )
    .await;

    let client = TcpStream::connect(gateway_addr).await.unwrap();
    let mut conn = BufReader::new(client);
    common::login(&mut conn).await;

    conn.write_all(b"PASV\r\n").await.unwrap();
    let pasv_reply = common::read_reply(&mut conn).await;
    assert!(pasv_reply.starts_with("227"), "{pasv_reply}");
    let (_, port) = iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply(&pasv_reply).unwrap();
    let mut data_conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    conn.write_all(b"STOR midway.txt\r\n").await.unwrap();
    let stor_reply = common::read_reply(&mut conn).await;
    assert!(stor_reply.starts_with("150"), "{stor_reply}");

    // The backend only reads a handful of bytes before "crashing" (dropping both its data and
    // control connections without ever sending a completion reply); write more than that to
    // prove the failure is reported even if some bytes did make it through.
    let _ = data_conn.write_all(b"this transfer will not finish").await;

    let final_reply = common::read_reply(&mut conn).await;
    assert!(final_reply.starts_with("426"), "{final_reply}");

    let result = await_session(session_task).await;
    assert!(
        result.is_ok(),
        "session::handle returned an error: {result:?}"
    );

    let guard = port_manager.allocate().await;
    assert!(
        guard.is_some(),
        "PASV port was not released after a mid-transfer backend disconnect"
    );
}
