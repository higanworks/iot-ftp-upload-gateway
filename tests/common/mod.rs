//! A minimal in-process FTP server standing in for a real backend in integration tests.
//! Supports just enough of the protocol for the gateway to relay a `STOR` through it:
//! `USER`/`PASS`/`PWD`/`TYPE`/`PASV`/`STOR`/`QUIT`. Not exported by the library; this lives in
//! `tests/` only, shared across integration test files via the `tests/common/mod.rs`
//! convention (a subdirectory so cargo doesn't treat it as its own test binary).
//!
//! `tests/common/mod.rs` is compiled separately for each integration test binary that includes
//! `mod common;`, so a helper unused by one particular test file still needs to compile clean
//! there -- hence the blanket `dead_code` allow rather than per-item ones.
#![allow(dead_code)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iot_ftp_upload_gateway::config::{BackendConfig, PassiveConfig, TimeoutConfig};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::protocol::reply::{parse_pasv_reply, pasv_reply};
use iot_ftp_upload_gateway::server::session;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Files "uploaded" to a mock backend so far: `(filename, content)`.
pub type UploadedFiles = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

pub struct MockBackend {
    pub addr: SocketAddr,
    pub uploads: UploadedFiles,
}

/// Starts a mock backend on an ephemeral port and returns immediately; it keeps accepting
/// connections in the background for the lifetime of the test process.
pub async fn spawn_mock_backend() -> MockBackend {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let uploads: UploadedFiles = Arc::new(Mutex::new(Vec::new()));

    let uploads_for_task = uploads.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(handle_control(stream, uploads_for_task.clone()));
        }
    });

    MockBackend { addr, uploads }
}

async fn handle_control(stream: TcpStream, uploads: UploadedFiles) {
    let mut conn = BufReader::new(stream);
    if conn.write_all(b"220 Mock backend ready\r\n").await.is_err() {
        return;
    }

    let mut pending_data: Option<TcpListener> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let Ok(bytes_read) = conn.read_line(&mut line).await else {
            break;
        };
        if bytes_read == 0 {
            break;
        }

        let trimmed = line.trim();
        let upper = trimmed.to_ascii_uppercase();

        if upper.starts_with("USER") {
            let _ = conn.write_all(b"331 Password required\r\n").await;
        } else if upper.starts_with("PASS") {
            let _ = conn.write_all(b"230 Logged in\r\n").await;
        } else if upper.starts_with("PWD") {
            let _ = conn.write_all(b"257 \"/\"\r\n").await;
        } else if upper.starts_with("TYPE") {
            let _ = conn.write_all(b"200 Type set\r\n").await;
        } else if upper.starts_with("PASV") {
            let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let data_port = data_listener.local_addr().unwrap().port();
            pending_data = Some(data_listener);
            let reply = pasv_reply(Ipv4Addr::LOCALHOST, data_port);
            let _ = conn.write_all(reply.as_bytes()).await;
        } else if upper.starts_with("STOR") {
            let filename = trimmed.get(5..).unwrap_or_default().to_string();
            let _ = conn.write_all(b"150 Opening data connection\r\n").await;
            if let Some(data_listener) = pending_data.take()
                && let Ok((mut data_stream, _)) = data_listener.accept().await
            {
                let mut buf = Vec::new();
                let _ = data_stream.read_to_end(&mut buf).await;
                uploads.lock().unwrap().push((filename, buf));
            }
            let _ = conn.write_all(b"226 Transfer complete\r\n").await;
        } else if upper.starts_with("QUIT") {
            let _ = conn.write_all(b"221 Goodbye\r\n").await;
            break;
        } else {
            let _ = conn.write_all(b"502 Command not implemented\r\n").await;
        }
    }
}

/// Connects to `addr`, retrying briefly -- used right after spawning a gateway/listener task,
/// since binding the listener happens asynchronously and racing a bare `connect` against it
/// would make the test flaky.
pub async fn connect_with_retry(addr: SocketAddr) -> TcpStream {
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(addr).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("failed to connect to {addr} after retrying for 1s");
}

/// Reads one line (a single FTP reply) from the control connection.
pub async fn read_reply(conn: &mut BufReader<TcpStream>) -> String {
    let mut line = String::new();
    conn.read_line(&mut line).await.unwrap();
    line
}

/// Drives the USER/PASS handshake an IoT client performs right after connecting.
pub async fn login(conn: &mut BufReader<TcpStream>) {
    assert!(read_reply(conn).await.starts_with("220"));
    conn.write_all(b"USER iot\r\n").await.unwrap();
    assert!(read_reply(conn).await.starts_with("331"));
    conn.write_all(b"PASS secret\r\n").await.unwrap();
    assert!(read_reply(conn).await.starts_with("230"));
}

