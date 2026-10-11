use teloxide::macros::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Cryptograph Bot commands")]
pub enum Command {
    #[command(description = "Start & main menu")]
    Start,
    #[command(description = "Quick menu restore")]
    B,
    #[command(description = "Command reference")]
    Help,
    #[command(description = "Get prices: /price or /price BTC ETH")]
    Price(String),
    #[command(description = "Top N coins by volume: /top 10")]
    Top(String),
    #[command(description = "Portfolio overview")]
    Portfolio,
    #[command(description = "Profit / loss summary")]
    Pnl,
    #[command(description = "Asset allocation chart")]
    Distribution,
    #[command(description = "Add purchase: /add BTC 0.01 600 hot:myexchange")]
    Add(String),
    #[command(
        description = "Transfer between wallets: /move BTC 0.5 from:ledger to:exchange fee:0.0001"
    )]
    Move(String),
    #[command(description = "Purchase history: /history BTC")]
    #[allow(dead_code)]
    History(String),
    #[command(description = "Wallet operations: /wallet add | list | refresh")]
    Wallet(String),
    #[command(description = "Alert operations: /alert add | list | del N | reset")]
    Alert(String),
    #[command(description = "Convert: /convert 100 BYN BTC")]
    Convert(String),
    #[command(description = "Watchlist: /watchlist add ETH | remove ETH | list")]
    Watchlist(String),
    #[command(description = "Correct a purchase record: /correct #ID field value")]
    Correct(String),
    #[command(description = "Adjust a holding manually: /adjust wallet:ledger BTC 0.9999")]
    Adjust(String),
    #[command(description = "Manually set total invested amount: /invested BTC 5000")]
    Invested(String),
    #[command(description = "Send JSON backup now")]
    Backup,
    #[command(description = "Restore from backup file (send file after command)")]
    Restore,
    #[command(description = "Get long-term investment advice: /advice")]
    Advice,
    #[command(description = "AI analysis: /ai | /ai BTC | /ai set BTC ETH LINK")]
    Ai(String),
    #[command(
        description = "AI portfolio corrector: /patch set BTC to 0.5 | balance doesn't match"
    )]
    Patch(String),
    #[command(description = "AI portfolio corrector (alias): /path set BTC to 0.5")]
    Path(String),
    #[command(description = "Ask AI anything about your portfolio: /ask why is ETH falling?")]
    Ask(String),
    #[command(description = "AI memory notes: /memo text | list | del N")]
    Memo(String),
    #[command(description = "Proactive goals: /goal text | list | del N | done N | resume N")]
    Goal(String),
    #[command(description = "Plan dashboard: /plan | status | check | ask question")]
    Plan(String),
    #[command(description = "Get recent crypto news: /news or /news BTC")]
    News(String),
    #[command(
        description = "Trading commander: /trade list | pnl | fee | allocate | constraints | rule"
    )]
    Trade(String),
    #[command(
        description = "Standing trade tasks: /tradetask list | add | pause | resume | advise"
    )]
    TradeTask(String),
    #[command(description = "Get Fear & Greed Index: /fng")]
    Fng,
    #[command(description = "Bot settings")]
    Settings,
    #[command(description = "Select AI model: /model list | /model set N")]
    Model(String),
    #[command(description = "Admin: allow user access: /allow USER_ID")]
    Allow(String),
    #[command(description = "Admin: revoke user access: /revoke USER_ID")]
    Revoke(String),
    #[command(description = "Admin: reset stats & caches (keeps portfolio): /reset")]
    Reset,
    #[command(description = "Admin: list approved users")]
    Acl,
    #[command(description = "Cancel current operation")]
    Cancel,
    #[command(description = "Show bot build version/hash")]
    Ver,
}
