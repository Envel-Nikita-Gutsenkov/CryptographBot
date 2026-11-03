//! "Bottom radar" — the explicit answer to *"is this the bottom, should I buy?"*
//!
//! The bot already reports price moves (`/alerts`), dip zones (`/dip`) and a
//! tail-risk ladder (`/sniper`). What was missing is a single verdict that says
//! *"here is a concrete price, here are the bottom signals, consider buying"* —
//! regardless of whether the coin is already in the portfolio.
//!
//! Everything is derived from [`crate::trading::sniper::DipSniperReport`], so
//! `/bottom`, the proactive alert and `/sniper` can never disagree. Pure and
//! deterministic: no network, no clock.

use crate::{
    models::onchain::DerivativesRead,
    trading::sniper::{
        fmt_money, price_decimals, CapBand, Capitulation, DipSniperReport, GUARD_PROFIT,
    },
};

/// One piece of evidence that fired this scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// 14-day drawdown Z-score is deep.
    ZScore,
    /// RSI14 is oversold.
    Oversold,
    /// Current volume is a >2σ spike.
    VolumeSpike,
    /// The last candle has a long lower wick (buyers absorbed the flush).
    LongWick,
    /// Open interest was flushed together with price — leverage reset.
    Flush,
}

impl Hit {
    pub fn label(self, russian: bool) -> &'static str {
        match (self, russian) {
            (Hit::ZScore, true) => "глубокая Z-просадка (14д)",
            (Hit::ZScore, false) => "deep 14d drawdown Z-score",
            (Hit::Oversold, true) => "RSI14 перепродан",
            (Hit::Oversold, false) => "RSI14 oversold",
            (Hit::VolumeSpike, true) => "всплеск объёма (капитуляция)",
            (Hit::VolumeSpike, false) => "volume spike (capitulation)",
            (Hit::LongWick, true) => "длинная нижняя тень (откуп)",
            (Hit::LongWick, false) => "long lower wick (absorption)",
            (Hit::Flush, true) => "сброс открытого интереса (leverage reset)",
            (Hit::Flush, false) => "open-interest flush (leverage reset)",
        }
    }
}

/// What to do right now, derived only from the numbers.
#[derive(Debug, Clone, PartialEq)]
pub enum BottomVerdict {
    /// Strong statistical evidence this is a local bottom.
    Bottom { price: f64 },
    /// Some evidence — accumulate in tranches, keep a bid lower.
    Accumulate { price: f64 },
    /// No bottom evidence; wait for `price`.
    Wait { price: f64 },
    /// Price is far above the average entry — do not add here.
    AboveAverage { price: f64 },
    /// Not enough data to say anything useful.
    NoData,
}

impl BottomVerdict {
    /// Whether the verdict tells the user to put money to work now.
    pub fn is_buy(&self) -> bool {
        matches!(
            self,
            BottomVerdict::Bottom { .. } | BottomVerdict::Accumulate { .. }
        )
    }

    pub fn label(&self, russian: bool) -> &'static str {
        match (self, russian) {
            (BottomVerdict::Bottom { .. }, true) => "ПОХОЖЕ НА ДНО — покупай частями",
            (BottomVerdict::Bottom { .. }, false) => "LIKELY BOTTOM — buy in tranches",
            (BottomVerdict::Accumulate { .. }, true) => "БЛИЗКО К ДНУ — набирай частями",
            (BottomVerdict::Accumulate { .. }, false) => "NEAR THE BOTTOM — accumulate",
            (BottomVerdict::Wait { .. }, true) => "НЕ ДНО — жди уровень",
            (BottomVerdict::Wait { .. }, false) => "NOT THE BOTTOM — wait for the level",
            (BottomVerdict::AboveAverage { .. }, true) => "ВЫШЕ СРЕДНЕЙ — не докупай здесь",
            (BottomVerdict::AboveAverage { .. }, false) => "ABOVE YOUR AVERAGE — do not add here",
            (BottomVerdict::NoData, true) => "НЕТ ДАННЫХ",
            (BottomVerdict::NoData, false) => "NO DATA",
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            BottomVerdict::Bottom { .. } => "🟢",
            BottomVerdict::Accumulate { .. } => "🟡",
            BottomVerdict::Wait { .. } => "⏳",
            BottomVerdict::AboveAverage { .. } => "🔴",
            BottomVerdict::NoData => "⚪",
        }
    }
}

