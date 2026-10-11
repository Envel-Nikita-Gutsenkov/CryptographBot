use crate::models::{AlertCondition, AlertRule, CmcQuote};

/// Check if the basic alert condition threshold is met.
pub fn is_basic_condition_met(rule: &AlertRule, quote: &CmcQuote) -> bool {
    match rule.condition {
        AlertCondition::PriceBelow => quote.price_usd < rule.threshold,
        AlertCondition::PriceAbove => quote.price_usd > rule.threshold,
        AlertCondition::DropPercent => {
            let pct = period_change(quote, rule.period.as_ref());
            pct <= -rule.threshold
        }
        AlertCondition::RisePercent => {
            let pct = period_change(quote, rule.period.as_ref());
            pct >= rule.threshold
        }
    }
}

/// Evaluate whether an alert rule should fire given current market data.
/// Returns `true` if the condition is met.
pub fn evaluate(rule: &AlertRule, quote: &CmcQuote) -> bool {
    if !is_basic_condition_met(rule, quote) {
        return false;
    }

    // Check if it already fired and we need to prevent spam
    if let Some(old_price) = rule.last_fired_price {
        match rule.condition {
            AlertCondition::PriceBelow => {
                // If it already fired below, don't fire again
                false
            }
            AlertCondition::PriceAbove => {
                // If it already fired above, don't fire again
                false
            }
            AlertCondition::DropPercent => {
                // Only fire again if the price dropped further from the last fired price by at least the threshold percentage
                quote.price_usd <= old_price * (1.0 - rule.threshold / 100.0)
            }
            AlertCondition::RisePercent => {
                // Only fire again if the price rose further from the last fired price by at least the threshold percentage
                quote.price_usd >= old_price * (1.0 + rule.threshold / 100.0)
            }
        }
    } else {
        true
    }
}

pub fn period_change(quote: &CmcQuote, period: Option<&crate::models::Period>) -> f64 {
    use crate::models::Period;
    match period {
        Some(Period::OneHour) => quote.change_1h.unwrap_or(0.0),
        Some(Period::FourHours) => quote.change_24h.unwrap_or(0.0),
        Some(Period::OneDay) => quote.change_24h.unwrap_or(0.0),
        Some(Period::SevenDays) => quote.change_7d.unwrap_or(0.0),
        Some(Period::ThirtyDays) => quote.change_30d.unwrap_or(0.0),
        Some(Period::NinetyDays) => quote.change_90d.unwrap_or(0.0),
        None => quote.change_24h.unwrap_or(0.0),
    }
}

/// Check cooldown: returns true if the rule is allowed to fire now.
pub fn cooldown_ok(rule: &AlertRule) -> bool {
    match rule.last_fired {
        None => true,
        Some(last) => {
            let elapsed = chrono::Utc::now() - last;
            elapsed.num_hours() >= rule.cooldown_hours as i64
        }
    }
}

/// Format an alert rule for display.
pub fn fmt_rule(rule: &AlertRule) -> String {
    use crate::models::AlertCondition;

    let cond = match rule.condition {
        AlertCondition::PriceBelow => format!("price < ${:.2}", rule.threshold),
        AlertCondition::PriceAbove => format!("price > ${:.2}", rule.threshold),
        AlertCondition::DropPercent => {
            let p = rule
                .period
                .as_ref()
                .map(|x| x.to_string())
                .unwrap_or("24h".into());
            format!("drop > {:.1}% over {}", rule.threshold, p)
        }
        AlertCondition::RisePercent => {
            let p = rule
                .period
                .as_ref()
                .map(|x| x.to_string())
                .unwrap_or("24h".into());
            format!("rise > {:.1}% over {}", rule.threshold, p)
        }
    };

    let label = rule
        .label
        .as_deref()
        .map(|l| format!(" [{l}]"))
        .unwrap_or_default();
    let status = if rule.enabled { "✅" } else { "⏸" };
    format!(
        "{} #{} {} {}{}  cooldown {}h",
        status, rule.id, rule.symbol, cond, label, rule.cooldown_hours
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AlertCondition, AlertRule, CmcQuote};

    fn mock_quote(price: f64, change_24h: f64, change_7d: f64) -> CmcQuote {
        CmcQuote {
            symbol: "BTC".into(),
            name: "Bitcoin".into(),
            price_usd: price,
            market_cap: 1000000.0,
            volume_24h: 500000.0,
            change_1h: Some(0.0),
            change_24h: Some(change_24h),
            change_7d: Some(change_7d),
            change_30d: Some(0.0),
            change_90d: Some(0.0),
            change_365d: None,
            change_730d: None,
            ath: None,
            ath_change_percentage: None,
            high_24h: None,
            low_24h: None,
            rank: 1,
            circulating_supply: 19000000.0,
            last_updated: chrono::Utc::now(),
            anomaly_warning: None,
        }
    }

    fn rule(cond: AlertCondition, threshold: f64) -> AlertRule {
        AlertRule {
            id: 1,
            symbol: "BTC".into(),
            condition: cond,
            threshold,
            period: None,
            cooldown_hours: 12,
            last_fired: None,
            consecutive_triggers: 0,
            enabled: true,
            label: None,
            last_fired_price: None,
        }
    }

    #[test]
    fn price_below_fires() {
        assert!(evaluate(
            &rule(AlertCondition::PriceBelow, 60_000.0),
            &mock_quote(59_000.0, 0.0, 0.0)
        ));
        assert!(!evaluate(
            &rule(AlertCondition::PriceBelow, 60_000.0),
            &mock_quote(61_000.0, 0.0, 0.0)
        ));
    }

    #[test]
    fn price_above_fires() {
        assert!(evaluate(
            &rule(AlertCondition::PriceAbove, 100_000.0),
            &mock_quote(101_000.0, 0.0, 0.0)
        ));
        assert!(!evaluate(
            &rule(AlertCondition::PriceAbove, 100_000.0),
            &mock_quote(99_000.0, 0.0, 0.0)
        ));
    }

    #[test]
    fn drop_percent_fires() {
        assert!(evaluate(
            &rule(AlertCondition::DropPercent, 5.0),
            &mock_quote(0.0, -6.0, 0.0)
        ));
        assert!(!evaluate(
            &rule(AlertCondition::DropPercent, 5.0),
            &mock_quote(0.0, -4.0, 0.0)
        ));
    }

    #[test]
    fn cooldown_respected() {
        let mut r = rule(AlertCondition::PriceBelow, 60_000.0);
        r.last_fired = Some(chrono::Utc::now() - chrono::Duration::hours(6));
        r.cooldown_hours = 12;
        assert!(!cooldown_ok(&r));

        r.last_fired = Some(chrono::Utc::now() - chrono::Duration::hours(13));
        assert!(cooldown_ok(&r));
    }

    #[test]
    fn hover_spam_prevention() {
        let mut r = rule(AlertCondition::PriceBelow, 60_000.0);
        let q_below = mock_quote(59_000.0, 0.0, 0.0);
        assert!(evaluate(&r, &q_below));
        assert!(is_basic_condition_met(&r, &q_below));

        // Simulate firing
        r.last_fired = Some(chrono::Utc::now());
        r.last_fired_price = Some(59_000.0);

        // evaluate should be false (prevent duplicates) while basic condition is true
        assert!(!evaluate(&r, &q_below));
        assert!(is_basic_condition_met(&r, &q_below));

        // Price rises above 60k: basic condition is false, but cooldown is still active
        let q_above = mock_quote(60_500.0, 0.0, 0.0);
        assert!(!is_basic_condition_met(&r, &q_above));
        assert!(!cooldown_ok(&r));
    }
}
