//! Backup versioning and migration.
//!
//! Every exported backup is wrapped in `BackupEnvelope` with an explicit
//! schema version. On import, the version is checked and migrations are
//! applied automatically before handing the `Portfolio` to the rest of the
//! app. Adding a new field to `Portfolio` or `UserSettings`? Mark it
//! `#[serde(default)]` in models.rs — existing backups will just get the
//! field's `Default` value when loaded. For breaking/structural changes
//! (rename, split, type change) increment `CURRENT_VERSION` and add a
//! migration arm below.

use serde::{Deserialize, Serialize};

use crate::{
    error::{BotError, Result},
    models::Portfolio,
};

/// Bump this whenever a migration is added.
#[allow(dead_code)]
pub const CURRENT_VERSION: u32 = 1;

/// The on-disk (and over-Telegram) format for backups.
#[derive(Serialize, Deserialize)]
#[serde(tag = "version")]
pub enum BackupEnvelope {
    #[serde(rename = "1")]
    V1 { data: Portfolio },
    // Future: V2 { data: PortfolioV2 }, …
}

impl BackupEnvelope {
    /// Wrap a portfolio into the current envelope version.
    pub fn pack(portfolio: Portfolio) -> Self {
        BackupEnvelope::V1 { data: portfolio }
    }

    /// Migrate any version to the current `Portfolio`.
    pub fn unpack(self) -> Portfolio {
        match self {
            BackupEnvelope::V1 { data } => data,
        }
    }
}

/// Serialize the portfolio to pretty JSON with version tag.
pub fn encode(portfolio: &Portfolio) -> Result<Vec<u8>> {
    let envelope = BackupEnvelope::pack(portfolio.clone());
    serde_json::to_vec_pretty(&envelope)
        .map_err(|e| BotError::Encryption(format!("backup encode: {e}")))
}

/// Deserialize from JSON, handle versioning and missing-field defaults.
/// Returns a descriptive error so the user can understand what went wrong.
pub fn decode(data: &[u8]) -> Result<Portfolio> {
    // Try versioned envelope first
    if let Ok(envelope) = serde_json::from_slice::<BackupEnvelope>(data) {
        return Ok(envelope.unpack());
    }

    // Fallback: legacy unversioned Portfolio JSON (backups made before v1)
    serde_json::from_slice::<Portfolio>(data).map_err(|e| {
        BotError::Encryption(format!(
            "Cannot parse backup (tried versioned + legacy): {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_backup_roundtrip_is_lossless() {
        let p = Portfolio::default();
        let bytes = encode(&p).expect("encode");
        let back = decode(&bytes).expect("decode");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&p).unwrap()
        );
    }

    #[test]
    fn legacy_unversioned_backup_still_loads() {
        // Backups made before the envelope existed must keep loading.
        let p = Portfolio::default();
        let bytes = serde_json::to_vec(&p).unwrap();
        assert!(decode(&bytes).is_ok());
    }

    #[test]
    fn garbage_is_rejected_with_an_error() {
        assert!(decode(b"this is not a backup").is_err());
        assert!(decode(b"").is_err());
    }

    #[test]
    fn version_tag_is_written() {
        let bytes = encode(&Portfolio::default()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["version"], CURRENT_VERSION.to_string());
    }
}
