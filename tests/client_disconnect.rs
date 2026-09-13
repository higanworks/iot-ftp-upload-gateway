//! PROJECT_TEST.ja.md 6章 "Client disconnect": an IoT device losing its connection at various
//! points is normal operation, not an exceptional condition. These tests confirm the gateway
//! doesn't panic or hang, releases the PASV port either way, and stays usable for the next
//! session.

mod common;

use std::net::Ipv4Addr;
use std::time::Duration;

use iot_ftp_upload_gateway::config::{BackendConfig, PassiveConfig, PortRange, TimeoutConfig};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply;
use iot_ftp_upload_gateway::server::session;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Spawns a session against the given backend/passive config, accepting a single client
/// connection on an ephemeral port. Returns the address to connect to as the client and a
/// handle to the session task, so the test can wait for it and assert it didn't panic.
async fn spawn_session(
    backend_config: BackendConfig,
    passive_config: PassiveConfig,
    timeouts: TimeoutConfig,
    port_manager: PortManager,
) -> (std::net::SocketAddr, JoinHandle<anyhow::Result<()>>) {
    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let (stream, peer_addr) = gateway_listener.accept().await.unwrap();
        session::handle(
            stream,
            peer_addr,
            backend_config,
            passive_config,
            timeouts,
            port_manager,
        )
        .await
    });

    (gateway_addr, handle)
}

/// Waits for a session task to finish, failing the test (rather than hanging forever) if it
/// doesn't within a few seconds.
async fn await_session(task: JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("session task hung instead of finishing")
        .expect("session task panicked")
}

#[tokio::test]
async fn client_disconnect_after_pasv_releases_port() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19800,
            end: 19800,
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

    {
        let client = TcpStream::connect(gateway_addr).await.unwrap();
        let mut conn = BufReader::new(client);
        common::login(&mut conn).await;

        conn.write_all(b"PASV\r\n").await.unwrap();
        let pasv_reply = common::read_reply(&mut conn).await;
        assert!(pasv_reply.starts_with("227"), "{pasv_reply}");

        // Abruptly disconnect: `conn` drops at the end of this block without ever opening the
        // data connection or sending QUIT.
    }

    let result = await_session(session_task).await;
    assert!(
        result.is_ok(),
        "session::handle returned an error: {result:?}"
    );

    // The pool has exactly one port; this only succeeds if the earlier allocation was released.
    let guard = port_manager.allocate().await;
    assert!(
        guard.is_some(),
        "PASV port was not released after client disconnect"
    );
}

#[tokio::test]
async fn client_disconnect_mid_upload_completes_without_hanging() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19810,
            end: 19810,
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

    {
        let client = TcpStream::connect(gateway_addr).await.unwrap();
        let mut conn = BufReader::new(client);
        common::login(&mut conn).await;

        conn.write_all(b"PASV\r\n").await.unwrap();
        let pasv_reply = common::read_reply(&mut conn).await;
        let (_, port) = parse_pasv_reply(&pasv_reply).unwrap();
        let mut data_conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        conn.write_all(b"STOR partial.txt\r\n").await.unwrap();
        let stor_reply = common::read_reply(&mut conn).await;
        assert!(stor_reply.starts_with("150"), "{stor_reply}");

        // Write only part of an intended payload, then vanish: both the data and control
        // connections drop here without a graceful shutdown, simulating a mobile client that
        // loses its network connection mid-upload.
        data_conn.write_all(b"only some of the ").await.unwrap();
    }

    let result = await_session(session_task).await;
    assert!(
        result.is_ok(),
        "session::handle returned an error: {result:?}"
    );

    let guard = port_manager.allocate().await;
    assert!(
        guard.is_some(),
        "PASV port was not released after a mid-upload disconnect"
    );
}

#[tokio::test]
async fn next_session_succeeds_after_a_prior_disconnect() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19820,
            end: 19820,
        },
    };
    let timeouts = TimeoutConfig::default();
    let port_manager = PortManager::new(passive_config.port_range);

    // First session: disconnect right after PASV without ever using the port.
    let (gateway_addr, session_task) = spawn_session(
        backend_config.clone(),
        passive_config,
        timeouts,
        port_manager.clone(),
    )
    .await;
    {
        let client = TcpStream::connect(gateway_addr).await.unwrap();
        let mut conn = BufReader::new(client);
        common::login(&mut conn).await;
        conn.write_all(b"PASV\r\n").await.unwrap();
        assert!(common::read_reply(&mut conn).await.starts_with("227"));
    }
    await_session(session_task)
        .await
        .expect("first session should end cleanly on disconnect");

    // Second session, same port_manager: a normal upload should succeed, proving the earlier
    // disconnect didn't leave the pool (or anything else) in a bad state.
    let (gateway_addr, _session_task) =
        spawn_session(backend_config, passive_config, timeouts, port_manager).await;
    let client = common::connect_with_retry(gateway_addr).await;
    let mut conn = BufReader::new(client);
    common::login(&mut conn).await;
    let reply = common::perform_upload(&mut conn, "after_disconnect.txt", b"still works").await;
    assert!(reply.starts_with("226"), "{reply}");

    let uploads = backend.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].0, "after_disconnect.txt");
    assert_eq!(uploads[0].1, b"still works");
}
