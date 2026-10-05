//! Integration tests for backend source-address rotation: the gateway connects to the backend
//! from a rotating set of local addresses, keeps one session on one address (control and data
//! connections alike), and caps concurrent data transfers per address.
//!
//! The mock backend records the peer address of every control and data connection it accepts.
//! Telling two source addresses apart needs two local addresses; Linux treats all of
//! 127.0.0.0/8 as local, so those tests are Linux-only. The rest run anywhere.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iot_ftp_upload_gateway::backend::dns_cache::DnsCache;
use iot_ftp_upload_gateway::backend::rotator::SourceRotator;
use iot_ftp_upload_gateway::config::{
    BackendConfig, LimitsConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::protocol::reply::pasv_reply;
use iot_ftp_upload_gateway::server::session;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct Observed {
    control_peers: Vec<IpAddr>,
    data_peers: Vec<IpAddr>,
}

type SharedObserved = Arc<Mutex<Observed>>;

/// A minimal FTP backend that records who connected to it. Reads each upload to EOF.
async fn spawn_recording_backend() -> (SocketAddr, SharedObserved) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed: SharedObserved = Arc::default();

    let shared = observed.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                break;
            };
            shared.lock().unwrap().control_peers.push(peer.ip());
            tokio::spawn(handle_control(stream, shared.clone()));
        }
    });
    (addr, observed)
}

async fn handle_control(stream: TcpStream, observed: SharedObserved) {
    let mut conn = BufReader::new(stream);
    let _ = conn.write_all(b"220 Recording backend\r\n").await;
    let mut pending_data: Option<TcpListener> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
        }
        let verb = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        match verb.as_str() {
            "USER" => conn.write_all(b"331 password\r\n").await.unwrap(),
            "PASS" => conn.write_all(b"230 in\r\n").await.unwrap(),
            "PASV" => {
                let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = data_listener.local_addr().unwrap().port();
                pending_data = Some(data_listener);
                let reply = pasv_reply(Ipv4Addr::LOCALHOST, port);
                conn.write_all(reply.as_bytes()).await.unwrap();
            }
            "STOR" => {
                conn.write_all(b"150 go\r\n").await.unwrap();
                let data_listener = pending_data.take().unwrap();
                let (mut data, peer) = data_listener.accept().await.unwrap();
                observed.lock().unwrap().data_peers.push(peer.ip());
                let mut content = Vec::new();
                let _ = data.read_to_end(&mut content).await;
                conn.write_all(b"226 done\r\n").await.unwrap();
            }
            "QUIT" => {
                let _ = conn.write_all(b"221 bye\r\n").await;
                break;
            }
            _ => conn.write_all(b"500 ?\r\n").await.unwrap(),
        }
    }
}

struct Gateway {
    addr: SocketAddr,
    backend_key: String,
}

/// Runs the real `session::handle` for every incoming connection, as `listener::run` does,
/// sharing one `PortManager`, `DnsCache` and `SourceRotator` across sessions.
async fn spawn_gateway(
    backend: SocketAddr,
    passive_ports: PortRange,
    rotator: SourceRotator,
    pasv_range: PortRange,
    connection_timeout_secs: u64,
) -> Gateway {
    let backend_config = BackendConfig {
        host: backend.ip().to_string(),
        port: backend.port(),
        passive_ports,
    };
    let backend_key = format!("{}:{}", backend_config.host, backend_config.port);
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: pasv_range,
    };
    let timeouts = TimeoutConfig {
        connection_timeout_secs,
        ..TimeoutConfig::default()
    };
    let port_manager = PortManager::new(pasv_range);
    let dns_cache = DnsCache::new();
    let metrics = iot_ftp_upload_gateway::metrics::Metrics::new();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut session_id = 0;
        loop {
            let Ok((stream, peer_addr)) = listener.accept().await else {
                break;
            };
            session_id += 1;
            let backend_config = backend_config.clone();
            let port_manager = port_manager.clone();
            let metrics = metrics.clone();
            let dns_cache = dns_cache.clone();
            let rotator = rotator.clone();
            tokio::spawn(async move {
                let _ = session::handle(
                    stream,
                    peer_addr,
                    session_id,
                    backend_config,
                    passive_config,
                    timeouts,
                    LimitsConfig::default(),
                    port_manager,
                    metrics,
                    dns_cache,
                    None,
                    Some(rotator),
                )
                .await;
            });
        }
    });
    Gateway { addr, backend_key }
}

