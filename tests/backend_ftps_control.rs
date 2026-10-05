//! Integration tests for the control-connection half of backend FTPS: the gateway upgrades its
//! connection to the backend with `AUTH TLS` / `PBSZ 0` / `PROT P` while the client keeps
//! speaking plain FTP, and fails closed (`421`, never plain FTP) when that can't be done.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

mod common;

use common::tls::{TestPki, test_pki};
use iot_ftp_upload_gateway::backend::dns_cache::DnsCache;
use iot_ftp_upload_gateway::backend::tls::BackendTlsConnector;
use iot_ftp_upload_gateway::config::{
    BackendConfig, BackendTlsMaxVersion, LimitsConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::server::session;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn connector_trusting(ca_pem: &str, server_name: Option<&str>, tag: &str) -> BackendTlsConnector {
    common::tls::connector_trusting(ca_pem, server_name, BackendTlsMaxVersion::V1_3, tag)
}

#[derive(Clone, Copy)]
enum Behavior {
    /// Speaks Explicit FTPS correctly.
    Good,
    /// Answers `AUTH TLS` with an error.
    RefuseAuth,
    /// Sends extra plaintext right behind its `234`.
    InjectAfterAuth,
}

type Commands = Arc<Mutex<Vec<String>>>;

async fn read_command<S: AsyncReadExt + Unpin>(conn: &mut BufReader<S>) -> Option<String> {
    let mut line = String::new();
    match conn.read_line(&mut line).await {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end().to_string()),
    }
}

/// A one-connection mock backend. Records every command line it receives (plain or TLS).
async fn spawn_ftps_backend(pki: &TestPki, behavior: Behavior) -> (SocketAddr, Commands) {
    let acceptor = common::tls::acceptor_for(pki);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let commands: Commands = Arc::new(Mutex::new(Vec::new()));

    let recorded = commands.clone();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut plain = BufReader::new(tcp);
        plain.write_all(b"220 Mock FTPS backend\r\n").await.unwrap();

        let Some(first) = read_command(&mut plain).await else {
            return;
        };
        recorded.lock().unwrap().push(first);

        match behavior {
            Behavior::Good => plain.write_all(b"234 AUTH TLS ok\r\n").await.unwrap(),
            Behavior::RefuseAuth => {
                plain.write_all(b"500 no TLS here\r\n").await.unwrap();
                while let Some(cmd) = read_command(&mut plain).await {
                    recorded.lock().unwrap().push(cmd);
                }
                return;
            }
            Behavior::InjectAfterAuth => {
                plain
                    .write_all(b"234 AUTH TLS ok\r\n200 injected\r\n")
                    .await
                    .unwrap();
                while read_command(&mut plain).await.is_some() {}
                return;
            }
        }

        let Ok(tls) = acceptor.accept(plain.into_inner()).await else {
            return;
        };
        let mut conn = BufReader::new(tls);
        while let Some(cmd) = read_command(&mut conn).await {
            recorded.lock().unwrap().push(cmd.clone());
            let reply: &[u8] = match cmd.split(' ').next().unwrap_or("") {
                "PBSZ" | "PROT" => b"200 ok\r\n",
                "USER" => b"331 need password\r\n",
                "QUIT" => {
                    let _ = conn.write_all(b"221 bye\r\n").await;
                    break;
                }
                _ => b"500 unknown\r\n",
            };
            conn.write_all(reply).await.unwrap();
        }
    });
    (addr, commands)
}

/// Hands one incoming client connection to the real `session::handle`, with backend TLS on.
async fn spawn_session(backend: SocketAddr, connector: BackendTlsConnector) -> SocketAddr {
    let backend_config = BackendConfig {
        host: backend.ip().to_string(),
        port: backend.port(),
        ..Default::default()
    };
    let port_range = PortRange {
        start: 19850,
        end: 19860,
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
            None,
        )
        .await
        .unwrap();
    });
    gateway_addr
}

async fn expect_line(client: &mut BufReader<TcpStream>) -> String {
    let mut line = String::new();
    client.read_line(&mut line).await.unwrap();
    line
}

#[tokio::test]
async fn plain_client_is_served_through_a_tls_backend_control_connection() {
    let pki = test_pki("127.0.0.1");
    let (backend, commands) = spawn_ftps_backend(&pki, Behavior::Good).await;
    let gateway = spawn_session(backend, connector_trusting(&pki.ca_pem, None, "ok")).await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    assert!(expect_line(&mut client).await.starts_with("220"));

    client.write_all(b"USER iot\r\n").await.unwrap();
    assert!(expect_line(&mut client).await.starts_with("331"));
    client.write_all(b"QUIT\r\n").await.unwrap();
    assert!(expect_line(&mut client).await.starts_with("221"));

    // The client never sent AUTH/PBSZ/PROT; the gateway did, ahead of everything else.
    assert_eq!(
        *commands.lock().unwrap(),
        ["AUTH TLS", "PBSZ 0", "PROT P", "USER iot", "QUIT"]
    );
}

#[tokio::test]
async fn backend_refusing_auth_tls_ends_the_session_without_plain_fallback() {
    let pki = test_pki("127.0.0.1");
    let (backend, commands) = spawn_ftps_backend(&pki, Behavior::RefuseAuth).await;
    let gateway = spawn_session(backend, connector_trusting(&pki.ca_pem, None, "refuse")).await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    // No `220` banner is passed on for a session that could not be secured.
    assert!(expect_line(&mut client).await.starts_with("421"));
    let _ = client.write_all(b"USER iot\r\n").await;
    assert_eq!(expect_line(&mut client).await, "");

    assert_eq!(*commands.lock().unwrap(), ["AUTH TLS"]);
}

#[tokio::test]
async fn certificate_for_the_wrong_name_ends_the_session() {
    let pki = test_pki("somewhere-else.example.com");
    let (backend, commands) = spawn_ftps_backend(&pki, Behavior::Good).await;
    let gateway = spawn_session(backend, connector_trusting(&pki.ca_pem, None, "name")).await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    assert!(expect_line(&mut client).await.starts_with("421"));
    assert_eq!(*commands.lock().unwrap(), ["AUTH TLS"]);
}

#[tokio::test]
async fn plaintext_injected_behind_the_234_ends_the_session() {
    let pki = test_pki("127.0.0.1");
    let (backend, _commands) = spawn_ftps_backend(&pki, Behavior::InjectAfterAuth).await;
    let gateway = spawn_session(backend, connector_trusting(&pki.ca_pem, None, "inject")).await;

    let mut client = BufReader::new(TcpStream::connect(gateway).await.unwrap());
    assert!(expect_line(&mut client).await.starts_with("421"));
}
