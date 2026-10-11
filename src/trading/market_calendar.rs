//! Market calendar knowledge: BTC halving phases and monthly seasonality.
//!
//! The deterministic engine learns regimes implicitly from real windows, but
//! this module gives the bot *explicit* cyclical knowledge:
//!
//! * **Halving calendar** — known BTC halving dates (block-based, so the next
//!   one is an estimate) and a coarse phase model (pre-halving, post-halving
//!   rally, expansion, late cycle, bear/distribution). Halvings historically
//!   mark the start of multi-month bull runs, so the phase is a useful prior.
//! * **Monthly seasonality** — average month-over-month return per calendar
//!   month, computed from real history. This is *trained data*, not folklore:
//!   it is derived from the same OHLCV series the backtests replay.
//!
//! Both are rendered as compact text blocks for the AI context and stored in
//! the sim DB (`seasonality_stats`) so the bundled seed ships them.

use chrono::{Datelike, Duration, NaiveDate};

/// Known BTC halving dates (UTC). The 2028+ ones are estimates — halvings are
/// block-height based (~210k blocks ≈ 4 years), so exact dates are only known
/// after the fact. Beyond the last anchor the calendar extrapolates every
/// ~4 years (see [`HALVING_INTERVAL_DAYS`]).
pub const HALVING_DATES: &[NaiveDate] = &[
    NaiveDate::from_ymd_opt(2012, 11, 28).unwrap(),
    NaiveDate::from_ymd_opt(2016, 7, 9).unwrap(),
    NaiveDate::from_ymd_opt(2020, 5, 11).unwrap(),
    NaiveDate::from_ymd_opt(2024, 4, 20).unwrap(),
    NaiveDate::from_ymd_opt(2028, 4, 15).unwrap(),
    NaiveDate::from_ymd_opt(2032, 4, 20).unwrap(),
    NaiveDate::from_ymd_opt(2036, 4, 20).unwrap(),
];

/// Estimated interval between halvings: 210,000 blocks ≈ 4 years (1461 days).
pub const HALVING_INTERVAL_DAYS: i64 = 1461;

/// Coarse phase of the ~4-year BTC cycle, keyed by days since the last halving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalvingPhase {
    /// Within 180 days before the next halving — historically a pre-rally
    /// window with elevated volatility.
    PreHalving,
    /// 0–365 days after — historically the strongest bull phase.
    PostHalvingRally,
    /// 366–730 days after — expansion, momentum often continues.
    Expansion,
    /// 731–1095 days after — late cycle, tops form here.
    LateCycle,
    /// >1095 days after — distribution/bear, dips are deeper.
    BearDistribution,
}

impl HalvingPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            HalvingPhase::PreHalving => "PRE_HALVING",
            HalvingPhase::PostHalvingRally => "POST_HALVING_RALLY",
            HalvingPhase::Expansion => "EXPANSION",
            HalvingPhase::LateCycle => "LATE_CYCLE",
            HalvingPhase::BearDistribution => "BEAR_DISTRIBUTION",
        }
    }
}

/// Last halving on or before `date`. Beyond the newest anchor the calendar
/// extrapolates in 4-year steps, so the phase model never runs out of dates.
fn last_halving_on_or_before(date: NaiveDate) -> Option<NaiveDate> {
    let mut last: Option<NaiveDate> = None;
    for &h in HALVING_DATES {
        if h <= date {
            last = Some(h);
        }
    }
    let Some(mut h) = last else { return None };
    while h + Duration::days(HALVING_INTERVAL_DAYS) <= date {
        h += Duration::days(HALVING_INTERVAL_DAYS);
    }
    Some(h)
}

/// Days since the most recent halving (or before the first one), plus the
/// phase. `days_since` is negative before the first halving.
pub fn halving_phase(date: NaiveDate) -> (HalvingPhase, i64) {
    // Known anchors take priority; extrapolate only beyond the last one.
    let next_known = HALVING_DATES.iter().find(|&&h| h > date).copied();
    match last_halving_on_or_before(date) {
        None => {
            // Before the first halving.
            (
                HalvingPhase::PreHalving,
                -(next_known.unwrap_or(date) - date).num_days(),
            )
        }
        Some(h) => {
            let days = (date - h).num_days();
            // Pre-halving: within 180 days before the next halving.
            let next = next_known.unwrap_or_else(|| h + Duration::days(HALVING_INTERVAL_DAYS));
            if (next - date).num_days() <= 180 {
                return (HalvingPhase::PreHalving, days);
            }
            let phase = if days <= 365 {
                HalvingPhase::PostHalvingRally
            } else if days <= 730 {
                HalvingPhase::Expansion
            } else if days <= 1095 {
                HalvingPhase::LateCycle
            } else {
                HalvingPhase::BearDistribution
            };
            (phase, days)
        }
    }
}

