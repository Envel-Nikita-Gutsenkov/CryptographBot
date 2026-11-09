use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wallet {
    pub id: u64,
    /// User-defined label, e.g. "Ledger Nano X", "Binance Main"
    pub name: String,
    pub kind: WalletKind,
    /// Blockchain address — for Cold/Hot wallets (enables auto-monitoring).
    pub address: Option<String>,
    pub network: Option<Network>,
    /// Exchange name — for Exchange wallets.
    pub exchange: Option<String>,
    /// Whether the bot should auto-poll this wallet's balance.
    pub monitored: bool,
    pub last_balance: HashMap<String, f64>,
    pub last_checked: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WalletKind {
    /// Hardware or paper wallet — user controls keys.
    Cold,
    /// Software or mobile wallet — user controls keys.
    Hot,
    /// Centralized exchange — custodial.
    Exchange,
}

impl WalletKind {
    pub fn emoji(&self) -> &'static str {
        match self {
            WalletKind::Cold => "🧊",
            WalletKind::Hot => "🔥",
            WalletKind::Exchange => "🏦",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            WalletKind::Cold => "Cold",
            WalletKind::Hot => "Hot",
            WalletKind::Exchange => "Exchange",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Network {
    Bitcoin,
    Ethereum,
    Solana,
    Tron,
    Bsc,
    Generic(String),
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Network::Bitcoin => write!(f, "Bitcoin"),
            Network::Ethereum => write!(f, "Ethereum"),
            Network::Solana => write!(f, "Solana"),
            Network::Tron => write!(f, "Tron"),
            Network::Bsc => write!(f, "BSC"),
            Network::Generic(n) => write!(f, "{n}"),
        }
    }
}

impl std::str::FromStr for Network {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_uppercase().as_str() {
            "BTC" | "BITCOIN" => Network::Bitcoin,
            "ETH" | "ETHEREUM" => Network::Ethereum,
            "SOL" | "SOLANA" => Network::Solana,
            "TRX" | "TRON" => Network::Tron,
            "BSC" | "BNB" | "BEP20" => Network::Bsc,
            other => Network::Generic(other.to_string()),
        })
    }
}
