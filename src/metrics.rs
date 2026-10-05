use std::fmt::Write as _;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::backend::rotator::{RotatorSnapshot, SourceTransferStat};

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
    uploads_started_total: AtomicU64,
    uploads_completed_total: AtomicU64,
    uploads_failed_total: AtomicU64,
    backend_pasv_failures_total: AtomicU64,
    backend_data_connection_failures_total: AtomicU64,
    data_connections_rejected_total: AtomicU64,
    backend_timeouts_total: AtomicU64,
    session_idle_timeouts_total: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Metrics> {
        Arc::new(Metrics {
            sessions_active: AtomicU64::new(0),
            uploads_active: AtomicU64::new(0),
            sessions_total: AtomicU64::new(0),
            upload_bytes_total: AtomicU64::new(0),
            connections_rejected_total: AtomicU64::new(0),
            uploads_started_total: AtomicU64::new(0),
            uploads_completed_total: AtomicU64::new(0),
            uploads_failed_total: AtomicU64::new(0),
            backend_pasv_failures_total: AtomicU64::new(0),
            backend_data_connection_failures_total: AtomicU64::new(0),
            data_connections_rejected_total: AtomicU64::new(0),
            backend_timeouts_total: AtomicU64::new(0),
            session_idle_timeouts_total: AtomicU64::new(0),
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

    /// Call when an upload gets as far as the `STOR` being sent to the Backend (both data
    /// connections are up). Counts it as started; it counts as completed only if the returned
    /// guard's `succeeded` is called, and as failed in every other case -- an error return, a
    /// `break`, a panic -- because the guard is what records the outcome when it is dropped, so
    /// no exit path can forget to.
    pub fn upload_attempted(self: &Arc<Self>) -> UploadAttempt {
        saturating_add(&self.uploads_started_total, 1);
        UploadAttempt {
            metrics: Arc::clone(self),
            succeeded: AtomicBool::new(false),
        }
    }

    /// The Backend gave no usable reply to `PASV` (none in time, cut off, too long, or not a
    /// `PASV` address).
    pub fn backend_pasv_failed(&self) {
        saturating_add(&self.backend_pasv_failures_total, 1);
    }

    /// The gateway could not complete a data connection to the Backend: connecting to the data
    /// port failed, or (with backend FTPS) the data connection's TLS handshake did.
    pub fn backend_data_connection_failed(&self) {
        saturating_add(&self.backend_data_connection_failures_total, 1);
    }

    /// A data connection to a PASV port came from a different IP address than the client's
    /// control connection and was closed (`limits.require_data_ip_match`).
    pub fn data_connection_rejected(&self) {
        saturating_add(&self.data_connections_rejected_total, 1);
    }

    /// A control session reached its idle timeout and was closed.
    pub fn session_idle_timed_out(&self) {
        saturating_add(&self.session_idle_timeouts_total, 1);
    }

    /// Call with any I/O error talking to the Backend; counts it if it was a timeout.
    pub fn observe_backend_io_error(&self, err: &io::Error) {
        if err.kind() == io::ErrorKind::TimedOut {
            saturating_add(&self.backend_timeouts_total, 1);
        }
    }

    /// Renders every metric in Prometheus text exposition format. The PASV port counts are
    /// passed in rather than tracked on `Metrics` itself, since `PortManager` already owns them
    /// (`PortManager::active_count` / `capacity`) -- tracking them twice would risk the two
    /// drifting apart. `rotation` is the `SourceRotator`'s snapshot when source rotation is on;
    /// the per-source metrics are left out entirely otherwise.
    pub fn render(
        &self,
        pasv_ports_active: usize,
        pasv_ports_capacity: usize,
        rotation: Option<&RotatorSnapshot>,
    ) -> String {
        let mut out = self.render_gateway_metrics(pasv_ports_active, pasv_ports_capacity);
        if let Some(rotation) = rotation {
            render_rotation_metrics(&mut out, rotation);
        }
        out
    }

    fn render_gateway_metrics(
        &self,
        pasv_ports_active: usize,
        pasv_ports_capacity: usize,
    ) -> String {
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
             # HELP ftp_gateway_pasv_ports_capacity Number of PASV/EPSV data ports in the configured range (capacity minus active is what is left).\n\
             # TYPE ftp_gateway_pasv_ports_capacity gauge\n\
             ftp_gateway_pasv_ports_capacity {pasv_ports_capacity}\n\
             # HELP ftp_gateway_sessions_total Total number of FTP control connections accepted.\n\
             # TYPE ftp_gateway_sessions_total counter\n\
             ftp_gateway_sessions_total {sessions_total}\n\
             # HELP ftp_gateway_upload_bytes_total Total bytes relayed from clients to a Backend across all completed STOR transfers.\n\
             # TYPE ftp_gateway_upload_bytes_total counter\n\
             ftp_gateway_upload_bytes_total {upload_bytes_total}\n\
             # HELP ftp_gateway_connections_rejected_total Total connections rejected by the per-IP connection limit.\n\
             # TYPE ftp_gateway_connections_rejected_total counter\n\
             ftp_gateway_connections_rejected_total {connections_rejected_total}\n\
             # HELP ftp_gateway_uploads_started_total Total STOR transfers that reached the Backend (both data connections up).\n\
             # TYPE ftp_gateway_uploads_started_total counter\n\
             ftp_gateway_uploads_started_total {uploads_started_total}\n\
             # HELP ftp_gateway_uploads_completed_total Total STOR transfers the Backend confirmed with a 2xx reply.\n\
             # TYPE ftp_gateway_uploads_completed_total counter\n\
             ftp_gateway_uploads_completed_total {uploads_completed_total}\n\
             # HELP ftp_gateway_uploads_failed_total Total started STOR transfers that did not complete.\n\
             # TYPE ftp_gateway_uploads_failed_total counter\n\
             ftp_gateway_uploads_failed_total {uploads_failed_total}\n\
             # HELP ftp_gateway_backend_pasv_failures_total Total times the Backend gave no usable reply to PASV.\n\
             # TYPE ftp_gateway_backend_pasv_failures_total counter\n\
             ftp_gateway_backend_pasv_failures_total {backend_pasv_failures_total}\n\
             # HELP ftp_gateway_backend_data_connection_failures_total Total failures to establish a data connection to the Backend (TCP connect or TLS handshake).\n\
             # TYPE ftp_gateway_backend_data_connection_failures_total counter\n\
             ftp_gateway_backend_data_connection_failures_total {backend_data_connection_failures_total}\n\
             # HELP ftp_gateway_data_connections_rejected_total Total client data connections closed for coming from a different IP address than the control connection.\n\
             # TYPE ftp_gateway_data_connections_rejected_total counter\n\
             ftp_gateway_data_connections_rejected_total {data_connections_rejected_total}\n\
             # HELP ftp_gateway_backend_timeouts_total Total timeouts waiting on or connecting to a Backend.\n\
             # TYPE ftp_gateway_backend_timeouts_total counter\n\
             ftp_gateway_backend_timeouts_total {backend_timeouts_total}\n\
             # HELP ftp_gateway_session_idle_timeouts_total Total control sessions closed for being idle too long.\n\
             # TYPE ftp_gateway_session_idle_timeouts_total counter\n\
             ftp_gateway_session_idle_timeouts_total {session_idle_timeouts_total}\n",
            sessions_active = self.sessions_active.load(Ordering::Relaxed),
            uploads_active = self.uploads_active.load(Ordering::Relaxed),
            sessions_total = self.sessions_total.load(Ordering::Relaxed),
            upload_bytes_total = self.upload_bytes_total.load(Ordering::Relaxed),
            connections_rejected_total = self.connections_rejected_total.load(Ordering::Relaxed),
            uploads_started_total = self.uploads_started_total.load(Ordering::Relaxed),
            uploads_completed_total = self.uploads_completed_total.load(Ordering::Relaxed),
            uploads_failed_total = self.uploads_failed_total.load(Ordering::Relaxed),
            backend_pasv_failures_total = self.backend_pasv_failures_total.load(Ordering::Relaxed),
            backend_data_connection_failures_total = self
                .backend_data_connection_failures_total
                .load(Ordering::Relaxed),
            data_connections_rejected_total =
                self.data_connections_rejected_total.load(Ordering::Relaxed),
            backend_timeouts_total = self.backend_timeouts_total.load(Ordering::Relaxed),
            session_idle_timeouts_total = self.session_idle_timeouts_total.load(Ordering::Relaxed),
        )
    }
}

/// Appends the per-source-address metrics. The number of series is bounded by the number of
/// source addresses on the host times the number of configured backends, never by anything a
/// client controls.
fn render_rotation_metrics(out: &mut String, rotation: &RotatorSnapshot) {
    // Writing to a `String` cannot fail.
    out.push_str(
        "# HELP ftp_gateway_backend_source_unhealthy 1 if the source address is currently left out of the rotation after failing to connect.\n\
         # TYPE ftp_gateway_backend_source_unhealthy gauge\n",
    );
    for health in &rotation.sources {
        let _ = writeln!(
            out,
            "ftp_gateway_backend_source_unhealthy{{source=\"{}\"}} {}",
            health.source,
            u8::from(health.unhealthy)
        );
    }

    let series = [
        TransferSeries {
            name: "ftp_gateway_backend_source_transfers_active",
            kind: "gauge",
            help: "Data transfers in flight from this source address to this backend.",
            value: |stat| stat.active as u64,
        },
        TransferSeries {
            name: "ftp_gateway_backend_source_transfers_capacity",
            kind: "gauge",
            help: "Most data transfers this source address may have in flight to this backend (its passive_ports size).",
            value: |stat| stat.capacity as u64,
        },
        TransferSeries {
            name: "ftp_gateway_backend_source_transfers_total",
            kind: "counter",
            help: "Total data transfers started from this source address to this backend.",
            value: |stat| stat.started_total,
        },
        TransferSeries {
            name: "ftp_gateway_backend_source_slot_timeouts_total",
            kind: "counter",
            help: "Total transfers that gave up waiting for a free slot on this source address (answered 425).",
            value: |stat| stat.slot_timeouts_total,
        },
    ];
    for TransferSeries {
        name,
        kind,
        help,
        value,
    } in series
    {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {kind}");
        for stat in &rotation.transfers {
            let _ = writeln!(
                out,
                "{name}{{backend=\"{}\",source=\"{}\"}} {}",
                escape_label_value(&stat.backend),
                stat.source,
                value(stat)
            );
        }
    }
}

/// One per-(backend, source address) metric: how it is named and described, and how to read its
/// value out of a `SourceTransferStat`.
struct TransferSeries {
    name: &'static str,
    kind: &'static str,
    help: &'static str,
    value: fn(&SourceTransferStat) -> u64,
}

/// Escapes a label value per the Prometheus text format (`\`, `"` and newline).
fn escape_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
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

/// Records how an upload that reached the Backend ended: `succeeded` counts it as completed; if
/// the guard is dropped without that call, it counts as failed.
pub struct UploadAttempt {
    metrics: Arc<Metrics>,
    succeeded: AtomicBool,
}

impl UploadAttempt {
    /// The Backend confirmed the upload with a 2xx reply.
    pub fn succeeded(self) {
        saturating_add(&self.metrics.uploads_completed_total, 1);
        self.succeeded.store(true, Ordering::Relaxed);
    }
}

impl Drop for UploadAttempt {
    fn drop(&mut self) {
        if !self.succeeded.load(Ordering::Relaxed) {
            saturating_add(&self.metrics.uploads_failed_total, 1);
        }
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

    fn counter(metrics: &Metrics, name: &str) -> u64 {
        let rendered = metrics.render(0, 0, None);
        let prefix = format!("{name} ");
        rendered
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("{name} missing from:\n{rendered}"))
            .parse()
            .unwrap()
    }

    #[test]
    fn an_upload_attempt_counts_as_failed_unless_it_succeeds() {
        let metrics = Metrics::new();

        metrics.upload_attempted().succeeded();
        assert_eq!(counter(&metrics, "ftp_gateway_uploads_started_total"), 1);
        assert_eq!(counter(&metrics, "ftp_gateway_uploads_completed_total"), 1);
        assert_eq!(counter(&metrics, "ftp_gateway_uploads_failed_total"), 0);

        // Dropped without `succeeded` -- whatever early exit that was -- is a failure.
        drop(metrics.upload_attempted());
        assert_eq!(counter(&metrics, "ftp_gateway_uploads_started_total"), 2);
        assert_eq!(counter(&metrics, "ftp_gateway_uploads_completed_total"), 1);
        assert_eq!(counter(&metrics, "ftp_gateway_uploads_failed_total"), 1);
    }

    #[test]
    fn an_upload_attempt_abandoned_by_a_panic_counts_as_failed() {
        let metrics = Metrics::new();
        let for_task = Arc::clone(&metrics);
        let result = std::panic::catch_unwind(move || {
            let _attempt = for_task.upload_attempted();
            panic!("session blew up mid-upload");
        });
        assert!(result.is_err());
        assert_eq!(counter(&metrics, "ftp_gateway_uploads_failed_total"), 1);
    }

    #[test]
    fn failure_counters_increment_independently() {
        let metrics = Metrics::new();
        metrics.backend_pasv_failed();
        metrics.backend_pasv_failed();
        metrics.backend_data_connection_failed();
        metrics.data_connection_rejected();
        metrics.session_idle_timed_out();

        assert_eq!(
            counter(&metrics, "ftp_gateway_backend_pasv_failures_total"),
            2
        );
        assert_eq!(
            counter(
                &metrics,
                "ftp_gateway_backend_data_connection_failures_total"
            ),
            1
        );
        assert_eq!(
            counter(&metrics, "ftp_gateway_data_connections_rejected_total"),
            1
        );
        assert_eq!(
            counter(&metrics, "ftp_gateway_session_idle_timeouts_total"),
            1
        );
        assert_eq!(counter(&metrics, "ftp_gateway_backend_timeouts_total"), 0);
    }

    #[test]
    fn only_timeouts_count_as_backend_timeouts() {
        let metrics = Metrics::new();
        metrics.observe_backend_io_error(&io::Error::from(io::ErrorKind::ConnectionRefused));
        metrics.observe_backend_io_error(&io::Error::from(io::ErrorKind::UnexpectedEof));
        assert_eq!(counter(&metrics, "ftp_gateway_backend_timeouts_total"), 0);

        metrics.observe_backend_io_error(&io::Error::from(io::ErrorKind::TimedOut));
        assert_eq!(counter(&metrics, "ftp_gateway_backend_timeouts_total"), 1);
    }

    #[test]
    fn the_new_counters_saturate_instead_of_wrapping() {
        let metrics = Metrics::new();
        for counter in [
            &metrics.uploads_started_total,
            &metrics.uploads_completed_total,
            &metrics.uploads_failed_total,
            &metrics.backend_pasv_failures_total,
            &metrics.backend_data_connection_failures_total,
            &metrics.data_connections_rejected_total,
            &metrics.backend_timeouts_total,
            &metrics.session_idle_timeouts_total,
        ] {
            counter.store(u64::MAX, Ordering::Relaxed);
        }

        metrics.upload_attempted().succeeded();
        drop(metrics.upload_attempted());
        metrics.backend_pasv_failed();
        metrics.backend_data_connection_failed();
        metrics.data_connection_rejected();
        metrics.session_idle_timed_out();
        metrics.observe_backend_io_error(&io::Error::from(io::ErrorKind::TimedOut));

        for name in [
            "ftp_gateway_uploads_started_total",
            "ftp_gateway_uploads_completed_total",
            "ftp_gateway_uploads_failed_total",
            "ftp_gateway_backend_pasv_failures_total",
            "ftp_gateway_backend_data_connection_failures_total",
            "ftp_gateway_data_connections_rejected_total",
            "ftp_gateway_backend_timeouts_total",
            "ftp_gateway_session_idle_timeouts_total",
        ] {
            assert_eq!(counter(&metrics, name), u64::MAX, "{name}");
        }
    }

    #[test]
    fn per_source_metrics_are_only_rendered_when_rotation_is_on() {
        let metrics = Metrics::new();
        assert!(
            !metrics
                .render(0, 0, None)
                .contains("ftp_gateway_backend_source_")
        );

        let empty = RotatorSnapshot::default();
        let rendered = metrics.render(0, 0, Some(&empty));
        assert!(rendered.contains("# TYPE ftp_gateway_backend_source_unhealthy gauge\n"));
        assert!(rendered.contains("# TYPE ftp_gateway_backend_source_transfers_total counter\n"));
    }

    #[test]
    fn per_source_metrics_carry_backend_and_source_labels() {
        use crate::backend::rotator::{SourceHealth, SourceTransferStat};
        use std::net::Ipv4Addr;

        let snapshot = RotatorSnapshot {
            sources: vec![
                SourceHealth {
                    source: Ipv4Addr::new(10, 0, 1, 10),
                    unhealthy: false,
                },
                SourceHealth {
                    source: Ipv4Addr::new(10, 0, 2, 20),
                    unhealthy: true,
                },
            ],
            transfers: vec![SourceTransferStat {
                backend: "s-1.server.transfer.example:21".to_string(),
                source: Ipv4Addr::new(10, 0, 1, 10),
                capacity: 9,
                active: 4,
                started_total: 120,
                slot_timeouts_total: 3,
            }],
        };
        let rendered = Metrics::new().render(0, 0, Some(&snapshot));
        for expected in [
            "ftp_gateway_backend_source_unhealthy{source=\"10.0.1.10\"} 0\n",
            "ftp_gateway_backend_source_unhealthy{source=\"10.0.2.20\"} 1\n",
            "ftp_gateway_backend_source_transfers_active{backend=\"s-1.server.transfer.example:21\",source=\"10.0.1.10\"} 4\n",
            "ftp_gateway_backend_source_transfers_capacity{backend=\"s-1.server.transfer.example:21\",source=\"10.0.1.10\"} 9\n",
            "ftp_gateway_backend_source_transfers_total{backend=\"s-1.server.transfer.example:21\",source=\"10.0.1.10\"} 120\n",
            "ftp_gateway_backend_source_slot_timeouts_total{backend=\"s-1.server.transfer.example:21\",source=\"10.0.1.10\"} 3\n",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in:\n{rendered}"
            );
        }
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label_value("plain:21"), "plain:21");
        assert_eq!(escape_label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn render_produces_expected_prometheus_text() {
        let metrics = Metrics::new();
        let _session = metrics.session_started();
        let _upload = metrics.upload_started();
        metrics.add_upload_bytes(42);
        metrics.connection_rejected();

        let rendered = metrics.render(3, 10, None);
        assert!(rendered.contains("ftp_gateway_sessions_active 1\n"));
        assert!(rendered.contains("ftp_gateway_uploads_active 1\n"));
        assert!(rendered.contains("ftp_gateway_pasv_ports_active 3\n"));
        assert!(rendered.contains("ftp_gateway_pasv_ports_capacity 10\n"));
        assert!(rendered.contains("ftp_gateway_sessions_total 1\n"));
        assert!(rendered.contains("ftp_gateway_upload_bytes_total 42\n"));
        assert!(rendered.contains("ftp_gateway_connections_rejected_total 1\n"));
    }
}
