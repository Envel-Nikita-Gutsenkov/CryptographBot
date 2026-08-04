use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectionEntry {
    pub at: DateTime<Utc>,
    pub field: String,
    pub old_value: String,
    pub new_value: String,
    pub reason: String,
}

/// A purchase event. The `cost_usd` field is the ground truth for P&L and is
/// NEVER mutated — corrections are appended to `corrections` with full audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Purchase {
    pub id: u64,
    pub symbol: String,
    /// Crypto amount received at purchase (before any transfers/fees).
    pub amount: f64,
    /// Original cost in USD — immutable cost basis.
    pub cost_usd: f64,
    /// Original cost in the currency the user actually paid with.
    pub paid_amount: f64,
    pub paid_currency: PaidCurrency,
    pub date: DateTime<Utc>,
    /// Exchange or P2P source (Binance, Bybit, P2P, etc.)
    pub source: String,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Full audit trail — every correction logged here.
    pub corrections: Vec<CorrectionEntry>,
}

impl Purchase {
    #[allow(dead_code)]
    pub fn cost_per_unit(&self) -> f64 {
        self.cost_usd / self.amount
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PaidCurrency {
    Usd,
    Byn,
    Eur,
    Rub,
    Other(String),
}



impl std::str::FromStr for PaidCurrency {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_uppercase().as_str() {
            "USD" => PaidCurrency::Usd,
            "BYN" => PaidCurrency::Byn,
            "EUR" => PaidCurrency::Eur,
            "RUB" => PaidCurrency::Rub,
            other => PaidCurrency::Other(other.to_string()),
        })
    }
}
