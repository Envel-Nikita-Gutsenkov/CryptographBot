//! Task supervisor: wraps tokio::spawn with panic isolation and automatic
//! restart (exponential backoff). Any spawned task that panics or returns
//! an error will be restarted without affecting other tasks or the process.

use std::{future::Future, time::Duration};

use tokio::time::sleep;
use tracing::{error, info, warn};

const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_MAX_SECS: u64 = 60;
const BACKOFF_FACTOR: u64 = 2;

/// Spawn a named task under supervision. The factory `make_fut` is called each
/// time (re)start is needed, producing a fresh future.
///
/// On panic or `Err` return the task is restarted with exponential backoff.
/// On `Ok(())` return (graceful shutdown) the task is **not** restarted.
pub fn spawn<F, Fut>(name: &'static str, make_fut: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let mut attempt: u32 = 0;

        loop {
            let fut = make_fut();
            let result = tokio::task::spawn(fut).await;

            match result {
                // Graceful exit — do not restart
                Ok(Ok(())) => {
                    info!(task = name, "Task exited cleanly — not restarting");
                    return;
                }
                // Task returned an error
                Ok(Err(e)) => {
                    attempt = attempt.saturating_add(1);
                    let delay = backoff_delay(attempt);
                    error!(
                        task = name,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "Task failed: {e:#} — restarting"
                    );
                    sleep(delay).await;
                }
                // Task panicked
                Err(join_err) if join_err.is_panic() => {
                    attempt = attempt.saturating_add(1);
                    let delay = backoff_delay(attempt);
                    error!(
                        task = name,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "Task panicked — restarting"
                    );
                    sleep(delay).await;
                }
                // Task was cancelled (e.g. runtime shutdown) — stop
                Err(_) => {
                    warn!(task = name, "Task cancelled — not restarting");
                    return;
                }
            }
        }
    });
}

/// Like [`spawn`] but for infinite loops that return `!` — wraps them in an
/// `async move { f().await; Ok(()) }` adapter.
pub fn spawn_infallible<F, Fut>(name: &'static str, make_fut: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    spawn(name, move || {
        let fut = make_fut();
        async move {
            fut.await;
            Ok(())
        }
    });
}

fn backoff_delay(attempt: u32) -> Duration {
    let ms = BACKOFF_BASE_MS
        .saturating_mul(BACKOFF_FACTOR.saturating_pow(attempt.min(10).saturating_sub(1)));
    Duration::from_millis(ms.min(BACKOFF_MAX_SECS * 1000))
}