fn ports(first: u16, last: u16) -> PortRange {
    PortRange {
        start: first,
        end: last,
    }
}

async fn connect_and_login(gateway: &Gateway) -> BufReader<TcpStream> {
    let mut client = BufReader::new(TcpStream::connect(gateway.addr).await.unwrap());
    common::login(&mut client).await;
    client
}

#[cfg(target_os = "linux")]
fn loopback(last: u8) -> Ipv4Addr {
    Ipv4Addr::new(127, 0, 0, last)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn sessions_rotate_across_sources_once_the_port_range_has_cycled() {
    let (backend, observed) = spawn_recording_backend().await;
    // Two data ports per source: the current source advances every 2 data connections.
    let gateway = spawn_gateway(
        backend,
        ports(8192, 8193),
        SourceRotator::new(vec![loopback(2), loopback(3)]),
        ports(19920, 19929),
        5,
    )
    .await;

    for i in 0..5 {
        let mut client = connect_and_login(&gateway).await;
        let reply = common::perform_upload(&mut client, &format!("f{i}.bin"), b"data").await;
        assert!(reply.starts_with("226"), "{reply}");
    }

    let observed = observed.lock().unwrap();
    let expected: Vec<IpAddr> = [2, 2, 3, 3, 2].map(|n| IpAddr::V4(loopback(n))).to_vec();
    assert_eq!(observed.control_peers, expected);
    // Each session's data connection came from the same address as its control connection.
    assert_eq!(observed.data_peers, expected);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_session_keeps_its_source_even_after_the_current_one_moves_on() {
    let (backend, observed) = spawn_recording_backend().await;
    // One data port per source: the current source advances after every data connection.
    let gateway = spawn_gateway(
        backend,
        ports(8192, 8192),
        SourceRotator::new(vec![loopback(2), loopback(3)]),
        ports(19930, 19939),
        5,
    )
    .await;

    let mut first = connect_and_login(&gateway).await;
    for i in 0..3 {
        let reply = common::perform_upload(&mut first, &format!("f{i}.bin"), b"data").await;
        assert!(reply.starts_with("226"), "{reply}");
    }
    // Meanwhile new sessions are handed the next source.
    let second = connect_and_login(&gateway).await;
    drop(second);

    let observed = observed.lock().unwrap();
    let first_source = IpAddr::V4(loopback(2));
    assert_eq!(observed.data_peers, [first_source; 3]);
    assert_eq!(
        observed.control_peers,
        [first_source, IpAddr::V4(loopback(3))],
        "the first session stays on .2; the next one starts on .3"
    );
}

/// The platform-independent counterpart of the Linux-only tests above: instead of telling source
/// addresses apart by what the backend sees, it watches the rotator's "current" source, which the
/// session moves by one data connection per upload and by the backend's port-range size.
#[tokio::test]
async fn the_current_source_advances_after_a_full_port_range_of_uploads() {
    let (backend, _observed) = spawn_recording_backend().await;
    let first = Ipv4Addr::LOCALHOST;
    let second = Ipv4Addr::new(127, 0, 0, 2); // sorts after `first`; never connected from here
    let rotator = SourceRotator::new(vec![first, second]);
    let gateway = spawn_gateway(
        backend,
        ports(8192, 8193),
        rotator.clone(),
        ports(19970, 19979),
        5,
    )
    .await;
    assert_eq!(rotator.candidates(&gateway.backend_key)[0], first);

    // Two data ports: the first upload leaves the current source alone, the second moves it on.
    let mut client = connect_and_login(&gateway).await;
    assert!(
        common::perform_upload(&mut client, "one.bin", b"data")
            .await
            .starts_with("226")
    );
    assert_eq!(rotator.candidates(&gateway.backend_key)[0], first);
    assert!(
        common::perform_upload(&mut client, "two.bin", b"data")
            .await
            .starts_with("226")
    );
    assert_eq!(rotator.candidates(&gateway.backend_key)[0], second);
}

#[tokio::test]
async fn transfers_beyond_a_sources_capacity_wait_and_then_fail_with_425() {
    let (backend, observed) = spawn_recording_backend().await;
    // One data port: one transfer at a time from the only source.
    let gateway = spawn_gateway(
        backend,
        ports(8192, 8192),
        SourceRotator::new(vec![Ipv4Addr::LOCALHOST]),
        ports(19940, 19949),
        1,
    )
    .await;

    // Session A starts an upload and leaves it open (no EOF on the data connection).
    let mut a = connect_and_login(&gateway).await;
    a.write_all(b"PASV\r\n").await.unwrap();
    let pasv = common::read_reply(&mut a).await;
    let (_, port) = iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply(&pasv).unwrap();
    let mut a_data = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    a.write_all(b"STOR a.bin\r\n").await.unwrap();
    assert!(common::read_reply(&mut a).await.starts_with("150"));
    a_data.write_all(b"partial").await.unwrap();

    // Session B asks for a transfer while A holds the only slot: it waits out
    // `connection_timeout_secs` and is told the data connection can't be opened.
    let mut b = connect_and_login(&gateway).await;
    b.write_all(b"PASV\r\n").await.unwrap();
    let pasv = common::read_reply(&mut b).await;
    let (_, port) = iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply(&pasv).unwrap();
    let _b_data = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    b.write_all(b"STOR b.bin\r\n").await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), common::read_reply(&mut b))
        .await
        .expect("session B should be answered once its wait times out");
    assert!(reply.starts_with("425"), "{reply}");
    assert_eq!(
        observed.lock().unwrap().data_peers.len(),
        1,
        "B never reached the backend"
    );

    // A finishes, which frees the slot: a new transfer now goes through.
    a_data.shutdown().await.unwrap();
    assert!(common::read_reply(&mut a).await.starts_with("226"));
    let mut c = connect_and_login(&gateway).await;
    let reply = common::perform_upload(&mut c, "c.bin", b"data").await;
    assert!(reply.starts_with("226"), "{reply}");
}

