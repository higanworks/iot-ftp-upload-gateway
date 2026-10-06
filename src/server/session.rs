use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::backend;
use crate::backend::dns_cache::DnsCache;
use crate::backend::rotator::{DataSlot, SourceRotator};
use crate::backend::tls::{self, BackendStream, BackendTlsConnector};
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
    /// Held until the transfer is over (dropped with the channel's scope), so a source address's
    /// data-transfer capacity stays claimed for exactly as long as the transfer runs. `None`
    /// when source rotation is off.
    _data_slot: Option<DataSlot>,
}

/// Accepts the client's data connection on its PASV `listener`, giving up after `timeout`.
///
/// With `expected_ip`, a connection from any other address is closed at once and the wait goes on:
/// the PASV port is open to the whole network from the moment it is announced, so whichever host
/// connects first would otherwise become the data connection for this client's upload. The
/// timeout covers the whole wait, so connecting and dropping repeatedly can neither keep the
/// legitimate client out nor stretch the wait.
async fn accept_client_data(
    listener: &TcpListener,
    expected_ip: Option<IpAddr>,
    timeout: Duration,
    metrics: &Metrics,
) -> Result<io::Result<(TcpStream, SocketAddr)>, tokio::time::error::Elapsed> {
    tokio::time::timeout(timeout, async {
        loop {
            let (stream, peer) = listener.accept().await?;
            match expected_ip {
                Some(expected) if peer.ip() != expected => {
                    metrics.data_connection_rejected();
                    tracing::warn!(
                        data_peer = %peer,
                        expected_ip = %expected,
                        "rejected a data connection from a different IP address than the control connection"
                    );
                    // `stream` is dropped here, closing the connection.
                }
                _ => return Ok::<_, io::Error>((stream, peer)),
            }
        }
    })
    .await
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
    ($metrics:expr, $expr:expr) => {
        match $expr {
            Ok(value) => value,
            Err(err) => {
                $metrics.observe_backend_io_error(&err);
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
async fn read_line_bounded<S>(
    conn: &mut BufReader<S>,
    line: &mut String,
    max_bytes: usize,
) -> io::Result<usize>
where
    S: AsyncRead + Unpin,
{
    let bytes_read = conn.take(max_bytes as u64).read_line(line).await?;
    if bytes_read == max_bytes && !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "line exceeds maximum allowed length",
        ));
    }
    Ok(bytes_read)
}

async fn read_line_with_timeout<S>(
    conn: &mut BufReader<S>,
    line: &mut String,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<usize>
where
    S: AsyncRead + Unpin,
{
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
    skip(client, peer_addr, backend_config, passive_config, timeouts, limits, port_manager, metrics, dns_cache, backend_tls, source_rotator),
    fields(
        session_id = session_id,
        client_ip = %peer_addr.ip(),
        backend_host = %backend_config.host,
        backend_port = backend_config.port,
        source_ip = tracing::field::Empty
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
    backend_tls: Option<BackendTlsConnector>,
    source_rotator: Option<SourceRotator>,
) -> anyhow::Result<()> {
    tracing::info!("session started");
    let _session_guard = metrics.session_started();

    // One TLS resumption store per session, so this session's data connections resume *its*
    // control connection's TLS session rather than some other session's.
    let backend_tls = backend_tls.map(|connector| connector.for_session());

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
    // The backend's PASV reply gets the same bounds as every other backend reply; connecting to
    // the data port it names is bounded like connecting to the backend itself.
    let connect_limits = relay::DataConnectLimits {
        reply_timeout: command_timeout,
        connect_timeout: connection_timeout,
        max_line_bytes: max_command_line_bytes,
    };

    let backend_key = format!("{}:{}", backend_config.host, backend_config.port);
    let connected = match &source_rotator {
        None => backend::connection::connect(&backend_config, connection_timeout, &dns_cache, None)
            .await
            .map(|stream| (stream, None))
            .map_err(|err| {
                metrics.observe_backend_io_error(&err);
                tracing::warn!(error = %err, "failed to connect to backend");
            }),
        Some(rotator) => {
            // The session takes the first source address that connects and keeps it: its data
            // connections must come from the same address as this control connection.
            let mut connected = Err(());
            for source in rotator.candidates(&backend_key) {
                match backend::connection::connect(
                    &backend_config,
                    connection_timeout,
                    &dns_cache,
                    Some(source),
                )
                .await
                {
                    Ok(stream) => {
                        connected = Ok((stream, Some(source)));
                        break;
                    }
                    Err(err) => {
                        metrics.observe_backend_io_error(&err);
                        tracing::warn!(source_ip = %source, error = %err, "failed to connect to backend from this source address");
                        rotator.report_connect_failure(source, &err);
                    }
                }
            }
            connected
        }
    };
    let (backend_stream, source_ip): (TcpStream, Option<Ipv4Addr>) = match connected {
        Ok(connected) => connected,
        Err(()) => {
            let mut client = client;
            let _ = client.write_all(b"421 Service not available\r\n").await;
            return Ok(());
        }
    };
    if let Some(source_ip) = source_ip {
        tracing::Span::current().record("source_ip", tracing::field::display(source_ip));
    }
    // Only set when rotation is on: the source address this session uses toward the backend
    // plus the slot accounting that goes with it.
    let rotation = source_rotator.zip(source_ip);
    let mut warned_port_outside_range = false;

    let mut client_conn = BufReader::new(client);
    let mut backend_conn = BufReader::new(BackendStream::Plain(backend_stream));
    let mut active_pasv: Option<PasvPortGuard> = None;

    // Relay the backend's initial banner to the client as-is.
    let mut line = String::new();
    let banner_bytes = backend_io!(
        metrics,
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

    // Explicit FTPS toward the backend (clients still speak plain FTP to us). Done before the
    // banner is passed on, so a client never sees a ready banner for a session that can't be
    // secured. Fails closed: no fallback to plain FTP.
    if let Some(connector) = &backend_tls {
        backend_conn = match tls::negotiate_explicit_ftps(
            backend_conn,
            connector,
            &backend_config.host,
            command_timeout,
            connection_timeout,
            max_command_line_bytes,
        )
        .await
        {
            Ok(conn) => conn,
            Err(err) => {
                metrics.observe_backend_io_error(&err);
                tracing::warn!(error = %err, "failed to establish TLS with backend");
                let _ = client_conn
                    .write_all(b"421 Service not available\r\n")
                    .await;
                return Ok(());
            }
        };
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
                    let mut backend_control_lost = false;
                    let expected_data_ip = limits.require_data_ip_match.then_some(peer_addr.ip());
                    let channel = match accept_client_data(guard.listener(), expected_data_ip, connection_timeout, &metrics).await {
                        Ok(Ok((client_data, data_peer))) => {
                            tracing::info!(%data_peer, port, "data connection accepted");
                            // With source rotation, claim this transfer's slot on the session's
                            // source address before asking the backend for a data port: the
                            // backend's data ports are the scarce thing being rationed.
                            let data_slot = match &rotation {
                                Some((rotator, source)) => {
                                    let slot = rotator
                                        .acquire_data_slot(
                                            &backend_key,
                                            backend_config.passive_ports.len(),
                                            *source,
                                            connection_timeout,
                                        )
                                        .await;
                                    if slot.is_none() {
                                        tracing::warn!(source_ip = %source, "no data transfer slot free on this source address (timed out)");
                                    }
                                    slot.map(Some)
                                }
                                None => Some(None),
                            };
                            match data_slot {
                                None => None,
                                Some(data_slot) => {
                                    let local_ip = rotation.as_ref().map(|(_, source)| *source);
                                    match relay::open_backend_data_connection(&mut backend_conn, local_ip, connect_limits).await {
                                        Ok(backend_data) => {
                                            if rotation.is_some()
                                                && !warned_port_outside_range
                                                && let Ok(addr) = backend_data.peer_addr()
                                                && !(backend_config.passive_ports.start..=backend_config.passive_ports.end).contains(&addr.port())
                                            {
                                                warned_port_outside_range = true;
                                                tracing::warn!(
                                                    backend_data_port = addr.port(),
                                                    configured_start = backend_config.passive_ports.start,
                                                    configured_end = backend_config.passive_ports.end,
                                                    "backend data port is outside its configured passive_ports range; transfer capacity per source address may be mis-sized"
                                                );
                                            }
                                            Some(DataChannel { client_data, backend_data, _data_slot: data_slot })
                                        }
                                        Err(err @ relay::DataConnectError::ControlConnection(_)) => {
                                            // No reply, a cut-off one, or an overlong one: the control
                                            // connection is out of step with us from here on.
                                            tracing::warn!(error = %err, "backend control connection unusable while opening a data connection");
                                            metrics.backend_pasv_failed();
                                            if let Some(io_err) = err.io_error() {
                                                metrics.observe_backend_io_error(io_err);
                                            }
                                            backend_control_lost = true;
                                            None
                                        }
                                        Err(err @ relay::DataConnectError::UnusableReply(_)) => {
                                            tracing::warn!(error = %err, "failed to open backend data connection");
                                            metrics.backend_pasv_failed();
                                            None
                                        }
                                        Err(err) => {
                                            tracing::warn!(error = %err, "failed to open backend data connection");
                                            metrics.backend_data_connection_failed();
                                            if let Some(io_err) = err.io_error() {
                                                metrics.observe_backend_io_error(io_err);
                                                if let Some((rotator, source)) = &rotation {
                                                    rotator.report_connect_failure(*source, io_err);
                                                }
                                            }
                                            None
                                        }
                                    }
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

                    if backend_control_lost {
                        // A late PASV reply would be taken for the answer to the next command, so
                        // this backend connection cannot be used any further.
                        let _ = client_conn
                            .write_all(b"421 Service not available\r\n")
                            .await;
                        break;
                    }

                    if let Some(mut channel) = channel {
                        let upload_start = Instant::now();
                        tracing::info!(%filename_log, "upload started");
                        // Records how this upload ends: failed unless `succeeded()` is called, so
                        // every early exit below counts without each one having to say so.
                        let upload_attempt = metrics.upload_attempted();

                        backend_io!(metrics, backend_conn.write_all(line.as_bytes()).await);

                        let mut backend_reply = String::new();
                        let reply_bytes = backend_io!(metrics,
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

                        if backend_tls.is_some() && !backend_reply.starts_with('1') {
                            // The backend refused the transfer: its final reply was just
                            // relayed, nothing more will follow on the control connection, and
                            // it will never start a TLS handshake on the data connection.
                            tracing::warn!(%filename_log, reply = %backend_reply.trim_end(), "upload failed");
                            continue;
                        }

                        // With backend FTPS the data connection's TLS handshake starts only now,
                        // after the transfer command (servers do not begin it any earlier).
                        let backend_data = match &backend_tls {
                            Some(connector) => tokio::time::timeout(
                                connection_timeout,
                                connector.connect(&backend_config.host, channel.backend_data),
                            )
                            .await
                            .unwrap_or_else(|_| {
                                Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "backend data connection TLS handshake timed out",
                                ))
                            }),
                            None => Ok(BackendStream::Plain(channel.backend_data)),
                        };

                        if let Err(err) = &backend_data {
                            metrics.backend_data_connection_failed();
                            metrics.observe_backend_io_error(err);
                        }

                        // Move the file bytes only after the backend confirmed it is ready (150).
                        let _upload_guard = metrics.upload_started();
                        let relay_result = match backend_data {
                            Ok(mut backend_data) => {
                                relay::relay_bidirectional(
                                    &mut channel.client_data,
                                    &mut backend_data,
                                    data_idle_timeout,
                                )
                                .await
                            }
                            // Handled like a relay failure: the backend's own completion reply
                            // (read below) is still relayed, keeping the control connection in sync.
                            Err(err) => Err(err),
                        };
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
                        let completion_bytes = backend_io!(metrics,
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
                            upload_attempt.succeeded();
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

                backend_io!(metrics, backend_conn.write_all(line.as_bytes()).await);

                let mut backend_reply = String::new();
                let reply_bytes = backend_io!(metrics,
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
                metrics.session_idle_timed_out();
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

#[cfg(test)]
mod tests {
    use super::*;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    /// Not this machine's address as seen by a client connecting from 127.0.0.1.
    const OTHER: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

    async fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    #[tokio::test]
    async fn accepts_a_connection_from_the_expected_ip() {
        let (listener, addr) = listener().await;
        let _client = TcpStream::connect(addr).await.unwrap();
        let metrics = Metrics::new();
        let (_stream, peer) =
            accept_client_data(&listener, Some(LOOPBACK), Duration::from_secs(5), &metrics)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(peer.ip(), LOOPBACK);
        assert!(
            metrics
                .render(0, 0, None)
                .contains("ftp_gateway_data_connections_rejected_total 0\n")
        );
    }

    #[tokio::test]
    async fn without_an_expected_ip_any_connection_is_accepted() {
        let (listener, addr) = listener().await;
        let _client = TcpStream::connect(addr).await.unwrap();
        let accepted =
            accept_client_data(&listener, None, Duration::from_secs(5), &Metrics::new()).await;
        assert!(accepted.unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_connection_from_another_ip_is_closed_and_the_wait_times_out() {
        let (listener, addr) = listener().await;
        // The client connects from 127.0.0.1, but the control connection "came from" 127.0.0.2.
        let metrics = Metrics::new();
        let (accepted, bytes_read) = tokio::join!(
            accept_client_data(&listener, Some(OTHER), Duration::from_millis(300), &metrics),
            async {
                let mut client = TcpStream::connect(addr).await.unwrap();
                let mut buf = [0u8; 1];
                // The gateway closes it: EOF (or a reset), never data.
                tokio::io::AsyncReadExt::read(&mut client, &mut buf)
                    .await
                    .unwrap_or(0)
            }
        );
        assert!(accepted.is_err(), "nothing acceptable ever connected");
        assert_eq!(bytes_read, 0);
        // The refusal was counted.
        assert!(
            metrics
                .render(0, 0, None)
                .contains("ftp_gateway_data_connections_rejected_total 1\n")
        );
    }

    /// Needs a second local address to connect from, which Linux gives every 127.0.0.0/8 address.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_rejected_connection_does_not_keep_out_the_expected_one() {
        use tokio::net::TcpSocket;

        let metrics = Metrics::new();
        let (listener, addr) = listener().await;
        let (accepted, _clients) = tokio::join!(
            accept_client_data(&listener, Some(LOOPBACK), Duration::from_secs(5), &metrics),
            async {
                // The intruder gets there first, from 127.0.0.2...
                let intruder = TcpSocket::new_v4().unwrap();
                intruder.bind("127.0.0.2:0".parse().unwrap()).unwrap();
                let intruder = intruder.connect(addr).await.unwrap();
                // ...then the real client, from 127.0.0.1.
                let client = TcpStream::connect(addr).await.unwrap();
                (intruder, client)
            }
        );
        let (_stream, peer) = accepted.unwrap().unwrap();
        assert_eq!(peer.ip(), LOOPBACK);
    }
}
