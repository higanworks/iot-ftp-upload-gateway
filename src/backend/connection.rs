use std::time::Duration;

use tokio::io;
use tokio::net::TcpStream;

use crate::config::BackendConfig;

pub async fn connect(backend: &BackendConfig, timeout: Duration) -> io::Result<TcpStream> {
    match tokio::time::timeout(
        timeout,
        TcpStream::connect((backend.host.as_str(), backend.port)),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "connection to backend timed out",
        )),
    }
}
