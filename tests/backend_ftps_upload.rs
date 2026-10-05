//! End-to-end upload through backend FTPS: a plain-FTP client uploads via the gateway, and the
//! gateway talks Explicit FTPS (`PROT P`) to the backend on both the control and data
//! connections. The mock backend, like vsftpd's default `require_ssl_reuse=YES`, refuses a data
//! connection that does not resume the control connection's TLS session.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use common::tls::{TestPki, acceptor_for, connector_trusting, test_pki};
use iot_ftp_upload_gateway::backend::dns_cache::DnsCache;
use iot_ftp_upload_gateway::config::{
    BackendConfig, BackendTlsMaxVersion, LimitsConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::protocol::reply::pasv_reply;
use iot_ftp_upload_gateway::server::session;
use rustls::HandshakeKind;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct Observed {
    /// `(filename, content)` of each completed upload.
    uploads: Vec<(String, Vec<u8>)>,
    /// How each data connection's TLS handshake was performed.
    data_handshakes: Vec<HandshakeKind>,
}

type SharedObserved = Arc<Mutex<Observed>>;

struct MockOptions {
    /// Close the data connection with a bare TCP close instead of a TLS `close_notify`.
    skip_close_notify: bool,
}

async fn spawn_ftps_backend(pki: &TestPki, options: MockOptions) -> (SocketAddr, SharedObserved) {
    let acceptor = acceptor_for(pki);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed: SharedObserved = Arc::default();

    let shared = observed.clone();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut plain = BufReader::new(tcp);
        plain.write_all(b"220 Mock FTPS backend\r\n").await.unwrap();
        let mut line = String::new();
        plain.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim_end(), "AUTH TLS");
        plain.write_all(b"234 AUTH TLS ok\r\n").await.unwrap();

        let mut conn = BufReader::new(acceptor.accept(plain.into_inner()).await.unwrap());
        let mut pending_data: Option<TcpListener> = None;
        loop {
            line.clear();
            if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let cmd = line.trim_end().to_string();
            match cmd.split(' ').next().unwrap_or("") {
                "PBSZ" | "PROT" => conn.write_all(b"200 ok\r\n").await.unwrap(),
                "USER" => conn.write_all(b"331 need password\r\n").await.unwrap(),
                "PASS" => conn.write_all(b"230 logged in\r\n").await.unwrap(),
                "PASV" => {
                    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = data_listener.local_addr().unwrap().port();
                    pending_data = Some(data_listener);
                    let reply = pasv_reply(Ipv4Addr::LOCALHOST, port);
                    conn.write_all(reply.as_bytes()).await.unwrap();
                }
                "STOR" => {
                    let filename = cmd[5..].to_string();
                    if filename == "refused.txt" {
                        // A refusal comes with no data-connection handshake and no later reply.
                        conn.write_all(b"550 not allowed\r\n").await.unwrap();
                        continue;
                    }
                    conn.write_all(b"150 opening data connection\r\n")
                        .await
                        .unwrap();
                    let data_listener = pending_data.take().unwrap();
                    let (data_tcp, _) = data_listener.accept().await.unwrap();
                    // A plaintext peer fails this handshake, so reaching the read below proves
                    // the data connection really was TLS.
                    let Ok(mut data) = acceptor.accept(data_tcp).await else {
                        conn.write_all(b"425 TLS required on data connection\r\n")
                            .await
                            .unwrap();
                        continue;
                    };
                    let kind = data.get_ref().1.handshake_kind().unwrap();
                    shared.lock().unwrap().data_handshakes.push(kind);
                    if kind != HandshakeKind::Resumed {
                        conn.write_all(b"522 TLS session reuse required\r\n")
                            .await
                            .unwrap();
                        continue;
                    }
                    let mut content = Vec::new();
                    // A client that closes cleanly sends close_notify; either way EOF ends it.
                    let _ = data.read_to_end(&mut content).await;
                    if options.skip_close_notify {
                        drop(data);
                    } else {
                        let _ = data.shutdown().await;
                    }
                    shared.lock().unwrap().uploads.push((filename, content));
                    conn.write_all(b"226 transfer complete\r\n").await.unwrap();
                }
                "QUIT" => {
                    let _ = conn.write_all(b"221 bye\r\n").await;
                    break;
                }
                _ => conn.write_all(b"500 unknown\r\n").await.unwrap(),
            }
        }
    });
    (addr, observed)
}