/// Composite "is this the bottom?" reading for one symbol.
#[derive(Debug, Clone)]
pub struct BottomSignal {
    pub symbol: String,
    pub price: f64,
    pub regime: String,
    pub rsi14: Option<f64>,
    pub z14: f64,
    pub capitulation: u8,
    pub cap_max: u8,
    pub cap_band: CapBand,
    /// 0..100 composite bottom confidence (capitulation + oversold + drawdown,
    /// minus the chance a deeper level still prints).
    pub bottom_score: u8,
    /// Probability of touching the first (shallowest) bucket inside 30 days.
    pub deeper_prob_30d: f64,
    /// Concrete level to bid for — the shallowest bucket.
    pub buy_price: f64,
    pub avg_entry: Option<f64>,
    pub cash: f64,
    pub verdict: BottomVerdict,
    /// Evidence that fired, positive signals first.
    pub hits: Vec<Hit>,
    /// The drop from the 14-day anchor, percent (negative).
    pub drawdown_pct: f64,
    /// Price is falling on fresh shorts — the drop can continue.
    pub shorts_piling: bool,
    /// Futures tape: funding / OI / long-short — tells whether the fall is
    /// exhausting (longs flushed) or just starting (shorts piling).
    pub derivatives: Option<crate::models::onchain::DerivativesSignal>,
    pub history_days: usize,
}

