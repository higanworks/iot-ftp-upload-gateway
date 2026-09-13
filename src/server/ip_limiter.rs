use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// Caps how many concurrent connections a single client IP may hold open, so one misbehaving or
/// malicious source can't exhaust the gateway's file descriptors/memory on its own
/// (PROJECT_SECURITY.md section 5, "Connection Exhaustion"). A limit of `0` disables the check
/// entirely -- `try_acquire` always succeeds -- restoring the gateway's original behavior of
/// imposing no artificial concurrency cap.
#[derive(Clone)]
pub struct IpConnectionLimiter {
    counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
    max_per_ip: usize,
}

impl IpConnectionLimiter {
    pub fn new(max_per_ip: usize) -> Self {
        IpConnectionLimiter {
            counts: Arc::new(Mutex::new(HashMap::new())),
            max_per_ip,
        }
    }

    /// Attempts to reserve one connection slot for `ip`. Returns `None` if `ip` already holds
    /// `max_per_ip` connections.
    pub fn try_acquire(&self, ip: IpAddr) -> Option<IpConnectionGuard> {
        if self.max_per_ip == 0 {
            return Some(IpConnectionGuard { counts: None, ip });
        }

        let mut counts = self.counts.lock().unwrap();
        let count = counts.entry(ip).or_insert(0);
        if *count >= self.max_per_ip {
            return None;
        }
        *count += 1;
        Some(IpConnectionGuard {
            counts: Some(Arc::clone(&self.counts)),
            ip,
        })
    }
}

/// Releases the reserved slot when dropped, i.e. when the session it was created for ends
/// (successfully, with an error, or via a panic unwinding through the spawned task).
pub struct IpConnectionGuard {
    counts: Option<Arc<Mutex<HashMap<IpAddr, usize>>>>,
    ip: IpAddr,
}

impl Drop for IpConnectionGuard {
    fn drop(&mut self) {
        let Some(counts) = &self.counts else {
            return;
        };
        let mut counts = counts.lock().unwrap();
        if let Some(count) = counts.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(octet: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, octet])
    }

    #[test]
    fn disabled_when_limit_is_zero() {
        let limiter = IpConnectionLimiter::new(0);
        let guards: Vec<_> = (0..100).map(|_| limiter.try_acquire(ip(1))).collect();
        assert!(guards.into_iter().all(|g| g.is_some()));
    }

    #[test]
    fn allows_up_to_the_limit_then_rejects() {
        let limiter = IpConnectionLimiter::new(2);
        let _first = limiter
            .try_acquire(ip(1))
            .expect("first connection allowed");
        let _second = limiter
            .try_acquire(ip(1))
            .expect("second connection allowed");
        assert!(limiter.try_acquire(ip(1)).is_none());
    }

    #[test]
    fn releasing_a_guard_frees_a_slot() {
        let limiter = IpConnectionLimiter::new(1);
        let first = limiter
            .try_acquire(ip(1))
            .expect("first connection allowed");
        assert!(limiter.try_acquire(ip(1)).is_none());

        drop(first);
        assert!(limiter.try_acquire(ip(1)).is_some());
    }

    #[test]
    fn tracks_different_ips_independently() {
        let limiter = IpConnectionLimiter::new(1);
        let _first = limiter.try_acquire(ip(1)).expect("ip 1 allowed");
        assert!(
            limiter.try_acquire(ip(2)).is_some(),
            "a different IP should have its own independent count"
        );
    }
}
