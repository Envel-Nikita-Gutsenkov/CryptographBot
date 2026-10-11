use dashmap::DashMap;
use parking_lot::RwLock;

use crate::{error::Result, models::Portfolio};

use super::migration;

pub struct Store {
    data: DashMap<u64, RwLock<Portfolio>>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            data: DashMap::new(),
        }
    }
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read<F, T>(&self, user_id: u64, f: F) -> T
    where
        F: FnOnce(&Portfolio) -> T,
    {
        let entry = self
            .data
            .entry(user_id)
            .or_insert_with(|| RwLock::new(Portfolio::default()));
        let guard = entry.read();
        f(&guard)
    }

    pub fn write<F, T>(&self, user_id: u64, f: F) -> T
    where
        F: FnOnce(&mut Portfolio) -> T,
    {
        let entry = self
            .data
            .entry(user_id)
            .or_insert_with(|| RwLock::new(Portfolio::default()));
        let mut guard = entry.write();
        f(&mut guard)
    }

    /// Export portfolio as versioned JSON for the user.
    pub fn export_backup(&self, user_id: u64) -> Result<Vec<u8>> {
        self.read(user_id, |p| migration::encode(p))
    }

    /// Decode a versioned or legacy-unversioned JSON backup.
    pub fn decode_backup(&self, data: &[u8]) -> Result<Portfolio> {
        migration::decode(data)
    }

    /// Apply a decoded portfolio for a user.
    pub fn restore(&self, user_id: u64, portfolio: Portfolio) {
        self.data.insert(user_id, RwLock::new(portfolio));
    }

    pub fn all_user_ids(&self) -> Vec<u64> {
        self.data.iter().map(|e| *e.key()).collect()
    }

    pub fn get_all_active_symbols(&self) -> Vec<String> {
        let mut symbols = std::collections::HashSet::new();
        for entry in self.data.iter() {
            let p = entry.value().read();
            for sym in &p.settings.default_watchlist {
                symbols.insert(sym.to_uppercase());
            }
            for sym in p.holdings_by_symbol().keys() {
                symbols.insert(sym.to_uppercase());
            }
            for alert in &p.alerts {
                symbols.insert(alert.symbol.to_uppercase());
            }
        }
        // Always include some default ones just in case
        symbols.insert("BTC".to_string());
        symbols.insert("ETH".to_string());
        symbols.insert("SOL".to_string());
        symbols.insert("BNB".to_string());
        symbols.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_roundtrip() {
        let store = Store::new();
        store.write(1, |p| p.settings.alert_quiet_hours_start = Some(22));
        let v = store.read(1, |p| p.settings.alert_quiet_hours_start);
        assert_eq!(v, Some(22));
    }

    #[test]
    fn backup_is_valid_json() {
        let store = Store::new();
        store.write(1, |p| {
            p.settings.alert_quiet_hours_start = Some(10);
        });
        let bytes = store.export_backup(1).unwrap();
        // Must be valid JSON
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.is_object());
    }

    #[test]
    fn backup_roundtrip() {
        let store = Store::new();
        store.write(1, |p| {
            p.settings.alert_quiet_hours_start = Some(8);
        });
        let bytes = store.export_backup(1).unwrap();
        let p = store.decode_backup(&bytes).unwrap();
        assert_eq!(p.settings.alert_quiet_hours_start, Some(8));
    }

    #[test]
    fn decode_bad_data_returns_error() {
        let store = Store::new();
        assert!(store.decode_backup(b"not json at all").is_err());
    }
}
