use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::anyhow;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::backend::connection::connect_from;
use crate::backend::tls::BackendStream;
use crate::protocol::reply::parse_pasv_reply;

/// Why opening a backend data connection failed. The distinction matters to the caller: only one
/// of the two leaves the backend control connection usable.
#[derive(Debug)]
pub enum DataConnectError {
    /// The backend control connection can no longer be trusted to be in step with our commands:
    /// the `PASV` reply never came, was cut off, or ran past the line limit. A reply that turns
    /// up late would be read as the answer to the *next* command, so the session must end.
    ControlConnection(anyhow::Error),
    /// The control connection is fine -- the reply was read in full -- but it was not a `PASV`
    /// address (a refusal such as `425`, say).
    UnusableReply(anyhow::Error),
    /// The control connection is fine and the reply gave an address, but connecting to it failed.
    ConnectFailed(anyhow::Error),
}

impl std::fmt::Display for DataConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataConnectError::ControlConnection(err)
            | DataConnectError::UnusableReply(err)
            | DataConnectError::ConnectFailed(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for DataConnectError {}

impl DataConnectError {
    /// The underlying I/O error, if there is one -- used to judge whether a failed connect says
    /// something against the source address it was made from.
    pub fn io_error(&self) -> Option<&io::Error> {
        match self {
            DataConnectError::ControlConnection(err)
            | DataConnectError::UnusableReply(err)
            | DataConnectError::ConnectFailed(err) => err.root_cause().downcast_ref::<io::Error>(),
        }
    }
}

/// Limits applied while opening a backend data connection.
#[derive(Debug, Clone, Copy)]
pub struct DataConnectLimits {
    /// How long to wait for the backend's reply to `PASV`.
    pub reply_timeout: Duration,
    /// How long to wait for the TCP connection to the data port.
    pub connect_timeout: Duration,
    /// Longest `PASV` reply line accepted, so a backend that never sends a newline cannot make
    /// the gateway buffer without bound.
    pub max_line_bytes: usize,
}

/// Asks the backend to open a passive data connection and connects to the address/port
/// it returns. The Gateway itself acts as a PASV client toward the backend here.
///
/// With `local_ip`, the connection is made from that local address -- the same one the control
/// connection uses, since backends tie a data connection to its control connection's client IP.
///
/// Every wait is bounded by `limits`: a backend that stops answering must not be able to hold the
/// session (and the PASV port and source-address slot it has claimed) forever.
///
/// This only opens the TCP connection. With backend FTPS the TLS handshake on a data connection
/// cannot happen here: servers start it only once the transfer command (`STOR`) has been
/// received, so the caller performs it after forwarding `STOR` and getting the `150` reply.
pub async fn open_backend_data_connection(
    backend_control: &mut BufReader<BackendStream>,
    local_ip: Option<Ipv4Addr>,
    limits: DataConnectLimits,
) -> Result<TcpStream, DataConnectError> {
    let reply = tokio::time::timeout(limits.reply_timeout, async {
        backend_control.write_all(b"PASV\r\n").await?;
        let mut line = String::new();
        let bytes_read = (&mut *backend_control)
            .take(limits.max_line_bytes as u64)
            .read_line(&mut line)
            .await?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "backend closed the control connection while opening a data connection",
            ));
        }
        if !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backend's PASV reply exceeds the maximum line length",
            ));
        }
        Ok(line)
    })
    .await;
    let line = match reply {
        Ok(Ok(line)) => line,
        Ok(Err(err)) => {
            return Err(DataConnectError::ControlConnection(
                anyhow::Error::new(err).context("reading the backend's PASV reply failed"),
            ));
        }
        Err(_) => {
            return Err(DataConnectError::ControlConnection(anyhow::Error::new(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the backend's PASV reply",
                ),
            )));
        }
    };

    let (ip, port) = parse_pasv_reply(&line).ok_or_else(|| {
        DataConnectError::UnusableReply(anyhow!(
            "backend returned an unparsable PASV reply: {line:?}"
        ))
    })?;

    let data_addr = SocketAddr::from((ip, port));
    tracing::debug!(reply = %line.trim_end(), backend_data_addr = %data_addr, "backend PASV reply");

    match tokio::time::timeout(limits.connect_timeout, connect_from(data_addr, local_ip)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => Err(DataConnectError::ConnectFailed(
            anyhow::Error::new(err).context(format!(
                "failed to connect to backend data port {data_addr}"
            )),
        )),
        Err(_) => Err(DataConnectError::ConnectFailed(
            anyhow::Error::new(io::Error::new(
                io::ErrorKind::TimedOut,
                "connecting to the backend data port timed out",
            ))
            .context(format!(
                "failed to connect to backend data port {data_addr}"
            )),
        )),
    }
}

