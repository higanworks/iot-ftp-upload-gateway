//! The client's data connection must come from the same IP address as its control connection
//! (`limits.require_data_ip_match`, on by default); otherwise any host that reaches the announced
//! PASV port first could have its bytes uploaded under another client's filename.
//!
//! Telling two client addresses apart needs a second local address to connect from; Linux treats
//! all of 127.0.0.0/8 as local, so these tests are Linux-only. The matching case is exercised on
//! every platform by the rest of the integration tests, which all run with the option on.
#![cfg(target_os = "linux")]

mod common;

use std::net::{Ipv4Addr, SocketAddr};

use common::spawn_session_with_limits;
use iot_ftp_upload_gateway::config::{
    BackendConfig, LimitsConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpSocket, TcpStream};

/// Connects to the gateway as a client whose control connection comes from 127.0.0.2 -- while
/// every data connection a test opens below comes from 127.0.0.1.
async fn control_connection_from_127_0_0_2(gateway: SocketAddr) -> BufReader<TcpStream> {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.2:0".parse().unwrap()).unwrap();
    BufReader::new(socket.connect(gateway).await.unwrap())
}

async fn start(
    require_data_ip_match: bool,
    port_range: PortRange,
) -> (common::MockBackend, SocketAddr) {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
        ..Default::default()
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range,
    };
    let timeouts = TimeoutConfig {
        // The wait for a data connection that never arrives.
        connection_timeout_secs: 1,
        ..TimeoutConfig::default()
    };
    let limits = LimitsConfig {
        require_data_ip_match,
        ..LimitsConfig::default()
    };
    let (gateway, _session) = spawn_session_with_limits(
        backend_config,
        passive_config,
        timeouts,
        limits,
        PortManager::new(port_range),
    )
    .await;
    (backend, gateway)
}

#[tokio::test]
async fn a_data_connection_from_a_different_ip_is_refused_and_nothing_is_uploaded() {
    let (backend, gateway) = start(
        true,
        PortRange {
            start: 19990,
            end: 19994,
        },
    )
    .await;
    let mut client = control_connection_from_127_0_0_2(gateway).await;
    common::login(&mut client).await;

    client.write_all(b"PASV\r\n").await.unwrap();
    let pasv = common::read_reply(&mut client).await;
    let (_, port) = parse_pasv_reply(&pasv).unwrap();
    // Opened from 127.0.0.1, not from the control connection's 127.0.0.2.
    let mut data = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    client.write_all(b"STOR stolen.bin\r\n").await.unwrap();
    let reply = common::read_reply(&mut client).await;
    assert!(reply.starts_with("425"), "{reply}");

    // Whatever that other host sends goes nowhere.
    let _ = data.write_all(b"attacker bytes").await;
    assert!(backend.uploads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_check_can_be_switched_off() {
    let (backend, gateway) = start(
        false,
        PortRange {
            start: 19995,
            end: 19999,
        },
    )
    .await;
    let mut client = control_connection_from_127_0_0_2(gateway).await;
    common::login(&mut client).await;

    let reply = common::perform_upload(&mut client, "ok.bin", b"payload").await;
    assert!(reply.starts_with("226"), "{reply}");
    let uploads = backend.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].1, b"payload");
}
