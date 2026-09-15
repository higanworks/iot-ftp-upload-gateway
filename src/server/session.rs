use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::backend;
use crate::backend::dns_cache::DnsCache;
use crate::config::{BackendConfig, LimitsConfig, PassiveConfig, TimeoutConfig};
use crate::metrics::Metrics;
use crate::pasv::port_manager::{PasvPortGuard, PortManager};
use crate::pasv::relay;
use crate::protocol::command::{FtpCommand, escape_control_chars, has_embedded_line_break};
use crate::protocol::reply;

/// A data connection ready to relay: the client's side has connected to the Gateway's PASV
/// port, and the Gateway has opened the matching data connection to the backend (by acting
/// as a PASV client toward it).
struct DataChannel {
    client_data: TcpStream,
    backend_data: TcpStream,
}

/// Treats an I/O failure on the *client* connection as a normal disconnect (PROJECT_ADDITION.ja.md
/// section 2: on an unreliable mobile network, connection loss is expected, not exceptional).
/// Logs at INFO and ends the session immediately rather than propagating as an error.
macro_rules! client_io {
    ($expr:expr) => {
        match $expr {
            Ok(value) => value,
            Err(err) => {
                tracing::info!(error = %err, "client disconnected");
                return Ok(());
            }
        }
    };
}

/// Treats an I/O failure or timeout talking to the *backend* as a backend-side problem: unlike
/// the client, the backend is our own infrastructure, so this is logged at WARN.
macro_rules! backend_io {
    ($expr:expr) => {
        match $expr {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, "backend connection failed");
                return Ok(());
            }
        }
    };
}

/// Reads one line, capping the number of bytes consumed to `max_bytes` (PROJECT_SECURITY.md
/// section 4/6): without this, a peer that never sends `\n` could make the gateway buffer an
/// unbounded amount of memory for a single line. If the cap is hit before a `\n` is found, this
/// returns an `InvalidData` error rather than continuing to read.
async fn read_line_bounded(
    conn: &mut BufReader<TcpStream>,
    line: &mut String,
    max_bytes: usize,
) -> io::Result<usize> {
    let bytes_read = conn.take(max_bytes as u64).read_line(line).await?;
    if bytes_read == max_bytes && !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "line exceeds maximum allowed length",
        ));
    }
    Ok(bytes_read)
}

async fn read_line_with_timeout(
    conn: &mut BufReader<TcpStream>,
    line: &mut String,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<usize> {
    match tokio::time::timeout(timeout, read_line_bounded(conn, line, max_bytes)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for backend reply",
        )),
    }
}

