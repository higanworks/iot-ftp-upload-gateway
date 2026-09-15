use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::backend::dns_cache::DnsCache;
use crate::backend::selector::BackendSelector;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::pasv::port_manager::PortManager;
use crate::shutdown;

use super::ip_limiter::IpConnectionLimiter;
use super::{metrics_server, session};

/// How long to wait for in-flight sessions to finish on their own after a shutdown
/// signal is received, before exiting regardless of what is still running.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Assigns each session a unique, human-readable id (independent of the client's ephemeral
/// source port) so every log line for one client connection can be grouped together, including
/// across a reconnect from the same IP. In-process only, like the rest of the gateway's state --
/// not meant to be globally unique across gateway instances or restarts.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub async fn run(config: Config) -> anyhow::Result<()> {
    let addr = SocketAddr::new(config.listen.address, config.listen.port);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "FTP listener started");

    let port_manager = PortManager::new(config.passive.port_range);
    let backend_selector = BackendSelector::new(config.backends.clone());
    let ip_limiter = IpConnectionLimiter::new(config.limits.max_connections_per_ip);
    let dns_cache = DnsCache::new();
    let metrics = Metrics::new();
    let mut sessions = JoinSet::new();

    if let Some(metrics_port) = config.metrics.port {
        let metrics_addr = SocketAddr::new(config.metrics.address, metrics_port);
        let metrics = Arc::clone(&metrics);
        let port_manager = port_manager.clone();
        tokio::spawn(async move {
            if let Err(err) = metrics_server::run(metrics_addr, metrics, port_manager).await {
                tracing::warn!(error = %err, "metrics endpoint stopped");
            }
        });
    }

    let shutdown_signal = shutdown::wait_for_shutdown_signal();
    tokio::pin!(shutdown_signal);

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown_signal => {
                tracing::info!("shutdown signal received; no longer accepting new connections");
                break;
            }

            accept_result = listener.accept() => {
                // A transient accept() failure (e.g. the host is temporarily out of file
                // descriptors) must not take down the whole gateway and every other
                // in-flight session with it -- log it and keep accepting.
                let (stream, peer_addr) = match accept_result {
                    Ok(pair) => pair,
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to accept connection");
                        continue;
                    }
                };
                let Some(ip_guard) = ip_limiter.try_acquire(peer_addr.ip()) else {
                    tracing::warn!(%peer_addr, "connection limit reached for this IP");
                    metrics.connection_rejected();
                    continue;
                };

                let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
                tracing::info!(%peer_addr, session_id, "accepted new connection");

                let backend = backend_selector.next();
                let passive = config.passive;
                let timeouts = config.timeouts;
                let limits = config.limits;
                let port_manager = port_manager.clone();
                let metrics = Arc::clone(&metrics);
                let dns_cache = dns_cache.clone();

                sessions.spawn(async move {
                    // Held for the whole session so its slot in `ip_limiter` is released
                    // exactly when the session ends (success, error, or panic).
                    let _ip_guard = ip_guard;
                    if let Err(err) = session::handle(
                        stream,
                        peer_addr,
                        session_id,
                        backend,
                        passive,
                        timeouts,
                        limits,
                        port_manager,
                        metrics,
                        dns_cache,
                    )
                    .await
                    {
                        tracing::warn!(%peer_addr, session_id, error = %err, "session ended with error");
                    }
                });
            }
        }
    }

    let active_sessions = sessions.len();
    if active_sessions > 0 {
        tracing::info!(active_sessions, "draining active sessions");
        let drained = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, async {
            while sessions.join_next().await.is_some() {}
        })
        .await
        .is_ok();

        if drained {
            tracing::info!("all sessions drained");
        } else {
            tracing::warn!("drain timeout elapsed with sessions still active; exiting anyway");
        }
    }

    Ok(())
}
