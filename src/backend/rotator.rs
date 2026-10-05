use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::bail;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::backend::source::{self, InterfaceLister};
use crate::config::BackendSourceConfig;

/// How long a source address that failed to connect is left out of the rotation before it is
/// tried again.
const UNHEALTHY_FOR: Duration = Duration::from_secs(60);

/// Chooses which local (source) address each backend session connects from, and caps how many
/// data transfers one source address has open to one backend at once.
///
/// Why: some backends (AWS Transfer Family) serve data connections from a tiny port range -- 9
/// ports -- so one source address can only have that many transfers in flight. Spreading
/// sessions over several source addresses multiplies that capacity.
///
/// - Each backend has a "current" source address, handed to new sessions. It advances to the next
///   address once the backend has had as many data connections started as it has data ports (a
///   full cycle of its port range). A session keeps the address it was given for its control and
///   data connections alike.
/// - Per (backend, source address) at most `capacity` (the backend's port-range size) data
///   transfers run at once; further ones wait.
/// - A source address that failed to connect is skipped for `UNHEALTHY_FOR`.
///
/// Cheap to clone (an `Arc` wrapper) and shared by every session, like `PortManager`.
#[derive(Clone)]
pub struct SourceRotator {
    state: Arc<Mutex<State>>,
}

struct State {
    /// Sorted and free of duplicates, so the rotation order is stable.
    sources: Vec<Ipv4Addr>,
    /// Source address -> when it becomes eligible again.
    unhealthy: HashMap<Ipv4Addr, Instant>,
    backends: HashMap<String, BackendState>,
}

#[derive(Default)]
struct BackendState {
    /// How many times the current source has advanced; the current source is
    /// `sources[advances % sources.len()]`.
    advances: usize,
    started_in_cycle: usize,
    slots: HashMap<Ipv4Addr, Arc<Semaphore>>,
}

/// One in-flight data transfer's claim on its (backend, source address) capacity. Dropping it
/// -- success, error, or panic -- frees the slot for the next waiter.
pub struct DataSlot {
    _permit: OwnedSemaphorePermit,
}

impl SourceRotator {
    pub fn new(mut sources: Vec<Ipv4Addr>) -> Self {
        sources.sort();
        sources.dedup();
        SourceRotator {
            state: Arc::new(Mutex::new(State {
                sources,
                unhealthy: HashMap::new(),
                backends: HashMap::new(),
            })),
        }
    }

