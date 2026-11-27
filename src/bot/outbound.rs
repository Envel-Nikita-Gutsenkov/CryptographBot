//! Outbound message log — the exact text the bot sent.
//!
//! When a user reports "the bot sent me this nonsense", the only way to debug it
//! is to read back what was actually delivered. Telegram does not let a bot read
//! its own outgoing history, so every report/alert path calls [`record`] with the
//! text right before (or right after) the send. Rows go to a bounded ring buffer
//! in SQLite (`outbound_messages`, [`crate::db::sqlite::OUTBOUND_KEEP_LAST`] rows
//! / [`crate::db::sqlite::OUTBOUND_MAX_AGE_DAYS`] days), so this never grows
//! without bound, and a one-line preview is written to the log so `docker logs`
//! also shows recent sends.
//!
//! The sink is process-global on purpose: report senders only have a `&Config`
//! and a chat id, not the store, and threading a store through every call site
//! would be a much larger change than the feature warrants.

use std::sync::OnceLock;

use crate::db::SqliteStore;

static SINK: OnceLock<SqliteStore> = OnceLock::new();

/// Wire the process-global sink. Called once at startup; later calls are no-ops.
pub fn init(store: SqliteStore) {
    if SINK.set(store).is_err() {
        tracing::debug!(target: "outbound", "outbound sink already initialized");
    }
}

/// Record a delivered message. Never fails and never blocks: the SQLite insert
/// is fire-and-forget on the DB worker (see `SqliteStore::record_outbound`).
///
/// `kind` is a short label for the sender: `rich`, `ai`, `security`, `alert`.
pub fn record(user_id: u64, kind: &str, text: &str) {
    let preview: String = text
        .chars()
        .take(140)
        .collect::<String>()
        .replace('\n', " ⏎ ");
    tracing::info!(
        target: "outbound",
        user_id,
        kind,
        chars = text.chars().count(),
        preview = %preview,
        "sent"
    );
    if let Some(store) = SINK.get() {
        store.record_outbound(user_id, kind, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_without_a_sink_is_a_noop() {
        // The global sink is unset in unit tests; this must not panic and must
        // not try to open a database.
        record(1, "rich", "hello world");
    }
}
