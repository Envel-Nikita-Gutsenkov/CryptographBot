//! Natural-language command parser.
//! No LLM — pure pattern matching on tokenized input.

#[derive(Debug, PartialEq)]
pub enum NlpCommand {
    AddPurchase {
        symbol: String,
        amount: f64,
        cost_usd: f64,
        wallet: Option<String>,
    },
    Move {
        symbol: String,
        amount: f64,
        from: String,
        to: String,
        fee: Option<f64>,
    },
    Price(Vec<String>),
    AlertAdd(String), // raw args forwarded to alert parser
    Convert {
        amount: f64,
        from: String,
        to: String,
    },
    Backup,
    Portfolio,
    Top(u32),
    Help,
}

/// Try to parse a free-text message as a bot command.
/// Returns None if not recognized.
pub fn parse(text: &str) -> Option<NlpCommand> {
    let upper = text.to_uppercase();
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let cmd = upper.split_whitespace().next().unwrap_or("");

    match cmd {
        // ADD BTC 0.001 60 [cold:ledger | hot:wallet | exchange:binance]
        "ADD" | "BUY" if parts.len() >= 4 => {
            let symbol = parts[1].to_uppercase();
            let amount = parts[2].parse().ok()?;
            let cost_usd = parts[3].parse().ok()?;
            let wallet = parts.get(4).map(|s| s.to_string());
            Some(NlpCommand::AddPurchase {
                symbol,
                amount,
                cost_usd,
                wallet,
            })
        }

        // MOVE BTC 0.5 from:ledger to:binance [fee:0.0001]
        "MOVE" | "TRANSFER" | "SEND" if parts.len() >= 5 => {
            let symbol = parts[1].to_uppercase();
            let amount = parts[2].parse().ok()?;
            let mut from = None;
            let mut to = None;
            let mut fee = None;
            for p in &parts[3..] {
                if let Some(v) = p.strip_prefix("from:") {
                    from = Some(v.to_string());
                } else if let Some(v) = p.strip_prefix("to:") {
                    to = Some(v.to_string());
                } else if let Some(v) = p.strip_prefix("fee:") {
                    fee = v.parse().ok();
                }
            }
            Some(NlpCommand::Move {
                symbol,
                amount,
                from: from?,
                to: to?,
                fee,
            })
        }

        // PRICE BTC ETH
        "PRICE" | "P" => {
            let syms: Vec<String> = parts[1..].iter().map(|s| s.to_uppercase()).collect();
            if syms.is_empty() {
                Some(NlpCommand::Price(vec![]))
            } else {
                Some(NlpCommand::Price(syms))
            }
        }

        // ALERT BTC < 60000 [cooldown:12h]
        "ALERT" if parts.len() >= 3 => Some(NlpCommand::AlertAdd(parts[1..].join(" "))),

        // CONVERT 100 BYN BTC
        "CONVERT" | "C" if parts.len() == 4 => {
            let amount = parts[1].parse().ok()?;
            let from = parts[2].to_uppercase();
            let to = parts[3].to_uppercase();
            Some(NlpCommand::Convert { amount, from, to })
        }

        "BACKUP" => Some(NlpCommand::Backup),
        "PORTFOLIO" | "PORT" => Some(NlpCommand::Portfolio),
        "TOP" => {
            let n = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            Some(NlpCommand::Top(n))
        }
        "HELP" => Some(NlpCommand::Help),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add() {
        let r = parse("ADD BTC 0.001 60 cold:ledger").unwrap();
        assert!(matches!(r, NlpCommand::AddPurchase { .. }));
        if let NlpCommand::AddPurchase {
            symbol,
            amount,
            cost_usd,
            wallet,
        } = r
        {
            assert_eq!(symbol, "BTC");
            assert_eq!(amount, 0.001);
            assert_eq!(cost_usd, 60.0);
            assert_eq!(wallet.as_deref(), Some("cold:ledger"));
        }
    }

    #[test]
    fn parse_move() {
        let r = parse("MOVE BTC 0.5 from:ledger to:binance fee:0.0001").unwrap();
        assert!(matches!(r, NlpCommand::Move { .. }));
    }

    #[test]
    fn parse_convert() {
        let r = parse("CONVERT 100 BYN BTC").unwrap();
        assert!(matches!(r, NlpCommand::Convert { amount, from, to }
            if amount == 100.0 && from == "BYN" && to == "BTC"));
    }

    #[test]
    fn unrecognized_returns_none() {
        assert!(parse("hello world").is_none());
        assert!(parse("").is_none());
    }
}
