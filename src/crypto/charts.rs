#![allow(dead_code)]
//! Premium dark-theme chart renderer using plotters.
//! All charts are generated in-memory via temp files (non-sensitive, ephemeral).

use std::collections::HashMap;

use plotters::prelude::*;
use rand::Rng;
use tracing::debug;

use crate::{
    error::{BotError, Result},
    format::{fmt_large_usd, format_thousands},
    models::{CmcQuote, Portfolio},
};

// ── Color palette ─────────────────────────────────────────────────────────────

const BG: RGBColor = RGBColor(10, 12, 16);
const SURFACE: RGBColor = RGBColor(21, 26, 35);
const BORDER: RGBColor = RGBColor(52, 61, 72);
const TEXT: RGBColor = RGBColor(255, 255, 255);
const DIM: RGBColor = RGBColor(160, 172, 186);
const GREEN: RGBColor = RGBColor(46, 204, 113);
const RED: RGBColor = RGBColor(231, 76, 60);
const BLUE: RGBColor = RGBColor(52, 152, 219);
const ORANGE: RGBColor = RGBColor(230, 126, 34);
const PURPLE: RGBColor = RGBColor(155, 89, 182);
const TEAL: RGBColor = RGBColor(26, 188, 156);
const YELLOW: RGBColor = RGBColor(241, 196, 15);

const ASSET_PALETTE: &[RGBColor] = &[BLUE, GREEN, ORANGE, PURPLE, TEAL, YELLOW, RED];

/// A softer gray used for "cost basis" bars so they stay visible on the dark
/// surface but read as secondary information.
const COST_BAR: RGBColor = RGBColor(78, 90, 106);

/// Compact price formatter tuned for charts (not chat): thousands separators,
/// no trailing noise. Keep in sync with `format::fmt_price` semantics but
/// trimmed so labels stay short and legible.
fn chart_price(p: f64) -> String {
    // Avoid `$-0.00`/`$-0.000000` from rounded-away values.
    let p = if p == 0.0 { 0.0 } else { p };
    let a = p.abs();
    if a >= 1_000.0 {
        format!("${}", format_thousands(p, 0))
    } else if a >= 1.0 {
        format!("${:.2}", p)
    } else if a >= 0.001 {
        format!("${:.4}", p)
    } else {
        format!("${:.6}", p)
    }
}

/// Short axis label (e.g. 1.2M, 840K) that keeps enough precision for the
/// given visible `span`, so narrow ranges don't collapse into duplicate ticks
/// (e.g. 3.04K / 3.08K instead of two identical "3.0K").
fn compact_axis(v: f64, span: f64) -> String {
    let a = v.abs();
    let (suffix, scale) = if a >= 1_000_000_000.0 {
        ("B", 1_000_000_000.0)
    } else if a >= 1_000_000.0 {
        ("M", 1_000_000.0)
    } else if a >= 1_000.0 {
        ("K", 1_000.0)
    } else {
        ("", 1.0)
    };
    let step = (span / scale).abs();
    let decimals = if step >= 10.0 {
        0
    } else if step >= 1.0 {
        1
    } else if step >= 0.1 {
        2
    } else if step >= 0.01 {
        3
    } else {
        4
    };
    format!("{:.*}{}", decimals, v / scale, suffix)
}

/// Change as a signed percent, or an em dash when unknown.
fn chart_change(pct: Option<f64>) -> String {
    match pct {
        Some(p) if p.is_finite() => {
            let rounded = (p * 100.0).round() / 100.0;
            // `-0.0` is `>= 0.0` yet formats as `-0.00` (→ `+-0.00%`).
            if rounded == 0.0 {
                "+0.00%".to_string()
            } else if rounded > 0.0 {
                format!("+{:.2}%", rounded)
            } else {
                format!("{:.2}%", rounded)
            }
        }
        _ => "—".to_string(),
    }
}

fn change_color(pct: Option<f64>) -> RGBColor {
    match pct {
        Some(p) if p < 0.0 => RED,
        Some(_) => GREEN,
        None => DIM,
    }
}

/// `+$1,234.56` / `-$42.30` / `$0.00` — never `-$0.00`.
fn signed_money(v: f64) -> String {
    let r = (v * 100.0).round() / 100.0;
    if r == 0.0 {
        format!("${}", format_thousands(0.0, 2))
    } else if r > 0.0 {
        format!("+${}", format_thousands(r, 2))
    } else {
        format!("-${}", format_thousands(-r, 2))
    }
}

/// `+2.40%` / `-2.40%` / `0.00%` — never `-0.00%`.
fn signed_pct(pct: f64) -> String {
    let r = (pct * 100.0).round() / 100.0;
    if r == 0.0 {
        "0.00%".to_string()
    } else if r > 0.0 {
        format!("+{:.2}%", r)
    } else {
        format!("{:.2}%", r)
    }
}

pub struct ChartRenderer;

/// Reduce a descending list of `(symbol, value)` pairs to at most `max_rows`
/// rows, folding the tail into a single `OTHERS` entry. Keeps the visible bars,
/// donut slices and legend from silently dropping small holdings.
fn fold_tail(values: &[(String, f64)], max_rows: usize) -> Vec<(String, f64)> {
    if values.len() <= max_rows || max_rows < 2 {
        return values.to_vec();
    }
    let mut head: Vec<(String, f64)> = values[..max_rows - 1].to_vec();
    let rest: f64 = values[max_rows - 1..].iter().map(|(_, v)| v).sum();
    head.push(("OTHERS".to_string(), rest));
    head
}

