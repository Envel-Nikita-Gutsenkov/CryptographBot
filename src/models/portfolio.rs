use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::{
    AlertCondition, AlertRule, Goal, Holding, Memo, Period, Purchase, Transfer, UserSettings,
    Wallet,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Portfolio {
    /// Immutable purchase records (cost basis).
    pub purchases: Vec<Purchase>,
    pub wallets: Vec<Wallet>,
    /// Current balances per wallet/symbol.
    pub holdings: Vec<Holding>,
    /// Full transfer history.
    pub transfers: Vec<Transfer>,
    pub alerts: Vec<AlertRule>,
    pub settings: UserSettings,
    /// Manual overrides for the total invested amount (cost basis) per symbol.
    pub manual_cost_basis: HashMap<String, f64>,
    // Monotonic ID counters
    pub next_purchase_id: u64,
    pub next_wallet_id: u64,
    pub next_holding_id: u64,
    pub next_transfer_id: u64,
    pub next_alert_id: u64,
    /// Personal memory notes the AI always considers (backed up with the portfolio).
    pub memos: Vec<Memo>,
    /// Proactive investment goals watched by the AI worker.
    pub goals: Vec<Goal>,
    pub next_memo_id: u64,
    pub next_goal_id: u64,
}

impl Default for Portfolio {
    fn default() -> Self {
        Self {
            purchases: Vec::new(),
            wallets: Vec::new(),
            holdings: Vec::new(),
            transfers: Vec::new(),
            alerts: vec![
                // BTC Price Alerts (breakout & support)
                AlertRule {
                    id: 1,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::PriceAbove,
                    threshold: 100000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default BTC 100k breakout".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 2,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::PriceAbove,
                    threshold: 120000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default BTC 120k breakout".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 3,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::PriceAbove,
                    threshold: 150000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default BTC 150k breakout".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 4,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::PriceAbove,
                    threshold: 180000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default BTC 180k breakout".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 5,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::PriceAbove,
                    threshold: 200000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default BTC 200k breakout".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 6,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::PriceBelow,
                    threshold: 60000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default BTC support level 60k".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 7,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::PriceBelow,
                    threshold: 50000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default BTC support level 50k".to_string()),
                    last_fired_price: None,
                },
                // ETH Price Alerts
                AlertRule {
                    id: 8,
                    symbol: "ETH".to_string(),
                    condition: AlertCondition::PriceAbove,
                    threshold: 5000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default ETH 5k breakout".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 9,
                    symbol: "ETH".to_string(),
                    condition: AlertCondition::PriceBelow,
                    threshold: 2000.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default ETH support level 2k".to_string()),
                    last_fired_price: None,
                },
                // SOL Price Alerts
                AlertRule {
                    id: 10,
                    symbol: "SOL".to_string(),
                    condition: AlertCondition::PriceAbove,
                    threshold: 250.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default SOL 250 breakout".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 11,
                    symbol: "SOL".to_string(),
                    condition: AlertCondition::PriceBelow,
                    threshold: 100.0,
                    period: None,
                    cooldown_hours: 24,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("Default SOL support level 100".to_string()),
                    last_fired_price: None,
                },
                // Growth and Drop Percentage alerts
                AlertRule {
                    id: 12,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::DropPercent,
                    threshold: 10.0,
                    period: Some(Period::OneDay),
                    cooldown_hours: 12,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("BTC strong 1d drop alert".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 13,
                    symbol: "BTC".to_string(),
                    condition: AlertCondition::RisePercent,
                    threshold: 10.0,
                    period: Some(Period::OneDay),
                    cooldown_hours: 12,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("BTC strong 1d rise alert".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 14,
                    symbol: "ETH".to_string(),
                    condition: AlertCondition::DropPercent,
                    threshold: 12.0,
                    period: Some(Period::OneDay),
                    cooldown_hours: 12,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("ETH strong 1d drop alert".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 15,
                    symbol: "ETH".to_string(),
                    condition: AlertCondition::RisePercent,
                    threshold: 12.0,
                    period: Some(Period::OneDay),
                    cooldown_hours: 12,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("ETH strong 1d rise alert".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 16,
                    symbol: "SOL".to_string(),
                    condition: AlertCondition::DropPercent,
                    threshold: 15.0,
                    period: Some(Period::OneDay),
                    cooldown_hours: 12,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("SOL strong 1d drop alert".to_string()),
                    last_fired_price: None,
                },
                AlertRule {
                    id: 17,
                    symbol: "SOL".to_string(),
                    condition: AlertCondition::RisePercent,
                    threshold: 15.0,
                    period: Some(Period::OneDay),
                    cooldown_hours: 12,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: Some("SOL strong 1d rise alert".to_string()),
                    last_fired_price: None,
                },
            ],
            settings: UserSettings::default(),
            manual_cost_basis: HashMap::new(),
            next_purchase_id: 1,
            next_wallet_id: 1,
            next_holding_id: 1,
            next_transfer_id: 1,
            next_alert_id: 18,
            memos: Vec::new(),
            goals: Vec::new(),
            next_memo_id: 1,
            next_goal_id: 1,
        }
    }
}

impl Portfolio {
    pub fn alloc_purchase_id(&mut self) -> u64 {
        let id = self.next_purchase_id;
        self.next_purchase_id += 1;
        id
    }
    pub fn alloc_wallet_id(&mut self) -> u64 {
        let id = self.next_wallet_id;
        self.next_wallet_id += 1;
        id
    }
    pub fn alloc_holding_id(&mut self) -> u64 {
        let id = self.next_holding_id;
        self.next_holding_id += 1;
        id
    }
    pub fn alloc_transfer_id(&mut self) -> u64 {
        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        id
    }
    pub fn alloc_alert_id(&mut self) -> u64 {
        let id = self.next_alert_id;
        self.next_alert_id += 1;
        id
    }
    pub fn alloc_memo_id(&mut self) -> u64 {
        let id = self.next_memo_id;
        self.next_memo_id += 1;
        id
    }
    pub fn alloc_goal_id(&mut self) -> u64 {
        let id = self.next_goal_id;
        self.next_goal_id += 1;
        id
    }

    /// Sum of all original purchase costs — or manual overrides if set.
    pub fn total_invested_usd(&self) -> f64 {
        let mut total = 0.0;
        let syms = self.holdings_by_symbol();
        for (sym, _amt) in syms {
            if let Some(&manual) = self.manual_cost_basis.get(&sym) {
                total += manual;
            } else {
                let cost: f64 = self
                    .purchases
                    .iter()
                    .filter(|p| p.symbol.eq_ignore_ascii_case(&sym))
                    .map(|p| p.cost_usd)
                    .sum();
                total += cost;
            }
        }
        total
    }

    /// Current holdings aggregated by symbol across all wallets.
    pub fn holdings_by_symbol(&self) -> HashMap<String, f64> {
        let mut map: HashMap<String, f64> = HashMap::new();
        for h in &self.holdings {
            *map.entry(h.symbol.clone()).or_insert(0.0) += h.amount;
        }
        map
    }

    /// Holdings grouped by wallet ID.
    #[allow(dead_code)]
    pub fn holdings_by_wallet(&self) -> HashMap<u64, Vec<&Holding>> {
        let mut map: HashMap<u64, Vec<&Holding>> = HashMap::new();
        for h in &self.holdings {
            map.entry(h.wallet_id).or_default().push(h);
        }
        map
    }

    /// Total network fees across all transfers (those with known USD value).
    pub fn total_fees_usd(&self) -> f64 {
        self.transfers.iter().filter_map(|t| t.fee_usd).sum()
    }

    /// Average buy price for a symbol based on purchase history or manual cost basis.
    pub fn avg_buy_price(&self, symbol: &str) -> Option<f64> {
        let total_amount: f64 = self
            .holdings_by_symbol()
            .get(&symbol.to_uppercase())
            .copied()
            .unwrap_or(0.0);
        if total_amount == 0.0 {
            return None;
        }
        if let Some(&manual_cost) = self.manual_cost_basis.get(&symbol.to_uppercase()) {
            return Some(manual_cost / total_amount);
        }

        let total_cost: f64 = self
            .purchases
            .iter()
            .filter(|p| p.symbol.eq_ignore_ascii_case(symbol))
            .map(|p| p.cost_usd)
            .sum();

        Some(total_cost / total_amount)
    }

    /// Find a holding mut ref for (wallet_id, symbol), creating if absent.
    pub fn holding_mut(&mut self, wallet_id: u64, symbol: &str) -> &mut Holding {
        if let Some(pos) = self
            .holdings
            .iter()
            .position(|h| h.wallet_id == wallet_id && h.symbol.eq_ignore_ascii_case(symbol))
        {
            return &mut self.holdings[pos];
        }
        let id = self.alloc_holding_id();
        self.holdings.push(Holding {
            id,
            wallet_id,
            symbol: symbol.to_uppercase(),
            amount: 0.0,
            corrections: Vec::new(),
        });
        self.holdings.last_mut().unwrap()
    }

    /// Apply a transfer: deduct from source, credit destination.
    /// Returns the fee in crypto units.
    pub fn apply_transfer(&mut self, transfer: &Transfer) {
        if let Some(from_id) = transfer.from_wallet_id {
            let h = self.holding_mut(from_id, &transfer.symbol);
            h.amount = (h.amount - transfer.amount_sent).max(0.0);
        }
        if let Some(to_id) = transfer.to_wallet_id {
            let h = self.holding_mut(to_id, &transfer.symbol);
            h.amount += transfer.amount_received;
        }
        self.transfers.push(transfer.clone());
    }

    #[allow(dead_code)]
    pub fn wallet_by_id(&self, id: u64) -> Option<&Wallet> {
        self.wallets.iter().find(|w| w.id == id)
    }

    #[allow(dead_code)]
    pub fn wallet_by_name(&self, name: &str) -> Option<&Wallet> {
        self.wallets
            .iter()
            .find(|w| w.name.eq_ignore_ascii_case(name))
    }
}
