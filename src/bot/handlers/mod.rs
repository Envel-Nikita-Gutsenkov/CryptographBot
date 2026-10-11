mod acl;
pub mod ai;
pub mod ai_patch;
pub mod alert;
pub mod ask;
pub mod backup;
pub mod fng;
pub mod memo_goal;
pub mod model_select;
pub mod news;
pub mod nlp;
pub mod portfolio;
pub mod price;
pub mod settings;
pub mod trade;
pub mod wallet;

pub use acl::{handle_acl_list, handle_allow, handle_revoke};
pub use ai::{handle_ai_callback, handle_ai_cmd, handle_ai_portfolio_callback, run_cli_analysis};
pub use ai_patch::{
    handle_patch_cancel, handle_patch_cmd, handle_patch_confirm, new_patch_store, PatchStore,
};
pub use alert::{
    handle_alert_add_cmd, handle_alert_delete, handle_alert_list, handle_alert_reset,
    handle_alert_toggle,
};
pub use ask::handle_ask_cmd;
pub use backup::{handle_backup, handle_restore_document};
pub use fng::handle_fng_cmd;
pub use memo_goal::{handle_goal_cmd, handle_memo_cmd, handle_plan_cmd};
pub use model_select::handle_model_cmd;
pub use news::{handle_news_callback, handle_news_cmd};
pub use portfolio::{
    handle_add, handle_adjust, handle_correct, handle_invested_cmd, handle_move, handle_pnl_chart,
    handle_portfolio, handle_portfolio_chart, handle_purchase_history,
};
pub use price::{handle_price, handle_top};
pub use settings::{handle_convert, handle_settings, handle_watchlist};
pub use trade::{handle_trade_cmd, handle_tradetask_cmd};
pub use wallet::{
    handle_wallet_add, handle_wallet_delete, handle_wallet_list, handle_wallet_refresh,
};