impl BottomSignal {
    /// Human label for the 0..100 score.
    pub fn score_band(&self, russian: bool) -> &'static str {
        match (self.bottom_score, russian) {
            (60..=100, true) => "ДНО ВЕРОЯТНО",
            (60..=100, false) => "BOTTOM LIKELY",
            (35..=59, true) => "БЛИЗКО К ДНУ",
            (35..=59, false) => "NEAR BOTTOM",
            (20..=34, true) => "НАБЛЮДАЕМ",
            (20..=34, false) => "WATCH",
            (_, true) => "НЕ ДНО",
            (_, false) => "NOT A BOTTOM",
        }
    }

    /// Telegram HTML rendering.
    pub fn render_html(&self, russian: bool) -> String {
        let dec = price_decimals(self.price);
        let mut out = String::new();

        let title = if russian {
            "🧭 <b>BOTTOM RADAR</b>"
        } else {
            "🧭 <b>BOTTOM RADAR</b>"
        };
        let rsi = self
            .rsi14
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "—".into());
        out.push_str(&format!(
            "{title} · {} · {} USD\n\
             regime {} · RSI14 {} · Z(14d) {:+.2}σ · dd {:+.1}% · cap {}/{} {}\n",
            self.symbol,
            fmt_money(self.price, dec),
            self.regime,
            rsi,
            self.z14,
            self.drawdown_pct,
            self.capitulation,
            self.cap_max,
            self.cap_band.icon(),
        ));

        // Score banner.
        out.push_str(&format!(
            "\n📊 <b>{}</b> · признаки дна <b>{}/100</b>\n",
            self.score_band(russian),
            self.bottom_score,
        ));

        if self.hits.is_empty() {
            out.push_str(&format!(
                "• {}\n",
                if russian {
                    "пока нет подтверждающих сигналов"
                } else {
                    "no confirming signals yet"
                }
            ));
        } else {
            for h in &self.hits {
                out.push_str(&format!(
                    "• {}\n",
                    html_escape::encode_text(h.label(russian))
                ));
            }
        }
        if self.shorts_piling {
            out.push_str(&format!(
                "• ⚠️ {}\n",
                if russian {
                    "падение на новых шортах — ещё не дно"
                } else {
                    "drop on fresh shorts — not the bottom yet"
                }
            ));
        }

        if let Some(d) = self.derivatives {
            let mut parts: Vec<String> = Vec::new();
            if let Some(f) = d.funding_avg_pct {
                parts.push(format!("funding {:+.3}%/8h", f * 100.0));
            }
            if let Some(oi) = d.oi_change_24h_pct {
                parts.push(format!("OI 24ч {oi:+.1}%"));
            }
            if let Some(oi) = d.oi_change_7d_pct {
                parts.push(format!("OI 7д {oi:+.1}%"));
            }
            if let Some(ls) = d.long_short_ratio {
                parts.push(format!("L/S {ls:.2}"));
            }
            if !parts.is_empty() {
                out.push_str(&format!(
                    "🛢 {} · {}\n",
                    html_escape::encode_text(&parts.join(" · ")),
                    html_escape::encode_text(d.read.verdict(russian))
                ));
            }
        }

        out.push_str(&format!(
            "\n🎯 <b>{} {}</b>\n",
            self.verdict.icon(),
            html_escape::encode_text(self.verdict.label(russian))
        ));

        // The concrete number the user asked for.
        let discount = (self.buy_price / self.price - 1.0) * 100.0;
        out.push_str(&format!(
            "{} <b>{}</b> · {:+.1}% · {}\n",
            if russian {
                "Уровень покупки:"
            } else {
                "Buy level:"
            },
            fmt_money(self.buy_price, dec),
            discount,
            html_escape::encode_text(&format!(
                "{} {:.0}%",
                if russian {
                    "шанс дойти за 30д ≈"
                } else {
                    "chance to touch in 30d ≈"
                },
                self.deeper_prob_30d * 100.0
            )),
        ));

        if let Some(avg) = self.avg_entry {
            let vs = (self.price / avg - 1.0) * 100.0;
            out.push_str(&format!(
                "{} <b>{}</b> · {}\n",
                if russian {
                    "Средняя закупка:"
                } else {
                    "Avg entry:"
                },
                fmt_money(avg, dec),
                html_escape::encode_text(&format!(
                    "{} {vs:+.1}%",
                    if russian {
                        "цена к ней"
                    } else {
                        "price vs it"
                    }
                )),
            ));
        }

        if self.cash > 0.0 {
            out.push_str(&format!(
                "{} <b>${:.0}</b>\n",
                if russian { "Кэш: " } else { "Cash: " },
                self.cash
            ));
        }

        // Plain-language plan.
        out.push_str(&format!(
            "\n<i>{}</i>\n<i>{} {} closes</i>",
            html_escape::encode_text(&self.plan(russian)),
            if russian {
                "История:"
            } else {
                "History:"
            },
            self.history_days
        ));
        out
    }

    /// One-sentence plan that matches the verdict.
    pub fn plan(&self, russian: bool) -> String {
        let dec = price_decimals(self.price);
        let level = fmt_money(self.buy_price, dec);
        match &self.verdict {
            BottomVerdict::Bottom { .. } => {
                if russian {
                    format!(
                        "Признаки дна сильные: возьми 1/3 кэша по рынку, остаток — лимиткой на {level}."
                    )
                } else {
                    format!(
                        "Bottom evidence is strong: take 1/3 at market, keep the rest as a bid at {level}."
                    )
                }
            }
            BottomVerdict::Accumulate { .. } => {
                if russian {
                    format!(
                        "Дно ещё не подтверждено: бери 1/4 сейчас, основную часть — на {level}."
                    )
                } else {
                    format!("Bottom is not confirmed: take 1/4 now, the bulk as a bid at {level}.")
                }
            }
            BottomVerdict::Wait { .. } => {
                if russian {
                    format!("Это не дно. Держи лимитку на {level} — там вероятность покупки лучше.")
                } else {
                    format!("Not a bottom. Keep a bid at {level} where the entry is better.")
                }
            }
            BottomVerdict::AboveAverage { .. } => {
                if russian {
                    format!("Цена выше твоей средней — докупка её размывает. Жди {level}.")
                } else {
                    format!(
                        "Price is above your average — an add would dilute it. Wait for {level}."
                    )
                }
            }
            BottomVerdict::NoData => {
                if russian {
                    "Мало данных для вердикта.".to_string()
                } else {
                    "Not enough data for a verdict.".to_string()
                }
            }
        }
    }

    /// Plain-text variant for logs / AI narration.
    pub fn plain(&self, russian: bool) -> String {
        let dec = price_decimals(self.price);
        let mut s = format!(
            "BOTTOM RADAR {} {} ({})\nregime {} · RSI14 {} · Z14 {:+.2}sigma · cap {}/{}\n\
             bottom score {}/100 ({})\nbuy level {} ({:+.1}%) · P(touch 30d) {:.0}%\n",
            self.symbol,
            fmt_money(self.price, dec),
            if russian { "ru" } else { "en" },
            self.regime,
            self.rsi14
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "-".into()),
            self.z14,
            self.capitulation,
            self.cap_max,
            self.bottom_score,
            self.score_band(russian),
            fmt_money(self.buy_price, dec),
            (self.buy_price / self.price - 1.0) * 100.0,
            self.deeper_prob_30d * 100.0,
        );
        if let Some(avg) = self.avg_entry {
            s.push_str(&format!(
                "avg entry {} · price vs avg {:+.1}%\n",
                fmt_money(avg, dec),
                (self.price / avg - 1.0) * 100.0
            ));
        }
        for h in &self.hits {
            s.push_str(&format!("signal: {}\n", h.label(russian)));
        }
        if let Some(d) = self.derivatives {
            let mut parts: Vec<String> = Vec::new();
            if let Some(f) = d.funding_avg_pct {
                parts.push(format!("funding {:+.3}%/8h", f * 100.0));
            }
            if let Some(oi) = d.oi_change_24h_pct {
                parts.push(format!("OI24h {oi:+.1}%"));
            }
            if let Some(ls) = d.long_short_ratio {
                parts.push(format!("L/S {ls:.2}"));
            }
            if !parts.is_empty() {
                s.push_str(&format!(
                    "futures: {} · {}\n",
                    parts.join(" · "),
                    d.read.verdict(russian)
                ));
            }
        }
        s.push_str(&format!(
            "verdict: {}\nplan: {}\n",
            self.verdict.label(russian),
            self.plan(russian)
        ));
        s
    }
}

