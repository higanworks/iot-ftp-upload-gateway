use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
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
    next_offset: Arc<AtomicU32>,
}

impl PortManager {
    pub fn new(range: PortRange) -> Self {
        PortManager {
            in_use: Arc::new(Mutex::new(HashSet::new())),
            range,
            next_offset: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Finds a free port and binds it. Returns None if the range has no free port.
    ///
    /// Starts the search just after wherever the previous call left off (wrapping around the
    /// range) rather than always rescanning from `range.start`: under a busy pool, always
    /// starting at the front would mean every allocation re-walks however many ports at the
    /// front are already taken before reaching a free one, and that walk (a mutex lock plus a
    /// bind attempt per candidate) gets more expensive the busier the pool is. A rotating start
    /// point keeps each allocation's search close to O(1) in the common case instead.
    pub async fn allocate(&self) -> Option<PasvPortGuard> {
        let range_size = self.range.end as u32 - self.range.start as u32 + 1;

        for _ in 0..range_size {
            let offset = self.next_offset.fetch_add(1, Ordering::Relaxed) % range_size;
            let port = self.range.start + offset as u16;

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

    /// Number of PASV/EPSV ports currently allocated, for the `ftp_gateway_pasv_ports_active`
    /// metric (`src/metrics.rs`).
    pub fn active_count(&self) -> usize {
        self.in_use.lock().unwrap().len()
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
    async fn active_count_tracks_allocation_and_release() {
        let manager = PortManager::new(PortRange {
            start: 19500,
            end: 19502,
        });
        assert_eq!(manager.active_count(), 0);

        let guard1 = manager.allocate().await.expect("first port available");
        assert_eq!(manager.active_count(), 1);

        let guard2 = manager.allocate().await.expect("second port available");
        assert_eq!(manager.active_count(), 2);

        drop(guard1);
        assert_eq!(manager.active_count(), 1);

        drop(guard2);
        assert_eq!(manager.active_count(), 0);
    }

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
    async fn rotates_start_point_instead_of_always_restarting_from_range_start() {
        let manager = PortManager::new(PortRange {
            start: 19600,
            end: 19602,
        });

        let guard1 = manager.allocate().await.expect("first port available");
        assert_eq!(guard1.port(), 19600);

        let guard2 = manager.allocate().await.expect("second port available");
        assert_eq!(guard2.port(), 19601);

        drop(guard1);

        // With 19600 free again, a "rescan from range.start every time" allocator would hand it
        // right back out. The rotating cursor instead keeps moving forward to 19602.
        let guard3 = manager.allocate().await.expect("third port available");
        assert_eq!(guard3.port(), 19602);
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
