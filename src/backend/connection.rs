use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io;
use tokio::net::{TcpSocket, TcpStream};

use crate::backend::dns_cache::DnsCache;
use crate::config::BackendConfig;

/// Connects to `backend`. With `source`, the connection is made from that local address (see
/// `connect_from`) instead of whichever one the OS would pick.
pub async fn connect(
    backend: &BackendConfig,
    timeout: Duration,
    dns_cache: &DnsCache,
    source: Option<Ipv4Addr>,
) -> io::Result<TcpStream> {
    match tokio::time::timeout(timeout, connect_inner(backend, dns_cache, source)).await {
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
async fn connect_inner(
    backend: &BackendConfig,
    dns_cache: &DnsCache,
    source: Option<Ipv4Addr>,
) -> io::Result<TcpStream> {
    let mut addrs = dns_cache.resolve(&backend.host, backend.port).await?;
    if source.is_some() {
        // An IPv4 source address can only reach IPv4 destinations.
        addrs.retain(SocketAddr::is_ipv4);
    }

    let mut last_err: Option<io::Error> = None;
    for addr in &addrs {
        match connect_from(*addr, source).await {
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

/// Opens a TCP connection to `addr`, from the local address `source` when given (the OS picks
/// the source address otherwise). Binding fails with `AddrNotAvailable` if `source` is not an
/// address of this host, which the caller treats as that source being unusable.
pub async fn connect_from(addr: SocketAddr, source: Option<Ipv4Addr>) -> io::Result<TcpStream> {
    let Some(source) = source else {
        return TcpStream::connect(addr).await;
    };
    if !addr.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "cannot connect to an IPv6 destination from an IPv4 source address",
        ));
    }
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::new(source.into(), 0))?;
    socket.connect(addr).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use tokio::net::TcpListener;

    fn backend_at(addr: SocketAddr) -> BackendConfig {
        BackendConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            ..BackendConfig::default()
        }
    }

    async fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    #[tokio::test]
    async fn connects_from_the_given_source_address() {
        let (listener, addr) = listener().await;
        let stream = connect(
            &backend_at(addr),
            Duration::from_secs(5),
            &DnsCache::new(),
            Some(Ipv4Addr::LOCALHOST),
        )
        .await
        .unwrap();
        let (accepted, peer) = listener.accept().await.unwrap();
        drop(accepted);
        assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(
            stream.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    /// Linux treats all of 127.0.0.0/8 as local, so a second loopback address is a second source
    /// the listener can tell apart. (macOS only has 127.0.0.1 unless aliased.)
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_peer_sees_the_chosen_source_address() {
        let (listener, addr) = listener().await;
        for source in [Ipv4Addr::new(127, 0, 0, 2), Ipv4Addr::new(127, 0, 0, 3)] {
            let _stream = connect(
                &backend_at(addr),
                Duration::from_secs(5),
                &DnsCache::new(),
                Some(source),
            )
            .await
            .unwrap();
            let (_accepted, peer) = listener.accept().await.unwrap();
            assert_eq!(peer.ip(), IpAddr::V4(source));
        }
    }

    #[tokio::test]
    async fn a_source_address_the_host_does_not_have_fails_to_bind() {
        let (_listener, addr) = listener().await;
        // TEST-NET-3 (RFC 5737): never assigned to a real interface.
        let err = connect(
            &backend_at(addr),
            Duration::from_secs(5),
            &DnsCache::new(),
            Some(Ipv4Addr::new(203, 0, 113, 9)),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable, "{err}");
    }

    #[tokio::test]
    async fn without_a_source_the_os_chooses() {
        let (listener, addr) = listener().await;
        let _stream = connect(
            &backend_at(addr),
            Duration::from_secs(5),
            &DnsCache::new(),
            None,
        )
        .await
        .unwrap();
        let (_accepted, peer) = listener.accept().await.unwrap();
        assert!(peer.ip().is_loopback());
    }

    #[tokio::test]
    async fn an_ipv6_only_backend_cannot_be_reached_from_an_ipv4_source() {
        let backend = BackendConfig {
            host: "::1".to_string(),
            port: 21,
            ..BackendConfig::default()
        };
        let err = connect(
            &backend,
            Duration::from_secs(5),
            &DnsCache::new(),
            Some(Ipv4Addr::LOCALHOST),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable, "{err}");
    }
}
