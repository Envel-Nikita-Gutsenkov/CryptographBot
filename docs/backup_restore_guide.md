# Cryptograph Backup & Restore Guide

Cryptograph allows you to generate and restore backups of your portfolio, settings, wallet allocations, and alert conditions. The backup format is human-readable JSON.

This guide provides instructions on how to structure the JSON schema so you can easily ask an AI (such as Claude, ChatGPT, Gemini) to generate your portfolio backup, allowing you to restore it instantly without entering assets manually.

---

## 1. Backup JSON Schema

Here is the structural JSON template for a Cryptograph backup:

```json
{
  "purchases": [
    {
      "id": 1,
      "symbol": "BTC",
      "amount": 0.05,
      "cost_usd": 3000.0,
      "paid_amount": 3000.0,
      "paid_currency": "Usd",
      "date": "2026-06-01T12:00:00Z",
      "source": "Binance",
      "notes": "Initial purchase",
      "created_at": "2026-06-01T12:00:00Z",
      "corrections": []
    }
  ],
  "wallets": [
    {
      "id": 1,
      "name": "MyTrezor",
      "kind": "Cold",
      "address": "bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh",
      "network": "Bitcoin",
      "exchange": null,
      "monitored": true,
      "last_balance": {},
      "last_checked": null
    }
  ],
  "holdings": [
    {
      "id": 1,
      "wallet_id": 1,
      "symbol": "BTC",
      "amount": 0.05,
      "corrections": []
    }
  ],
  "transfers": [],
  "alerts": [
    {
      "id": 1,
      "symbol": "BTC",
      "condition": "PriceAbove",
      "threshold": 150000.0,
      "period": null,
      "cooldown_hours": 24,
      "last_fired": null,
      "consecutive_triggers": 0,
      "enabled": true,
      "label": "Take Profit Advice"
    }
  ],
  "settings": {
    "default_watchlist": ["BTC", "ETH", "SOL"],
    "currency_display": "Usd",
    "backup_on_change": true,
    "alert_quiet_hours_start": null,
    "alert_quiet_hours_end": null
  },
  "next_purchase_id": 2,
  "next_wallet_id": 2,
  "next_holding_id": 2,
  "next_transfer_id": 1,
  "next_alert_id": 2
}
```

### Supported Enums
- **`paid_currency`**: `"Usd"`, `"Byn"`, `"Eur"`, `"Rub"`, or `{"Other": "SYMBOL"}`
- **`kind` (Wallet)**: `"Cold"`, `"Hot"`, `"Exchange"`
- **`network`**: `"Bitcoin"`, `"Ethereum"`, `"Solana"`, `"Tron"`, `"Bsc"`, or `{"Generic": "Name"}`
- **`condition` (Alert)**: `"PriceBelow"`, `"PriceAbove"`, `"DropPercent"`, `"RisePercent"`
- **`period`**: `null`, `"OneHour"`, `"FourHours"`, `"OneDay"`, `"SevenDays"`, `"ThirtyDays"`, `"NinetyDays"`
- **`currency_display`**: `"Usd"`, `"Byn"`, `"Both"`

---

## 2. Restoring Backup inside Telegram

You can restore your portfolio in two ways:

### Method A: Text Restore (Recommended for AI-Generated Backups)
Simply send a message containing the backup block. The text must start with `backup` and contain the JSON within triple-backticks (markdown code block).

**Example Message:**
```text
backup
```json
{
  "purchases": [],
  "wallets": [],
  "holdings": [],
  "transfers": [],
  "alerts": [],
  "settings": {
    "default_watchlist": ["BTC", "ETH"],
    "currency_display": "Usd",
    "backup_on_change": true,
    "alert_quiet_hours_start": null,
    "alert_quiet_hours_end": null
  },
  "next_purchase_id": 1,
  "next_wallet_id": 1,
  "next_holding_id": 1,
  "next_transfer_id": 1,
  "next_alert_id": 1
}
```
```

### Method B: File Upload
Alternatively, save the JSON above as a file (e.g., `backup.json` or `portfolio.cgrp`) and send it directly to the bot inside the chat.
