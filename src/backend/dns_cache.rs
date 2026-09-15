use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a resolved set of addresses is reused before the next connection attempt triggers a
/// fresh lookup. A flat TTL rather than honoring the resolver's actual DNS TTL -- simpler, and
/// still cuts the resolution cost for the common case of many short-lived sessions arriving in
/// a burst, while keeping backend IP changes (failover, rotation) visible within a bounded delay.
const DNS_CACHE_TTL: Duration = Duration::from_secs(30);

/// Caches `host:port` -> resolved addresses so repeated backend connections (one per FTP
/// session) don't each pay for a fresh DNS lookup. Shared across all sessions; cheap to clone
/// (an `Arc` wrapper).
#[derive(Clone)]
pub struct DnsCache {
    entries: Arc<Mutex<HashMap<String, CacheEntry>>>,
}

struct CacheEntry {
    addrs: Vec<SocketAddr>,
    resolved_at: Instant,
}

impl DnsCache {
    pub fn new() -> Self {
        DnsCache {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolves `host:port`, reusing a cached result if it was looked up within
    /// `DNS_CACHE_TTL`. Returns every address the resolver reported, in the order it reported
    /// them, matching `ToSocketAddrs`' multi-address behavior (try each in turn until one
    /// connects).
    pub async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let key = format!("{host}:{port}");

        if let Some(addrs) = self.cached(&key) {
            return Ok(addrs);
        }

        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
        self.entries.lock().unwrap().insert(
            key,
            CacheEntry {
                addrs: addrs.clone(),
                resolved_at: Instant::now(),
            },
        );
        Ok(addrs)
    }

    fn cached(&self, key: &str) -> Option<Vec<SocketAddr>> {
        let entries = self.entries.lock().unwrap();
        let entry = entries.get(key)?;
        if entry.resolved_at.elapsed() < DNS_CACHE_TTL {
            Some(entry.addrs.clone())
        } else {
            None
        }
    }
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_and_caches_localhost() {
        let cache = DnsCache::new();
        let first = cache.resolve("localhost", 21).await.unwrap();
        assert!(!first.is_empty());

        // Second call within the TTL must hit the cache and return the same result rather than
        // resolving again -- there's no direct way to observe "did a lookup happen" from outside,
        // so this just pins down that the cached value is returned unchanged.
        let second = cache.resolve("localhost", 21).await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn expired_entry_is_not_reused() {
        let cache = DnsCache::new();
        let key = "localhost:21".to_string();
        cache.entries.lock().unwrap().insert(
            key,
            CacheEntry {
                addrs: vec!["203.0.113.1:21".parse().unwrap()],
                resolved_at: Instant::now() - DNS_CACHE_TTL - Duration::from_secs(1),
            },
        );

        let resolved = cache.resolve("localhost", 21).await.unwrap();
        assert_ne!(resolved, vec!["203.0.113.1:21".parse().unwrap()]);
    }

    #[tokio::test]
    async fn fresh_entry_short_circuits_lookup() {
        let cache = DnsCache::new();
        let key = "some.internal.host:21".to_string();
        let stub_addr: SocketAddr = "203.0.113.1:21".parse().unwrap();
        cache.entries.lock().unwrap().insert(
            key,
            CacheEntry {
                addrs: vec![stub_addr],
                resolved_at: Instant::now(),
            },
        );

        // "some.internal.host" isn't a resolvable name -- if this reached the resolver instead
        // of the cache, it would return an error rather than the stubbed address.
        let resolved = cache.resolve("some.internal.host", 21).await.unwrap();
        assert_eq!(resolved, vec![stub_addr]);
    }
}
