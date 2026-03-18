// Core modules
pub mod api;
pub mod clob_sdk;
pub mod config;
pub mod models;

// Trading & strategy
pub mod backtest;
pub mod merge;
pub mod simulation;
pub mod trader;

// Monitoring & activity
pub mod activity_stream;
pub mod detector;
pub mod monitor;
pub mod rtds;

// Copy trading & web
pub mod copy_trading;
pub mod web_state;

// Utilities
pub mod logging;

// Re-exports
pub use api::PolymarketApi;
pub use config::Config;
pub use logging::{init_history_file, log_trading_event, log_to_history};
pub use models::TokenPrice;

/// Logging macro for history file output.
#[macro_export]
macro_rules! log_println {
    ($($arg:tt)*) => {
        {
            let message = format!($($arg)*);
            $crate::log_to_history(&format!("{}\n", message));
        }
    };
}
