// Sports trailing bot: trade a single market by slug. Trail the token whose price is going down first (buy when price >= lowest + trailing_stop), then trail and buy the opposite token. Option: once per market or continuous (repeat after both bought).

use anyhow::{Context, Result};
use clap::Parser;
use polymarket_trading_bot::config::{Args, Config};
use log::warn;
use std::sync::Arc;
use chrono::DateTime;
use rust_decimal::prelude::ToPrimitive;

use polymarket_trading_bot::api::PolymarketApi;
use polymarket_trading_bot::detector::{BuyOpportunity, TokenType};
use polymarket_trading_bot::trader::Trader;

const MIN_FIRST_BUY_COST: f64 = 1.0;

fn ask_f64(token: &polymarket_trading_bot::models::TokenPrice) -> f64 {
    token
        .ask
        .as_ref()
        .and_then(|d| d.to_f64())
        .or_else(|| token.bid.as_ref().and_then(|d| d.to_f64()))
        .unwrap_or(0.0)
}

fn first_buy_units_and_investment(base_shares: f64, price: f64) -> (f64, f64) {
    let min_units = (MIN_FIRST_BUY_COST / price).max(base_shares);
    let units = (min_units * 100.0).ceil() / 100.0;
    let investment = units * price;
    (units, investment)
}

/// Parse ISO 8601 end date to Unix timestamp. Returns None if unparseable.
fn parse_end_date_iso(s: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp() as u64)
}

