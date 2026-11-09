# DEX Execution — Legal On-Ramp for Belarus P2P Withdrawal

## Why DEX, not Binance

Binance is unavailable/illegal for Belarus residents. The legal path is:

1. **Buy crypto on a P2P exchange** (e.g. Bybit P2P, KuCoin P2P, local
   Telegram P2P groups) with BYN/RUB via SBP/cards.
2. **Swap into the target asset on a DEX** (Jupiter on Solana, Uniswap on
   Ethereum) — no KYC, no CEX account needed.
3. **Withdraw to a self-custody wallet** (Phantom / MetaMask / Ledger).

The bot never touches a CEX. All execution is on-chain.

## Target DEXes

| Chain    | DEX / Aggregator | Why |
|----------|------------------|-----|
| Solana   | **Jupiter**       | Best liquidity for SOL/USDC, low fees, official quote+swap API |
| Ethereum | **Uniswap v3**    | Deep liquidity for ETH/LINK, standard router contract |

## Planned flow (bot side)

1. **Quote** — bot asks the aggregator for a quote
   (`POST https://quote-api.jup.ag/v6/quote?inputMint=...&outputMint=...&amount=...`).
   The quote includes price impact + fees, so the engine can re-check the
   no-loss invariant at the *actual* fill price before signing.
2. **Guardrail re-check** — same as the manual-execution path: the engine
   re-runs `plan_buy`/`plan_sell` with the quoted price. If the quote is
   worse than `min_exit_price` (sell) or above the ATH guard (buy), the
   swap is refused.
3. **Sign & submit** — the operator's wallet (Phantom/MetaMask) signs the
   transaction; the bot never holds the private key. Two modes:
   - **Manual**: bot prints the swap payload, operator confirms in their
     wallet UI (mirrors Commander mode).
   - **Semi-auto**: bot submits the unsigned tx, operator approves.
4. **Reconcile** — after confirmation, the bot records the fill at the
   executed price (from the tx receipt) and updates lots/balances.

## Wallet keys

- **Never** store private keys in `.env` or the DB.
- Use a hardware wallet or a hot wallet with a tiny balance for gas only.
- The bot needs only a **public address** to build quotes and verify fills.

## Status

- [x] Flow documented (this file)
- [x] Jupiter quote API integration (`src/trading/dex.rs`) — `jupiter_quote`
      + `parse_jupiter_quote`, public API, no key required
- [x] Uniswap router integration — via the 0x Swap API (`zeroex_quote` +
      `parse_zeroex_quote`, aggregates Uniswap v2/v3 + friends)
- [x] Manual sign mode (wallet UI confirmation) — `build_swap_payload`
      renders the operator-facing swap; CLI `trade dex sign <SYMBOL> <AMOUNT>`
- [x] Fill reconciliation from tx receipts — `reconcile_fill` (slippage vs
      quoted price, refuses >2% deviation); CLI `trade dex reconcile`

Remaining (blocked on real wallet keys): live swap submission + atomic
balance reconciliation — the operator signs in Phantom/MetaMask, the bot
never holds the private key.