/// Days until the next halving. Never `None` after the first halving — the
/// calendar extrapolates every ~4 years beyond the last known anchor.
pub fn days_to_next_halving(date: NaiveDate) -> Option<i64> {
    // Known anchors take priority; extrapolate only beyond the last one.
    if let Some(&h) = HALVING_DATES.iter().find(|&&h| h > date) {
        return Some((h - date).num_days());
    }
    let last = last_halving_on_or_before(date)?;
    let next = last + Duration::days(HALVING_INTERVAL_DAYS);
    Some((next - date).num_days())
}

/// Average month-over-month return (%) per calendar month (1..=12) over the
/// given history. Months with no data report 0.0. The first month of the
/// series has no previous close, so it never contributes.
#[allow(dead_code)] // kept for tests; the AI context uses the detailed variant
pub fn monthly_seasonality(history: &[(i64, f64)]) -> Vec<(u32, f64)> {
    monthly_seasonality_detailed(history)
        .into_iter()
        .map(|(m, avg, _)| (m, avg))
        .collect()
}

/// Like [`monthly_seasonality`], but also returns how many month-pairs each
/// average is based on, so callers can store the sample count alongside.
pub fn monthly_seasonality_detailed(history: &[(i64, f64)]) -> Vec<(u32, f64, u32)> {
    // Bucket closes by calendar month: (year, month) -> last close.
    let mut by_month: std::collections::BTreeMap<(i32, u32), f64> =
        std::collections::BTreeMap::new();
    for &(ts_ms, price) in history {
        let secs = ts_ms / 1000;
        if let Some(dt) = chrono::DateTime::from_timestamp(secs, 0) {
            let d = dt.date_naive();
            by_month.insert((d.year(), d.month()), price);
        }
    }
    let mut sums = [0.0f64; 12];
    let mut counts = [0u32; 12];
    let mut prev: Option<((i32, u32), f64)> = None;
    for ((ym, m), close) in by_month {
        if let Some(((py, pm), pclose)) = prev {
            // Only compare consecutive calendar months (a gap means missing
            // data — skip rather than measure a multi-month jump).
            let gap = (ym * 12 + m as i32) - (py * 12 + pm as i32);
            if gap == 1 && pclose > 0.0 {
                let ret = (close - pclose) / pclose * 100.0;
                sums[m as usize - 1] += ret;
                counts[m as usize - 1] += 1;
            }
        }
        prev = Some(((ym, m), close));
    }
    (1..=12)
        .map(|m| {
            let avg = if counts[m as usize - 1] > 0 {
                sums[m as usize - 1] / counts[m as usize - 1] as f64
            } else {
                0.0
            };
            (m, avg, counts[m as usize - 1])
        })
        .collect()
}

/// Compact AI-ready block: halving phase + per-month seasonality.
/// `as_of` is the reference date for the halving phase.
#[allow(dead_code)] // kept for tests; the AI context uses the stats variant
pub fn seasonality_block(symbol: &str, history: &[(i64, f64)], as_of: NaiveDate) -> String {
    seasonality_block_from_stats(symbol, &monthly_seasonality_detailed(history), as_of)
}

