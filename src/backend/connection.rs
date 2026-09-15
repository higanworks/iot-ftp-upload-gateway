use std::time::Duration;

use tokio::io;
use tokio::net::TcpStream;

use crate::backend::dns_cache::DnsCache;
use crate::config::BackendConfig;

pub async fn connect(
    backend: &BackendConfig,
    timeout: Duration,
    dns_cache: &DnsCache,
) -> io::Result<TcpStream> {
    match tokio::time::timeout(timeout, connect_inner(backend, dns_cache)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "connection to backend timed out",
        )),
    }
}

/// Resolves `backend.host` (via `dns_cache`, which may answer from its cache without touching
/// the resolver) and tries each returned address in turn until one connects, matching the
/// multi-address behavior `TcpStream::connect` itself has when given a hostname directly.
async fn connect_inner(backend: &BackendConfig, dns_cache: &DnsCache) -> io::Result<TcpStream> {
    let addrs = dns_cache.resolve(&backend.host, backend.port).await?;

    let mut last_err: Option<io::Error> = None;
    for addr in &addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                // The control connection is a long-running series of small one-line
                // command/reply round trips; leaving Nagle's algorithm enabled would add its
                // characteristic latency to each one.
                if let Err(err) = stream.set_nodelay(true) {
                    tracing::debug!(error = %err, "failed to set TCP_NODELAY on backend connection");
                }
                return Ok(stream);
            }
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no addresses resolved for backend host",
        )
    }))
}
