//! `MKD` is one of the commands the Gateway forwards to the Backend (PROJECT_SECURITY.md
//! section 2), added so clients like `curl --ftp-create-dirs` -- which probe with `CWD`, `MKD`
//! on failure, then `CWD` again for each path component -- can create the directories a `STOR`
//! path implies. Commands outside that allow-list must be rejected by the Gateway itself rather
//! than forwarded.

mod common;

use std::net::Ipv4Addr;

use common::{await_session, connect_with_retry, login, read_reply, spawn_session_with_limits};
use iot_ftp_upload_gateway::config::{
    BackendConfig, LimitsConfig, PassiveConfig, PortRange, TimeoutConfig,
};
use iot_ftp_upload_gateway::pasv::port_manager::PortManager;
use iot_ftp_upload_gateway::protocol::reply::parse_pasv_reply;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[tokio::test]
async fn mkd_is_forwarded_to_backend() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19960,
            end: 19960,
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

    conn.write_all(b"MKD newdir\r\n").await.unwrap();
    let reply = read_reply(&mut conn).await;
    assert!(reply.starts_with("257"), "{reply}");

    conn.write_all(b"QUIT\r\n").await.unwrap();
    await_session(session_task).await.unwrap();

    let created_dirs = backend.created_dirs.lock().unwrap();
    assert_eq!(*created_dirs, vec!["newdir".to_string()]);
}

/// Reproduces the exact command sequence a real `curl --ftp-create-dirs` upload sends (verified
/// directly against this Gateway binary): `CWD` each path component, `MKD` + retry `CWD` on a
/// `550`, then `STOR` the bare filename once every component exists.
#[tokio::test]
async fn ftp_create_dirs_style_upload_succeeds() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19961,
            end: 19961,
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

    for component in ["sub", "dir"] {
        conn.write_all(format!("CWD {component}\r\n").as_bytes())
            .await
            .unwrap();
        let cwd_reply = read_reply(&mut conn).await;
        assert!(cwd_reply.starts_with("550"), "{cwd_reply}");

        conn.write_all(format!("MKD {component}\r\n").as_bytes())
            .await
            .unwrap();
        let mkd_reply = read_reply(&mut conn).await;
        assert!(mkd_reply.starts_with("257"), "{mkd_reply}");

        conn.write_all(format!("CWD {component}\r\n").as_bytes())
            .await
            .unwrap();
        let cwd_retry_reply = read_reply(&mut conn).await;
        assert!(cwd_retry_reply.starts_with("250"), "{cwd_retry_reply}");
    }

    conn.write_all(b"PASV\r\n").await.unwrap();
    let pasv_reply_line = read_reply(&mut conn).await;
    assert!(pasv_reply_line.starts_with("227"), "{pasv_reply_line}");
    let (_, port) = parse_pasv_reply(&pasv_reply_line).unwrap();
    let mut data_conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    conn.write_all(b"STOR file.txt\r\n").await.unwrap();
    let stor_reply = read_reply(&mut conn).await;
    assert!(stor_reply.starts_with("150"), "{stor_reply}");

    data_conn.write_all(b"payload").await.unwrap();
    data_conn.shutdown().await.unwrap();
    let completion_reply = read_reply(&mut conn).await;
    assert!(completion_reply.starts_with("226"), "{completion_reply}");

    conn.write_all(b"QUIT\r\n").await.unwrap();
    await_session(session_task).await.unwrap();

    let created_dirs = backend.created_dirs.lock().unwrap();
    assert_eq!(*created_dirs, vec!["sub".to_string(), "dir".to_string()]);

    let uploads = backend.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert_eq!(uploads[0].0, "file.txt");
    assert_eq!(uploads[0].1, b"payload");
}

#[tokio::test]
async fn unsupported_command_is_rejected_without_reaching_backend() {
    let backend = common::spawn_mock_backend().await;
    let backend_config = BackendConfig {
        host: backend.addr.ip().to_string(),
        port: backend.addr.port(),
    };
    let passive_config = PassiveConfig {
        address: Ipv4Addr::LOCALHOST,
        port_range: PortRange {
            start: 19962,
            end: 19962,
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

    conn.write_all(b"RETR somefile.txt\r\n").await.unwrap();
    let reply = read_reply(&mut conn).await;
    // The Gateway's own allow-list rejection, not the mock backend's distinctly-worded fallback
    // reply -- proves the command never reached the backend.
    assert_eq!(reply, "502 Command not implemented\r\n");

    conn.write_all(b"QUIT\r\n").await.unwrap();
    let quit_reply = read_reply(&mut conn).await;
    assert!(quit_reply.starts_with("221"), "{quit_reply}");
    await_session(session_task).await.unwrap();
}