async fn fetch_token_price(
    api: &PolymarketApi,
    token_id: &str,
) -> Option<polymarket_trading_bot::models::TokenPrice> {
    let bid = api.get_price(token_id, "BUY").await.ok();
    let ask = api.get_price(token_id, "SELL").await.ok();
    if bid.is_some() || ask.is_some() {
        Some(polymarket_trading_bot::models::TokenPrice {
            token_id: token_id.to_string(),
            bid,
            ask,
        })
    } else {
        None
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum SportsTrailingState {
    /// Trail both tokens; buy the one that first satisfies ask >= lowest + trailing_stop.
    WaitingFirst {
        low0: f64,
        high0: f64,
        low1: f64,
        high1: f64,
    },
    /// First buy in flight (skip updates until resolved).
    FirstBuyPending {
        first_is_token0: bool,
        first_price: f64,
        shares: f64,
        opp_lowest: f64,
        revert_low0: f64,
        revert_high0: f64,
        revert_low1: f64,
        revert_high1: f64,
    },
    /// First token bought; trail opposite.
    FirstBought {
        first_is_token0: bool,
        first_price: f64,
        shares: f64,
        opposite_lowest: f64,
    },
    /// Both bought for this round. If continuous, will reset to WaitingFirst.
    Done,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;
    let is_simulation = args.is_simulation();

    let slug = config
        .trading
        .slug
        .as_ref()
        .filter(|s| !s.is_empty())
        .context("Config must set trading.slug (e.g. your sports market slug)")?;

    let continuous = config.trading.continuous;
    let trailing_stop = config.trading.trailing_stop_point.unwrap_or(0.03);
    let shares = config
        .trading
        .trailing_shares
        .unwrap_or_else(|| config.trading.fixed_trade_amount / 0.5);
    let check_interval_ms = config.trading.check_interval_ms;

    eprintln!("🚀 Sports Trailing Bot — slug: {}", slug);
    eprintln!(
        "Mode: {} | Continuous: {}",
        if is_simulation { "SIMULATION" } else { "LIVE" },
        continuous
    );
    eprintln!(
        "Trailing stop: {:.4} | Shares per side: {} | Check interval: {} ms",
        trailing_stop, shares, check_interval_ms
    );

    let api = Arc::new(PolymarketApi::new(
        config.polymarket.gamma_api_url.clone(),
        config.polymarket.clob_api_url.clone(),
        config.polymarket.api_key.clone(),
        config.polymarket.api_secret.clone(),
        config.polymarket.api_passphrase.clone(),
        config.polymarket.private_key.clone(),
        config.polymarket.proxy_wallet_address.clone(),
        config.polymarket.signature_type,
    ));

    eprintln!("\n═══════════════════════════════════════════════════════════");
    eprintln!("🔐 Authenticating...");
    eprintln!("═══════════════════════════════════════════════════════════");
    api.authenticate().await.context("Authentication failed")?;
    eprintln!("✅ Authentication successful!\n");

    let market = api.get_market_by_slug(slug).await.context("Failed to load market by slug")?;
    let condition_id = market.condition_id.clone();
    let details = api
        .get_market(&condition_id)
        .await
        .context("Failed to get market details (tokens, end time)")?;

    if details.tokens.len() < 2 {
        anyhow::bail!("Market must have at least two outcome tokens, got {}", details.tokens.len());
    }
    let token0_id = details.tokens[0].token_id.clone();
    let token1_id = details.tokens[1].token_id.clone();
    let out0 = &details.tokens[0].outcome;
    let out1 = &details.tokens[1].outcome;

    let end_ts = parse_end_date_iso(&details.end_date_iso)
        .or_else(|| market.end_date_iso.as_deref().and_then(parse_end_date_iso))
        .or_else(|| market.end_date_iso_alt.as_deref().and_then(parse_end_date_iso));

    eprintln!("Market: {} | Condition: {}...", market.question, &condition_id[..condition_id.len().min(24)]);
    eprintln!("Token0 ({}): {}...", out0, &token0_id[..token0_id.len().min(20)]);
    eprintln!("Token1 ({}): {}...", out1, &token1_id[..token1_id.len().min(20)]);
    if let Some(et) = end_ts {
        eprintln!("End time (Unix): {}", et);
    }

    let trader = Arc::new(Trader::new(api.clone(), config.trading.clone(), is_simulation, None)?);
    let state: Arc<tokio::sync::Mutex<SportsTrailingState>> =
        Arc::new(tokio::sync::Mutex::new(SportsTrailingState::WaitingFirst {
            low0: 1.0,
            high0: 0.0,
            low1: 1.0,
            high1: 0.0,
        }));

    let period_timestamp = end_ts.unwrap_or(0).saturating_sub(3600); // placeholder for logging

    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let time_remaining_seconds = end_ts.map(|et| et.saturating_sub(now)).unwrap_or(999_999);
        if time_remaining_seconds == 0 {
            eprintln!("Market ended (time remaining 0). Exiting.");
            break;
        }

        let (price0, price1) = tokio::join!(
            fetch_token_price(api.as_ref(), &token0_id),
            fetch_token_price(api.as_ref(), &token1_id),
        );

        let (Some(p0), Some(p1)) = (price0, price1) else {
            tokio::time::sleep(tokio::time::Duration::from_millis(check_interval_ms)).await;
            continue;
        };

        let ask0 = ask_f64(&p0);
        let ask1 = ask_f64(&p1);

        {
            let guard = state.lock().await;
            if let SportsTrailingState::FirstBuyPending { .. } = &*guard {
                drop(guard);
                tokio::time::sleep(tokio::time::Duration::from_millis(check_interval_ms)).await;
                continue;
            }
        }

        let mut guard = state.lock().await;
        match &mut *guard {
            SportsTrailingState::WaitingFirst {
                low0,
                high0,
                low1,
                high1,
            } => {
                let old_high0 = *high0;
                let old_high1 = *high1;
                *low0 = (*low0).min(ask0);
                *high0 = (*high0).max(ask0);
                *low1 = (*low1).min(ask1);
                *high1 = (*high1).max(ask1);

                let trigger0 = *low0 + trailing_stop;
                let trigger1 = *low1 + trailing_stop;

                if ask0 > old_high0 {
                    *low0 = ask0;
                }
                if ask1 > old_high1 {
                    *low1 = ask1;
                }

                let buy0 = ask0 >= trigger0 && ask0 <= old_high0;
                let buy1 = ask1 >= trigger1 && ask1 <= old_high1;

                let do_buy0 = buy0 && !buy1;
                let do_buy1 = buy1 && !buy0;
                let do_both = buy0 && buy1;
                let (buy_first_0, price) = if do_buy0 {
                    (true, ask0)
                } else if do_buy1 {
                    (false, ask1)
                } else if do_both {
                    (ask0 <= ask1, if ask0 <= ask1 { ask0 } else { ask1 })
                } else {
                    (false, 0.0)
                };

                if do_buy0 || do_buy1 || do_both {
                    drop(guard);
                    execute_first_buy(
                        state.clone(),
                        trader.clone(),
                        buy_first_0,
                        price,
                        shares,
                        &p0,
                        &p1,
                        &condition_id,
                        period_timestamp,
                        time_remaining_seconds,
                        out0,
                        out1,
                    )
                    .await;
                    tokio::time::sleep(tokio::time::Duration::from_millis(check_interval_ms)).await;
                    continue;
                }
            }
            SportsTrailingState::FirstBought {
                first_is_token0,
                first_price,
                shares: first_shares,
                opposite_lowest,
            } => {
                let (opp_ask, _opp_token, opp_id, is_first_side) = if *first_is_token0 {
                    (ask1, &p1, &token1_id, false)
                } else {
                    (ask0, &p0, &token0_id, true)
                };
                *opposite_lowest = (*opposite_lowest).min(opp_ask);
                let trigger_at = *opposite_lowest + trailing_stop;
                if opp_ask >= trigger_at {
                    let _first_price_val = *first_price;
                    let first_shares_val = *first_shares;
                    let _first_is_0 = *first_is_token0;
                    drop(guard);
                    let investment = first_shares_val * opp_ask;
                    let opp = BuyOpportunity {
                        condition_id: condition_id.clone(),
                        token_id: opp_id.clone(),
                        token_type: if is_first_side {
                            TokenType::BtcUp
                        } else {
                            TokenType::BtcDown
                        },
                        bid_price: opp_ask,
                        period_timestamp,
                        time_remaining_seconds,
                        time_elapsed_seconds: 0,
                        use_market_order: true,
                        investment_amount_override: Some(investment),
                        is_individual_hedge: false,
                        is_standard_hedge: false,
                        dual_limit_shares: Some(first_shares_val),
                    };
                    if let Err(e) = trader.execute_buy(&opp).await {
                        warn!("Sports trailing second buy failed: {}", e);
                    } else {
                        polymarket_trading_bot::log_println!(
                            "📈 Sports trailing second buy: {} at ${:.4} x {:.6}",
                            if is_first_side { out0 } else { out1 },
                            opp_ask,
                            first_shares_val
                        );
                        let mut g = state.lock().await;
                        if continuous {
                            *g = SportsTrailingState::WaitingFirst {
                                low0: 1.0,
                                high0: 0.0,
                                low1: 1.0,
                                high1: 0.0,
                            };
                        } else {
                            *g = SportsTrailingState::Done;
                        }
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(check_interval_ms)).await;
                    continue;
                }
            }
            SportsTrailingState::Done => {
                if !continuous {
                    drop(guard);
                    tokio::time::sleep(tokio::time::Duration::from_millis(check_interval_ms)).await;
                    continue;
                }
                *guard = SportsTrailingState::WaitingFirst {
                    low0: ask0,
                    high0: ask0,
                    low1: ask1,
                    high1: ask1,
                };
            }
            SportsTrailingState::FirstBuyPending { .. } => {}
        }
        drop(guard);
        tokio::time::sleep(tokio::time::Duration::from_millis(check_interval_ms)).await;
    }

    Ok(())
}

async fn execute_first_buy(
    state: Arc<tokio::sync::Mutex<SportsTrailingState>>,
    trader: Arc<Trader>,
    first_is_token0: bool,
    buy_price: f64,
    base_shares: f64,
    p0: &polymarket_trading_bot::models::TokenPrice,
    p1: &polymarket_trading_bot::models::TokenPrice,
    condition_id: &str,
    period_timestamp: u64,
    time_remaining_seconds: u64,
    out0: &str,
    out1: &str,
) {
    let (units, investment) = first_buy_units_and_investment(base_shares, buy_price);
    let (token_id, token_type, opp_ask) = if first_is_token0 {
        (p0.token_id.clone(), TokenType::BtcUp, ask_f64(p1))
    } else {
        (p1.token_id.clone(), TokenType::BtcDown, ask_f64(p0))
    };

    let revert_low0 = ask_f64(p0).min(1.0);
    let revert_high0 = ask_f64(p0).max(0.0);
    let revert_low1 = ask_f64(p1).min(1.0);
    let revert_high1 = ask_f64(p1).max(0.0);

    {
        let mut g = state.lock().await;
        *g = SportsTrailingState::FirstBuyPending {
            first_is_token0,
            first_price: buy_price,
            shares: units,
            opp_lowest: opp_ask,
            revert_low0,
            revert_high0,
            revert_low1,
            revert_high1,
        };
    }

    let opp = BuyOpportunity {
        condition_id: condition_id.to_string(),
        token_id: token_id.clone(),
        token_type,
        bid_price: buy_price,
        period_timestamp,
        time_remaining_seconds,
        time_elapsed_seconds: 0,
        use_market_order: true,
        investment_amount_override: Some(investment),
        is_individual_hedge: false,
        is_standard_hedge: false,
        dual_limit_shares: Some(units),
    };

    let result = trader.execute_buy(&opp).await;
    let mut g = state.lock().await;
    match result {
        Err(e) => {
            warn!("Sports trailing first buy failed: {}", e);
            *g = SportsTrailingState::WaitingFirst {
                low0: revert_low0,
                high0: revert_high0,
                low1: revert_low1,
                high1: revert_high1,
            };
        }
        Ok(()) => {
            polymarket_trading_bot::log_println!(
                "📈 Sports trailing first buy: {} at ${:.4} x {:.6} (cost ${:.2})",
                if first_is_token0 { out0 } else { out1 },
                buy_price,
                units,
                investment
            );
            *g = SportsTrailingState::FirstBought {
                first_is_token0,
                first_price: buy_price,
                shares: units,
                opposite_lowest: opp_ask,
            };
        }
    }
}
