use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Movement of crypto between wallets (or to/from external).
/// Fees are tracked precisely: fee = amount_sent - amount_received.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transfer {
    pub id: u64,
    /// None = incoming from purchase/external.
    pub from_wallet_id: Option<u64>,
    /// None = sent to external (sale/withdrawal).
    pub to_wallet_id: Option<u64>,
    pub symbol: String,
    /// What left the source wallet.
    pub amount_sent: f64,
    /// What arrived at the destination wallet.
    pub amount_received: f64,
    /// Network fee = amount_sent - amount_received (computed, stored for reporting).
    pub fee: f64,
    /// Fee value in USD at the time of transfer (optional, user can provide).
    pub fee_usd: Option<f64>,
    /// Fee denomination — can differ from symbol (e.g., ETH fee for USDT ERC-20 transfer).
    pub fee_symbol: Option<String>,
    pub date: DateTime<Utc>,
    pub tx_hash: Option<String>,
    pub notes: Option<String>,
}

impl Transfer {
    pub fn new(
        id: u64,
        from_wallet_id: Option<u64>,
        to_wallet_id: Option<u64>,
        symbol: String,
        amount_sent: f64,
        amount_received: f64,
        fee_usd: Option<f64>,
        fee_symbol: Option<String>,
        date: DateTime<Utc>,
        tx_hash: Option<String>,
        notes: Option<String>,
    ) -> Self {
        let fee = (amount_sent - amount_received).max(0.0);
        Self {
            id,
            from_wallet_id,
            to_wallet_id,
            symbol,
            amount_sent,
            amount_received,
            fee,
            fee_usd,
            fee_symbol,
            date,
            tx_hash,
            notes,
        }
    }
}
