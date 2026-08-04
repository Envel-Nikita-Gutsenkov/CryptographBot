#![allow(dead_code)]
//! Premium dark-theme chart renderer using plotters.
//! All charts are generated in-memory via temp files (non-sensitive, ephemeral).

use std::collections::HashMap;

use plotters::prelude::*;
use rand::Rng;
use tracing::debug;

use crate::{
    error::{BotError, Result},
    format::{fmt_change_plain, fmt_large_usd, fmt_price},
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

pub struct ChartRenderer;

impl ChartRenderer {
    /// Portfolio allocation pie chart + summary bars.
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
        let pnl = total - invested;
        let pnl_pct = if invested > 0.0 {
            pnl / invested * 100.0
        } else {
            0.0
        };

        render_png(900, 520, |root| {
            root.fill(&BG)?;

            // Title
            root.draw_text(
                "PORTFOLIO OVERVIEW",
                &TextStyle::from(("sans-serif", 18).into_font()).color(&DIM),
                (30, 20),
            )?;

            // Total value
            root.draw_text(
                &format!("${:.2}", total),
                &TextStyle::from(("sans-serif", 36).into_font()).color(&TEXT),
                (30, 50),
            )?;

            // P&L
            let pnl_color = if pnl >= 0.0 { &GREEN } else { &RED };
            let pnl_str = if pnl >= 0.0 {
                format!("+{:.2}% (+${:.2})", pnl_pct, pnl.abs())
            } else {
                format!("-{:.2}% (-${:.2})", pnl_pct.abs(), pnl.abs())
            };
            root.draw_text(
                &pnl_str,
                &TextStyle::from(("sans-serif", 16).into_font()).color(pnl_color),
                (30, 95),
            )?;

            // Invested label
            root.draw_text(
                &format!(
                    "Invested: ${:.2}  |  Fees: ${:.2}",
                    invested,
                    portfolio.total_fees_usd()
                ),
                &TextStyle::from(("sans-serif", 13).into_font()).color(&DIM),
                (30, 120),
            )?;

            // Divider
            root.draw(&PathElement::new(vec![(20, 145), (880, 145)], &BORDER))?;

            // Asset bars (left panel)
            let bar_area = root.margin(160, 10, 10, 420);
            let row_h = 42usize;
            let bar_gap = 10usize;

            for (i, (sym, val)) in values.iter().enumerate().take(7) {
                let color = ASSET_PALETTE[i % ASSET_PALETTE.len()];
                let y0 = i * (row_h + bar_gap);

                // Background card
                bar_area.draw(&Rectangle::new(
                    [(0, y0 as i32), (430, (y0 + row_h) as i32)],
                    ShapeStyle {
                        color: SURFACE.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;

                // Symbol label
                bar_area.draw_text(
                    sym,
                    &TextStyle::from(("sans-serif", 14).into_font()).color(&TEXT),
                    (12, (y0 + 6) as i32),
                )?;

                // Value + pct
                let pct = val / total * 100.0;
                let label_text = format!("{:.1}%  (${:.0})", pct, val);
                bar_area.draw_text(
                    &label_text,
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&TEXT),
                    (300, (y0 + 7) as i32),
                )?;

                // Progress bar track
                bar_area.draw(&Rectangle::new(
                    [(12, (y0 + 26) as i32), (418, (y0 + 32) as i32)],
                    ShapeStyle {
                        color: BORDER.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;

                // Progress bar fill
                let fill_w = ((pct / 100.0) * 406.0) as u32;
                if fill_w > 0 {
                    bar_area.draw(&Rectangle::new(
                        [
                            (12, (y0 + 26) as i32),
                            ((12 + fill_w) as i32, (y0 + 32) as i32),
                        ],
                        ShapeStyle {
                            color: color.to_rgba(),
                            filled: true,
                            stroke_width: 0,
                        },
                    ))?;
                }
            }

            // Allocation donut (right panel) — simplified as filled arcs
            draw_donut(root, &values, total, 680, 330, 120)?;

            // Legend
            let legend_area = root.margin(10, 10, 475, 10);
            for (i, (sym, val)) in values.iter().enumerate().take(6) {
                let color = ASSET_PALETTE[i % ASSET_PALETTE.len()];
                let pct = val / total * 100.0;
                let y = 160 + i * 22;
                legend_area.draw(&Rectangle::new(
                    [(0, (y - 10) as i32), (12, y as i32)],
                    ShapeStyle {
                        color: color.to_rgba(),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;
                legend_area.draw_text(
                    &format!("{} {:.1}%", sym, pct),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&TEXT),
                    (16, (y - 10) as i32),
                )?;
            }

            root.present()?;
            Ok(())
        })
        .await
    }

    /// Horizontal bar chart — top N coins by 24h volume with % changes.
    pub async fn top_movers(quotes: &[CmcQuote]) -> Result<Vec<u8>> {
        render_png(880, 60 + quotes.len() as u32 * 52, |root| {
            root.fill(&BG)?;

            root.draw_text(
                "TOP BY VOLUME 24H",
                &TextStyle::from(("sans-serif", 16).into_font()).color(&DIM),
                (20, 16),
            )?;

            let chart_area = root.margin(45, 10, 10, 10);
            let n = quotes.len();
            let row_h = 46i32;

            // Find max abs change for scaling
            let max_abs = quotes
                .iter()
                .map(|q| q.change_24h.unwrap_or(0.0).abs())
                .fold(0.0f64, f64::max)
                .max(1.0);

            for (i, q) in quotes.iter().enumerate() {
                let y = i as i32 * row_h;
                let color = if q.change_24h.unwrap_or(0.0) >= 0.0 {
                    GREEN
                } else {
                    RED
                };
                let bar_max_w = 300i32;
                let bar_w =
                    ((q.change_24h.unwrap_or(0.0).abs() / max_abs) * bar_max_w as f64) as i32;

                // Row bg (alternating)
                if i % 2 == 0 {
                    chart_area.draw(&Rectangle::new(
                        [(0, y), (850, y + row_h - 2)],
                        ShapeStyle {
                            color: SURFACE.mix(0.5),
                            filled: true,
                            stroke_width: 0,
                        },
                    ))?;
                }

                // Rank + symbol
                chart_area.draw_text(
                    &format!("#{} {}", q.rank, q.symbol),
                    &TextStyle::from(("sans-serif", 14).into_font()).color(&TEXT),
                    (4, y + 4),
                )?;

                // Price
                chart_area.draw_text(
                    &fmt_price(q.price_usd),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (4, y + 24),
                )?;

                // Change bar
                let bar_x = 160i32;
                if bar_w > 0 {
                    chart_area.draw(&Rectangle::new(
                        [(bar_x, y + 8), (bar_x + bar_w, y + 36)],
                        ShapeStyle {
                            color: color.mix(0.9),
                            filled: true,
                            stroke_width: 0,
                        },
                    ))?;
                }

                // Change text
                chart_area.draw_text(
                    &fmt_change_plain(q.change_24h),
                    &TextStyle::from(("sans-serif", 14).into_font()).color(&color),
                    (bar_x + bar_max_w + 6, y + 10),
                )?;

                // Volume
                chart_area.draw_text(
                    &format!("Vol: {}", fmt_large_usd(q.volume_24h)),
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (bar_x + bar_max_w + 6, y + 26),
                )?;

                // Changes 7d / 30d
                chart_area.draw_text(
                    &format!(
                        "7d: {}  30d: {}",
                        fmt_change_plain(q.change_7d),
                        fmt_change_plain(q.change_30d)
                    ),
                    &TextStyle::from(("sans-serif", 11).into_font()).color(&DIM),
                    (570, y + 10),
                )?;
                chart_area.draw_text(
                    &format!("1y≈: {}", fmt_change_plain(q.change_90d)),
                    &TextStyle::from(("sans-serif", 11).into_font()).color(&DIM),
                    (570, y + 26),
                )?;
            }

            // Bottom divider
            let h = n as i32 * row_h + 45;
            root.draw(&PathElement::new(vec![(10, h), (870, h)], &BORDER))?;

            root.present()?;
            Ok(())
        })
        .await
    }

    /// P&L bar chart: cost vs current value per asset.
    pub async fn pnl_breakdown(
        portfolio: &Portfolio,
        prices: &HashMap<String, f64>,
    ) -> Result<Vec<u8>> {
        let mut rows: Vec<(String, f64, f64)> = portfolio
            .purchases
            .iter()
            .fold(HashMap::<String, (f64, f64)>::new(), |mut acc, p| {
                let e = acc.entry(p.symbol.clone()).or_insert((0.0, 0.0));
                e.0 += p.cost_usd;
                e.1 += p.amount;
                acc
            })
            .into_iter()
            .filter_map(|(sym, (cost, amt))| prices.get(&sym).map(|&p| (sym, cost, amt * p)))
            .collect();

        rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

        if rows.is_empty() {
            return Err(BotError::ChartError("No data for P&L chart".into()));
        }

        let h = 80u32 + rows.len() as u32 * 60;
        render_png(820, h, |root| {
            root.fill(&BG)?;

            root.draw_text(
                "P&L BREAKDOWN",
                &TextStyle::from(("sans-serif", 16).into_font()).color(&DIM),
                (20, 16),
            )?;

            let max_val = rows
                .iter()
                .map(|(_, cost, val)| cost.max(*val))
                .fold(0.0f64, f64::max)
                .max(1.0);

            let chart_area = root.margin(45, 10, 10, 20);
            let row_h = 56i32;
            let bar_max = 500i32;

            for (i, (sym, cost, val)) in rows.iter().enumerate() {
                let y = i as i32 * row_h;
                let color = ASSET_PALETTE[i % ASSET_PALETTE.len()];
                let pnl = val - cost;
                let pnl_color = if pnl >= 0.0 { GREEN } else { RED };

                // Cost bar (dimmer)
                let cost_w = ((cost / max_val) * bar_max as f64) as i32;
                chart_area.draw(&Rectangle::new(
                    [(120, y + 4), (120 + cost_w, y + 24)],
                    ShapeStyle {
                        color: SURFACE.mix(0.9),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;

                // Value bar
                let val_w = ((val / max_val) * bar_max as f64) as i32;
                chart_area.draw(&Rectangle::new(
                    [(120, y + 28), (120 + val_w, y + 48)],
                    ShapeStyle {
                        color: color.mix(0.85),
                        filled: true,
                        stroke_width: 0,
                    },
                ))?;

                // Symbol
                chart_area.draw_text(
                    sym,
                    &TextStyle::from(("sans-serif", 14).into_font()).color(&TEXT),
                    (4, y + 10),
                )?;

                // Cost label
                chart_area.draw_text(
                    &format!("Cost: ${:.0}", cost),
                    &TextStyle::from(("sans-serif", 11).into_font()).color(&DIM),
                    (4, y + 28),
                )?;

                // P&L label
                let sign = if pnl >= 0.0 { "+" } else { "" };
                chart_area.draw_text(
                    &format!("{}${:.0}", sign, pnl),
                    &TextStyle::from(("sans-serif", 13).into_font()).color(&pnl_color),
                    (630, y + 16),
                )?;
            }

            root.present()?;
            Ok(())
        })
        .await
    }

    /// Price change sparkline card for a single symbol.
    pub async fn price_card(quote: &CmcQuote) -> Result<Vec<u8>> {
        render_png(640, 200, |root| {
            root.fill(&BG)?;

            // Large symbol + price
            root.draw_text(
                &quote.symbol,
                &TextStyle::from(("sans-serif", 28).into_font()).color(&TEXT),
                (24, 20),
            )?;
            root.draw_text(
                &fmt_price(quote.price_usd),
                &TextStyle::from(("sans-serif", 24).into_font()).color(&TEXT),
                (24, 60),
            )?;

            let c24_color = if quote.change_24h.unwrap_or(0.0) >= 0.0 {
                &GREEN
            } else {
                &RED
            };
            root.draw_text(
                &format!("24h: {}", fmt_change_plain(quote.change_24h)),
                &TextStyle::from(("sans-serif", 16).into_font()).color(c24_color),
                (24, 96),
            )?;

            // Change grid: 1h / 7d / 30d / 90d
            let changes = [
                ("1h", quote.change_1h),
                ("7d", quote.change_7d),
                ("30d", quote.change_30d),
                ("90d", quote.change_90d),
            ];

            for (i, (label, pct)) in changes.iter().enumerate() {
                let x = 24 + i as i32 * 150;
                let color = if pct.unwrap_or(0.0) >= 0.0 {
                    &GREEN
                } else {
                    &RED
                };
                root.draw_text(
                    label,
                    &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                    (x, 140),
                )?;
                root.draw_text(
                    &fmt_change_plain(*pct),
                    &TextStyle::from(("sans-serif", 14).into_font()).color(color),
                    (x, 158),
                )?;
            }

            // Vol + MCap
            root.draw_text(
                &format!(
                    "Vol 24h: {}  |  MCap: {}",
                    fmt_large_usd(quote.volume_24h),
                    fmt_large_usd(quote.market_cap)
                ),
                &TextStyle::from(("sans-serif", 12).into_font()).color(&DIM),
                (24, 182),
            )?;

            root.present()?;
            Ok(())
        })
        .await
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
        let points = sanitize_history(points);
        if points.len() < 2 {
            return Err(BotError::ChartError(
                "Not enough valid data to chart after validation".into(),
            ));
        }

        let first_point = points.first().unwrap();
        let last_point = points.last().unwrap();
        let baseline = first_point.1;
        let current_price = last_point.1;

        // Compute min and max values for y axis scaling
        let prices_only: Vec<f64> = points.iter().map(|p| p.1).collect();
        let min_price = prices_only.iter().copied().fold(f64::INFINITY, f64::min);
        let max_price = prices_only
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);

        // Add some margin to min/max
        let price_range = max_price - min_price;
        let y_margin = if price_range > 0.0 {
            price_range * 0.1
        } else {
            baseline * 0.1
        };
        let y_min = (min_price - y_margin).max(0.0);
        let y_max = max_price + y_margin;

        let x_min = points.first().unwrap().0;
        let x_max = points.last().unwrap().0;

        render_png(800, 420, |root| {
            root.fill(&BG)?;

            // Draw header: symbol, current price, change
            let change_pct = if baseline > 0.0 {
                (current_price - baseline) / baseline * 100.0
            } else {
                0.0
            };
            let change_color = if change_pct >= 0.0 { GREEN } else { RED };
            let change_sign = if change_pct >= 0.0 { "+" } else { "" };

            root.draw_text(
                &symbol.to_uppercase(),
                &TextStyle::from(("sans-serif", 24).into_font()).color(&TEXT),
                (30, 20),
            )?;

            root.draw_text(
                &fmt_price(current_price),
                &TextStyle::from(("sans-serif", 28).into_font()).color(&TEXT),
                (30, 52),
                // (30, 52),
            )?;

            root.draw_text(
                &format!(
                    "{}{:.2}% ({})",
                    change_sign,
                    change_pct,
                    period_label.to_uppercase()
                ),
                &TextStyle::from(("sans-serif", 15).into_font()).color(&change_color),
                (30, 90),
            )?;

            // Draw chart area
            let mut chart = ChartBuilder::on(&root)
                .margin_top(120)
                .margin_bottom(20)
                .margin_left(20)
                .margin_right(20)
                .x_label_area_size(30)
                .y_label_area_size(60)
                .build_cartesian_2d(x_min..x_max, y_min..y_max)?;

            // Configure mesh and axes
            chart
                .configure_mesh()
                .disable_x_mesh()
                .disable_y_mesh()
                .axis_style(BORDER)
                .y_label_formatter(&|y| {
                    if *y >= 1000.0 {
                        format!("{:.2}K", y / 1000.0)
                    } else {
                        format!("{:.2}", y)
                    }
                })
                .x_label_formatter(&|x| {
                    let dt = chrono::DateTime::from_timestamp(*x / 1000, 0)
                        .unwrap_or_else(|| chrono::Utc::now().into());
                    dt.format("%m-%d %H:%M").to_string()
                })
                .label_style(TextStyle::from(("sans-serif", 11).into_font()).color(&DIM))
                .y_labels(6)
                .x_labels(5)
                .draw()?;

            // Draw Area fills
            // 1. Positive area (above baseline)
            let pos_points: Vec<(i64, f64)> = points
                .iter()
                .map(|&(x, y)| (x, if y >= baseline { y } else { baseline }))
                .collect();
            chart.draw_series(AreaSeries::new(pos_points, baseline, GREEN.mix(0.12)))?;

            // 2. Negative area (below baseline)
            let neg_points: Vec<(i64, f64)> = points
                .iter()
                .map(|&(x, y)| (x, if y <= baseline { y } else { baseline }))
                .collect();
            chart.draw_series(AreaSeries::new(neg_points, baseline, RED.mix(0.12)))?;

            // Draw Baseline
            chart.draw_series(LineSeries::new(
                vec![(x_min, baseline), (x_max, baseline)],
                ShapeStyle {
                    color: BORDER.mix(0.8),
                    filled: false,
                    stroke_width: 1,
                },
            ))?;

            // Draw dynamic-colored line segments
            for win in points.windows(2) {
                let p1 = win[0];
                let p2 = win[1];
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
                    let x_cross = p1.0 + (t * (p2.0 - p1.0) as f64) as i64;
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

            root.present()?;
            Ok(())
        })
        .await
    }
}

// ── Donut chart helper ────────────────────────────────────────────────────────

fn draw_donut(
    root: &DrawingArea<BitMapBackend<'_>, plotters::coord::Shift>,
    values: &[(String, f64)],
    total: f64,
    cx: i32,
    cy: i32,
    radius: i32,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::f64::consts::PI;

    let mut angle = -PI / 2.0; // start at top

    for (i, (_, val)) in values.iter().enumerate() {
        let sweep = (val / total) * 2.0 * PI;
        let color = ASSET_PALETTE[i % ASSET_PALETTE.len()];

        // Draw filled sector as polygon approximation
        let steps = ((sweep * 30.0) as usize).max(4);
        let mut points = vec![(cx, cy)];
        for s in 0..=steps {
            let a = angle + (s as f64 / steps as f64) * sweep;
            let x = cx + (a.cos() * radius as f64) as i32;
            let y = cy + (a.sin() * radius as f64) as i32;
            points.push((x, y));
        }

        root.draw(&Polygon::new(
            points,
            ShapeStyle {
                color: color.mix(0.9),
                filled: true,
                stroke_width: 0,
            },
        ))?;

        angle += sweep;
    }

    // Inner circle (donut hole)
    let hole_r = (radius as f64 * 0.55) as i32;
    root.draw(&Circle::new(
        (cx, cy),
        hole_r as u32,
        ShapeStyle {
            color: BG.to_rgba(),
            filled: true,
            stroke_width: 0,
        },
    ))?;

    // Center text: total
    let total_str = format!("${:.2}", total);
    let total_font_size = if total_str.len() > 8 { 16 } else { 18 };
    let cx_offset = (total_str.len() as f64 * total_font_size as f64 * 0.28) as i32;

    root.draw_text(
        "TOTAL",
        &TextStyle::from(("sans-serif", 13).into_font()).color(&DIM),
        (cx - 20, cy - 18),
    )?;
    root.draw_text(
        &total_str,
        &TextStyle::from(("sans-serif", total_font_size).into_font()).color(&TEXT),
        (cx - cx_offset, cy + 2),
    )?;

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
    fn sanitize_empty() {
        assert!(sanitize_history(&[]).is_empty());
    }
}