/// `session_id`, the client's IP, and the selected backend are recorded on the span so every
/// log line emitted while handling this session (including from helper functions called within
/// it) carries them automatically, without repeating them on every call site. `session_id`
/// (rather than `peer_addr`, which is still logged separately at session start/end) is the
/// intended key for grouping one client's log lines together, since it stays meaningful even if
/// the client reconnects from the same IP with a different ephemeral source port; `client_ip`
/// (the IP alone, without the ephemeral port) is what's meant for filtering/aggregating across
/// sessions from the same device.
// Each parameter is its own independently-meaningful piece of session setup (matching this
// codebase's existing split of config into BackendConfig/PassiveConfig/TimeoutConfig/
// LimitsConfig), so bundling them into one wrapper struct just to satisfy this lint's default
// threshold wouldn't clarify anything.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "session",
    skip(client, peer_addr, backend_config, passive_config, timeouts, limits, port_manager, metrics, dns_cache),
    fields(
        session_id = session_id,
        client_ip = %peer_addr.ip(),
        backend_host = %backend_config.host,
        backend_port = backend_config.port
    )
)]
pub async fn handle(
    client: TcpStream,
    peer_addr: SocketAddr,
    session_id: u64,
    backend_config: BackendConfig,
    passive_config: PassiveConfig,
    timeouts: TimeoutConfig,
    limits: LimitsConfig,
    port_manager: PortManager,
    metrics: Arc<Metrics>,
    dns_cache: DnsCache,
) -> anyhow::Result<()> {
    tracing::info!("session started");
    let _session_guard = metrics.session_started();

    // The control connection is a long-running series of small one-line command/reply round
    // trips; leaving Nagle's algorithm enabled would add its characteristic latency to each one.
    if let Err(err) = client.set_nodelay(true) {
        tracing::debug!(error = %err, "failed to set TCP_NODELAY on client connection");
    }

    let connection_timeout = Duration::from_secs(timeouts.connection_timeout_secs);
    let idle_timeout = Duration::from_secs(timeouts.idle_timeout_secs);
    let command_timeout = Duration::from_secs(timeouts.command_timeout_secs);
    let data_idle_timeout = Duration::from_secs(timeouts.data_idle_timeout_secs);
    let max_command_line_bytes = limits.max_command_line_bytes;

    let backend_stream =
        match backend::connection::connect(&backend_config, connection_timeout, &dns_cache).await {
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

    // Relay the backend's initial banner to the client as-is.
    let mut line = String::new();
    let banner_bytes = backend_io!(
        read_line_with_timeout(
            &mut backend_conn,
            &mut line,
            command_timeout,
            max_command_line_bytes
        )
        .await
    );
    if banner_bytes == 0 {
        tracing::warn!("backend closed connection unexpectedly");
        let _ = client_conn
            .write_all(b"421 Service not available\r\n")
            .await;
        return Ok(());
    }
    client_io!(client_conn.write_all(line.as_bytes()).await);

    loop {
        line.clear();
        tokio::select! {
            biased;

            control_result = read_line_bounded(&mut client_conn, &mut line, max_command_line_bytes) => {
                let bytes_read = match control_result {
                    Ok(n) => n,
                    Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                        tracing::warn!(error = %err, "client command line exceeds maximum length");
                        let _ = client_conn.write_all(b"500 Command line too long\r\n").await;
                        break;
                    }
                    Err(err) => {
                        tracing::info!(error = %err, "client disconnected");
                        return Ok(());
                    }
                };
                if bytes_read == 0 {
                    tracing::info!("client disconnected");
                    break;
                }

                let trimmed_line = line.trim_end_matches(['\r', '\n']);
                if has_embedded_line_break(trimmed_line) {
                    // A CR or LF survived inside what the control-line reader treated as a
                    // single command -- forwarding `line` as-is could let the Backend split it
                    // into two commands (PROJECT_SECURITY.md section 4). Reject outright rather
                    // than forwarding any part of it.
                    tracing::warn!(
                        "rejected command line containing embedded CR/LF (possible command injection attempt)"
                    );
                    client_io!(
                        client_conn
                            .write_all(b"501 Syntax error in parameters or arguments\r\n")
                            .await
                    );
                    continue;
                }
                let command = FtpCommand::parse(trimmed_line);
                // Raw per-command traffic is high-volume and low-signal for normal operation
                // (CloudWatch Logs Insights bills per byte ingested) -- kept at DEBUG rather
                // than INFO; the commands that matter operationally (PASV/STOR outcomes,
                // QUIT ending the session) are already logged at INFO in their own right below.
                tracing::debug!(command = %command.as_log_str(), "received command");

                if let FtpCommand::Unknown(_) = command {
                    // Only the commands PROJECT_SECURITY.md section 2 lists as supported are
                    // ever forwarded to the Backend -- an unrecognized verb (RETR, DELE, LIST,
                    // ...) is rejected here rather than relayed, keeping the Gateway's attack
                    // surface limited to what it actually implements.
                    tracing::warn!(command = %command.as_log_str(), "rejected unsupported command");
                    client_io!(
                        client_conn
                            .write_all(b"502 Command not implemented\r\n")
                            .await
                    );
                    continue;
                }

                if matches!(command, FtpCommand::Pasv | FtpCommand::Epsv) {
                    if let Some(old) = active_pasv.take() {
                        tracing::info!(port = old.port(), "PASV port released (replaced by new PASV)");
                        // `old` is dropped here, releasing the port back to the pool.
                    }

                    match port_manager.allocate().await {
                        Some(guard) => {
                            let port = guard.port();
                            tracing::info!(port, "PASV port allocated");
                            // EPSV's reply carries no address (RFC 2428): the client reuses the
                            // control connection's address. PASV must spell one out, so it uses
                            // the configured advertised address.
                            let response = if let FtpCommand::Epsv = command {
                                reply::epsv_reply(port)
                            } else {
                                reply::pasv_reply(passive_config.address, port)
                            };
                            client_io!(client_conn.write_all(response.as_bytes()).await);
                            active_pasv = Some(guard);
                        }
                        None => {
                            tracing::warn!("PASV port pool exhausted");
                            client_io!(
                                client_conn
                                    .write_all(b"425 Can't open data connection\r\n")
                                    .await
                            );
                        }
                    }
                    continue;
                }

                if let FtpCommand::Stor(filename) = &command
                    && let Some(guard) = active_pasv.take()
                {
                    let filename = filename.clone();
                    // Logged separately from `filename` itself: a client-supplied filename could
                    // contain control/ANSI-escape characters (though not a literal `\n` -- the
                    // control-line reader already stops there), which would otherwise be written
                    // verbatim into human-readable text logs (PROJECT_SECURITY.md section 11).
                    // The real `filename` is still forwarded to the backend untouched below.
                    let filename_log = escape_control_chars(&filename);
                    let port = guard.port();

                    // The client is expected to have already connected to the PASV port (or to
                    // be connecting right now); accept it here rather than racing this against
                    // control-line reads in the select loop above, which starves this accept
                    // when the client sends further commands back-to-back before we get a turn.
                    let channel = match tokio::time::timeout(connection_timeout, guard.listener().accept()).await {
                        Ok(Ok((client_data, data_peer))) => {
                            tracing::info!(%data_peer, port, "data connection accepted");
                            match relay::open_backend_data_connection(&mut backend_conn).await {
                                Ok(backend_data) => Some(DataChannel { client_data, backend_data }),
                                Err(err) => {
                                    tracing::warn!(error = %err, "failed to open backend data connection");
                                    None
                                }
                            }
                        }
                        Ok(Err(err)) => {
                            tracing::info!(error = %err, "client failed to open data connection");
                            None
                        }
                        Err(_) => {
                            tracing::info!("client never opened the data connection (timed out)");
                            None
                        }
                    };
                    // `guard` is dropped here either way, releasing the port back to the pool.
                    tracing::info!(port, "PASV port released");

                    if let Some(mut channel) = channel {
                        let upload_start = Instant::now();
                        tracing::info!(%filename_log, "upload started");

                        backend_io!(backend_conn.write_all(line.as_bytes()).await);

                        let mut backend_reply = String::new();
                        let reply_bytes = backend_io!(
                            read_line_with_timeout(
                                &mut backend_conn,
                                &mut backend_reply,
                                command_timeout,
                                max_command_line_bytes
                            )
                            .await
                        );
                        if reply_bytes == 0 {
                            tracing::warn!("backend closed connection unexpectedly");
                            let _ = client_conn
                                .write_all(b"421 Service not available\r\n")
                                .await;
                            break;
                        }
                        client_io!(client_conn.write_all(backend_reply.as_bytes()).await);

                        // Move the file bytes only after the backend confirmed it is ready (150).
                        let _upload_guard = metrics.upload_started();
                        let relay_result = relay::relay_bidirectional(
                            &mut channel.client_data,
                            &mut channel.backend_data,
                            data_idle_timeout,
                        )
                        .await;
                        match relay_result {
                            Ok((bytes_uploaded, _)) => {
                                metrics.add_upload_bytes(bytes_uploaded);
                                tracing::info!(%filename_log, bytes_uploaded, "upload data transferred");
                            }
                            Err(err) => {
                                let duration_ms = upload_start.elapsed().as_millis() as u64;
                                tracing::warn!(%filename_log, error = %err, duration_ms, "upload failed: data relay error");
                            }
                        }
                        tracing::info!(%filename_log, "data connection closed");

                        let mut completion = String::new();
                        let completion_bytes = backend_io!(
                            read_line_with_timeout(
                                &mut backend_conn,
                                &mut completion,
                                command_timeout,
                                max_command_line_bytes
                            )
                            .await
                        );
                        if completion_bytes == 0 {
                            tracing::warn!("backend closed connection unexpectedly");
                            let _ = client_conn
                                .write_all(b"426 Connection closed; transfer aborted\r\n")
                                .await;
                            break;
                        }
                        let completion_trimmed = completion.trim_end_matches(['\r', '\n']);
                        let duration_ms = upload_start.elapsed().as_millis() as u64;
                        if completion_trimmed.starts_with('2') {
                            tracing::info!(%filename_log, reply = %completion_trimmed, duration_ms, "upload finished");
                        } else {
                            tracing::warn!(%filename_log, reply = %completion_trimmed, duration_ms, "upload failed");
                        }
                        client_io!(client_conn.write_all(completion.as_bytes()).await);
                        continue;
                    }

                    // Could not establish the data connection; let the client know and move on
                    // rather than forwarding STOR to a backend that never got its own PASV.
                    client_io!(
                        client_conn
                            .write_all(b"425 Can't open data connection\r\n")
                            .await
                    );
                    continue;
                }
                // If this was a STOR without a PASV port allocated, fall through and let
                // the backend respond with its own error (e.g. "425 Use PASV first").

                backend_io!(backend_conn.write_all(line.as_bytes()).await);

                let mut backend_reply = String::new();
                let reply_bytes = backend_io!(
                    read_line_with_timeout(
                        &mut backend_conn,
                        &mut backend_reply,
                        command_timeout,
                        max_command_line_bytes
                    )
                    .await
                );
                if reply_bytes == 0 {
                    tracing::warn!("backend closed connection unexpectedly");
                    let _ = client_conn
                        .write_all(b"421 Service not available\r\n")
                        .await;
                    break;
                }
                client_io!(client_conn.write_all(backend_reply.as_bytes()).await);

                if let FtpCommand::Quit = command {
                    // Standard FTP behavior: once the backend's goodbye has been relayed, end
                    // the session ourselves rather than waiting for the client to close its end
                    // (which it may delay, tying up a backend connection and a PASV port slot
                    // in the meantime).
                    break;
                }
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

    tracing::info!("session ended");
    Ok(())
}