/// Relays bytes bidirectionally between the client's data connection and the backend's
/// data connection until either side closes. The data connection is treated as an opaque
/// byte stream; its contents are never interpreted as FTP protocol.
///
/// `idle_timeout` bounds inactivity, not total transfer time: the timer resets on every byte
/// moved in either direction, so a slow-but-active upload (e.g. over a mobile network) is never
/// cut off, but a connection that goes completely silent is detected and cleaned up rather than
/// held open forever.
pub async fn relay_bidirectional(
    client_data: &mut TcpStream,
    backend_data: &mut BackendStream,
    idle_timeout: Duration,
) -> io::Result<(u64, u64)> {
    let (mut client_read, mut client_write) = client_data.split();
    // A TLS stream can't be `split()` by reference like a TcpStream; `tokio::io::split` works
    // for both variants of `BackendStream`.
    let (mut backend_read, mut backend_write) = tokio::io::split(backend_data);

    tokio::try_join!(
        copy_with_idle_timeout(&mut client_read, &mut backend_write, idle_timeout),
        copy_with_idle_timeout(&mut backend_read, &mut client_write, idle_timeout),
    )
}

/// Size of the buffer used to shuttle bytes between the client and backend data connections.
/// Larger than a typical default (8 KiB) to cut the number of read/write syscalls per MB
/// transferred during bulk uploads, this gateway's core workload.
const RELAY_BUFFER_BYTES: usize = 65536;

