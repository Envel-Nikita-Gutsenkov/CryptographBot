use serde::{Deserialize, Serialize};
use super::purchase::CorrectionEntry;

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