/// Same as [`seasonality_block`], but takes already-computed per-month stats
/// `(month, avg_return_pct, samples)` — e.g. rows loaded from the sim DB.
pub fn seasonality_block_from_stats(
    symbol: &str,
    season: &[(u32, f64, u32)],
    as_of: NaiveDate,
) -> String {
    let (phase, days_since) = halving_phase(as_of);
    let next = days_to_next_halving(as_of);
    let mut s = format!(
        "{} cycle: phase {} ({}d since last halving",
        symbol.to_uppercase(),
        phase.as_str(),
        days_since
    );
    if let Some(n) = next {
        s.push_str(&format!(", ~{}d to next)", n));
    } else {
        s.push_str(")");
    }
    let best: Vec<String> = season
        .iter()
        .filter(|(_, r, _)| *r > 0.0)
        .map(|(m, r, _)| format!("{m}: {:+.1}%", r))
        .collect();
    let worst: Vec<String> = season
        .iter()
        .filter(|(_, r, _)| *r < 0.0)
        .map(|(m, r, _)| format!("{m}: {:+.1}%", r))
        .collect();
    if !best.is_empty() || !worst.is_empty() {
        s.push_str(&format!(
            "; historically strongest months: {}; weakest: {}",
            if best.is_empty() {
                "none".into()
            } else {
                best.join(", ")
            },
            if worst.is_empty() {
                "none".into()
            } else {
                worst.join(", ")
            }
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_phase_boundaries() {
        // 2020-05-11 halving; 2020-06-01 is 21 days after -> rally.
        let (p, d) = halving_phase(NaiveDate::from_ymd_opt(2020, 6, 1).unwrap());
        assert_eq!(p, HalvingPhase::PostHalvingRally);
        assert_eq!(d, 21);
        // 2021-05-11 is 365 days after -> still rally boundary.
        let (p, _) = halving_phase(NaiveDate::from_ymd_opt(2021, 5, 11).unwrap());
        assert_eq!(p, HalvingPhase::PostHalvingRally);
        // 2021-05-12 -> expansion.
        let (p, _) = halving_phase(NaiveDate::from_ymd_opt(2021, 5, 12).unwrap());
        assert_eq!(p, HalvingPhase::Expansion);
        // 2022-05-11 is 730 days -> expansion boundary.
        let (p, _) = halving_phase(NaiveDate::from_ymd_opt(2022, 5, 11).unwrap());
        assert_eq!(p, HalvingPhase::Expansion);
        // 2022-05-12 -> late cycle.
        let (p, _) = halving_phase(NaiveDate::from_ymd_opt(2022, 5, 12).unwrap());
        assert_eq!(p, HalvingPhase::LateCycle);
        // 2023-05-11 is 1095 days -> late cycle boundary.
        let (p, _) = halving_phase(NaiveDate::from_ymd_opt(2023, 5, 11).unwrap());
        assert_eq!(p, HalvingPhase::LateCycle);
        // 2023-05-12 -> bear/distribution.
        let (p, _) = halving_phase(NaiveDate::from_ymd_opt(2023, 5, 12).unwrap());
        assert_eq!(p, HalvingPhase::BearDistribution);
        // Pre-halving: 2024-01-20 is 91 days before 2024-04-20.
        let (p, _) = halving_phase(NaiveDate::from_ymd_opt(2024, 1, 20).unwrap());
        assert_eq!(p, HalvingPhase::PreHalving);
    }

    #[test]
    fn days_to_next_halving_always_positive() {
        let d = days_to_next_halving(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
        assert_eq!(
            d,
            Some(
                (NaiveDate::from_ymd_opt(2024, 4, 20).unwrap()
                    - NaiveDate::from_ymd_opt(2024, 1, 1).unwrap())
                .num_days()
            )
        );
        // Beyond the last known anchor the calendar extrapolates every ~4 years.
        assert!(days_to_next_halving(NaiveDate::from_ymd_opt(2030, 1, 1).unwrap()).is_some());
        assert!(days_to_next_halving(NaiveDate::from_ymd_opt(2040, 1, 1).unwrap()).is_some());
        // Before the first halving the next known date is returned.
        let d2011 = days_to_next_halving(NaiveDate::from_ymd_opt(2011, 1, 1).unwrap());
        assert_eq!(
            d2011,
            Some(
                (NaiveDate::from_ymd_opt(2012, 11, 28).unwrap()
                    - NaiveDate::from_ymd_opt(2011, 1, 1).unwrap())
                .num_days()
            )
        );
    }

    #[test]
    fn halving_phase_extrapolates_beyond_last_anchor() {
        // 2030-01-01 is 626 days after 2028-04-15 -> expansion.
        let (p, d) = halving_phase(NaiveDate::from_ymd_opt(2030, 1, 1).unwrap());
        assert_eq!(p, HalvingPhase::Expansion);
        assert_eq!(d, 626);
        // 2033-01-01: extrapolated last halving 2032-04-15 -> 261 days -> rally.
        let (p, _) = halving_phase(NaiveDate::from_ymd_opt(2033, 1, 1).unwrap());
        assert_eq!(p, HalvingPhase::PostHalvingRally);
    }

    #[test]
    fn seasonality_averages_consecutive_months_only() {
        // Two years of +10% every January (2019-12-31 -> 2020-01-31, etc.).
        let mut h = Vec::new();
        let mut d = NaiveDate::from_ymd_opt(2019, 12, 31).unwrap();
        let mut price = 100.0;
        for _ in 0..24 {
            h.push((
                d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis(),
                price,
            ));
            d += Duration::days(30);
            if d.month() == 1 {
                price *= 1.10;
            }
        }
        let season = monthly_seasonality(&h);
        let jan = season.iter().find(|(m, _)| *m == 1).unwrap().1;
        assert!((jan - 10.0).abs() < 1.0, "jan avg {jan:.2}%");
        // A month with no data reports 0.0, never NaN.
        for (_, r) in &season {
            assert!(r.is_finite());
        }
    }
}