/// Drives PASV -> STOR -> data write -> completion over an already-logged-in control
/// connection and returns the final reply line (expected to start with "226" on success).
pub async fn perform_upload(
    conn: &mut BufReader<TcpStream>,
    filename: &str,
    payload: &[u8],
) -> String {
    conn.write_all(b"PASV\r\n").await.unwrap();
    let pasv_reply_line = read_reply(conn).await;
    assert!(pasv_reply_line.starts_with("227"), "{pasv_reply_line}");
    let (_, port) = parse_pasv_reply(&pasv_reply_line).unwrap();

    let mut data_conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    conn.write_all(format!("STOR {filename}\r\n").as_bytes())
        .await
        .unwrap();
    let stor_reply = read_reply(conn).await;
    assert!(stor_reply.starts_with("150"), "{stor_reply}");

    data_conn.write_all(payload).await.unwrap();
    data_conn.shutdown().await.unwrap();

    read_reply(conn).await
}

/// Spawns a session against the given backend/passive config, accepting a single client
/// connection on an ephemeral port. Returns the address to connect to as the client and a
/// handle to the session task, so a test can wait for it and assert it didn't panic.
pub async fn spawn_session(
    backend_config: BackendConfig,
    passive_config: PassiveConfig,
    timeouts: TimeoutConfig,
    port_manager: PortManager,
) -> (SocketAddr, JoinHandle<anyhow::Result<()>>) {
    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let (stream, peer_addr) = gateway_listener.accept().await.unwrap();
        session::handle(
            stream,
            peer_addr,
            1,
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
pub async fn await_session(task: JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("session task hung instead of finishing")
        .expect("session task panicked")
}

/// Returns an address nothing is listening on, for simulating a backend that refuses
/// connections outright: binds an ephemeral port and immediately drops the listener, freeing
/// the port before returning.
pub async fn unused_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

/// Spawns a mock backend that accepts a connection and immediately closes it without sending
/// anything -- simulates a backend that is completely unreachable/crashed right as the gateway
/// connects, after the TCP handshake but before any FTP protocol exchange.
pub async fn spawn_mock_backend_closing_immediately() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            drop(stream);
        }
    });
    addr
}

/// Spawns a mock backend that serves `USER`/`PASS` normally, then closes the connection as soon
/// as it receives the next command -- simulates a backend that crashes right after login,
/// before the client's first `PASV`.
pub async fn spawn_mock_backend_disconnecting_after_login() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut conn = BufReader::new(stream);
                if conn.write_all(b"220 Mock backend ready\r\n").await.is_err() {
                    return;
                }
                let mut line = String::new();
                for expected_reply in [
                    b"331 Password required\r\n".as_slice(),
                    b"230 Logged in\r\n",
                ] {
                    line.clear();
                    if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if conn.write_all(expected_reply).await.is_err() {
                        return;
                    }
                }
                // "Crashes" right after login: `conn` drops here, closing the connection
                // without reading or responding to anything the client sends next.
            });
        }
    });
    addr
}

/// Spawns a mock backend that serves the full login + PASV handshake normally, but once a
/// `STOR` arrives, reads only a few bytes off the data connection before dropping both the
/// data and control connections without ever sending a completion reply -- simulating the
/// whole backend process crashing partway through a transfer.
pub async fn spawn_mock_backend_disconnecting_during_transfer() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut conn = BufReader::new(stream);
                if conn.write_all(b"220 Mock backend ready\r\n").await.is_err() {
                    return;
                }

                let mut pending_data: Option<TcpListener> = None;
                let mut line = String::new();

                loop {
                    line.clear();
                    let Ok(bytes_read) = conn.read_line(&mut line).await else {
                        return;
                    };
                    if bytes_read == 0 {
                        return;
                    }

                    let upper = line.trim().to_ascii_uppercase();

                    if upper.starts_with("USER") {
                        let _ = conn.write_all(b"331 Password required\r\n").await;
                    } else if upper.starts_with("PASS") {
                        let _ = conn.write_all(b"230 Logged in\r\n").await;
                    } else if upper.starts_with("PWD") {
                        let _ = conn.write_all(b"257 \"/\"\r\n").await;
                    } else if upper.starts_with("TYPE") {
                        let _ = conn.write_all(b"200 Type set\r\n").await;
                    } else if upper.starts_with("PASV") {
                        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let data_port = data_listener.local_addr().unwrap().port();
                        pending_data = Some(data_listener);
                        let reply = pasv_reply(Ipv4Addr::LOCALHOST, data_port);
                        let _ = conn.write_all(reply.as_bytes()).await;
                    } else if upper.starts_with("STOR") {
                        let _ = conn.write_all(b"150 Opening data connection\r\n").await;
                        if let Some(data_listener) = pending_data.take()
                            && let Ok((mut data_stream, _)) = data_listener.accept().await
                        {
                            let mut buf = [0u8; 4];
                            let _ = data_stream.read(&mut buf).await;
                            // The backend "crashes" here: `data_stream` and `conn` both drop at
                            // the end of this task without a completion reply ever being sent.
                        }
                        return;
                    } else {
                        let _ = conn.write_all(b"502 Command not implemented\r\n").await;
                    }
                }
            });
        }
    });
    addr
}
