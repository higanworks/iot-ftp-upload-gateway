use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;

use crate::config::PortRange;

/// Manages the PASV data port pool. Picks a free port from the configured range
/// and only hands out ports that could actually be bound.
///
/// Always binds on `0.0.0.0`, independent of the address advertised to clients in the PASV
/// reply (`PassiveConfig::address`). Behind NAT or in a container (e.g. AWS ECS, Docker with
/// published ports), the address reachable by clients is rarely the same address the process
/// should bind on inside its own network namespace — binding to the advertised address would
/// make the listener unreachable for exactly the deployments this gateway targets.
#[derive(Clone)]
pub struct PortManager {
    in_use: Arc<Mutex<HashSet<u16>>>,
    range: PortRange,
}

impl PortManager {
    pub fn new(range: PortRange) -> Self {
        PortManager {
            in_use: Arc::new(Mutex::new(HashSet::new())),
            range,
        }
    }

    /// Finds a free port and binds it. Returns None if the range has no free port.
    pub async fn allocate(&self) -> Option<PasvPortGuard> {
        for port in self.range.start..=self.range.end {
            let reserved = {
                let mut in_use = self.in_use.lock().unwrap();
                in_use.insert(port)
            };
            if !reserved {
                continue;
            }

            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
            match TcpListener::bind(addr).await {
                Ok(listener) => {
                    return Some(PasvPortGuard {
                        in_use: Arc::clone(&self.in_use),
                        port,
                        listener,
                    });
                }
                Err(_) => {
                    // Likely taken by another process. Release the reservation and try the next candidate.
                    self.in_use.lock().unwrap().remove(&port);
                }
            }
        }
        None
    }
}

/// An allocated PASV port. Automatically released back to the pool when dropped.
pub struct PasvPortGuard {
    in_use: Arc<Mutex<HashSet<u16>>>,
    port: u16,
    listener: TcpListener,
}

impl PasvPortGuard {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }
}

impl Drop for PasvPortGuard {
    fn drop(&mut self) {
        self.in_use.lock().unwrap().remove(&self.port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allocates_port_within_range() {
        let manager = PortManager::new(PortRange {
            start: 19100,
            end: 19110,
        });
        let guard = manager.allocate().await.expect("port available");
        assert!((19100..=19110).contains(&guard.port()));
    }

    #[tokio::test]
    async fn allocates_distinct_ports_concurrently() {
        let manager = PortManager::new(PortRange {
            start: 19200,
            end: 19210,
        });
        let guard1 = manager.allocate().await.expect("first port available");
        let guard2 = manager.allocate().await.expect("second port available");
        assert_ne!(guard1.port(), guard2.port());
    }

    #[tokio::test]
    async fn releases_port_on_drop() {
        let manager = PortManager::new(PortRange {
            start: 19300,
            end: 19300,
        });
        let guard = manager.allocate().await.expect("port available");
        let port = guard.port();
        drop(guard);

        let guard2 = manager
            .allocate()
            .await
            .expect("port available again after release");
        assert_eq!(guard2.port(), port);
    }

    #[tokio::test]
    async fn returns_none_when_pool_exhausted() {
        let manager = PortManager::new(PortRange {
            start: 19400,
            end: 19400,
        });
        let _guard = manager.allocate().await.expect("first allocation succeeds");
        let second = manager.allocate().await;
        assert!(second.is_none());
    }
}
