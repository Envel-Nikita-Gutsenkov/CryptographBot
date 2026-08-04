use serde::{Deserialize, Serialize};

use crate::models::{AlertCondition, Period, Portfolio, WalletKind};

/// FSM state for the Teloxide dialogue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum State {
    #[default]
    Idle,

    // Alert creation wizard
    AlertStep1,
    AlertStep2 {
        symbol: String,
        condition: AlertCondition,
    },
    AlertStep3 {
        symbol: String,
        condition: AlertCondition,
        threshold: f64,
        period: Option<Period>,
    },

    // Transfer wizard
    TransferStep1 { symbol: String },
    TransferStep2 { symbol: String, from_wallet_id: u64 },
    TransferStep3 {
        symbol: String,
        from_wallet_id: u64,
        to_wallet_id: u64,
        amount_sent: f64,
    },

    // Wallet addition
    WalletStep1,
    WalletStep2 { name: String, kind: WalletKind },

    // Backup restore confirmation
    AwaitingRestoreConfirm { portfolio_snapshot: Box<Portfolio> },

    // Purchase correction
    CorrectingPurchase { purchase_id: u64 },

    // Holding manual override
    CorrectingHolding { wallet_id: u64, symbol: String },

    // AI analysis wizard
    AiWizard,
}