/// Copies from `reader` to `writer` until EOF, shutting down `writer` when the source is
/// exhausted (preserving TCP half-close: the other direction of the pair keeps running
/// independently). Returns a `TimedOut` error if no byte is read within `idle_timeout`.
///
/// A TLS peer that drops the TCP connection without `close_notify` surfaces as `UnexpectedEof`
/// on read; that is treated as a plain end-of-stream rather than a failure. Both directions of
/// this relay are guarded by the application-level reply on the control connection, not by TLS
/// truncation detection, so this loses nothing -- and many FTP servers close that way.
///
/// The idle timer is a single `Sleep` reset on every read rather than a fresh
/// `tokio::time::timeout` per call: re-wrapping every read would register and cancel a new
/// timer-wheel entry for every `RELAY_BUFFER_BYTES` chunk moved, for the whole duration of every
/// transfer -- the hottest loop in the gateway.
async fn copy_with_idle_timeout<R, W>(
    reader: &mut R,
    writer: &mut W,
    idle_timeout: Duration,
) -> io::Result<u64>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = [0u8; RELAY_BUFFER_BYTES];
    let mut total = 0u64;
    let sleep = tokio::time::sleep(idle_timeout);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            biased;

            read_result = reader.read(&mut buf) => {
                let read = match read_result {
                    Ok(n) => n,
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => 0,
                    Err(err) => return Err(err),
                };
                if read == 0 {
                    writer.shutdown().await?;
                    return Ok(total);
                }
                writer.write_all(&buf[..read]).await?;
                total += read as u64;
                sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
            }

            _ = &mut sleep => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "data connection idle timeout",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    use crate::protocol::reply::pasv_reply;

    #[tokio::test]
    async fn opens_data_connection_via_backend_pasv() {
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_port = data_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = data_listener.accept().await.unwrap();
        });

        let control_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_addr = control_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = control_listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(line, "PASV\r\n");
            let reply = pasv_reply(Ipv4Addr::LOCALHOST, data_port);
            stream.write_all(reply.as_bytes()).await.unwrap();
        });

        let control_stream = TcpStream::connect(control_addr).await.unwrap();
        let mut backend_control = BufReader::new(BackendStream::Plain(control_stream));

        let data_stream = open_backend_data_connection(&mut backend_control, None, limits())
            .await
            .unwrap();
        assert_eq!(data_stream.peer_addr().unwrap().port(), data_port);
    }

    /// A backend whose PASV reply points at a data listener; returns the control address and the
    /// data listener's address of that backend.
    async fn spawn_pasv_backend() -> (std::net::SocketAddr, TcpListener) {
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_port = data_listener.local_addr().unwrap().port();
        let control_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_addr = control_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = control_listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let reply = pasv_reply(Ipv4Addr::LOCALHOST, data_port);
            stream.write_all(reply.as_bytes()).await.unwrap();
            // Keep the control connection open until the test is done with it.
            let _ = stream.read_line(&mut line).await;
        });
        (control_addr, data_listener)
    }

    #[tokio::test]
    async fn data_connection_is_made_from_the_given_local_ip() {
        let (control_addr, data_listener) = spawn_pasv_backend().await;
        let control = TcpStream::connect(control_addr).await.unwrap();
        let mut backend_control = BufReader::new(BackendStream::Plain(control));

        let data =
            open_backend_data_connection(&mut backend_control, Some(Ipv4Addr::LOCALHOST), limits())
                .await
                .unwrap();
        let (_accepted, peer) = data_listener.accept().await.unwrap();
        assert_eq!(peer.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(data.local_addr().unwrap().ip(), peer.ip());
    }

    #[tokio::test]
    async fn data_connection_fails_when_the_local_ip_is_not_this_hosts() {
        let (control_addr, _data_listener) = spawn_pasv_backend().await;
        let control = TcpStream::connect(control_addr).await.unwrap();
        let mut backend_control = BufReader::new(BackendStream::Plain(control));

        // TEST-NET-3 (RFC 5737): never assigned to a real interface.
        let result = open_backend_data_connection(
            &mut backend_control,
            Some(Ipv4Addr::new(203, 0, 113, 9)),
            limits(),
        )
        .await;
        assert!(result.is_err());
    }

    fn limits() -> DataConnectLimits {
        DataConnectLimits {
            reply_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            max_line_bytes: 4096,
        }
    }

    /// A control connection to a backend whose behavior is `script`, which gets the backend's
    /// side of the connection after it has received the `PASV` command.
    async fn control_with_backend<F, Fut>(script: F) -> BufReader<BackendStream>
    where
        F: FnOnce(BufReader<TcpStream>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(line, "PASV\r\n");
            script(stream).await;
        });
        let control = TcpStream::connect(addr).await.unwrap();
        BufReader::new(BackendStream::Plain(control))
    }

    fn is_control_error(result: &Result<TcpStream, DataConnectError>) -> bool {
        matches!(result, Err(DataConnectError::ControlConnection(_)))
    }

    #[tokio::test]
    async fn a_backend_that_never_answers_pasv_times_out_as_a_control_connection_failure() {
        let mut control = control_with_backend(|stream| async move {
            // Never replies; keep the connection open past the client's timeout.
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(stream);
        })
        .await;
        let started = std::time::Instant::now();
        let result = open_backend_data_connection(
            &mut control,
            None,
            DataConnectLimits {
                reply_timeout: Duration::from_millis(150),
                ..limits()
            },
        )
        .await;
        assert!(is_control_error(&result), "{result:?}");
        // Reported as a timeout, so the caller can count it as one.
        let io_kind = result
            .as_ref()
            .err()
            .and_then(|e| e.io_error())
            .map(|e| e.kind());
        assert_eq!(io_kind, Some(io::ErrorKind::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn an_overlong_pasv_reply_is_a_control_connection_failure() {
        let mut control = control_with_backend(|mut stream| async move {
            // No newline, well past the limit.
            let _ = stream.write_all(&[b'x'; 5000]).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;
        let result = open_backend_data_connection(
            &mut control,
            None,
            DataConnectLimits {
                max_line_bytes: 64,
                ..limits()
            },
        )
        .await;
        assert!(is_control_error(&result), "{result:?}");
    }

    #[tokio::test]
    async fn the_backend_closing_the_control_connection_is_a_control_connection_failure() {
        let mut control = control_with_backend(|stream| async move { drop(stream) }).await;
        let result = open_backend_data_connection(&mut control, None, limits()).await;
        assert!(is_control_error(&result), "{result:?}");
    }

    #[tokio::test]
    async fn a_complete_but_unusable_reply_leaves_the_control_connection_in_step() {
        let mut control = control_with_backend(|mut stream| async move {
            stream
                .write_all(b"425 Can't open data connection\r\n")
                .await
                .unwrap();
            stream.write_all(b"200 next reply\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
        .await;
        let result = open_backend_data_connection(&mut control, None, limits()).await;
        assert!(
            matches!(result, Err(DataConnectError::UnusableReply(_))),
            "{result:?}"
        );

        // The 425 was consumed whole, so the next line read is the *next* reply.
        let mut next = String::new();
        control.read_line(&mut next).await.unwrap();
        assert_eq!(next, "200 next reply\r\n");
    }

    #[tokio::test]
    async fn failing_to_connect_to_the_announced_data_port_is_only_a_data_connection_failure() {
        // A port that nothing listens on.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        let mut control = control_with_backend(move |mut stream| async move {
            let reply = pasv_reply(Ipv4Addr::LOCALHOST, closed_port);
            stream.write_all(reply.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
        .await;
        let result = open_backend_data_connection(&mut control, None, limits()).await;
        assert!(
            matches!(result, Err(DataConnectError::ConnectFailed(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn connecting_to_an_unresponsive_data_port_is_bounded_by_the_connect_timeout() {
        // TEST-NET-1 (RFC 5737) is not routed: the connect either hangs until the timeout or
        // fails at once, depending on the network; either way it must end quickly.
        let mut control = control_with_backend(|mut stream| async move {
            let reply = pasv_reply(Ipv4Addr::new(192, 0, 2, 1), 40000);
            stream.write_all(reply.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;
        let started = std::time::Instant::now();
        let result = open_backend_data_connection(
            &mut control,
            None,
            DataConnectLimits {
                connect_timeout: Duration::from_millis(200),
                ..limits()
            },
        )
        .await;
        assert!(
            matches!(result, Err(DataConnectError::ConnectFailed(_))),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn relay_moves_bytes_both_directions() {
        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();
        let accept_client = tokio::spawn(async move { client_listener.accept().await.unwrap().0 });
        let mut client_side_b = TcpStream::connect(client_addr).await.unwrap();
        let mut client_data = accept_client.await.unwrap();

        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();
        let accept_backend =
            tokio::spawn(async move { backend_listener.accept().await.unwrap().0 });
        let mut backend_side_b = TcpStream::connect(backend_addr).await.unwrap();
        let mut backend_data = BackendStream::Plain(accept_backend.await.unwrap());

        let relay_task = tokio::spawn(async move {
            relay_bidirectional(&mut client_data, &mut backend_data, Duration::from_secs(5)).await
        });

        // `client_side_b` plays the role of the real FTP client sending a file.
        client_side_b.write_all(b"hello from client").await.unwrap();
        client_side_b.shutdown().await.unwrap();

        // `backend_side_b` plays the role of the backend receiving the file and replying.
        let mut received = Vec::new();
        backend_side_b.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"hello from client");

        backend_side_b.write_all(b"ack from backend").await.unwrap();
        backend_side_b.shutdown().await.unwrap();

        let mut received_back = Vec::new();
        client_side_b.read_to_end(&mut received_back).await.unwrap();
        assert_eq!(received_back, b"ack from backend");

        relay_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn relay_times_out_when_both_sides_go_silent() {
        let client_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_listener.local_addr().unwrap();
        let accept_client = tokio::spawn(async move { client_listener.accept().await.unwrap().0 });
        let _client_side_b = TcpStream::connect(client_addr).await.unwrap();
        let mut client_data = accept_client.await.unwrap();

        let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend_listener.local_addr().unwrap();
        let accept_backend =
            tokio::spawn(async move { backend_listener.accept().await.unwrap().0 });
        let _backend_side_b = TcpStream::connect(backend_addr).await.unwrap();
        let mut backend_data = BackendStream::Plain(accept_backend.await.unwrap());

        // Neither side ever sends anything or closes; the relay must time out rather than
        // hang forever.
        let err = relay_bidirectional(
            &mut client_data,
            &mut backend_data,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
