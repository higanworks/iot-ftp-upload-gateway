use std::io;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::protocol::reply::parse_pasv_reply;

/// Asks the backend to open a passive data connection and connects to the address/port
/// it returns. The Gateway itself acts as a PASV client toward the backend here.
pub async fn open_backend_data_connection(
    backend_control: &mut BufReader<TcpStream>,
) -> anyhow::Result<TcpStream> {
    backend_control.write_all(b"PASV\r\n").await?;

    let mut line = String::new();
    let bytes_read = backend_control.read_line(&mut line).await?;
    if bytes_read == 0 {
        bail!("backend closed the control connection while opening a data connection");
    }

    let (ip, port) = parse_pasv_reply(&line)
        .ok_or_else(|| anyhow!("backend returned an unparsable PASV reply: {line:?}"))?;

    TcpStream::connect((ip, port))
        .await
        .context("failed to connect to backend data port")
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
    backend_data: &mut TcpStream,
    idle_timeout: Duration,
) -> io::Result<(u64, u64)> {
    let (mut client_read, mut client_write) = client_data.split();
    let (mut backend_read, mut backend_write) = backend_data.split();

    tokio::try_join!(
        copy_with_idle_timeout(&mut client_read, &mut backend_write, idle_timeout),
        copy_with_idle_timeout(&mut backend_read, &mut client_write, idle_timeout),
    )
}

/// Copies from `reader` to `writer` until EOF, shutting down `writer` when the source is
/// exhausted (preserving TCP half-close: the other direction of the pair keeps running
/// independently). Returns a `TimedOut` error if no byte is read within `idle_timeout`.
async fn copy_with_idle_timeout<R, W>(
    reader: &mut R,
    writer: &mut W,
    idle_timeout: Duration,
) -> io::Result<u64>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = [0u8; 8192];
    let mut total = 0u64;
    loop {
        let read = match tokio::time::timeout(idle_timeout, reader.read(&mut buf)).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "data connection idle timeout",
                ));
            }
        };
        if read == 0 {
            writer.shutdown().await?;
            return Ok(total);
        }
        writer.write_all(&buf[..read]).await?;
        total += read as u64;
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
        let mut backend_control = BufReader::new(control_stream);

        let data_stream = open_backend_data_connection(&mut backend_control)
            .await
            .unwrap();
        assert_eq!(data_stream.peer_addr().unwrap().port(), data_port);
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
        let mut backend_data = accept_backend.await.unwrap();

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
        let mut backend_data = accept_backend.await.unwrap();

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
