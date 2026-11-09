//! Trading subsystem storage layer (Phase 1 of the trading module plan).
//!
//! Implements the dual-database schema from `docs/trading/TRADING_MODULE_PLAN.md`:
//! the SAME code writes to either `trading_live.db` or `trading_sim.db`; every row
//! carries a `mode` column so a simulation row can never be confused with live.
//!
//! Design invariants enforced at the storage layer (defense in depth — the engine
//! re-checks them, but the store never lets them be violated):
//!
//! * Capital boundary: `deploy_funds` / `open_lot` refuse to spend more than
//!   `available_usd` of an allocation (epsilon-compared, `EPSILON = 1e-9`).
//! * Immutable lot entries: `cost_basis` is computed as
//!   `entry_price * amount + fee_paid_usd` and stored once; it is never editable.
//! * Input validation: negative / non-finite prices, amounts and fees are rejected
//!   at the boundary, so garbage never reaches the ledger.
//!
//! All monetary values are `REAL` (USD), amounts `REAL` (coin units). Display
//! rounding uses `round_to_8`; every comparison uses `EPSILON`.
//!
//! The module is split into logical files (each < 900 lines):
//! * `types.rs` — enums, row structs and `New*` input structs
//! * `schema.rs` — DDL (`init_schema`) and column backfills (`ensure_column`)
//! * `crud.rs` — the `TradingStore` implementation (all CRUD)
//! * `rows.rs` — SQL row mappers + connection helpers
//! * `tests.rs` — unit tests

use anyhow::{bail, Result};
use rusqlite::Connection;
use std::sync::Mutex;

/// Comparison epsilon for all monetary checks (see plan §3 notes).
pub const EPSILON: f64 = 1e-9;

/// Production database file (never touched by simulation).
pub const LIVE_DB_FILE: &str = "trading_live.db";
/// Paper-trading / backtest database file (never touched by live).
pub const SIM_DB_FILE: &str = "trading_sim.db";

/// Optional bundled seed database copied into place on first open, so a fresh
/// install starts with trained profiles + lessons instead of a blank slate.
/// Override the path with the `TRADING_SEED_DB` env var; set it to an empty
/// value to disable seeding entirely.
pub const SEED_DB_FILE: &str = "data/trading_sim_seed.db";

/// Round to 8 decimal places for display/storage consistency.
pub fn round_to_8(x: f64) -> f64 {
    (x * 1e8).round() / 1e8
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn new_id() -> String {
    format!(
        "{}-{:016x}",
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u64>()
    )
}

mod crud;
mod rows;
mod schema;
#[cfg(test)]
mod tests;
mod types;

pub(crate) use rows::*;
pub(crate) use schema::init_schema;
pub use types::*;

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_positive(name: &str, v: f64) -> Result<()> {
    if !v.is_finite() || v <= 0.0 {
        bail!("{name} must be a positive finite number, got {v}");
    }
    Ok(())
}

fn validate_non_negative(name: &str, v: f64) -> Result<()> {
    if !v.is_finite() || v < 0.0 {
        bail!("{name} must be a non-negative finite number, got {v}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Thread-safe handle to one trading database (live or simulation).
///
/// A `Mutex<Connection>` keeps the store `Send + Sync` so it can be shared as
/// `Arc<TradingStore>` between the bot workers and the CLI. SQLite WAL mode
/// additionally allows concurrent readers from other processes.
pub struct TradingStore {
    mode: TradingMode,
    db_path: String,
    conn: Mutex<Connection>,
}
