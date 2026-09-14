//! PROJECT_SECURITY.md section 4/6/18: the gateway must not panic or let a client smuggle an
//! extra command past a single control line, and must not buffer an unbounded amount of memory
//! for a client that never terminates a line.

mod common;

use std::net::Ipv4Addr;

use common::{
    await_session, connect_with_retry, login, perform_upload, read_reply,
    spawn_session_with_limits,
};
use iot_ftp_upload_gateway::config::{
    BackendConfig, LimitsConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn stor_with_cr_in_filename_is_rejected_without_reaching_backend() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19950,
            end: 19950,
        },
    };
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) = spawn_session_with_limits(
        backend_config,
        passive_config,
        TimeoutConfig::default(),
        LimitsConfig::default(),
        port_manager,
    )
    .await;

    let client = connect_with_retry(gateway_addr).await;
    let mut conn = BufReader::new(client);
    login(&mut conn).await;

    conn.write_all(b"PASV\r\n").await.unwrap();
    let pasv_reply_line = read_reply(&mut conn).await;
    assert!(pasv_reply_line.starts_with("227"), "{pasv_reply_line}");

    let filename = "evil\rfile.txt";
    conn.write_all(format!("STOR {filename}\r\n").as_bytes())
        .await
        .unwrap();
    let reply = read_reply(&mut conn).await;
    assert!(
        reply.starts_with("501"),
        "embedded \\r in the STOR argument must be rejected outright: {reply}"
    );

    conn.write_all(b"QUIT\r\n").await.unwrap();
    let quit_reply = read_reply(&mut conn).await;
    assert!(quit_reply.starts_with("221"), "{quit_reply}");
    await_session(session_task).await.unwrap();

    let uploads = backend.uploads.lock().unwrap();
    assert!(
        uploads.is_empty(),
        "the rejected STOR line must never reach the backend: {uploads:?}"
    );
}

#[tokio::test]
async fn command_with_embedded_cr_does_not_smuggle_a_second_command() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19953,
            end: 19953,
        },
    };
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) = spawn_session_with_limits(
        backend_config,
        passive_config,
        TimeoutConfig::default(),
        LimitsConfig::default(),
        port_manager,
    )
    .await;

    let client = connect_with_retry(gateway_addr).await;
    let mut conn = BufReader::new(client);
    login(&mut conn).await;

    // A single control line whose only real "\n" is the last byte: read_line delivers it as one
    // line, and the embedded "\r" must not let the smuggled "QUIT" reach the backend.
    conn.write_all(b"CWD foo\rQUIT\r\n").await.unwrap();
    let reply = read_reply(&mut conn).await;
    assert!(reply.starts_with("501"), "{reply}");

    // If the smuggled QUIT had reached the backend, the session would already be over and this
    // PWD would go unanswered (or the connection would be closed).
    conn.write_all(b"PWD\r\n").await.unwrap();
    let pwd_reply = read_reply(&mut conn).await;
    assert!(pwd_reply.starts_with("257"), "{pwd_reply}");

    conn.write_all(b"QUIT\r\n").await.unwrap();
    let quit_reply = read_reply(&mut conn).await;
    assert!(quit_reply.starts_with("221"), "{quit_reply}");
    await_session(session_task).await.unwrap();
}

#[tokio::test]
async fn stor_with_backslash_in_filename_uploads_normally() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19954,
            end: 19954,
        },
    };
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) = spawn_session_with_limits(
        backend_config,
        passive_config,
        TimeoutConfig::default(),
        LimitsConfig::default(),
        port_manager,
    )
    .await;

    let client = connect_with_retry(gateway_addr).await;
    let mut conn = BufReader::new(client);
    login(&mut conn).await;

    // Backslash is not an FTP special character and must not be treated as one.
    let filename = r"back\slash\file.txt";
    let reply = perform_upload(&mut conn, filename, b"payload").await;
    assert!(reply.starts_with("226"), "{reply}");

    conn.write_all(b"QUIT\r\n").await.unwrap();
    await_session(session_task).await.unwrap();

    let uploads = backend.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert_eq!(uploads[0].0, filename);
    assert_eq!(uploads[0].1, b"payload");
}

#[tokio::test]
async fn stor_with_nul_byte_in_filename_uploads_without_crashing() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19951,
            end: 19951,
        },
    };
    let port_manager = PortManager::new(passive_config.port_range);

    let (gateway_addr, session_task) = spawn_session_with_limits(
        backend_config,
        passive_config,
        TimeoutConfig::default(),
        LimitsConfig::default(),
        port_manager,
    )
    .await;

    let client = connect_with_retry(gateway_addr).await;
    let mut conn = BufReader::new(client);
    login(&mut conn).await;

    let filename = "nul\0file.txt";
    let reply = perform_upload(&mut conn, filename, b"payload").await;
    assert!(reply.starts_with("226"), "{reply}");

    conn.write_all(b"QUIT\r\n").await.unwrap();
    await_session(session_task).await.unwrap();

    let uploads = backend.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert_eq!(uploads[0].0, filename);
}

#[tokio::test]
async fn oversized_command_line_is_rejected() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19952,
            end: 19952,
        },
    };
    let port_manager = PortManager::new(passive_config.port_range);
    // Small enough that a normal login still fits comfortably, but trivial to exceed without
    // sending megabytes of data.
    let limits = LimitsConfig {
        max_command_line_bytes: 64,
        ..LimitsConfig::default()
    };

    let (gateway_addr, session_task) = spawn_session_with_limits(
        backend_config,
        passive_config,
        TimeoutConfig::default(),
        limits,
        port_manager,
    )
    .await;

    let client = connect_with_retry(gateway_addr).await;
    let mut conn = BufReader::new(client);
    login(&mut conn).await;

    // No trailing "\r\n" -- the point is that the line never terminates within the byte cap.
    let oversized = format!("CWD {}", "A".repeat(500));
    conn.write_all(oversized.as_bytes()).await.unwrap();

    let mut reply = String::new();
    conn.read_line(&mut reply).await.unwrap();
    assert!(reply.starts_with("500"), "{reply}");

    let result = await_session(session_task).await;
    assert!(
        result.is_ok(),
        "session::handle returned an error: {result:?}"
    );
}
