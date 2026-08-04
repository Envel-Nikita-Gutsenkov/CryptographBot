//! In-process watchdog: memory RSS guard + heartbeat hang detection.
//! On breach → std::process::exit() so the supervisor (systemd/Docker) restarts.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::time::interval;
use tracing::{info, warn};

pub struct Watchdog {
    /// Unix timestamp (secs) of last heartbeat ping.
    last_heartbeat: Arc<AtomicU64>,
    max_rss_mb: u64,
    hang_timeout_secs: u64,
}

impl Watchdog {
    pub fn new(max_rss_mb: u64, hang_timeout_secs: u64) -> Self {
        Self {
            last_heartbeat: Arc::new(AtomicU64::new(now_secs())),
            max_rss_mb,
            hang_timeout_secs,
        }
    }

    /// Call periodically from any async task to prove liveness.
    #[allow(dead_code)]
    pub fn ping(&self) {
        self.last_heartbeat.store(now_secs(), Ordering::Relaxed);
    }

    /// Returns a cheap cloneable ping handle (Arc'd counter).
    pub fn pinger(&self) -> WatchdogPinger {
        WatchdogPinger(Arc::clone(&self.last_heartbeat))
    }

    /// Background task — runs forever, exits the process on breach.
    pub async fn run(self) {
        let mut ticker = interval(Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        info!(
            "Watchdog started: max_rss={}MB hang_timeout={}s",
            self.max_rss_mb, self.hang_timeout_secs
        );

        loop {
            ticker.tick().await;

            // Memory check (Linux: /proc/self/status)
            let rss_mb = current_rss_mb();
            if rss_mb > self.max_rss_mb {
                warn!("WATCHDOG: RSS {}MB > limit {}MB — restarting", rss_mb, self.max_rss_mb);
                std::process::exit(2);
            }

            // Heartbeat / hang check
            let elapsed = now_secs().saturating_sub(self.last_heartbeat.load(Ordering::Relaxed));
            if elapsed > self.hang_timeout_secs {
                warn!("WATCHDOG: No heartbeat for {elapsed}s — restarting");
                std::process::exit(3);
            }

            info!("Watchdog OK: rss={rss_mb}MB last_beat={elapsed}s ago");
        }
    }
}

/// Cheap handle to update the watchdog heartbeat.
#[derive(Clone)]
pub struct WatchdogPinger(Arc<AtomicU64>);

impl WatchdogPinger {
    pub fn ping(&self) {
        self.0.store(now_secs(), Ordering::Relaxed);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Linux: read VmRSS from /proc/self/status. Returns 0 on non-Linux or error.
fn current_rss_mb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    return rest
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0)
                        / 1024; // kB → MB
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_updates_timestamp() {
        let wd = Watchdog::new(512, 120);
        let before = wd.last_heartbeat.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(10));
        wd.ping();
        let after = wd.last_heartbeat.load(Ordering::Relaxed);
        assert!(after >= before);
    }

    #[test]
    fn pinger_updates_same_counter() {
        let wd = Watchdog::new(512, 120);
        let pinger = wd.pinger();
        pinger.ping();
        let v = wd.last_heartbeat.load(Ordering::Relaxed);
        assert!(v > 0);
    }
}
