//! The counters on `/metrics` must reflect what really happened to a session: uploads that
//! complete, uploads that start and fail, backend `PASV` failures and timeouts, and idle
//! timeouts. Each test drives a real `session::handle` with a `Metrics` it can read back.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use common::await_session;
use iot_ftp_upload_gateway::backend::dns_cache::DnsCache;
use iot_ftp_upload_gateway::config::{
    BackendConfig, LimitsConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use iot_ftp_upload_gateway::metrics::Metrics;
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply;
use iot_ftp_upload_gateway::server::session;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

fn value(metrics: &Metrics, name: &str) -> u64 {
    let rendered = metrics.render(0, 0, None);
    let prefix = format!("{name} ");
    rendered
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("{name} missing from:\n{rendered}"))
        .parse()
        .unwrap()
}

/// Runs one session against `backend`, returning the client address, the session task, and the
/// `Metrics` the session reports into.
async fn spawn_session(
    backend: SocketAddr,
    port_range: PortRange,
    timeouts: TimeoutConfig,
) -> (SocketAddr, JoinHandle<anyhow::Result<()>>, Arc<Metrics>) {
    let backend_config = BackendConfig {
        host: backend.ip().to_string(),
        port: backend.port(),
        ..Default::default()
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range,
    };
    let metrics = Metrics::new();
    let for_session = Arc::clone(&metrics);

    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, peer_addr) = gateway_listener.accept().await.unwrap();
        session::handle(
            stream,
            peer_addr,
            1,
            backend_config,
            passive_config,
            timeouts,
            LimitsConfig::default(),
            PortManager::new(port_range),
            for_session,
            DnsCache::new(),
            None,
            None,
        )
        .await
    });
    (gateway_addr, task, metrics)
}

#[tokio::test]
async fn a_completed_upload_is_counted_as_started_and_completed() {
    let backend = common::spawn_mock_backend().await;
    let (gateway, task, metrics) = spawn_session(
        backend.addr,
        PortRange {
            start: 19400,
            end: 19404,
        },
        TimeoutConfig::default(),
    )
    .await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    common::login(&mut client).await;
    let reply = common::perform_upload(&mut client, "ok.bin", b"twelve bytes").await;
    assert!(reply.starts_with("226"), "{reply}");

    assert_eq!(value(&metrics, "ftp_gateway_uploads_started_total"), 1);
    assert_eq!(value(&metrics, "ftp_gateway_uploads_completed_total"), 1);
    assert_eq!(value(&metrics, "ftp_gateway_uploads_failed_total"), 0);
    assert_eq!(value(&metrics, "ftp_gateway_upload_bytes_total"), 12);
    assert_eq!(
        value(&metrics, "ftp_gateway_backend_pasv_failures_total"),
        0
    );

    drop(client);
    let _ = await_session(task).await;
}

#[tokio::test]
async fn an_upload_the_backend_drops_midway_is_counted_as_failed() {
    let backend = common::spawn_mock_backend_disconnecting_during_transfer().await;
    let (gateway, task, metrics) = spawn_session(
        backend,
        PortRange {
            start: 19405,
            end: 19409,
        },
        TimeoutConfig::default(),
    )
    .await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    common::login(&mut client).await;
    client.write_all(b"PASV\r\n").await.unwrap();
    let pasv = common::read_reply(&mut client).await;
    let (_, port) = parse_pasv_reply(&pasv).unwrap();
    let mut data = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    client.write_all(b"STOR midway.bin\r\n").await.unwrap();
    assert!(common::read_reply(&mut client).await.starts_with("150"));
    let _ = data.write_all(b"this will not finish").await;
    assert!(common::read_reply(&mut client).await.starts_with("426"));
    let _ = await_session(task).await;

    assert_eq!(value(&metrics, "ftp_gateway_uploads_started_total"), 1);
    assert_eq!(value(&metrics, "ftp_gateway_uploads_completed_total"), 0);
    assert_eq!(value(&metrics, "ftp_gateway_uploads_failed_total"), 1);
}

#[tokio::test]
async fn a_backend_that_stops_answering_pasv_is_counted_as_a_pasv_failure_and_a_timeout() {
    // A backend that logs in, then never answers PASV.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
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
                _ => tokio::time::sleep(Duration::from_secs(60)).await,
            }
        }
    });

    let (gateway, task, metrics) = spawn_session(
        backend,
        PortRange {
            start: 19410,
            end: 19414,
        },
        TimeoutConfig {
            command_timeout_secs: 1,
            ..TimeoutConfig::default()
        },
    )
    .await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    common::login(&mut client).await;
    client.write_all(b"PASV\r\n").await.unwrap();
    let pasv = common::read_reply(&mut client).await;
    let (_, port) = parse_pasv_reply(&pasv).unwrap();
    let _data = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    client.write_all(b"STOR hang.bin\r\n").await.unwrap();
    assert!(common::read_reply(&mut client).await.starts_with("421"));
    let _ = await_session(task).await;

    assert_eq!(
        value(&metrics, "ftp_gateway_backend_pasv_failures_total"),
        1
    );
    assert_eq!(value(&metrics, "ftp_gateway_backend_timeouts_total"), 1);
    // The upload never got as far as reaching the backend, so it is neither started nor failed.
    assert_eq!(value(&metrics, "ftp_gateway_uploads_started_total"), 0);
    assert_eq!(value(&metrics, "ftp_gateway_uploads_failed_total"), 0);
}

#[tokio::test]
async fn an_idle_session_is_counted_when_it_times_out() {
    let backend = common::spawn_mock_backend().await;
    let (gateway, task, metrics) = spawn_session(
        backend.addr,
        PortRange {
            start: 19415,
            end: 19419,
        },
        TimeoutConfig {
            idle_timeout_secs: 1,
            ..TimeoutConfig::default()
        },
    )
    .await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    common::login(&mut client).await;
    // Say nothing: the gateway closes the control connection after the idle timeout.
    let reply = tokio::time::timeout(Duration::from_secs(10), common::read_reply(&mut client))
        .await
        .expect("the session should time out on its own");
    assert!(reply.starts_with("421"), "{reply}");
    let _ = await_session(task).await;

    assert_eq!(
        value(&metrics, "ftp_gateway_session_idle_timeouts_total"),
        1
    );
}
