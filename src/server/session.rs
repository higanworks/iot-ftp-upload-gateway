use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::backend;
use crate::config::{BackendConfig, PassiveConfig, TimeoutConfig};
use crate::pasv::port_manager::{PasvPortGuard, PortManager};
use crate::pasv::relay;
use crate::protocol::command::FtpCommand;
use crate::protocol::reply;

/// A data connection that is ready to use: the client's side has connected to the Gateway's
/// PASV port, and the Gateway has already opened the matching data connection to the backend
/// (by acting as a PASV client toward it). It sits idle until a data-transfer command (STOR)
/// arrives on the control connection.
struct DataChannel {
    client_data: TcpStream,
    backend_data: TcpStream,
}

/// `peer_addr` and the selected backend are recorded on the span so every log line emitted
/// while handling this session (including from helper functions called within it) carries
/// them automatically, without repeating `%peer_addr` on every call site.
#[tracing::instrument(
    name = "session",
    skip(client, backend_config, passive_config, timeouts, port_manager),
    fields(backend_host = %backend_config.host, backend_port = backend_config.port)
)]
pub async fn handle(
    client: TcpStream,
    peer_addr: SocketAddr,
    backend_config: BackendConfig,
    passive_config: PassiveConfig,
    timeouts: TimeoutConfig,
    port_manager: PortManager,
) -> anyhow::Result<()> {
    tracing::info!("session started");

    let connection_timeout = Duration::from_secs(timeouts.connection_timeout_secs);
    let idle_timeout = Duration::from_secs(timeouts.idle_timeout_secs);

    let backend_stream =
        match backend::connection::connect(&backend_config, connection_timeout).await {
            Ok(stream) => stream,
            Err(err) => {
                tracing::warn!(error = %err, "failed to connect to backend");
                let mut client = client;
                let _ = client.write_all(b"421 Service not available\r\n").await;
                return Ok(());
            }
        };

    let mut client_conn = BufReader::new(client);
    let mut backend_conn = BufReader::new(backend_stream);
    let mut active_pasv: Option<PasvPortGuard> = None;
    let mut data_channel: Option<DataChannel> = None;

    // Relay the backend's initial banner to the client as-is.
    let mut line = String::new();
    let banner_bytes = backend_conn.read_line(&mut line).await?;
    if banner_bytes > 0 {
        client_conn.write_all(line.as_bytes()).await?;
    }

    loop {
        line.clear();
        tokio::select! {
            biased;

            control_result = client_conn.read_line(&mut line) => {
                let bytes_read = control_result?;
                if bytes_read == 0 {
                    break;
                }

                let command = FtpCommand::parse(line.trim_end_matches(['\r', '\n']));
                tracing::info!(command = %command.as_log_str(), "received command");

                if let FtpCommand::Pasv = command {
                    if let Some(old) = active_pasv.take() {
                        tracing::info!(port = old.port(), "PASV port released (replaced by new PASV)");
                        // `old` is dropped here, releasing the port back to the pool.
                    }
                    if data_channel.take().is_some() {
                        tracing::info!("data connection closed (unused, replaced by new PASV)");
                    }

                    match port_manager.allocate(passive_config.address).await {
                        Some(guard) => {
                            let port = guard.port();
                            tracing::info!(port, "PASV port allocated");
                            let response = reply::pasv_reply(passive_config.address, port);
                            client_conn.write_all(response.as_bytes()).await?;
                            active_pasv = Some(guard);
                        }
                        None => {
                            tracing::warn!("PASV port pool exhausted");
                            client_conn
                                .write_all(b"425 Can't open data connection\r\n")
                                .await?;
                        }
                    }
                    continue;
                }

                if let FtpCommand::Stor(filename) = &command
                    && let Some(mut channel) = data_channel.take()
                {
                    let filename = filename.clone();
                    tracing::info!(%filename, "upload started");

                    backend_conn.write_all(line.as_bytes()).await?;

                    let mut backend_reply = String::new();
                    let reply_bytes = backend_conn.read_line(&mut backend_reply).await?;
                    if reply_bytes == 0 {
                        tracing::warn!("backend closed connection unexpectedly");
                        break;
                    }
                    client_conn.write_all(backend_reply.as_bytes()).await?;

                    // Move the file bytes only after the backend confirmed it is ready (150).
                    match relay::relay_bidirectional(&mut channel.client_data, &mut channel.backend_data).await {
                        Ok((bytes_uploaded, _)) => {
                            tracing::info!(%filename, bytes_uploaded, "upload data transferred");
                        }
                        Err(err) => {
                            tracing::warn!(%filename, error = %err, "upload failed: data relay error");
                        }
                    }
                    tracing::info!(%filename, "data connection closed");

                    let mut completion = String::new();
                    let completion_bytes = backend_conn.read_line(&mut completion).await?;
                    if completion_bytes == 0 {
                        tracing::warn!("backend closed connection unexpectedly");
                        break;
                    }
                    let completion_trimmed = completion.trim_end_matches(['\r', '\n']);
                    if completion_trimmed.starts_with('2') {
                        tracing::info!(%filename, reply = %completion_trimmed, "upload finished");
                    } else {
                        tracing::warn!(%filename, reply = %completion_trimmed, "upload failed");
                    }
                    client_conn.write_all(completion.as_bytes()).await?;
                    continue;
                }
                // If this was a STOR without a ready data connection, fall through and let
                // the backend respond with its own error (e.g. "425 Use PASV first").

                backend_conn.write_all(line.as_bytes()).await?;

                let mut backend_reply = String::new();
                let reply_bytes = backend_conn.read_line(&mut backend_reply).await?;
                if reply_bytes == 0 {
                    tracing::warn!("backend closed connection unexpectedly");
                    break;
                }
                client_conn.write_all(backend_reply.as_bytes()).await?;
            }

            accept_result = accept_pasv_data(active_pasv.as_ref()), if active_pasv.is_some() => {
                let port = active_pasv
                    .as_ref()
                    .expect("branch only runs while active_pasv is Some")
                    .port();

                match accept_result {
                    Ok((client_data, data_peer)) => {
                        tracing::info!(%data_peer, port, "data connection accepted");

                        match relay::open_backend_data_connection(&mut backend_conn).await {
                            Ok(backend_data) => {
                                data_channel = Some(DataChannel { client_data, backend_data });
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "failed to open backend data connection");
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to accept data connection");
                    }
                }

                // The listener has served its single connection; release the port immediately.
                active_pasv = None;
                tracing::info!(port, "PASV port released");
            }

            _ = tokio::time::sleep(idle_timeout) => {
                tracing::warn!(idle_timeout_secs = timeouts.idle_timeout_secs, "idle timeout reached");
                let _ = client_conn.write_all(b"421 Idle timeout, closing control connection\r\n").await;
                break;
            }
        }
    }

    if let Some(guard) = active_pasv.take() {
        tracing::info!(port = guard.port(), "PASV port released");
    }
    if data_channel.take().is_some() {
        tracing::info!("data connection closed (unused, session ending)");
    }

    tracing::info!("session ended");
    Ok(())
}

async fn accept_pasv_data(
    guard: Option<&PasvPortGuard>,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    guard
        .expect("select! only polls this branch when active_pasv is Some")
        .listener()
        .accept()
        .await
}
