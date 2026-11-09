use super::purchase::CorrectionEntry;
use serde::{Deserialize, Serialize};

/// Current balance of a symbol in a specific wallet.
/// Derived from purchases + transfers, but overridable with corrections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holding {
    pub id: u64,
    pub wallet_id: u64,
    pub symbol: String,
    pub amount: f64,
    pub corrections: Vec<CorrectionEntry>,
}
