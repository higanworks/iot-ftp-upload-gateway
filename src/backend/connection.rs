use std::time::Duration;

use tokio::io;
use tokio::net::TcpStream;

use crate::config::BackendConfig;

pub async fn connect(backend: &BackendConfig, timeout: Duration) -> io::Result<TcpStream> {
    let stream = match tokio::time::timeout(
        timeout,
        TcpStream::connect((backend.host.as_str(), backend.port)),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection to backend timed out",
            ));
        }
    };
    // The control connection is a long-running series of small one-line command/reply
    // round trips; leaving Nagle's algorithm enabled would add its characteristic latency to
    // each one.
    if let Err(err) = stream.set_nodelay(true) {
        tracing::debug!(error = %err, "failed to set TCP_NODELAY on backend connection");
    }
    Ok(stream)
}
