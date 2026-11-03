# Model Mathematics

Everything in this document runs on daily closes already stored on disk. There is
no model call, no provider call and no randomness: two invocations on the same
candle series return the same bytes. The language model never produces these
numbers — it can only read them.

| Command | Entry point | Module |
|---|---|---|
| `/forecast`, `/forecast verify` | `forecast`, `backtest` | `src/trading/forecast.rs` |
| `/dip` | `dip_report` | `src/trading/dipzones.rs` |
| `/sniper` | `sniper_report_with_derivatives` | `src/trading/sniper.rs` |
| `/bottom` | `bottom_signal` | `src/trading/bottom.rs` |
| regime tag | `classify_closes` | `src/trading/regime.rs` |

---

## 1. Notation and input

- `P_t` — daily close, ascending, one row per UTC day.
- `r_t = ln(P_t / P_{t−1})` — daily log return.
- `h` — horizon in days. `σ` — daily volatility as a fraction (`0.036` = 3.6%).
- `μ` — daily log drift.
- `N` — number of closes the drift was fitted on; fixed at 200 for the drift
  estimation-error term.
- Non-positive closes are dropped before any transform; a series shorter than
  the required window falls back to the documented default instead of erroring.

---

## 2. Volatility

```text
σ²_EWMA,t = λ · σ²_EWMA,t−1 + (1 − λ) · r²_t        λ = 0.94  (RiskMetrics daily)
σ_E       = √σ²_EWMA,t                              seeded with the first 20 r²
σ_t       = 0.5 · std₂₀(r) + 0.5 · σ_E              floor 0.3%/day, hard cap 10 (1000%/day) against corrupt data
```

Why a blend: `std₂₀` is stable but lags a regime change; EWMA reacts to the
latest shock but overreacts to a single print. The floor exists because a
perfectly flat series has zero dispersion, which collapses the cone to a point
and makes every horizon equally "certain".

## 3. Drift

```text
r_k   = ln(P_last / P_{last−k}) / k        for k ∈ {20, 50, 200}
μ_raw = Σ(w_k · r_k) / Σ(w_k)              w = 0.5 / 0.3 / 0.2, renormalised over available k
```

Regime term:

| Regime | Effect on `μ_raw` | Reason |
|---|---|---|
| `SIDEWAYS` | set to 0 | a flat market has no slope; a leaked 20-day slope created fake downtrends |
| `EUPHORIA` | × 0.5 | mean-reversion risk dominates near the ATH |
| `DIP` | + 0.0008 | fear zone tends to bounce, conservatively |
| `BEAR` | − 0.0003 | structural downtrend |
| `BULL` | + 0.0002 | structural uptrend |

RSI term, applied only when the series actually has dispersion (a flat series
reads RSI = 100 as an artifact):

```text
RSI14 ≥ 70  →  μ −= 0.0008
RSI14 ≤ 30  →  μ += 0.0008
```

Hard clamps: `|μ| ≤ 0.005` per day, `|μ·h| ≤ 0.35` cumulative over any horizon.
A short-term momentum spike is not extrapolated into a fantasy year.

### Seasonality

```text
months with N ≥ 2      → median monthly return m
adjustment             = clip(m / 100 / 30, ±0.0002)   per day
```

The median is used instead of the mean because a calendar month has roughly
eight observations: one parabolic year owns the arithmetic mean. The cap keeps a
seven-sample statistic from moving the model in any meaningful way. The report
prints both median and win rate and marks the mean as noise.

---

## 4. Regime classifier

First match wins, evaluated on the same close series:

| Order | Regime | Condition |
|---|---|---|
| 1 | `DIP` | `P < SMA50 · 0.90` **and** `P ≥ SMA200 · 0.80` |
| 2 | `EUPHORIA` | within 5% of the ATH **and** `RSI14 ≥ 65` **and** `P > SMA20 · 1.03` |
| 3 | `BEAR` | `SMA20 < SMA50` **and** `P < SMA200` |
| 4 | `BULL` | `SMA20 > SMA50` **and** `P > SMA200` |
| 5 | `SIDEWAYS` | everything else, including series shorter than 20 closes |