impl ChartRenderer {
    /// Portfolio allocation: ranked allocation bars (left) + donut & legend
    /// (right) with the live total in the donut hole.
    pub async fn portfolio_overview(
        portfolio: &Portfolio,
        prices: &HashMap<String, f64>,
    ) -> Result<Vec<u8>> {
        let holdings = portfolio.holdings_by_symbol();
        if holdings.is_empty() {
            return Err(BotError::ChartError("No holdings to chart".into()));
        }

        // Compute USD values
        let mut values: Vec<(String, f64)> = holdings
            .iter()
            .filter_map(|(sym, &amt)| prices.get(sym.as_str()).map(|&p| (sym.clone(), amt * p)))
            .filter(|(_, v)| *v > 0.0)
            .collect();
        values.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let total: f64 = values.iter().map(|(_, v)| v).sum();
        let invested = portfolio.total_invested_usd();
        let fees = portfolio.total_fees_usd();
        let pnl = total - invested;
        let pnl_pct = if invested > 0.0 {
            pnl / invested * 100.0
        } else {
            0.0
        };
        let n_assets = values.len();

        // Keep bars, donut and legend consistent: show the top 6 assets and
        // fold the remainder into a single OTHERS slice. Previously the 7th+
        // assets vanished from the legend while still inflating the total, so
        // the visible percentages did not sum to 100%.
        const MAX_ROWS: usize = 7;
        let display = fold_tail(&values, MAX_ROWS);

        // Grow the canvas only as far as the taller of the two panels needs, so
        // a one- or two-asset portfolio does not render a mostly-empty card.
        let rows_bottom = 186 + display.len() as i32 * 58;
        let legend_rows = ((display.len() + 1) / 2) as i32;
        let legend_bottom = 340 + 118 + 26 + legend_rows * 22;
        let height = (rows_bottom.max(legend_bottom) + 28).max(300) as u32;

        render_png(1040, height, |root| {
            root.fill(&BG)?;

            // ── Header ──────────────────────────────────────────────────────
            root.draw_text(
                "PORTFOLIO OVERVIEW",
                &TextStyle::from(("sans-serif", 15).into_font()).color(&DIM),
                (34, 22),
            )?;
            root.draw_text(
                &chart_price(total),
                &TextStyle::from(("sans-serif", 42).into_font()).color(&TEXT),
                (34, 48),
            )?;

            let pnl_color = if pnl >= 0.0 { GREEN } else { RED };
            let pnl_sign = if pnl >= 0.0 { "+" } else { "-" };
            root.draw_text(
                &format!(
                    "{}${}  ({}{:.2}%)",
                    pnl_sign,
                    format_thousands(pnl.abs(), 2),
                    pnl_sign,
                    pnl_pct.abs()
                ),
                &TextStyle::from(("sans-serif", 19).into_font()).color(&pnl_color),
                (34, 108),
            )?;
            // Fees are only worth a slot when they are non-zero; otherwise the
            // line is just noise (and used to read the ugly `Fees $-0.00`).
            let fee_part = if fees >= 0.005 {
                format!("   ·   Fees ${}", format_thousands(fees, 2))
            } else {
                String::new()
            };
            let shown_part = if display.len() < n_assets {
                format!("   ·   top {} + others", display.len() - 1)
            } else {
                String::new()
            };
            root.draw_text(
                &format!(
                    "Invested ${}{}   ·   {} {}{}",
                    format_thousands(invested, 2),
                    fee_part,
                    n_assets,
                    if n_assets == 1 { "asset" } else { "assets" },
                    shown_part
                ),
                &TextStyle::from(("sans-serif", 13).into_font()).color(&DIM),
                (34, 140),
            )?;

            root.draw(&PathElement::new(vec![(28, 168), (1012, 168)], &BORDER))?;

            // ── Left: ranked allocation bars ────────────────────────────────
            let card_x0 = 28i32;
            let card_x1 = 596i32;
            let top = 186i32;
            let row_h = 52i32;
            let row_gap = 6i32;
            let bar_x0 = 64i32;
            let bar_x1 = 576i32;

            for (i, (sym, val)) in display.iter().enumerate() {
                let color = ASSET_PALETTE[i % ASSET_PALETTE.len()];
                let y = top + i as i32 * (row_h + row_gap);
                let pct = if total > 0.0 {
                    val / total * 100.0
                } else {
                    0.0
                };

                root.draw(&Rectangle::new(
                    [(card_x0, y), (card_x1, y + row_h)],
                    ShapeStyle {
                        color: SURFACE.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;
                // Color chip
                root.draw(&Rectangle::new(
                    [(card_x0 + 14, y + 13), (card_x0 + 26, y + 25)],
                    ShapeStyle {
                        color: color.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;

                root.draw_text(
                    sym,
                    &TextStyle::from(("sans-serif", 16).into_font()).color(&TEXT),
                    (card_x0 + 36, y + 6),
                )?;
                // USD value on its own line, above the bar so the fill can
                // never paint over the glyphs.
                root.draw_text(
                    &chart_price(*val),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (card_x0 + 36, y + 26),
                )?;

                // Right-aligned percentage (short string, fixed column).
                let pct_label = format!("{:.1}%", pct);
                let pct_x = bar_x1 - 8 - (pct_label.len() as i32) * 9;
                root.draw_text(
                    &pct_label,
                    &TextStyle::from(("sans-serif", 15).into_font()).color(&color),
                    (pct_x, y + 6),
                )?;

                // Track + fill (kept below the text baseline).
                root.draw(&Rectangle::new(
                    [(bar_x0, y + 43), (bar_x1, y + 49)],
                    ShapeStyle {
                        color: BORDER.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;
                let fill_w = ((pct / 100.0) * (bar_x1 - bar_x0) as f64).round() as i32;
                if fill_w > 0 {
                    root.draw(&Rectangle::new(
                        [(bar_x0, y + 43), (bar_x0 + fill_w, y + 49)],
                        ShapeStyle {
                            color: color.to_rgba(),
                            filled: true,
                            stroke_width: 0,
                        },
                    ))?;
                }
            }

            // ── Right: donut + legend ───────────────────────────────────────
            let cx = 786i32;
            let cy = 340i32;
            let radius = 118i32;
            draw_donut(root, &display, total, cx, cy, radius)?;

            // Center label (TOTAL + count), drawn above the hole.
            let total_label = chart_price(total);
            let tsize = if total_label.len() > 9 { 18 } else { 22 };
            let tx = cx - (total_label.len() as i32) * (tsize / 3);
            root.draw_text(
                "TOTAL",
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (cx - 22, cy - 22),
            )?;
            root.draw_text(
                &total_label,
                &TextStyle::from(("sans-serif", tsize).into_font()).color(&TEXT),
                (tx, cy - 2),
            )?;

            // Legend below the donut, single column.
            let legend_top = cy + radius + 26;
            for (i, (sym, val)) in display.iter().enumerate() {
                let color = ASSET_PALETTE[i % ASSET_PALETTE.len()];
                let pct = if total > 0.0 {
                    val / total * 100.0
                } else {
                    0.0
                };
                let col = (i % 2) as i32;
                let row = (i / 2) as i32;
                let lx = cx - radius + col * 160;
                let ly = legend_top + row * 22;
                root.draw(&Rectangle::new(
                    [(lx, ly), (lx + 10, ly + 10)],
                    ShapeStyle {
                        color: color.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;
                root.draw_text(
                    &format!("{} {:.1}%", sym, pct),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&TEXT),
                    (lx + 16, ly - 3),
                )?;
            }

            root.present()?;
            Ok(())
        })
        .await
    }

    /// Top coins by 24h volume: diverging 24h-change bars around a zero axis,
    /// with price, volume and 7d/30d context for each row.
    pub async fn top_movers(quotes: &[CmcQuote]) -> Result<Vec<u8>> {
        // The title promises "by volume": don't rely on caller ordering.
        let mut quotes: Vec<&CmcQuote> = quotes.iter().collect();
        quotes.sort_by(|a, b| {
            b.volume_24h
                .partial_cmp(&a.volume_24h)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let n = quotes.len();
        let header = 118u32;
        let row_h = 66i32;
        render_png(940, header + n as u32 * row_h as u32 + 14, |root| {
            root.fill(&BG)?;

            root.draw_text(
                "TOP MOVERS BY VOLUME 24H",
                &TextStyle::from(("sans-serif", 17).into_font()).color(&TEXT),
                (30, 22),
            )?;
            root.draw_text(
                "24h change · bars scaled to the largest mover",
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (30, 48),
            )?;
            root.draw(&PathElement::new(vec![(28, 72), (912, 72)], &BORDER))?;

            // Column headers
            root.draw_text(
                "ASSET / PRICE",
                &TextStyle::from(("sans-serif", 11).into_font()).color(&DIM),
                (34, 84),
            )?;
            root.draw_text(
                "24h",
                &TextStyle::from(("sans-serif", 11).into_font()).color(&DIM),
                (672, 84),
            )?;
            root.draw_text(
                "7d",
                &TextStyle::from(("sans-serif", 11).into_font()).color(&DIM),
                (800, 84),
            )?;
            root.draw_text(
                "30d",
                &TextStyle::from(("sans-serif", 11).into_font()).color(&DIM),
                (868, 84),
            )?;

            let x0 = 500i32;
            let half = 150i32;
            // Find max abs change for scaling
            let max_abs = quotes
                .iter()
                .map(|q| q.change_24h.unwrap_or(0.0).abs())
                .fold(0.0f64, f64::max)
                .max(1.0);

            for (i, q) in quotes.iter().enumerate() {
                let y = header as i32 + i as i32 * row_h;
                let ch = q.change_24h.unwrap_or(0.0);
                let color = change_color(q.change_24h);

                if i % 2 == 0 {
                    root.draw(&Rectangle::new(
                        [(20, y), (920, y + row_h - 4)],
                        ShapeStyle {
                            color: SURFACE.mix(0.55),
                            filled: true,
                            stroke_width: 0,
                        },
                    ))?;
                }

                // Rank
                root.draw_text(
                    &format!("#{}", q.rank),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (34, y + 12),
                )?;
                // Symbol + price
                root.draw_text(
                    q.symbol.as_str(),
                    &TextStyle::from(("sans-serif", 16).into_font()).color(&TEXT),
                    (66, y + 8),
                )?;
                root.draw_text(
                    &chart_price(q.price_usd),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (66, y + 32),
                )?;

                // Zero axis + diverging bar
                root.draw(&PathElement::new(
                    vec![(x0, y + 10), (x0, y + 48)],
                    BORDER.mix(0.9),
                ))?;
                let bar_w = ((ch.abs() / max_abs) * half as f64).round() as i32;
                if bar_w > 0 {
                    let (bx0, bx1) = if ch >= 0.0 {
                        (x0, x0 + bar_w)
                    } else {
                        (x0 - bar_w, x0)
                    };
                    root.draw(&Rectangle::new(
                        [(bx0, y + 20), (bx1.max(bx0 + 1), y + 40)],
                        ShapeStyle {
                            color: color.mix(0.85),
                            filled: true,
                            stroke_width: 0,
                        },
                    ))?;
                }

                // 24h change + volume
                root.draw_text(
                    &chart_change(q.change_24h),
                    &TextStyle::from(("sans-serif", 16).into_font()).color(&color),
                    (672, y + 8),
                )?;
                root.draw_text(
                    &format!("Vol {}", fmt_large_usd(q.volume_24h)),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (672, y + 32),
                )?;

                // 7d / 30d (columns already labelled in the header)
                root.draw_text(
                    &chart_change(q.change_7d),
                    &TextStyle::from(("sans-serif", 13).into_font())
                        .color(&change_color(q.change_7d)),
                    (800, y + 16),
                )?;
                root.draw_text(
                    &chart_change(q.change_30d),
                    &TextStyle::from(("sans-serif", 13).into_font())
                        .color(&change_color(q.change_30d)),
                    (868, y + 16),
                )?;
            }

            root.present()?;
            Ok(())
        })
        .await
    }

    /// P&L breakdown: cost-vs-value bars per asset with a header summary and
    /// per-asset P&L in both dollars and percent.
    pub async fn pnl_breakdown(
        portfolio: &Portfolio,
        prices: &HashMap<String, f64>,
    ) -> Result<Vec<u8>> {
        // Build from current holdings, not from the purchase log: after a
        // partial sale the log still contains the whole original lot and the
        // chart showed a cost basis for coins the user no longer owns.
        let mut rows: Vec<(String, f64, f64)> = portfolio
            .holdings_by_symbol()
            .into_iter()
            .filter_map(|(sym, amt)| {
                prices.get(&sym).map(|&p| {
                    let cost = portfolio.avg_buy_price(&sym).unwrap_or(0.0) * amt;
                    (sym, cost, amt * p)
                })
            })
            .collect();

        rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

        if rows.is_empty() {
            return Err(BotError::ChartError("No data for P&L chart".into()));
        }

        let total_cost: f64 = rows.iter().map(|r| r.1).sum();
        let total_val: f64 = rows.iter().map(|r| r.2).sum();
        let total_pnl = total_val - total_cost;
        let total_pnl_pct = if total_cost > 0.0 {
            total_pnl / total_cost * 100.0
        } else {
            0.0
        };
        let total_color = if total_pnl >= 0.0 { GREEN } else { RED };

        let row_h = 66i32;
        let header = 168u32;
        let height = header + rows.len() as u32 * row_h as u32 + 12;
        render_png(960, height, |root| {
            root.fill(&BG)?;

            root.draw_text(
                "P&L BREAKDOWN",
                &TextStyle::from(("sans-serif", 17).into_font()).color(&TEXT),
                (30, 22),
            )?;
            root.draw_text(
                &format!(
                    "Value {}   ·   Cost {}   ·   {} {}",
                    chart_price(total_val),
                    chart_price(total_cost),
                    rows.len(),
                    if rows.len() == 1 { "asset" } else { "assets" }
                ),
                &TextStyle::from(("sans-serif", 13).into_font()).color(&DIM),
                (30, 50),
            )?;
            root.draw_text(
                &format!(
                    "{}   ({})",
                    signed_money(total_pnl),
                    signed_pct(total_pnl_pct)
                ),
                &TextStyle::from(("sans-serif", 22).into_font()).color(&total_color),
                (30, 78),
            )?;

            // Legend
            root.draw(&Rectangle::new(
                [(30, 120), (42, 132)],
                ShapeStyle {
                    color: COST_BAR.to_rgba(),
                    filled: true,
                    stroke_width: 0,
                },
            ))?;
            root.draw_text(
                "Cost basis",
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (50, 119),
            )?;
            for (k, color) in ASSET_PALETTE.iter().take(3).enumerate() {
                let sx = 160 + k as i32 * 10;
                root.draw(&Rectangle::new(
                    [(sx, 120), (sx + 8, 132)],
                    ShapeStyle {
                        color: color.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;
            }
            root.draw_text(
                "Current value (asset colour)",
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (194, 119),
            )?;
            root.draw(&PathElement::new(vec![(28, 148), (932, 148)], &BORDER))?;

            let max_val = rows
                .iter()
                .map(|(_, cost, val)| cost.max(*val))
                .fold(0.0f64, f64::max)
                .max(1.0);
            let bx0 = 210i32;
            let bar_max = 500i32;

            for (i, (sym, cost, val)) in rows.iter().enumerate() {
                let y = header as i32 + i as i32 * row_h;
                let color = ASSET_PALETTE[i % ASSET_PALETTE.len()];
                let pnl = val - cost;
                let pnl_pct = if *cost > 0.0 { pnl / cost * 100.0 } else { 0.0 };
                let pnl_color = if pnl >= 0.0 { GREEN } else { RED };

                root.draw_text(
                    sym,
                    &TextStyle::from(("sans-serif", 16).into_font()).color(&TEXT),
                    (34, y + 4),
                )?;
                root.draw_text(
                    &format!("Cost {}", chart_price(*cost)),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (34, y + 28),
                )?;

                // Cost bar (muted gray) + value bar (asset color)
                let cost_w = ((cost / max_val) * bar_max as f64).round() as i32;
                let val_w = ((val / max_val) * bar_max as f64).round() as i32;
                root.draw(&Rectangle::new(
                    [(bx0, y + 10), (bx0 + cost_w.max(1), y + 22)],
                    ShapeStyle {
                        color: COST_BAR.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;
                root.draw(&Rectangle::new(
                    [(bx0, y + 30), (bx0 + val_w.max(1), y + 42)],
                    ShapeStyle {
                        color: color.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;

                // Value at the end of its bar (dim), plus P&L block on the right.
                root.draw_text(
                    &chart_price(*val),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&TEXT),
                    (bx0 + val_w + 10, y + 26),
                )?;

                root.draw_text(
                    &signed_money(pnl),
                    &TextStyle::from(("sans-serif", 16).into_font()).color(&pnl_color),
                    (846, y + 8),
                )?;
                root.draw_text(
                    &signed_pct(pnl_pct),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&pnl_color),
                    (846, y + 32),
                )?;
            }

            root.present()?;
            Ok(())
        })
        .await
    }

    /// Compact quote card for a single symbol: hero price + multi-horizon
    /// changes + volume/market-cap footer.
    pub async fn price_card(quote: &CmcQuote) -> Result<Vec<u8>> {
        render_png(720, 300, |root| {
            root.fill(&BG)?;

            // Hero: symbol + price + 24h change
            root.draw_text(
                quote.symbol.as_str(),
                &TextStyle::from(("sans-serif", 30).into_font()).color(&TEXT),
                (36, 20),
            )?;
            root.draw_text(
                &chart_price(quote.price_usd),
                &TextStyle::from(("sans-serif", 32).into_font()).color(&TEXT),
                (36, 56),
            )?;
            let c24 = change_color(quote.change_24h);
            root.draw_text(
                &format!("24h  {}", chart_change(quote.change_24h)),
                &TextStyle::from(("sans-serif", 18).into_font()).color(&c24),
                (36, 104),
            )?;

            root.draw(&PathElement::new(vec![(28, 144), (692, 144)], &BORDER))?;

            // Multi-horizon grid: 1h / 7d / 30d / 90d
            let changes = [
                ("1h", quote.change_1h),
                ("7d", quote.change_7d),
                ("30d", quote.change_30d),
                ("90d", quote.change_90d),
            ];
            for (i, (label, pct)) in changes.iter().enumerate() {
                let x = 36 + i as i32 * 168;
                root.draw_text(
                    label,
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (x, 166),
                )?;
                root.draw_text(
                    &chart_change(*pct),
                    &TextStyle::from(("sans-serif", 18).into_font()).color(&change_color(*pct)),
                    (x, 186),
                )?;
            }

            root.draw(&PathElement::new(vec![(28, 228), (692, 228)], &BORDER))?;

            // Footer stats. Unknown values must render as a dash, not as a
            // literal "$0.00" / "Rank #0" — some providers omit market cap and
            // rank, which made a perfectly known coin look like a worthless one.
            let vol_s = if quote.volume_24h > 0.0 {
                fmt_large_usd(quote.volume_24h)
            } else {
                "—".to_string()
            };
            let mcap_s = if quote.market_cap > 0.0 {
                fmt_large_usd(quote.market_cap)
            } else {
                "—".to_string()
            };
            let rank_s = if quote.rank > 0 {
                format!("Rank #{}", quote.rank)
            } else {
                "Rank —".to_string()
            };
            root.draw_text(
                &format!("24h Vol  {}", vol_s),
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (36, 248),
            )?;
            root.draw_text(
                &format!("Market Cap  {}", mcap_s),
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (300, 248),
            )?;
            root.draw_text(
                &rank_s,
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (588, 248),
            )?;

            root.present()?;
            Ok(())
        })
        .await
    }
}

/// Normalise a history series to millisecond epochs. Current epochs in seconds
/// (~1.7e9) and milliseconds (~1.7e12) are separated cleanly by `1e11`, so a
/// stray seconds-based provider cannot render `1970` axis labels.
fn normalize_ts_units(points: &[(i64, f64)]) -> Vec<(i64, f64)> {
    let max_ts = points.iter().map(|p| p.0).max().unwrap_or(0);
    if !points.is_empty() && max_ts < 100_000_000_000 {
        points
            .iter()
            .map(|(t, p)| (t.saturating_mul(1000), *p))
            .collect()
    } else {
        points.to_vec()
    }
}

/// Validate and clean historical price points before charting/analysis:
/// - sort by timestamp, dedupe same-timestamp (keep the freshest),
/// - drop non-positive / non-finite prices,
/// - drop transient outliers: any point deviating >50% from the MEDIAN of its
///   local neighborhood (up to 2 neighbours on each side). Catches both single
///   bad samples and 2-point plateaus (e.g. two consecutive garbage quotes),
///   while a real sustained move quickly establishes a new median level and is
///   preserved.
pub fn sanitize_history(points: &[(i64, f64)]) -> Vec<(i64, f64)> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut pts: Vec<(i64, f64)> = points
        .iter()
        .filter(|(_, p)| p.is_finite() && *p > 0.0)
        .copied()
        .collect();
    pts.sort_by_key(|p| p.0);
    // For duplicate timestamps keep the LAST (freshest) sample: reverse → dedup
    // (keeps first) → restore ascending order.
    pts.reverse();
    pts.dedup_by_key(|p| p.0);
    pts.sort_by_key(|p| p.0);

    if pts.len() < 5 {
        return pts; // too few points for a reliable neighborhood median
    }

    // Median of the whole series — hard gate against absurd points (e.g. a
    // 1.6e-5 sample mixed into an ~8.5 series). 100x leaves genuine multi-x
    // moves untouched while catching unit/scale garbage.
    let mut all: Vec<f64> = pts.iter().map(|p| p.1).collect();
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let global_median = all[all.len() / 2];

    // Pass 1: flag points deviating >50% from the median of their +/-3
    // neighbors. A transient spike or a short plateau is flagged even when its
    // own window is contaminated by the bad points themselves — pass 2 then
    // re-checks using only neighbors that survived pass 1, so 3+ consecutive
    // fake samples (e.g. LINK at 13 while it never left ~8.5) are removed too.
    let mut flagged = vec![false; pts.len()];
    for i in 1..pts.len() - 1 {
        let mut nbr: Vec<f64> = Vec::with_capacity(6);
        for j in i.saturating_sub(3)..=i.saturating_add(3) {
            if j != i && j < pts.len() {
                nbr.push(pts[j].1);
            }
        }
        nbr.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = if nbr.len() % 2 == 0 {
            (nbr[nbr.len() / 2 - 1] + nbr[nbr.len() / 2]) / 2.0
        } else {
            nbr[nbr.len() / 2]
        };
        if median > 0.0 && (pts[i].1 - median).abs() / median > 0.5 {
            flagged[i] = true;
        }
    }

    let median_of = |vals: &[f64]| -> f64 {
        let mut v: Vec<f64> = vals.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() {
            0.0
        } else if v.len() % 2 == 0 {
            (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
        } else {
            v[v.len() / 2]
        }
    };

    let mut out = Vec::with_capacity(pts.len());
    out.push(pts[0]);
    for i in 1..pts.len() - 1 {
        if flagged[i] {
            continue; // pass-1 outlier — drop
        }
        // Pass 2: recompute the neighborhood median using only clean neighbors
        // so a bad run cannot pull its own median up and hide itself.
        let mut nbr: Vec<f64> = Vec::with_capacity(6);
        for j in i.saturating_sub(3)..=i.saturating_add(3) {
            if j != i && j < pts.len() && !flagged[j] {
                nbr.push(pts[j].1);
            }
        }
        let median = median_of(&nbr);
        let dev_local = median > 0.0 && (pts[i].1 - median).abs() / median > 0.5;
        let dev_global =
            global_median > 0.0 && (pts[i].1 - global_median).abs() / global_median > 100.0;
        if dev_local || dev_global {
            continue; // transient outlier — drop
        }
        out.push(pts[i]);
    }
    if let Some(&last) = pts.last() {
        out.push(last);
    }
    out
}

impl ChartRenderer {
    pub async fn historical_price_chart(
        symbol: &str,
        points: &[(i64, f64)],
        period_label: &str,
    ) -> Result<Vec<u8>> {
        // Providers hand us millisecond epochs, but be defensive: a stray
        // seconds-based series used to render `1970-01-20` axis labels.
        let points = normalize_ts_units(points);
        let points = sanitize_history(&points);
        if points.len() < 2 {
            return Err(BotError::ChartError(
                "Not enough valid data to chart after validation".into(),
            ));
        }

        let first_point = points.first().unwrap();
        let last_point = points.last().unwrap();
        let baseline = first_point.1;
        let current_price = last_point.1;

        let prices_only: Vec<f64> = points.iter().map(|p| p.1).collect();
        let min_price = prices_only.iter().copied().fold(f64::INFINITY, f64::min);
        let max_price = prices_only
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);

        let price_range = max_price - min_price;
        let y_margin = if price_range > 0.0 {
            price_range * 0.08
        } else {
            baseline * 0.1
        };
        let y_min = (min_price - y_margin).max(0.0);
        let y_max = max_price + y_margin;
        let y_span = (y_max - y_min).max(f64::MIN_POSITIVE);

        let x_min = points.first().unwrap().0;
        let x_max = points.last().unwrap().0;
        let span_ms = (x_max - x_min).max(1);
        let span_hours = span_ms as f64 / 3_600_000.0;
        // Plot on a compact "time offset" axis (minutes for short windows,
        // hours otherwise). Raw millisecond values are ~1.7e12 wide, which
        // makes plotters' tick picker collapse to duplicate/empty labels.
        let unit_ms: f64 = if span_hours <= 6.0 {
            60_000.0
        } else {
            3_600_000.0
        };
        let span_units = span_ms as f64 / unit_ms;
        // Include the time of day whenever more than one tick can fall on the
        // same calendar day; otherwise intraday windows rendered duplicate
        // labels such as "11-23" twice.
        let x_fmt = if span_hours <= 24.0 * 7.0 {
            "%m-%d %H:%M"
        } else if span_hours <= 24.0 * 180.0 {
            "%m-%d"
        } else {
            "%Y-%m"
        };
        // Long "MM-DD HH:MM" labels need more horizontal room per tick.
        let x_label_count = if x_fmt.contains(":") { 4 } else { 5 };
        let x_of = |ts: i64| (ts - x_min) as f64 / unit_ms;
        let period = period_label.to_uppercase();

        render_png(920, 460, |root| {
            root.fill(&BG)?;

            let change_pct = if baseline > 0.0 {
                (current_price - baseline) / baseline * 100.0
            } else {
                0.0
            };
            // Normalise `-0.0` so the sign and the printed number agree.
            let change_pct = if change_pct == 0.0 { 0.0 } else { change_pct };
            let ch_color = if change_pct >= 0.0 { GREEN } else { RED };
            let change_sign = if change_pct >= 0.0 { "+" } else { "" };

            // ── Header ──────────────────────────────────────────────────────
            root.draw_text(
                &format!("{}  ·  {}", symbol.to_uppercase(), period),
                &TextStyle::from(("sans-serif", 15).into_font()).color(&DIM),
                (30, 20),
            )?;
            root.draw_text(
                &chart_price(current_price),
                &TextStyle::from(("sans-serif", 32).into_font()).color(&TEXT),
                (30, 42),
            )?;
            root.draw_text(
                &format!("{}{:.2}% over period", change_sign, change_pct),
                &TextStyle::from(("sans-serif", 16).into_font()).color(&ch_color),
                (30, 88),
            )?;
            root.draw_text(
                &format!(
                    "High {}   ·   Low {}   ·   {} points",
                    chart_price(max_price),
                    chart_price(min_price),
                    points.len()
                ),
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (30, 114),
            )?;

            // ── Chart area ─────────────────────────────────────────────────
            let mut chart = ChartBuilder::on(&root)
                .margin_top(140)
                .margin_bottom(24)
                .margin_left(20)
                .margin_right(24)
                .x_label_area_size(32)
                .y_label_area_size(74)
                .build_cartesian_2d(0f64..span_units, y_min..y_max)?;

            chart
                .configure_mesh()
                .disable_x_mesh()
                // No faint sub-grid: it showed through the translucent area fill
                // as horizontal stripes and made the line hard to read.
                .max_light_lines(0)
                .bold_line_style(BORDER.mix(0.55))
                .light_line_style(BORDER.mix(0.35))
                .axis_style(BORDER.mix(0.9))
                .y_label_formatter(&|y| compact_axis(*y, y_span))
                .x_label_formatter(&|x| {
                    let ts = x_min + (*x * unit_ms).round() as i64;
                    let dt = chrono::DateTime::from_timestamp(ts / 1000, 0)
                        .unwrap_or_else(|| chrono::Utc::now().into());
                    dt.format(x_fmt).to_string()
                })
                .label_style(TextStyle::from(("sans-serif", 11).into_font()).color(&DIM))
                .y_labels(6)
                .x_labels(x_label_count)
                .draw()?;

            // Area fills above / below the first price (baseline).
            let pos_points: Vec<(f64, f64)> = points
                .iter()
                .map(|&(x, y)| (x_of(x), if y >= baseline { y } else { baseline }))
                .collect();
            chart.draw_series(AreaSeries::new(pos_points, baseline, GREEN.mix(0.14)))?;

            let neg_points: Vec<(f64, f64)> = points
                .iter()
                .map(|&(x, y)| (x_of(x), if y <= baseline { y } else { baseline }))
                .collect();
            chart.draw_series(AreaSeries::new(neg_points, baseline, RED.mix(0.14)))?;

            // Baseline (open price of the window).
            chart.draw_series(LineSeries::new(
                vec![(0.0, baseline), (span_units, baseline)],
                ShapeStyle {
                    color: BORDER.mix(0.8),
                    filled: false,
                    stroke_width: 1,
                },
            ))?;

            // Line colored by whether it is above/below the baseline.
            for win in points.windows(2) {
                let p1 = (x_of(win[0].0), win[0].1);
                let p2 = (x_of(win[1].0), win[1].1);
                let y1 = p1.1;
                let y2 = p2.1;

                if (y1 >= baseline && y2 >= baseline) || (y1 <= baseline && y2 <= baseline) {
                    let color = if y1 >= baseline { GREEN } else { RED };
                    chart.draw_series(LineSeries::new(
                        vec![p1, p2],
                        ShapeStyle {
                            color: color.to_rgba(),
                            filled: false,
                            stroke_width: 2,
                        },
                    ))?;
                } else {
                    let t = (baseline - y1) / (y2 - y1);
                    let x_cross = p1.0 + t * (p2.0 - p1.0);
                    let p_cross = (x_cross, baseline);
                    let c1 = if y1 >= baseline { GREEN } else { RED };
                    let c2 = if y2 >= baseline { GREEN } else { RED };
                    chart.draw_series(LineSeries::new(
                        vec![p1, p_cross],
                        ShapeStyle {
                            color: c1.to_rgba(),
                            filled: false,
                            stroke_width: 2,
                        },
                    ))?;
                    chart.draw_series(LineSeries::new(
                        vec![p_cross, p2],
                        ShapeStyle {
                            color: c2.to_rgba(),
                            filled: false,
                            stroke_width: 2,
                        },
                    ))?;
                }
            }

            // Latest-price marker.
            chart.draw_series(std::iter::once(Circle::new(
                (span_units, current_price),
                4,
                ShapeStyle {
                    color: ch_color.to_rgba(),
                    filled: true,
                    stroke_width: 0,
                },
            )))?;

            root.present()?;
            Ok(())
        })
        .await
    }
}

// ── Donut chart helper ────────────────────────────────────────────────────────

/// Draw a donut made of subtle arc segments (2π sweep, small gaps between
/// slices). The center hole is cleared to the background so callers can place
/// text there. Colors follow `ASSET_PALETTE` in the same order as the value
/// list, so bars / legend / donut stay consistent.
fn draw_donut(
    root: &DrawingArea<BitMapBackend<'_>, plotters::coord::Shift>,
    values: &[(String, f64)],
    total: f64,
    cx: i32,
    cy: i32,
    radius: i32,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::f64::consts::PI;

    if total <= 0.0 || values.is_empty() {
        return Ok(());
    }

    // Start at 12 o'clock and sweep clockwise.
    let mut angle = -PI / 2.0;
    let gap = if values.len() > 1 { 0.014 } else { 0.0 };

    for (i, (_, val)) in values.iter().enumerate() {
        let full_sweep = (val / total) * 2.0 * PI;
        if full_sweep <= 0.0 {
            continue;
        }
        let color = ASSET_PALETTE[i % ASSET_PALETTE.len()];

        // Leave a small gap on each edge (but never eat a tiny slice).
        let (a0, sweep) = if full_sweep > gap * 2.5 {
            (angle + gap / 2.0, full_sweep - gap)
        } else {
            (angle, full_sweep)
        };

        let steps = ((sweep * 40.0) as usize).max(4);
        let mut points = Vec::with_capacity(steps + 2);
        points.push((cx, cy));
        for s in 0..=steps {
            let a = a0 + (s as f64 / steps as f64) * sweep;
            let x = cx + (a.cos() * radius as f64).round() as i32;
            let y = cy + (a.sin() * radius as f64).round() as i32;
            points.push((x, y));
        }

        root.draw(&Polygon::new(
            points,
            ShapeStyle {
                color: color.to_rgba(),
                filled: true,
                stroke_width: 0,
            },
        ))?;

        angle += full_sweep;
    }

    // Punch the donut hole (also erases the polygon fan center).
    let hole_r = (radius as f64 * 0.58) as i32;
    root.draw(&Circle::new(
        (cx, cy),
        hole_r as u32,
        ShapeStyle {
            color: BG.to_rgba(),
            filled: true,
            stroke_width: 0,
        },
    ))?;

    // Subtle inner ring for a crisper edge.
    root.draw(&Circle::new(
        (cx, cy),
        hole_r as u32,
        ShapeStyle {
            color: BORDER.mix(0.7),
            filled: false,
            stroke_width: 1,
        },
    ))?;

    Ok(())
}

// ── Render helper ─────────────────────────────────────────────────────────────

static FONT_REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn ensure_font_registered() {
    FONT_REGISTERED.get_or_init(|| {
        let font_data = include_bytes!("Roboto-Regular.ttf");
        match plotters::style::register_font(
            "sans-serif",
            plotters::style::FontStyle::Normal,
            font_data,
        ) {
            Ok(_) => {
                tracing::debug!("Roboto font registered successfully");
                true
            }
            Err(_e) => {
                tracing::warn!("Failed to register Roboto font (falling back to system font)");
                false
            }
        }
    });
}

async fn render_png<F>(width: u32, height: u32, draw: F) -> Result<Vec<u8>>
where
    F: FnOnce(
        &DrawingArea<BitMapBackend<'_>, plotters::coord::Shift>,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>,
{
    ensure_font_registered();

    let id: u32 = rand::thread_rng().gen();
    let path = std::env::temp_dir().join(format!("cg_{id}.png"));
    debug!("Rendering chart to {:?}", path);

    {
        let root = BitMapBackend::new(&path, (width, height)).into_drawing_area();
        draw(&root).map_err(|e| BotError::ChartError(e.to_string()))?;
        root.present()
            .map_err(|e| BotError::ChartError(e.to_string()))?;
    }

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| BotError::ChartError(format!("read chart file: {e}")))?;
    tokio::fs::remove_file(&path).await.ok();
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_change_normalises_negative_zero() {
        // A tiny negative rounds to zero; the chart must not print `+-0.00%`.
        for v in [-0.0001, -0.0, 0.0, 1e-9] {
            let s = chart_change(Some(v));
            assert!(!s.contains("+-"), "double sign for {v}: {s}");
            assert!(!s.contains("-0.00"), "negative zero for {v}: {s}");
        }
        assert_eq!(chart_change(Some(-0.0001)), "+0.00%");
        assert_eq!(chart_change(Some(-2.5)), "-2.50%");
        assert_eq!(chart_change(Some(2.5)), "+2.50%");
        assert_eq!(chart_change(None), "—");
        assert_eq!(chart_change(Some(f64::NAN)), "—");
    }

    #[test]
    fn signed_helpers_never_render_negative_zero() {
        for v in [-0.004, -0.0, 0.0, 0.004] {
            assert!(!signed_money(v).contains("-$"), "money neg-zero: {}", signed_money(v));
            assert!(!signed_pct(v).contains("-0.00"), "pct neg-zero: {}", signed_pct(v));
        }
        assert_eq!(signed_money(-42.3), "-$42.30");
        assert_eq!(signed_money(42.3), "+$42.30");
        assert_eq!(signed_pct(-4.2), "-4.20%");
        assert_eq!(signed_pct(4.2), "+4.20%");
        assert_eq!(signed_pct(0.0), "0.00%");
    }

    #[test]
    fn seconds_epochs_are_normalised_to_millis() {
        let secs = vec![(1_700_000_000i64, 1.0), (1_700_086_400, 2.0)];
        let out = normalize_ts_units(&secs);
        assert_eq!(out[0].0, 1_700_000_000_000);
        assert_eq!(out[1].0, 1_700_086_400_000);

        // Already-millisecond input is left untouched.
        let ms = vec![(1_700_000_000_000i64, 1.0)];
        assert_eq!(normalize_ts_units(&ms)[0].0, 1_700_000_000_000);
    }

    #[test]
    fn sanitize_drops_transient_x2_spike() {
        // A single x2-and-back point that never existed in reality.
        let pts = vec![
            (1_000, 100.0),
            (2_000, 100.0),
            (3_000, 250.0), // fake spike
            (4_000, 100.0),
            (5_000, 100.0),
        ];
        let out = sanitize_history(&pts);
        assert_eq!(out.len(), 4, "spike must be removed");
        assert!(out.iter().all(|(_, p)| *p <= 100.0 + f64::EPSILON));
    }

    #[test]
    fn sanitize_drops_two_point_plateau() {
        // Two consecutive bad samples (x2 and back) must BOTH be removed.
        let pts = vec![
            (1_000, 100.0),
            (2_000, 100.0),
            (3_000, 250.0), // bad
            (4_000, 250.0), // bad
            (5_000, 100.0),
            (6_000, 100.0),
        ];
        let out = sanitize_history(&pts);
        assert_eq!(out.len(), 4, "both plateau points must be removed");
        assert!(out.iter().all(|(_, p)| *p <= 100.0 + f64::EPSILON));
    }

    #[test]
    fn sanitize_drops_three_point_plateau() {
        // LINK case: 3 consecutive fake samples at 13 in an ~8.5 series.
        let pts = vec![
            (1_000, 8.5),
            (2_000, 8.5),
            (3_000, 13.0), // fake
            (4_000, 13.0), // fake
            (5_000, 13.0), // fake
            (6_000, 8.5),
            (7_000, 8.5),
        ];
        let out = sanitize_history(&pts);
        assert_eq!(out.len(), 4, "all three plateau points must be removed");
        assert!(out.iter().all(|(_, p)| *p <= 8.5 + f64::EPSILON));
    }

    #[test]
    fn sanitize_keeps_real_sustained_move() {
        // A genuine doubling that persists must NOT be removed.
        let pts = vec![
            (1_000, 100.0),
            (2_000, 100.0),
            (3_000, 200.0),
            (4_000, 200.0),
            (5_000, 200.0),
        ];
        let out = sanitize_history(&pts);
        assert_eq!(out.len(), 5, "real move must be preserved");
    }

    #[test]
    fn sanitize_sorts_dedupes_and_drops_invalid() {
        let pts = vec![
            (2_000, 10.0),
            (1_000, 0.0),      // invalid price
            (2_000, 12.0),     // duplicate ts — keep last (freshest)
            (3_000, f64::NAN), // invalid
            (4_000, -5.0),     // invalid
        ];
        let out = sanitize_history(&pts);
        let ts: Vec<i64> = out.iter().map(|(t, _)| *t).collect();
        assert_eq!(ts, vec![2_000], "only the valid, deduped point survives");
        assert_eq!(out[0].1, 12.0, "keep the freshest sample for duplicate ts");
    }

    #[test]
    fn axis_labels_stay_distinct_on_narrow_ranges() {
        // 3.0K/3.1K style labels would collapse e.g. 3040 and 3083.
        let span = 60.0;
        let lo = compact_axis(3_040.0, span);
        let hi = compact_axis(3_083.0, span);
        assert_ne!(lo, hi, "narrow range must not produce identical ticks");
        assert!(lo.ends_with('K') && hi.ends_with('K'));

        // Wide ranges keep the compact one-decimal form.
        assert_eq!(compact_axis(84_000.0, 3_000.0), "84.0K");
        assert_eq!(compact_axis(1_200_000.0, 3_000_000.0), "1.2M");
    }

    #[test]
    fn sanitize_empty() {
        assert!(sanitize_history(&[]).is_empty());
    }

    // ── Renderer smoke tests ────────────────────────────────────────────────

    fn is_png(bytes: &[u8]) -> bool {
        bytes.len() > 8 && bytes[0..4] == [0x89, b'P', b'N', b'G']
    }

    fn demo_portfolio(symbols: &[(&str, f64, f64)]) -> Portfolio {
        let mut p = Portfolio::default();
        for (i, (sym, amount, cost)) in symbols.iter().enumerate() {
            p.holdings.push(crate::models::Holding {
                id: i as u64 + 1,
                wallet_id: 1,
                symbol: sym.to_string(),
                amount: *amount,
                corrections: vec![],
            });
            p.purchases.push(crate::models::Purchase {
                id: i as u64 + 1,
                symbol: sym.to_string(),
                amount: *amount,
                cost_usd: *cost,
                paid_amount: *cost,
                paid_currency: crate::models::PaidCurrency::Usd,
                date: chrono::Utc::now(),
                source: "test".into(),
                notes: None,
                created_at: chrono::Utc::now(),
                corrections: vec![],
            });
        }
        p
    }

    #[tokio::test]
    async fn renders_history_card_and_movers_as_valid_png() {
        let base = chrono::Utc::now().timestamp_millis();
        let pts: Vec<(i64, f64)> = (0..30)
            .map(|i| (base + i * 3_600_000, 100.0 + (i as f64) * 1.5))
            .collect();
        let hist = ChartRenderer::historical_price_chart("BTC", &pts, "1d")
            .await
            .unwrap();
        assert!(is_png(&hist), "history chart must be a PNG");

        let q = CmcQuote {
            symbol: "BTC".into(),
            name: "Bitcoin".into(),
            price_usd: 84_000.0,
            market_cap: 1.6e12,
            volume_24h: 3.2e10,
            change_1h: None,
            change_24h: Some(2.5),
            change_7d: Some(-1.0),
            change_30d: None,
            change_90d: Some(12.0),
            change_365d: None,
            change_730d: None,
            ath: None,
            ath_change_percentage: None,
            high_24h: None,
            low_24h: None,
            rank: 1,
            circulating_supply: 1.9e7,
            last_updated: chrono::Utc::now(),
            anomaly_warning: None,
        };
        let card = ChartRenderer::price_card(&q).await.unwrap();
        assert!(is_png(&card), "price card must be a PNG");

        let top = ChartRenderer::top_movers(&[q]).await.unwrap();
        assert!(is_png(&top), "top movers must be a PNG");
    }

    #[test]
    fn fold_tail_aggregates_small_holdings_into_others() {
        let vals: Vec<(String, f64)> = (0..9)
            .map(|i| (format!("A{i}"), 100.0 - i as f64))
            .collect();
        let total: f64 = vals.iter().map(|(_, v)| v).sum();
        let folded = fold_tail(&vals, 7);
        assert_eq!(folded.len(), 7);
        assert_eq!(folded.last().unwrap().0, "OTHERS");
        // Nothing is lost: the folded list still sums to the original total.
        let folded_total: f64 = folded.iter().map(|(_, v)| v).sum();
        assert!((folded_total - total).abs() < 1e-9);

        // Short lists pass through untouched.
        let short = fold_tail(&vals[..3], 7);
        assert_eq!(short.len(), 3);
    }

    #[tokio::test]
    async fn renders_single_asset_portfolio_donut() {
        let port = demo_portfolio(&[("BTC", 0.5, 20_000.0)]);
        let prices = std::collections::HashMap::from([("BTC".to_string(), 84_000.0)]);
        let img = ChartRenderer::portfolio_overview(&port, &prices)
            .await
            .unwrap();
        assert!(is_png(&img), "single-asset portfolio must render");

        let pnl = ChartRenderer::pnl_breakdown(&port, &prices).await.unwrap();
        assert!(is_png(&pnl), "single-asset P&L must render");
    }

    #[tokio::test]
    async fn rejects_empty_portfolio() {
        let port = Portfolio::default();
        let prices: HashMap<String, f64> = HashMap::new();
        assert!(ChartRenderer::portfolio_overview(&port, &prices)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn price_card_renders_with_unknown_market_cap_and_rank() {
        // Exchange-only providers can leave market cap and rank at zero. The
        // card must still render (and show a dash rather than "$0.00"/"#0").
        let quote = CmcQuote {
            symbol: "PEPE".into(),
            name: "Pepe".into(),
            price_usd: 0.00000123,
            market_cap: 0.0,
            volume_24h: 0.0,
            change_1h: None,
            change_24h: Some(0.0),
            change_7d: None,
            change_30d: None,
            change_90d: None,
            change_365d: None,
            change_730d: None,
            ath: None,
            ath_change_percentage: None,
            high_24h: None,
            low_24h: None,
            rank: 0,
            circulating_supply: 0.0,
            last_updated: chrono::Utc::now(),
            anomaly_warning: None,
        };
        let img = ChartRenderer::price_card(&quote).await.unwrap();
        assert!(is_png(&img), "price card must render without cap/rank");
    }
}
