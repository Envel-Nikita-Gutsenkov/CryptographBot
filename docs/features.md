# Cryptograph Features

Cryptograph is packed with professional financial tracking features designed for absolute privacy and maximum usability.

## 1. Conversational FSM Wizards

To ensure an exceptionally smooth user experience, Cryptograph uses stateful step-by-step dialogue wizards instead of complex command syntaxes.

### 🔔 Price Alert Wizard
Accessed via the **Add Alert** button inside the Alerts menu:
1. **Step 1**: The user is prompted to type a cryptocurrency symbol (e.g. `BTC`).
2. **Step 2**: An inline keyboard is rendered, offering conditional options:
   - `📉 Price Below`
   - `📈 Price Above`
   - `⬇️ Drop % over period`
   - `⬆️ Rise % over period`
3. **Step 3**:
   - If a percentage condition is picked, the user selects a timeframe (`1h`, `4h`, `24h`, `7d`, `30d`, `90d`) and then enters the change percentage threshold (e.g. `5` for 5%).
   - If a price condition is picked, the user enters the absolute USD price threshold (e.g. `150000`).

### 👛 Wallet Registration Wizard
Accessed via the **Add Wallet** button inside the Wallets menu:
1. **Step 1**: An inline keyboard is displayed to pick the wallet's category:
   - `🧊 Cold` (Hardware or paper wallet)
   - `🔥 Hot` (Software or mobile app)
   - `🏦 Exchange` (Centralized exchange)
2. **Step 2**: The user enters the wallet specification.
   - For simple non-monitored tracking: `[Wallet Name]` (e.g. `MyBinance`).
   - For automatic blockchain tracking: `[Wallet Name] [SYM] [ADDRESS]` (e.g. `Trezor BTC bc1q...`).

---

## 2. Market Intelligence & Advice

The `/advice` command generates sophisticated market intelligence reports using live metrics:
- **Market Sentiment Index**: Evaluates aggregate market trends and calculates the **Fear & Greed Index** based on trading volumes, BTC dominance, volatility, and trend directions.
- **DCA/Buy Recommendations**: Analyzes your average purchase price relative to current asset valuations and provides suggestions on whether to hold, realize profits, or accumulate (DCA) during corrections.
- **Halving Timelines**: Displays estimates for the next Bitcoin halving event (target: 2028-2029) to assist in planning multi-year exits.

---

## 3. High-Fidelity Infographic Charts

Cryptograph generates premium charts inside the chat interface using the native `Plotters` engine without using external charting servers:
- **`/pnl` (Profit & Loss Timeline)**: A time-series chart mapping investment growth, current valuation, and cumulative profits/losses.
- **`/distribution` (Asset Allocation)**: A pie chart mapping portfolio weight distribution by USD value.
- **Top Movers**: Horizontal bar charts comparing 24-hour performance dynamics across the top market cap coins.

---

## 4. Anti-Spam Alerts & Quiet Hours

- **Anti-Spam Engine**: Alerts require the threshold condition to be met for at least **two consecutive checks** (5 minutes apart) to prevent false alerts caused by quick price wicks.
- **Quiet Hours**: Users can set time periods during which low-severity alerts are queued and silenced (e.g., `23:00 - 07:00 UTC`), protecting sleep. High-priority alerts can bypass this filter.
