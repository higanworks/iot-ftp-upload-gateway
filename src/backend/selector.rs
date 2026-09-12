use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::BackendConfig;

/// Picks a backend using round-robin. A session selects its backend once when its control
/// connection is established and keeps using that same `BackendConfig` for the rest of the
/// session (including any PASV data connections), so round-robin here is what guarantees a
/// session's control and data connections always land on the same backend.
#[derive(Clone)]
pub struct BackendSelector {
    backends: Arc<Vec<BackendConfig>>,
    next: Arc<AtomicUsize>,
}

impl BackendSelector {
    /// Panics if `backends` is empty; `Config::load()` already guarantees this never happens
    /// for a validated configuration.
    pub fn new(backends: Vec<BackendConfig>) -> Self {
        assert!(
            !backends.is_empty(),
            "BackendSelector requires at least one backend"
        );
        BackendSelector {
            backends: Arc::new(backends),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn next(&self) -> BackendConfig {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.backends.len();
        self.backends[index].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(host: &str) -> BackendConfig {
        BackendConfig {
            host: host.to_string(),
            port: 21,
        }
    }

    #[test]
    fn cycles_through_backends_in_order() {
        let selector = BackendSelector::new(vec![backend("a"), backend("b"), backend("c")]);
        assert_eq!(selector.next().host, "a");
        assert_eq!(selector.next().host, "b");
        assert_eq!(selector.next().host, "c");
        assert_eq!(selector.next().host, "a");
    }

    #[test]
    fn single_backend_always_returned() {
        let selector = BackendSelector::new(vec![backend("only")]);
        assert_eq!(selector.next().host, "only");
        assert_eq!(selector.next().host, "only");
    }
}
