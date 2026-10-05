//! A backend that stops answering `PASV` must not be able to hold a gateway session -- and the
//! PASV port it has claimed -- forever. The gateway gives up after `command_timeout_secs`, tells
//! the client `421`, and ends the session: a `PASV` reply that turned up late would otherwise be
//! mistaken for the reply to the next command.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use common::{await_session, spawn_session};
use iot_ftp_upload_gateway::config::{BackendConfig, PassiveConfig, PortRange, TimeoutConfig};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// A backend that logs a client in normally, then goes silent on `PASV`.
async fn spawn_backend_that_hangs_on_pasv() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = BufReader::new(stream);
        conn.write_all(b"220 Mock backend\r\n").await.unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            match line.split_whitespace().next().unwrap_or("") {
                "USER" => conn.write_all(b"331 password\r\n").await.unwrap(),
                "PASS" => conn.write_all(b"230 in\r\n").await.unwrap(),
                "PASV" => {
                    // Never answers (until the test is long over).
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                }
                _ => conn.write_all(b"500 ?\r\n").await.unwrap(),
            }
        }
    });
    addr
}

#[tokio::test]
async fn backend_not_answering_pasv_ends_the_session_with_421_and_releases_the_port() {
    let backend = spawn_backend_that_hangs_on_pasv().await;
    let backend_config = BackendConfig {
        host: backend.ip().to_string(),
        port: backend.port(),
        ..Default::default()
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19980,
            end: 19984,
        },
    };
    let timeouts = TimeoutConfig {
        command_timeout_secs: 1,
        ..TimeoutConfig::default()
    };
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) = spawn_session(
        backend_config,
        passive_config,
        timeouts,
        port_manager.clone(),
    )
    .await;

    let mut client = BufReader::new(TcpStream::connect(gateway_addr).await.unwrap());
    common::login(&mut client).await;

    // The client asks for a data port and connects to it, as a real client would.
    client.write_all(b"PASV\r\n").await.unwrap();
    let pasv = common::read_reply(&mut client).await;
    let (_, port) = parse_pasv_reply(&pasv).unwrap();
    let _data = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert_eq!(port_manager.active_count(), 1);

    // STOR makes the gateway ask the backend for *its* data port -- and the backend never answers.
    let started = Instant::now();
    client.write_all(b"STOR hang.bin\r\n").await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), common::read_reply(&mut client))
        .await
        .expect("the gateway must give up on its own instead of waiting for the backend forever");
    assert!(reply.starts_with("421"), "{reply}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );

    // The session is over, cleanly, and the PASV port is free again.
    let result = await_session(session_task).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(port_manager.active_count(), 0);
}
