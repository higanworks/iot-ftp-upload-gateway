use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::metrics::Metrics;
use crate::pasv::port_manager::PortManager;

/// How many bytes the request line may occupy before this gives up on it -- a real Prometheus
/// scraper sends a short `GET /metrics HTTP/1.1` line, so this is generous headroom rather than
/// a limit meant to accommodate any real request. Mirrors the same "bound untrusted input"
/// posture as `LimitsConfig::max_command_line_bytes` for the FTP control connection
/// (PROJECT_SECURITY.md section 6).
const MAX_REQUEST_LINE_BYTES: usize = 8192;

/// Ceiling on the *combined* size of every header line, not just each one individually --
/// without this, a client sending unbounded small header lines (each comfortably under any
/// per-line cap) could keep this handler looping indefinitely.
const MAX_HEADER_BYTES: usize = 8192;

/// Bounds how long this will wait for a client to finish sending its request line and headers.
/// Defends against a slowloris-style client that opens a connection and then sends bytes (or
/// nothing at all) too slowly to ever trip the size caps above.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Serves `GET /metrics` in Prometheus text exposition format on `addr`, until the process
/// exits. Deliberately minimal (no routing, no keep-alive, no request body handling) since this
/// endpoint exists to answer exactly one kind of request; matches this gateway's existing
/// preference for hand-written protocol handling over pulling in a framework (PROJECT_SECURITY.md
/// section 16, "依存crateを必要最小限にする").
pub async fn run(
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    port_manager: PortManager,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "metrics endpoint started");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(error = %err, "failed to accept metrics connection");
                continue;
            }
        };
        let metrics = Arc::clone(&metrics);
        let port_manager = port_manager.clone();
        tokio::spawn(async move {
            if let Err(err) =
                handle_request(stream, &metrics, &port_manager, REQUEST_READ_TIMEOUT).await
            {
                tracing::debug!(error = %err, "metrics request failed");
            }
        });
    }
}

/// The one thing this minimal parser cares about from a request: its method and path. Header
/// lines are read (to keep the connection well-formed) but never inspected.
struct RequestLine {
    method: String,
    path: String,
}

enum ParsedRequest {
    /// A well-formed method + path was read.
    Ok(RequestLine),
    /// The client closed the connection before sending a complete request line.
    ConnectionClosed,
    /// The request line or headers didn't fit in the size caps, or the request line was empty --
    /// not a real I/O error, just not a request this server can answer.
    Malformed,
}

async fn handle_request(
    stream: TcpStream,
    metrics: &Metrics,
    port_manager: &PortManager,
    read_timeout: Duration,
) -> std::io::Result<()> {
    let mut conn = BufReader::new(stream);

    let parsed = match tokio::time::timeout(read_timeout, read_request(&mut conn)).await {
        Ok(result) => result?,
        // Took too long to send a complete request -- drop the connection without responding
        // rather than risk waiting on it indefinitely.
        Err(_) => return Ok(()),
    };

    match parsed {
        ParsedRequest::ConnectionClosed => Ok(()),
        ParsedRequest::Malformed => {
            write_response(&mut conn, "400 Bad Request", "text/plain", "").await
        }
        ParsedRequest::Ok(RequestLine { method, path }) => {
            if method != "GET" {
                return write_response(&mut conn, "405 Method Not Allowed", "text/plain", "").await;
            }
            if path != "/metrics" {
                return write_response(&mut conn, "404 Not Found", "text/plain", "").await;
            }

            let body = metrics.render(port_manager.active_count());
            write_response(&mut conn, "200 OK", "text/plain; version=0.0.4", &body).await
        }
    }
}

/// Reads and minimally parses one request: the request line, then every header line up to (and
/// including) the blank line that ends them -- headers are discarded unread, this endpoint has
/// no use for any of them. Bounded on every axis an untrusted peer controls: request-line bytes,
/// total header bytes, and (via the caller's `tokio::time::timeout`) wall-clock time.
async fn read_request(conn: &mut BufReader<TcpStream>) -> std::io::Result<ParsedRequest> {
    let mut request_line = String::new();
    let bytes_read = (&mut *conn)
        .take(MAX_REQUEST_LINE_BYTES as u64)
        .read_line(&mut request_line)
        .await?;
    if bytes_read == 0 {
        return Ok(ParsedRequest::ConnectionClosed);
    }
    if bytes_read == MAX_REQUEST_LINE_BYTES && !request_line.ends_with('\n') {
        return Ok(ParsedRequest::Malformed);
    }

    let mut header_budget = MAX_HEADER_BYTES;
    let mut header_line = String::new();
    loop {
        if header_budget == 0 {
            return Ok(ParsedRequest::Malformed);
        }
        header_line.clear();
        let cap = header_budget.min(MAX_REQUEST_LINE_BYTES);
        let n = (&mut *conn)
            .take(cap as u64)
            .read_line(&mut header_line)
            .await?;
        if n == 0 {
            // Connection closed after the request line but before the blank line that would
            // normally end the headers -- nothing more is coming; treat headers as done.
            break;
        }
        header_budget -= n;
        if n == cap && !header_line.ends_with('\n') {
            return Ok(ParsedRequest::Malformed);
        }
        if header_line == "\r\n" || header_line == "\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    if method.is_empty() {
        return Ok(ParsedRequest::Malformed);
    }

    Ok(ParsedRequest::Ok(RequestLine { method, path }))
}