/// Derive the bottom reading from the deterministic sniper report.
pub fn bottom_signal(report: &DipSniperReport) -> BottomSignal {
    let price = report.price;
    let cap: &Capitulation = &report.capitulation;
    let mut score = 0.0f64;
    let mut hits: Vec<Hit> = Vec::new();

    // Capitulation score is already a 0..100 blend of Z/RSI/volume/wick/OI.
    let ratio = (cap.score as f64 / cap.max_score.max(1) as f64).clamp(0.0, 1.0);
    score += ratio * 55.0;

    // Oversold RSI, graded 40 → 20.
    if let Some(rsi) = report.rsi14 {
        if rsi <= 40.0 {
            score += ((40.0 - rsi) / 20.0).clamp(0.0, 1.0) * 15.0;
            if rsi <= 35.0 {
                hits.push(Hit::Oversold);
            }
        }
    }

    // Deep drawdown Z, graded -1σ → -2.5σ.
    if report.z14 <= -1.0 {
        score += ((-1.0 - report.z14) / 1.5).clamp(0.0, 1.0) * 15.0;
        hits.push(Hit::ZScore);
    }

    if cap.volume_spike {
        score += 5.0;
        hits.push(Hit::VolumeSpike);
    }
    if cap.long_wick {
        score += 5.0;
        hits.push(Hit::LongWick);
    }
    if cap.oi_flush {
        score += 5.0;
        hits.push(Hit::Flush);
    }
    if cap.z_hit {
        // Already partly inside the Z component; mark it as fired for display.
        if !hits.contains(&Hit::ZScore) {
            hits.push(Hit::ZScore);
        }
    }

    // A level that is still likely to print means "this is not the bottom yet".
    let deeper_prob = report
        .buckets
        .first()
        .map(|b| b.prob_touch_30d)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    if deeper_prob > 0.35 {
        // A likely-to-print deeper level discounts "this is THE bottom now", but
        // it must not swamp an otherwise strong capitulation reading.
        score -= ((deeper_prob - 0.35) * 60.0).min(20.0);
    }
    // Fresh shorts mean the fall can continue.
    let shorts = report
        .derivatives
        .map(|d| d.read == DerivativesRead::ShortsPiling)
        .unwrap_or(false);
    if shorts {
        score -= 15.0;
        // A short-driven flush is not a bottom signal.
        hits.retain(|h| *h != Hit::Flush);
    }

    let bottom_score = score.clamp(0.0, 100.0).round() as u8;
    let buy_price = report
        .buckets
        .first()
        .map(|b| b.limit_price)
        .filter(|p| p.is_finite() && *p > 0.0)
        .unwrap_or(price * 0.85);

    let avg = report.avg_entry.filter(|a| a.is_finite() && *a > 0.0);
    let too_high = avg
        .map(|a| price > a * (1.0 + GUARD_PROFIT))
        .unwrap_or(false);

    let verdict = if !price.is_finite() || price <= 0.0 {
        BottomVerdict::NoData
    } else if too_high {
        // Hard rule: an add above your average only dilutes it. The bottom
        // score cannot override this — the user asked to protect the entry.
        BottomVerdict::AboveAverage { price: buy_price }
    } else if bottom_score >= 60 {
        BottomVerdict::Bottom { price: buy_price }
    } else if bottom_score >= 35 {
        BottomVerdict::Accumulate { price: buy_price }
    } else {
        BottomVerdict::Wait { price: buy_price }
    };

    BottomSignal {
        symbol: report.symbol.clone(),
        price,
        regime: report.regime.clone(),
        rsi14: report.rsi14,
        z14: report.z14,
        capitulation: cap.score,
        cap_max: cap.max_score,
        cap_band: cap.band,
        bottom_score,
        deeper_prob_30d: deeper_prob,
        buy_price,
        avg_entry: avg,
        cash: report.cash,
        verdict,
        hits,
        drawdown_pct: (price / report.anchor_price.max(f64::MIN_POSITIVE) - 1.0) * 100.0,
        shorts_piling: shorts,
        derivatives: report.derivatives,
        history_days: report.history_days,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::sniper::sniper_report;
    use crate::trading::Forecast;

    fn series(n: usize, start: f64, step: f64) -> Vec<(i64, f64)> {
        (0..n)
            .map(|i| (i as i64 * 86_400_000, start + step * i as f64))
            .collect()
    }

    fn forecast_for(symbol: &str, price: f64, vol: f64, drift: f64, rsi: Option<f64>) -> Forecast {
        let hist = series(300, price, 0.0);
        let mut f = crate::trading::forecast::forecast(symbol, price, &hist);
        f.vol_daily_pct = vol;
        f.drift_daily_pct = drift;
        f.rsi14 = rsi;
        f.regime = "SIDEWAYS".into();
        f
    }

    fn calm_report() -> DipSniperReport {
        let f = forecast_for("BTC", 77_000.0, 1.5, -0.02, Some(50.0));
        sniper_report(
            &f,
            &series(300, 60_000.0, 10.0),
            None,
            None,
            None,
            0.0,
            0.0,
            false,
        )
    }

    fn crashed_report(avg: Option<f64>) -> DipSniperReport {
        // Price deep below the 14d anchor with very high vol → deep Z, oversold.
        let mut f = forecast_for("ETH", 1_800.0, 4.0, -0.5, Some(18.0));
        f.regime = "DIP".into();
        let mut hist = series(200, 2_600.0, 0.0);
        hist.push((200 * 86_400_000, 1_800.0));
        sniper_report(&f, &hist, None, None, avg, 10.0, 1_000.0, false)
    }

    #[test]
    fn calm_market_is_not_a_bottom() {
        let s = bottom_signal(&calm_report());
        assert!(matches!(s.verdict, BottomVerdict::Wait { .. }));
        assert!(s.bottom_score < 40, "score {}", s.bottom_score);
        assert!(!s.verdict.is_buy());
    }

    #[test]
    fn capitulation_scores_a_bottom_and_buys() {
        let s = bottom_signal(&crashed_report(None));
        assert!(matches!(s.verdict, BottomVerdict::Bottom { .. }));
        assert!(s.verdict.is_buy());
        assert!(s.bottom_score >= 60, "score {}", s.bottom_score);
    }

    #[test]
    fn above_average_blocks_the_add() {
        let s = bottom_signal(&crashed_report(Some(1_000.0)));
        assert!(matches!(s.verdict, BottomVerdict::AboveAverage { .. }));
        assert!(!s.verdict.is_buy());
    }

    #[test]
    fn buy_level_is_below_spot_and_bounded() {
        let s = bottom_signal(&calm_report());
        assert!(s.buy_price < s.price);
        assert!(s.buy_price > 0.0);
        assert!((0.0..=1.0).contains(&s.deeper_prob_30d));
    }

    #[test]
    fn html_has_verdict_and_a_price() {
        let s = bottom_signal(&calm_report());
        let html = s.render_html(true);
        assert!(html.contains("BOTTOM RADAR"));
        assert!(html.contains("Уровень покупки"));
        assert!(html.contains(&s.verdict.label(true)));
    }

    #[test]
    fn plain_round_trips_scores() {
        let s = bottom_signal(&crashed_report(None));
        let p = s.plain(false);
        assert!(p.contains("bottom score"));
        assert!(p.contains("verdict:"));
    }

    #[test]
    fn derivatives_line_is_rendered() {
        use crate::models::onchain::{DerivativesRead, DerivativesSignal};
        let mut r = calm_report();
        r.derivatives = Some(DerivativesSignal {
            funding_avg_pct: Some(-0.0001),
            oi_change_24h_pct: Some(-4.2),
            oi_change_7d_pct: Some(-9.0),
            long_short_ratio: Some(1.1),
            taker_buy_sell: None,
            price_change_24h_pct: -2.0,
            read: DerivativesRead::LongsFlushed,
        });
        let html = bottom_signal(&r).render_html(true);
        assert!(html.contains("funding"));
        assert!(html.contains("OI 24ч"));
        assert!(html.contains("лонги вымыты"));
        let plain = bottom_signal(&r).plain(false);
        assert!(plain.contains("futures:"));
        assert!(plain.contains("longs flushed"));
    }
}
