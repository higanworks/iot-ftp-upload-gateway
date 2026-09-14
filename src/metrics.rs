use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide counters and gauges exposed via the `/metrics` HTTP endpoint
/// (`server::metrics_server`) in Prometheus text exposition format. Cheap to clone (an `Arc`
/// wrapper is expected at the call sites -- see `Metrics::new`) and safe to update concurrently
/// from many sessions at once; every field is a lock-free atomic.
pub struct Metrics {
    sessions_active: AtomicU64,
    uploads_active: AtomicU64,
    sessions_total: AtomicU64,
    upload_bytes_total: AtomicU64,
    connections_rejected_total: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Metrics> {
        Arc::new(Metrics {
            sessions_active: AtomicU64::new(0),
            uploads_active: AtomicU64::new(0),
            sessions_total: AtomicU64::new(0),
            upload_bytes_total: AtomicU64::new(0),
            connections_rejected_total: AtomicU64::new(0),
        })
    }

    /// Call once per accepted connection, before the session starts running. Returns a guard
    /// that decrements `sessions_active` back down when the session ends (success, error, or
    /// panic unwinding through the spawned task) -- the same RAII pattern already used by
    /// `PortManager`/`PasvPortGuard` and `IpConnectionLimiter`/`IpConnectionGuard`.
    pub fn session_started(self: &Arc<Self>) -> SessionGuard {
        saturating_add(&self.sessions_total, 1);
        self.sessions_active.fetch_add(1, Ordering::Relaxed);
        SessionGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Call once a `STOR` data transfer actually starts moving bytes (PASV data connection
    /// established on both sides). Returns a guard that decrements `uploads_active` back down
    /// when the transfer ends, regardless of outcome.
    pub fn upload_started(self: &Arc<Self>) -> UploadGuard {
        self.uploads_active.fetch_add(1, Ordering::Relaxed);
        UploadGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Adds to the running total of bytes successfully relayed from a client to a Backend
    /// across all `STOR` transfers so far. Saturates at `u64::MAX` rather than wrapping --
    /// astronomically unlikely to matter in practice, but a wrapped counter would silently drop
    /// back to a small value and misrepresent the true historical total, whereas saturation
    /// preserves the invariant that this counter never decreases.
    pub fn add_upload_bytes(&self, bytes: u64) {
        saturating_add(&self.upload_bytes_total, bytes);
    }

    /// Call each time a connection is turned away by `IpConnectionLimiter` before a session
    /// ever starts (PROJECT_SECURITY.md section 5, "Connection Exhaustion").
    pub fn connection_rejected(&self) {
        saturating_add(&self.connections_rejected_total, 1);
    }

    /// Renders every metric in Prometheus text exposition format. `pasv_ports_active` is passed
    /// in rather than tracked on `Metrics` itself, since `PortManager` already owns that count
    /// (`PortManager::active_count`) -- tracking it twice would risk the two drifting apart.
    pub fn render(&self, pasv_ports_active: usize) -> String {
        format!(
            "# HELP ftp_gateway_sessions_active Number of FTP control connections currently open.\n\
             # TYPE ftp_gateway_sessions_active gauge\n\
             ftp_gateway_sessions_active {sessions_active}\n\
             # HELP ftp_gateway_uploads_active Number of STOR transfers currently in progress.\n\
             # TYPE ftp_gateway_uploads_active gauge\n\
             ftp_gateway_uploads_active {uploads_active}\n\
             # HELP ftp_gateway_pasv_ports_active Number of PASV/EPSV data ports currently allocated.\n\
             # TYPE ftp_gateway_pasv_ports_active gauge\n\
             ftp_gateway_pasv_ports_active {pasv_ports_active}\n\
             # HELP ftp_gateway_sessions_total Total number of FTP control connections accepted.\n\
             # TYPE ftp_gateway_sessions_total counter\n\
             ftp_gateway_sessions_total {sessions_total}\n\
             # HELP ftp_gateway_upload_bytes_total Total bytes relayed from clients to a Backend across all completed STOR transfers.\n\
             # TYPE ftp_gateway_upload_bytes_total counter\n\
             ftp_gateway_upload_bytes_total {upload_bytes_total}\n\
             # HELP ftp_gateway_connections_rejected_total Total connections rejected by the per-IP connection limit.\n\
             # TYPE ftp_gateway_connections_rejected_total counter\n\
             ftp_gateway_connections_rejected_total {connections_rejected_total}\n",
            sessions_active = self.sessions_active.load(Ordering::Relaxed),
            uploads_active = self.uploads_active.load(Ordering::Relaxed),
            sessions_total = self.sessions_total.load(Ordering::Relaxed),
            upload_bytes_total = self.upload_bytes_total.load(Ordering::Relaxed),
            connections_rejected_total = self.connections_rejected_total.load(Ordering::Relaxed),
        )
    }
}

/// Adds `delta` to `counter`, saturating at `u64::MAX` instead of wrapping. Used by every
/// `*_total` counter on `Metrics`: under sustained load (the load-testing scenario this was
/// asked to hold up under) these are cumulative counters that must never decrease, wrap back to
/// a small value, or panic once `u64::MAX` is reached -- they just stop climbing.
/// `AtomicU64::fetch_add` alone would never panic either (integer overflow on atomics always
/// wraps, even in debug builds), but wrapping back to a tiny number would misrepresent the true
/// historical total and look like a nonsensical counter reset to anything scraping it.
fn saturating_add(counter: &AtomicU64, delta: u64) {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(delta))
        })
        .ok();
}