async fn write_response(
    conn: &mut BufReader<TcpStream>,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );
    conn.get_mut().write_all(response.as_bytes()).await?;
    conn.get_mut().shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortRange;
    use tokio::io::AsyncReadExt;

    /// Binds an ephemeral listener, spawns a single `handle_request` call against the next
    /// connection it accepts (with a generous default timeout unless overridden), and returns
    /// the address a test should connect to as the "client" plus a handle to await the result.
    async fn spawn_handler_with_timeout(
        read_timeout: Duration,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = Metrics::new();
        let port_manager = PortManager::new(PortRange { start: 0, end: 0 });
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_request(stream, &metrics, &port_manager, read_timeout).await
        });
        (addr, handle)
    }

    async fn spawn_handler() -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        spawn_handler_with_timeout(Duration::from_secs(5)).await
    }

    #[tokio::test]
    async fn serves_metrics_on_get() {
        let (addr, handle) = spawn_handler().await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        handle.await.unwrap().unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(
            response.contains("ftp_gateway_sessions_active 0\n"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn rejects_unknown_path_with_404() {
        let (addr, handle) = spawn_handler().await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /other HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        handle.await.unwrap().unwrap();

        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn rejects_non_get_method_with_405() {
        let (addr, handle) = spawn_handler().await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"POST /metrics HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        handle.await.unwrap().unwrap();

        assert!(
            response.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn rejects_oversized_request_line_without_panicking() {
        let (addr, handle) = spawn_handler().await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        // No CRLF anywhere -- exceeds MAX_REQUEST_LINE_BYTES before a line terminator ever shows
        // up, the malformed-input case a naive `read_line` cap wouldn't itself catch.
        let oversized = vec![b'A'; MAX_REQUEST_LINE_BYTES + 1];
        client.write_all(&oversized).await.unwrap();
        client.shutdown().await.unwrap();

        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        handle.await.unwrap().unwrap();

        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn rejects_excessive_header_bytes_without_hanging() {
        let (addr, handle) = spawn_handler().await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /metrics HTTP/1.1\r\n")
            .await
            .unwrap();

        // Many small, well-formed header lines whose *combined* size exceeds MAX_HEADER_BYTES,
        // without ever sending the blank line that would end them -- must not loop forever.
        // Written from a background task, concurrently with this test reading the response
        // below: if the writes were sequenced strictly before the read (as a real slowloris
        // client sending in one burst might behave), a server that stops reading mid-stream
        // (exactly the behavior under test) could leave the writer blocked on a full send
        // buffer while nothing ever drains it -- a self-inflicted deadlock in the *test*, not
        // the server. Reading concurrently avoids that regardless of buffer sizes/scheduling.
        let (client_read_half, mut client_write_half) = client.into_split();
        let writer = tokio::spawn(async move {
            let header_line = b"X-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n";
            let lines_needed = MAX_HEADER_BYTES / header_line.len() + 2;
            for _ in 0..lines_needed {
                if client_write_half.write_all(header_line).await.is_err() {
                    break;
                }
            }
        });

        let mut response = String::new();
        let mut client_read_half = client_read_half;
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            client_read_half.read_to_string(&mut response),
        )
        .await
        .expect("must not hang reading the response");
        handle.await.unwrap().unwrap();
        let _ = writer.await;

        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn client_closing_immediately_is_handled_without_panicking() {
        let (addr, handle) = spawn_handler().await;

        let client = TcpStream::connect(addr).await.unwrap();
        drop(client);

        // No request line ever arrived -- nothing to respond to, and no panic.
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn slow_client_is_dropped_after_the_read_timeout() {
        let (addr, handle) = spawn_handler_with_timeout(Duration::from_millis(100)).await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        // Sends part of a request line and then goes silent -- a slowloris-style client that
        // never completes it. Must not hang past the configured read timeout.
        client.write_all(b"GET /metr").await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler must return once the read timeout elapses");
        result.unwrap().unwrap();

        // The server drops the connection outright rather than racing a response against an
        // already-uncooperative client; the socket closing (0-byte read) confirms that.
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.is_empty(), "{response:?}");
    }
}