    /// Lists the host's interfaces now and fails if that yields no usable source address (so a
    /// wrong `include_interfaces` is caught at startup), then keeps the list current by listing
    /// again every `refresh_secs` in a background task.
    pub fn start(
        lister: Arc<dyn InterfaceLister>,
        config: &BackendSourceConfig,
    ) -> anyhow::Result<SourceRotator> {
        let sources = source::resolve_sources(lister.as_ref(), config)?;
        if sources.is_empty() {
            bail!(
                "backend_source: no usable source addresses (include_interfaces: {:?}, exclude_interfaces: {:?})",
                config.include_interfaces,
                config.exclude_interfaces
            );
        }
        tracing::info!(sources = ?sources, "backend source address rotation enabled");

        let rotator = SourceRotator::new(sources);
        let refresher = rotator.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(config.refresh_secs));
            interval.tick().await; // the first tick is immediate; the initial listing is done
            loop {
                interval.tick().await;
                refresher.refresh_once(lister.as_ref(), &config);
            }
        });
        Ok(rotator)
    }

    /// One re-listing. A failure, or a listing with nothing usable in it, keeps the previous
    /// addresses: an interface list that is momentarily empty should not stop every backend
    /// connection.
    pub fn refresh_once(&self, lister: &dyn InterfaceLister, config: &BackendSourceConfig) {
        match source::resolve_sources(lister, config) {
            Ok(sources) if sources.is_empty() => {
                tracing::warn!(
                    "backend_source: no usable source addresses found; keeping the previous ones"
                );
            }
            Ok(sources) => {
                if self.set_sources(sources.clone()) {
                    tracing::info!(sources = ?sources, "backend source addresses changed");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "backend_source: failed to list interfaces; keeping the previous addresses");
            }
        }
    }

    /// Replaces the source addresses. Returns whether the set changed.
    pub fn set_sources(&self, mut sources: Vec<Ipv4Addr>) -> bool {
        sources.sort();
        sources.dedup();
        let mut state = self.state.lock().unwrap();
        if state.sources == sources {
            return false;
        }
        state.unhealthy.retain(|ip, _| sources.contains(ip));
        state.sources = sources;
        true
    }

    pub fn sources(&self) -> Vec<Ipv4Addr> {
        self.state.lock().unwrap().sources.clone()
    }

    /// Source addresses to try for a new session to `backend`, best first: the backend's current
    /// one, then the rest in order, skipping any that recently failed. If every one has failed
    /// recently they are all returned anyway, so the gateway keeps trying rather than refusing
    /// every session until the timeout passes.
    pub fn candidates(&self, backend: &str) -> Vec<Ipv4Addr> {
        self.candidates_at(backend, Instant::now())
    }

    fn candidates_at(&self, backend: &str, now: Instant) -> Vec<Ipv4Addr> {
        let state = self.state.lock().unwrap();
        let len = state.sources.len();
        if len == 0 {
            return Vec::new();
        }
        let start = state.backends.get(backend).map_or(0, |b| b.advances) % len;
        let ordered: Vec<Ipv4Addr> = (0..len).map(|i| state.sources[(start + i) % len]).collect();
        let healthy: Vec<Ipv4Addr> = ordered
            .iter()
            .copied()
            .filter(|ip| !state.is_unhealthy(*ip, now))
            .collect();
        if healthy.is_empty() { ordered } else { healthy }
    }

    /// Leaves `source` out of the rotation if `err` -- a failure to connect from it -- looks like
    /// the source's fault rather than the backend's. Returns whether it did.
    ///
    /// A refused or reset connection means the backend is down or turning clients away, which
    /// every source address would run into equally, so it says nothing against this one.
    pub fn report_connect_failure(&self, source: Ipv4Addr, err: &io::Error) -> bool {
        let source_fault = matches!(
            err.kind(),
            io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::NetworkUnreachable
                | io::ErrorKind::HostUnreachable
                | io::ErrorKind::TimedOut
                | io::ErrorKind::PermissionDenied
        );
        if source_fault {
            self.mark_unhealthy(source);
        }
        source_fault
    }

    /// Leaves `source` out of the rotation for a while after it failed to connect.
    pub fn mark_unhealthy(&self, source: Ipv4Addr) {
        self.mark_unhealthy_at(source, Instant::now());
    }

    fn mark_unhealthy_at(&self, source: Ipv4Addr, now: Instant) {
        self.state
            .lock()
            .unwrap()
            .unhealthy
            .insert(source, now + UNHEALTHY_FOR);
    }

    /// Claims a slot for one data transfer from `source` to `backend`, waiting up to `wait` for
    /// one to free up; `None` if none did. `capacity` is the backend's data-port count. Counts as
    /// one data connection started for the backend's rotation, which advances after `capacity`
    /// of them.
    pub async fn acquire_data_slot(
        &self,
        backend: &str,
        capacity: usize,
        source: Ipv4Addr,
        wait: Duration,
    ) -> Option<DataSlot> {
        let capacity = capacity.max(1);
        let semaphore = {
            let mut state = self.state.lock().unwrap();
            let backend_state = state.backends.entry(backend.to_string()).or_default();
            Arc::clone(
                backend_state
                    .slots
                    .entry(source)
                    .or_insert_with(|| Arc::new(Semaphore::new(capacity))),
            )
        };
        let permit = tokio::time::timeout(wait, semaphore.acquire_owned())
            .await
            .ok()?
            .ok()?;

        let mut state = self.state.lock().unwrap();
        let backend_state = state.backends.entry(backend.to_string()).or_default();
        backend_state.started_in_cycle += 1;
        if backend_state.started_in_cycle >= capacity {
            backend_state.started_in_cycle = 0;
            backend_state.advances += 1;
            tracing::debug!(
                backend,
                "port range cycled; new sessions move to the next source address"
            );
        }
        Some(DataSlot { _permit: permit })
    }
}