The `DIP` upper bound matters: a full bear collapse (price far under the 200-day
mean) is not a dip, it is a different regime. The `1.03` momentum margin keeps a
flat series at its high from reading as euphoria.

---

## 5. The price cone

Two independent uncertainties add in quadrature:

```text
σ_h    = √( σ²·h + (σ²/N)·h² )                     N = 200
μ_h    = μ · scale / √(1 + h/decay)                momentum decays with horizon
target = P₀ · exp(μ_h · h)                          cumulative drift clamped ±35%
low    = target · exp(−q · σ_h)
high   = target · exp(+q · σ_h)
P(up)  = 1 − T_ν(−μ_h·h / σ_h)
conf   = clip(1 / (1 + 4·σ_h), 0.05, 0.95)
```

- `q = t⁻¹(0.84135; ν = 4) ≈ 1.142`. `ν = 4` widens the ~68% band by ~14%
  versus the Gaussian and matches crypto's fat-tailed daily returns.
- `σ²·h` is diffusion (random walk). `(σ²/N)·h²` is the drift's own estimation
  error: the fitted drift is only known to `σ/√N`, and that error compounds
  linearly with `h`. At `h = 1` it is negligible; at `h = 90` it adds ~20% width;
  at `h = 180` ~38%.
- `confidence` is reported per horizon and shrinks as the cone widens. It is a
  trust weight, not a probability.

---

## 6. Touch and terminal probabilities

For `L < P₀`, `a = ln(L / P₀)`:

```text
P_touch(H) = Φ((a − μH)/(σ√H)) + exp(2μa/σ²) · Φ((a + μH)/(σ√H))
P_close_below(H) = Φ((a − μH)/(σ√H))
```