#[tokio::test]
async fn a_source_that_cannot_be_bound_is_skipped_and_marked_unhealthy() {
    let (backend, observed) = spawn_recording_backend().await;
    // 1.2.3.4 sorts before 127.0.0.1, so it is tried first; no host has it assigned, so
    // binding to it fails.
    let unusable = Ipv4Addr::new(1, 2, 3, 4);
    let rotator = SourceRotator::new(vec![unusable, Ipv4Addr::LOCALHOST]);
    let gateway = spawn_gateway(
        backend,
        ports(8192, 8200),
        rotator.clone(),
        ports(19950, 19959),
        5,
    )
    .await;
    assert_eq!(rotator.candidates(&gateway.backend_key)[0], unusable);

    let mut client = connect_and_login(&gateway).await;
    let reply = common::perform_upload(&mut client, "f.bin", b"data").await;
    assert!(reply.starts_with("226"), "{reply}");

    let observed = observed.lock().unwrap();
    assert_eq!(observed.control_peers, [IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    assert_eq!(
        rotator.candidates(&gateway.backend_key),
        [Ipv4Addr::LOCALHOST],
        "the unusable source is left out of the rotation"
    );
}

#[tokio::test]
async fn when_no_source_can_connect_the_client_gets_421() {
    let (backend, _observed) = spawn_recording_backend().await;
    let gateway = spawn_gateway(
        backend,
        ports(8192, 8200),
        SourceRotator::new(vec![Ipv4Addr::new(1, 2, 3, 4)]),
        ports(19960, 19969),
        5,
    )
    .await;

    let mut client = BufReader::new(TcpStream::connect(gateway.addr).await.unwrap());
    assert!(common::read_reply(&mut client).await.starts_with("421"));
}