impl State {
    fn is_unhealthy(&self, ip: Ipv4Addr, now: Instant) -> bool {
        self.unhealthy.get(&ip).is_some_and(|until| *until > now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::source::InterfaceAddr;
    use crate::config::BackendSourceMode;

    const BACKEND: &str = "transfer.example.com:21";

    fn ip(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 0, 0, last)
    }

    fn rotator(sources: &[u8]) -> SourceRotator {
        SourceRotator::new(sources.iter().map(|&n| ip(n)).collect())
    }

    async fn slot(rotator: &SourceRotator, backend: &str, capacity: usize, source: u8) -> DataSlot {
        rotator
            .acquire_data_slot(backend, capacity, ip(source), Duration::from_secs(1))
            .await
            .expect("slot should be available")
    }

    #[test]
    fn new_sessions_start_at_the_first_source_and_rotate_through_the_rest() {
        let rotator = rotator(&[3, 1, 2]);
        assert_eq!(rotator.candidates(BACKEND), [ip(1), ip(2), ip(3)]);
    }

    #[test]
    fn duplicates_are_dropped() {
        assert_eq!(rotator(&[2, 2, 1]).sources(), [ip(1), ip(2)]);
    }

    #[test]
    fn no_sources_means_no_candidates() {
        assert!(rotator(&[]).candidates(BACKEND).is_empty());
    }

    #[tokio::test]
    async fn current_source_advances_after_a_full_cycle_of_data_connections() {
        let rotator = rotator(&[1, 2, 3]);
        let capacity = 3;
        assert_eq!(rotator.candidates(BACKEND)[0], ip(1));

        // The first `capacity - 1` data connections leave the current source alone.
        for _ in 0..capacity - 1 {
            drop(slot(&rotator, BACKEND, capacity, 1).await);
            assert_eq!(rotator.candidates(BACKEND)[0], ip(1));
        }
        // The `capacity`-th completes the cycle.
        drop(slot(&rotator, BACKEND, capacity, 1).await);
        assert_eq!(rotator.candidates(BACKEND), [ip(2), ip(3), ip(1)]);

        // And it wraps around after the last source.
        for _ in 0..capacity * 2 {
            drop(slot(&rotator, BACKEND, capacity, 2).await);
        }
        assert_eq!(rotator.candidates(BACKEND)[0], ip(1));
    }

    #[tokio::test]
    async fn rotation_is_tracked_per_backend() {
        let rotator = rotator(&[1, 2]);
        for _ in 0..2 {
            drop(slot(&rotator, "a:21", 2, 1).await);
        }
        assert_eq!(rotator.candidates("a:21")[0], ip(2));
        assert_eq!(rotator.candidates("b:21")[0], ip(1));
    }

    #[tokio::test]
    async fn capacity_caps_concurrent_transfers_per_source() {
        let rotator = rotator(&[1, 2]);
        let first = slot(&rotator, BACKEND, 2, 1).await;
        let _second = slot(&rotator, BACKEND, 2, 1).await;

        let third = rotator
            .acquire_data_slot(BACKEND, 2, ip(1), Duration::from_millis(50))
            .await;
        assert!(
            third.is_none(),
            "a third transfer on a full source must wait out and fail"
        );

        drop(first);
        assert!(
            rotator
                .acquire_data_slot(BACKEND, 2, ip(1), Duration::from_millis(50))
                .await
                .is_some(),
            "a freed slot is reusable"
        );
    }

    #[tokio::test]
    async fn a_waiter_gets_the_slot_when_one_frees_up() {
        let rotator = rotator(&[1]);
        let held = slot(&rotator, BACKEND, 1, 1).await;

        let waiting = {
            let rotator = rotator.clone();
            tokio::spawn(async move {
                rotator
                    .acquire_data_slot(BACKEND, 1, ip(1), Duration::from_secs(5))
                    .await
                    .is_some()
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        assert!(waiting.await.unwrap());
    }

    #[tokio::test]
    async fn capacity_is_per_source_and_per_backend() {
        let rotator = rotator(&[1, 2]);
        let _a = slot(&rotator, "a:21", 1, 1).await;
        // A full source doesn't affect another source, or the same source for another backend.
        let _b = slot(&rotator, "a:21", 1, 2).await;
        let _c = slot(&rotator, "b:21", 1, 1).await;
    }

    #[test]
    fn unhealthy_sources_are_skipped_until_they_recover() {
        let rotator = rotator(&[1, 2, 3]);
        let now = Instant::now();
        rotator.mark_unhealthy_at(ip(1), now);

        assert_eq!(rotator.candidates_at(BACKEND, now), [ip(2), ip(3)]);
        assert_eq!(
            rotator.candidates_at(BACKEND, now + UNHEALTHY_FOR - Duration::from_secs(1)),
            [ip(2), ip(3)]
        );
        assert_eq!(
            rotator.candidates_at(BACKEND, now + UNHEALTHY_FOR),
            [ip(1), ip(2), ip(3)]
        );
    }

    #[test]
    fn only_source_side_connect_failures_mark_a_source_unhealthy() {
        let rotator = rotator(&[1, 2]);
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
        ] {
            assert!(!rotator.report_connect_failure(ip(1), &io::Error::from(kind)));
        }
        assert_eq!(rotator.candidates(BACKEND), [ip(1), ip(2)]);

        for kind in [
            io::ErrorKind::AddrNotAvailable,
            io::ErrorKind::NetworkUnreachable,
            io::ErrorKind::HostUnreachable,
            io::ErrorKind::TimedOut,
            io::ErrorKind::PermissionDenied,
        ] {
            assert!(rotator.report_connect_failure(ip(1), &io::Error::from(kind)));
        }
        assert_eq!(rotator.candidates(BACKEND), [ip(2)]);
    }

    #[test]
    fn when_every_source_is_unhealthy_all_are_still_offered() {
        let rotator = rotator(&[1, 2]);
        let now = Instant::now();
        rotator.mark_unhealthy_at(ip(1), now);
        rotator.mark_unhealthy_at(ip(2), now);
        assert_eq!(rotator.candidates_at(BACKEND, now), [ip(1), ip(2)]);
    }

    #[test]
    fn set_sources_reports_changes_and_forgets_removed_sources_health() {
        let rotator = rotator(&[1, 2]);
        assert!(
            !rotator.set_sources(vec![ip(2), ip(1)]),
            "same set, different order"
        );
        rotator.mark_unhealthy(ip(1));

        assert!(rotator.set_sources(vec![ip(2), ip(3)]));
        assert_eq!(rotator.sources(), [ip(2), ip(3)]);
        // ip(1) was removed, so if it comes back it starts out healthy.
        assert!(rotator.set_sources(vec![ip(1), ip(2), ip(3)]));
        assert_eq!(rotator.candidates(BACKEND), [ip(1), ip(2), ip(3)]);
    }

    struct FixedInterfaces(Vec<InterfaceAddr>);

    impl InterfaceLister for FixedInterfaces {
        fn list(&self) -> std::io::Result<Vec<InterfaceAddr>> {
            Ok(self.0.clone())
        }
    }

    struct FailingInterfaces;

    impl InterfaceLister for FailingInterfaces {
        fn list(&self) -> std::io::Result<Vec<InterfaceAddr>> {
            Err(std::io::Error::other("getifaddrs failed"))
        }
    }

    fn host(addrs: &[(&str, u8)]) -> FixedInterfaces {
        FixedInterfaces(
            addrs
                .iter()
                .map(|(name, last)| InterfaceAddr {
                    interface: name.to_string(),
                    ip: ip(*last),
                })
                .collect(),
        )
    }

    fn rotate_config() -> BackendSourceConfig {
        BackendSourceConfig {
            mode: BackendSourceMode::Rotate,
            ..BackendSourceConfig::default()
        }
    }

    #[tokio::test]
    async fn start_fails_when_no_source_address_is_usable() {
        let lister = Arc::new(host(&[("lo", 1), ("docker0", 2)]));
        assert!(SourceRotator::start(lister, &rotate_config()).is_err());
        assert!(SourceRotator::start(Arc::new(FailingInterfaces), &rotate_config()).is_err());
    }

    #[tokio::test]
    async fn start_uses_the_listed_sources() {
        let lister = Arc::new(host(&[("ens5", 5), ("ens6", 6), ("docker0", 9)]));
        let rotator = SourceRotator::start(lister, &rotate_config()).unwrap();
        assert_eq!(rotator.sources(), [ip(5), ip(6)]);
    }

    #[test]
    fn refresh_picks_up_a_newly_attached_interface() {
        let rotator = rotator(&[5]);
        rotator.refresh_once(&host(&[("ens5", 5), ("ens6", 6)]), &rotate_config());
        assert_eq!(rotator.sources(), [ip(5), ip(6)]);
    }

    #[test]
    fn refresh_keeps_the_previous_sources_when_listing_fails_or_finds_nothing() {
        let rotator = rotator(&[5, 6]);
        rotator.refresh_once(&FailingInterfaces, &rotate_config());
        assert_eq!(rotator.sources(), [ip(5), ip(6)]);
        rotator.refresh_once(&host(&[("lo", 1)]), &rotate_config());
        assert_eq!(rotator.sources(), [ip(5), ip(6)]);
    }
}