/// Hands one incoming client connection to the real `session::handle`, with backend TLS on.
async fn spawn_session(
    backend: SocketAddr,
    pki: &TestPki,
    max_version: BackendTlsMaxVersion,
    port_range: PortRange,
    tag: &str,
) -> SocketAddr {
    let connector = connector_trusting(&pki.ca_pem, None, max_version, tag);
    let backend_config = BackendConfig {
        host: backend.ip().to_string(),
        port: backend.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range,
    };
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
            TimeoutConfig::default(),
            LimitsConfig::default(),
            PortManager::new(port_range),
            iot_ftp_upload_gateway::metrics::Metrics::new(),
            DnsCache::new(),
            Some(connector),
        )
        .await
        .unwrap();
    });
    gateway_addr
}

async fn upload_through_gateway(
    max_version: BackendTlsMaxVersion,
    skip_close_notify: bool,
    port_range: PortRange,
    tag: &str,
) -> SharedObserved {
    let pki = test_pki("127.0.0.1");
    let (backend, observed) = spawn_ftps_backend(&pki, MockOptions { skip_close_notify }).await;
    let gateway = spawn_session(backend, &pki, max_version, port_range, tag).await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    common::login(&mut client).await;
    let payload = vec![b'x'; 300_000];
    let reply = common::perform_upload(&mut client, "big.bin", &payload).await;
    assert!(reply.starts_with("226"), "{reply}");

    {
        let observed = observed.lock().unwrap();
        assert_eq!(observed.uploads.len(), 1);
        assert_eq!(observed.uploads[0].0, "big.bin");
        assert_eq!(observed.uploads[0].1, payload);
        assert_eq!(observed.data_handshakes, [HandshakeKind::Resumed]);
    }
    observed
}

#[tokio::test]
async fn upload_over_tls13_resumes_the_control_session_on_the_data_connection() {
    upload_through_gateway(
        BackendTlsMaxVersion::V1_3,
        false,
        PortRange {
            start: 19870,
            end: 19875,
        },
        "tls13",
    )
    .await;
}

#[tokio::test]
async fn upload_over_tls12_resumes_the_control_session_on_the_data_connection() {
    upload_through_gateway(
        BackendTlsMaxVersion::V1_2,
        false,
        PortRange {
            start: 19876,
            end: 19880,
        },
        "tls12",
    )
    .await;
}

#[tokio::test]
async fn backend_closing_data_without_close_notify_still_completes_the_upload() {
    upload_through_gateway(
        BackendTlsMaxVersion::V1_3,
        true,
        PortRange {
            start: 19881,
            end: 19885,
        },
        "no-close-notify",
    )
    .await;
}

#[tokio::test]
async fn refused_stor_is_relayed_and_the_session_stays_usable() {
    let pki = test_pki("127.0.0.1");
    let (backend, observed) = spawn_ftps_backend(
        &pki,
        MockOptions {
            skip_close_notify: false,
        },
    )
    .await;
    let gateway = spawn_session(
        backend,
        &pki,
        BackendTlsMaxVersion::V1_3,
        PortRange {
            start: 19886,
            end: 19890,
        },
        "refused",
    )
    .await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    common::login(&mut client).await;

    client.write_all(b"PASV\r\n").await.unwrap();
    let pasv = common::read_reply(&mut client).await;
    let (_, port) = iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply(&pasv).unwrap();
    let _data = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    client.write_all(b"STOR refused.txt\r\n").await.unwrap();
    assert!(common::read_reply(&mut client).await.starts_with("550"));

    // The control connection is still in sync: the next command gets its own reply.
    client.write_all(b"QUIT\r\n").await.unwrap();
    assert!(common::read_reply(&mut client).await.starts_with("221"));

    let observed = observed.lock().unwrap();
    assert!(observed.uploads.is_empty());
    assert!(observed.data_handshakes.is_empty());
}
