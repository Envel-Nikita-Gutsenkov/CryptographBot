//! Local OHLCV history cache on disk (`data/history/{SYMBOL}.csv`).
//!
//! The full daily-close series for our 4 symbols is ~2800 candles each — a
//! few hundred KB of CSV. Caching it (and shipping it in the repo) means
//! backtests, sim sweeps and training never touch the network for data they
//! already have, and agents can read the raw candles directly from disk.
//! Format: `ts_ms,close` one per line, oldest first, sorted ascending.

use std::path::{Path, PathBuf};

pub const HISTORY_DIR: &str = "data/history";

pub fn history_path(symbol: &str) -> PathBuf {
    PathBuf::from(HISTORY_DIR).join(format!("{}.csv", symbol.to_uppercase()))
}

fn load_history_at(path: &Path) -> Option<Vec<(i64, f64)>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split(',');
        let (Some(ts), Some(close)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(ts), Ok(close)) = (ts.trim().parse::<i64>(), close.trim().parse::<f64>()) else {
            continue;
        };
        out.push((ts, close));
    }
    out.sort_unstable_by_key(|(ts, _)| *ts);
    out.dedup_by_key(|(ts, _)| *ts);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Load cached daily closes for `symbol` (None if no cache file or empty).
pub fn load_history(symbol: &str) -> Option<Vec<(i64, f64)>> {
    load_history_at(&history_path(symbol))
}

/// Persist daily closes for `symbol` (oldest first). Creates the dir.
pub fn save_history(symbol: &str, history: &[(i64, f64)]) -> anyhow::Result<()> {
    std::fs::create_dir_all(HISTORY_DIR)?;
    let path = history_path(symbol);
    let mut text = String::with_capacity(history.len() * 24);
    text.push_str("# ts_ms,close\n");
    for (ts, close) in history {
        text.push_str(&format!("{ts},{close:.8}\n"));
    }
    std::fs::write(&path, text)?;
    Ok(())
}

/// Cached range for `symbol`: (first_ts_ms, last_ts_ms) or None.
pub fn cached_range(symbol: &str) -> Option<(i64, i64)> {
    let h = load_history(symbol)?;
    Some((h.first()?.0, h.last()?.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_dedup() {
        let dir = std::env::temp_dir().join("cryptograph_history_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("BTC.csv");
        std::fs::write(&path, "# ts_ms,close\n1,100.0\n2,101.5\n2,999.0\n3,102.0\n").unwrap();
        let h = load_history_at(&path).unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[0], (1, 100.0));
        assert_eq!(h[1], (2, 101.5)); // dedup keeps first
        assert_eq!(h[2], (3, 102.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