/// Decrements `sessions_active` on drop -- held for the lifetime of one session.
pub struct SessionGuard {
    metrics: Arc<Metrics>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.metrics.sessions_active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Decrements `uploads_active` on drop -- held for the duration of one `STOR` data transfer.
pub struct UploadGuard {
    metrics: Arc<Metrics>,
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        self.metrics.uploads_active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_guard_tracks_active_and_total() {
        let metrics = Metrics::new();
        let guard = metrics.session_started();
        assert_eq!(metrics.sessions_active.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.sessions_total.load(Ordering::Relaxed), 1);

        drop(guard);
        assert_eq!(metrics.sessions_active.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.sessions_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn upload_guard_tracks_active_uploads() {
        let metrics = Metrics::new();
        let guard = metrics.upload_started();
        assert_eq!(metrics.uploads_active.load(Ordering::Relaxed), 1);

        drop(guard);
        assert_eq!(metrics.uploads_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn add_upload_bytes_accumulates() {
        let metrics = Metrics::new();
        metrics.add_upload_bytes(100);
        metrics.add_upload_bytes(250);
        assert_eq!(metrics.upload_bytes_total.load(Ordering::Relaxed), 350);
    }

    #[test]
    fn add_upload_bytes_saturates_instead_of_wrapping() {
        let metrics = Metrics::new();
        metrics.add_upload_bytes(u64::MAX - 10);
        metrics.add_upload_bytes(1000);
        assert_eq!(metrics.upload_bytes_total.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn add_upload_bytes_holds_at_u64_max_on_further_additions() {
        let metrics = Metrics::new();
        metrics
            .upload_bytes_total
            .store(u64::MAX, Ordering::Relaxed);
        metrics.add_upload_bytes(1);
        assert_eq!(metrics.upload_bytes_total.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn connection_rejected_increments() {
        let metrics = Metrics::new();
        metrics.connection_rejected();
        metrics.connection_rejected();
        assert_eq!(
            metrics.connections_rejected_total.load(Ordering::Relaxed),
            2
        );
    }

    #[test]
    fn connections_rejected_total_saturates_instead_of_wrapping() {
        let metrics = Metrics::new();
        metrics
            .connections_rejected_total
            .store(u64::MAX, Ordering::Relaxed);
        metrics.connection_rejected();
        assert_eq!(
            metrics.connections_rejected_total.load(Ordering::Relaxed),
            u64::MAX
        );
    }

    #[test]
    fn sessions_total_saturates_instead_of_wrapping() {
        let metrics = Metrics::new();
        metrics
            .sessions_total
            .store(u64::MAX - 1, Ordering::Relaxed);
        let _guard1 = metrics.session_started();
        let _guard2 = metrics.session_started();
        assert_eq!(metrics.sessions_total.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn saturating_add_helper_never_wraps_past_u64_max() {
        let counter = AtomicU64::new(u64::MAX);
        saturating_add(&counter, u64::MAX);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn render_produces_expected_prometheus_text() {
        let metrics = Metrics::new();
        let _session = metrics.session_started();
        let _upload = metrics.upload_started();
        metrics.add_upload_bytes(42);
        metrics.connection_rejected();

        let rendered = metrics.render(3);
        assert!(rendered.contains("ftp_gateway_sessions_active 1\n"));
        assert!(rendered.contains("ftp_gateway_uploads_active 1\n"));
        assert!(rendered.contains("ftp_gateway_pasv_ports_active 3\n"));
        assert!(rendered.contains("ftp_gateway_sessions_total 1\n"));
        assert!(rendered.contains("ftp_gateway_upload_bytes_total 42\n"));
        assert!(rendered.contains("ftp_gateway_connections_rejected_total 1\n"));
    }
}