The first is the running-minimum law for Brownian motion with drift ("does it
ever get there"), the second is the terminal probability ("is it still cheap
when I would buy"). Both use `σ_eff = σ · q` — the same effective diffusion as
the drawn cone — so a bucket placed on the visible band reports the same
probability the band implies.

Numerical care: the exponential is evaluated in log space
(`2μa/σ² + ln Φ(z₂)`), because with small `σ` and negative `μ` the factor
overflows while the CDF underflows, producing `NaN`.

---

## 7. Accumulation ladder (`/sniper`)

```text
anchor  = max(close) over the last 14 days
Z₁₄     = ln(P / anchor) / (σ · √14)
σ₃₀     = cone width at h = 30 (same function as /forecast)
limitₖ  = P₀ · exp(μ·30 + tₖ · σ₃₀),   tₖ = t⁻¹(qₖ; ν = 4)
q       = 12% / 4% / 1%
weights = 20% / 35% / 45%  (protect mode: 10% / 35% / 55%)
new_avg = (pos·avg + spend/limit) / (pos + spend/limit)
```

Anchoring to a rolling high rather than the previous close is deliberate: with a
previous-close anchor, a slow bleed would walk the ladder down every day and
never trigger.

The average-entry guard activates when `P > avg · 1.25`; a bucket is flagged
when it would raise `avg` by more than 10%, and the allocation shifts to the
deeper buckets instead.

### Capitulation score

Points are summed and normalised over the signals that are actually available
(so a report without volume or derivatives is not systematically undervalued):

| Signal | Points |
|---|---|
| `Z₁₄ ≤ −2.0` | 40 |
| `RSI14 < 25` | 25 |
| volume > `SMA20 + 2·stddev` | 20 |
| lower wick > 50% of candle range | 15 |
| futures longs flushed | 15 |

```text
ratio  = score / max_score
≥ 0.80 CAPITULATION · ≥ 0.50 ALERT · ≥ 0.25 WATCH · else SLEEP
```

### Futures tape

| Price 24h | Open interest 24h | Read |
|---|---|---|
| ≤ −1% | ≤ −3% | `LongsFlushed` — selling is exhausting |
| ≤ −1% | ≥ +3% | `ShortsPiling` — the fall can continue |
| ≥ +1% | ≥ +3% | `TrendUp` |
| any | funding ≤ −0.015% | `SqueezeRisk` |
| otherwise | — | `Neutral` |

---

## 8. Bottom radar (`/bottom`)

```text
score  = 55 · (capitulation / max_score)
       + 15 · clip((40 − RSI14) / 20)
       + 15 · clip((−1 − Z₁₄) / 1.5)
       + 5 · [volume spike] + 5 · [long wick] + 5 · [OI flush]
```

Then the score is discounted when the shallowest bucket still has a high
probability of printing — a level that is likely to be reached means "not the
bottom yet".

| Score | Band |
|---|---|
| 60–100 | BOTTOM LIKELY |
| 35–59 | NEAR BOTTOM |
| 20–34 | WATCH |
| 0–19 | NOT A BOTTOM |

---

## 9. Calibration

The geometry above is fixed. Three parameters are fitted per symbol:

| Parameter | Grid |
|---|---|
| `decay` (momentum half-life, days) | 10, 20, 30, 45, 60, 90, 180, ∞ |
| `drift_scale` | 0.3, 0.5, 0.75, 1.0 |
| `vol_mult` (cone width) | 0.7, 0.85, 1.0, 1.15, 1.3, 1.45 |

Walk-forward setup: the last 400 days, origins every 3 days, each origin using
only closes up to that point. Nearest-horizon samples where the outcome is not
yet realised are skipped. 8 × 4 × 6 = 192 combinations, scored by:

```text
score = MAPE
      + 0.5 · miss_rate
      + 1.0 · |coverage − 0.68|
      + 2.0 · mean_h |coverage_h − 0.68|
```

`mean_h` is the per-horizon term: without it, five well-covered short horizons
can satisfy the average while the quarter stays badly under-covered. `vol_mult`
is allowed below 1.0 so calibration can narrow the cone after the Student-t
quantile widened it — otherwise the two effects double-count.

Fewer than 120 closes, or fewer than 10 usable origins, returns the per-symbol
profile default instead of a fitting artifact.

---

## 10. Reported accuracy

For each horizon, the same replay produces:

```text
direction_hit_rate = hits / samples
MAPE               = mean |target/actual − 1| · 100
coverage           = share of outcomes inside [low, high]
effective_origins  = samples / h        (overlapping windows are not independent)
Wilson interval    = binomial CI on the hit rate
```

`effective_origins` is the honest denominator: 133 overlapping origins at `h=30`
are not 133 independent bets. Direction hit rates near 50% are reported as
inconclusive rather than dressed up as an edge.

Reproduce offline:

```bash
cryptograph forecast verify BTC
cryptograph backtest BTC ETH SOL LINK      # shipped vs calibrated parameters
```

---

## 11. Horizon profiles

| Symbols | Ladder |
|---|---|
| BTC, ETH | 1, 3, 5, 7, 14, 30, 90, 180 |
| SOL, LINK | 1, 3, 5, 7, 14, 30, 60, 90 |
| any other | 1, 3, 5, 7, 14, 30, capped at `history_days / 4` |

The cap is data-driven: a half-year claim from two years of history is a number
without support.

---

## 12. Deterministic security scan

Not a statistical model, but the same rule applies — it must not depend on a
language model's judgement. Over cached headlines:

1. Extract an incident class from a keyword set (`hack`, `exploit`, `breach`,
   `drain`, `stolen`, …); `hackathon` is removed first.
2. Extract the affected entity where one is named.
3. Significance gate:
   - custody entity (exchange / bridge / stablecoin) → always report;
   - wallet brand → only together with an active-compromise word;
   - generic class → only above the major-loss threshold parsed from the text.
4. Deduplicate per topic, so alternating incidents cannot re-trigger each other.

---

## 13. Known limitations

- **Fat tails are modelled, not solved.** `ν = 4` widens the band correctly on
  average; a liquidation cascade can still leave it in one candle.
- **Drift is not predictive at short horizons.** Direction hit rates hover near
  a coin flip, and the reports say so. The value is in calibrated uncertainty
  and in pricing a level, not in calling the next candle.
- **Seasonality is informational only.** Eight samples per month cannot support
  a decision; the ±0.02%/day cap enforces that.
- **Long horizons are structurally weaker.** Cone coverage at a quarter degrades
  because a single regime can end inside it; calibration reports that instead of
  hiding it.
- **Quantiles and thresholds are engineering choices.** `ν = 4`, the 12%/4%/1%
  bucket quantiles and the Z-score bands are tuned for crypto drawdowns, not
  derived from first principles.
