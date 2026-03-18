//! Copy-trading backend: follow leader addresses from trade.toml.
//! Config: config.json (polymarket credentials + API keys), trade.toml (targets, filters, exit).
//! Serves UI at port (trade.toml) and /api/state for the Leptos frontend.

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::sse::{Event, KeepAlive, Sse},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use async_stream::stream;
use log::info;
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, Mutex};
use tower_http::services::ServeDir;
use serde::Deserialize;
use serde_json::Value;
use polymarket_trading_bot::activity_stream;
use polymarket_trading_bot::clob_sdk;
use polymarket_trading_bot::api::PolymarketApi;
use polymarket_trading_bot::config::Config;
use polymarket_trading_bot::copy_trading::{spawn_exit_loop, CopyTradingConfig};
use polymarket_trading_bot::web_state::{self, SharedState};

#[derive(Parser, Debug)]
#[command(name = "main_copytrading")]
#[command(about = "Copy-trade from leader addresses (trade.toml). Uses config.json for Polymarket credentials.")]
pub struct CopyArgs {
    /// Config file (polymarket credentials, API key)
    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,

    /// Copy-trading config (targets, filters, exit)
    #[arg(short, long, default_value = "trade.toml")]
    pub trade_config: PathBuf,

    /// Simulation mode
    #[arg(long)]
    pub simulation: bool,

    /// Directory to serve UI from (default: frontend/dist). Build with: cd frontend && trunk build
    #[arg(long, default_value = "frontend/dist")]
    pub ui_dir: PathBuf,
}

pub type NotifyTx = broadcast::Sender<()>;

#[derive(Clone)]
struct AppState {
    web: SharedState,
    ui_dir: PathBuf,
    notify: NotifyTx,
    /// Shared client for OpenRouter (connection reuse, pool); keeps agent chat fast.
    openrouter_client: reqwest::Client,
}

async fn sse_state_updates(State(app): State<AppState>) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let mut rx = app.notify.subscribe();
    let stream = stream! {
        loop {
            match rx.recv().await {
                Ok(_) => yield Ok(Event::default().data("update")),
                Err(broadcast::error::RecvError::Lagged(_)) => yield Ok(Event::default().data("update")),
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaderboardQuery {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    time_period: Option<String>,
    #[serde(default)]
    order_by: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

async fn api_leaderboard(
    Query(q): Query<LeaderboardQuery>,
) -> Result<axum::Json<Value>, (StatusCode, &'static str)> {
    let category = q.category.as_deref().unwrap_or("OVERALL");
    let time_period = q.time_period.as_deref().unwrap_or("WEEK");
    let order_by = q.order_by.as_deref().unwrap_or("PNL");
    let limit = q.limit.unwrap_or(25).clamp(1, 50);
    let offset = q.offset.unwrap_or(0).min(1000);
    let limit_s = limit.to_string();
    let offset_s = offset.to_string();
    let url = reqwest::Url::parse_with_params(
        "https://data-api.polymarket.com/v1/leaderboard",
        &[
            ("category", category),
            ("timePeriod", time_period),
            ("orderBy", order_by),
            ("limit", limit_s.as_str()),
            ("offset", offset_s.as_str()),
        ],
    )
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "leaderboard URL"))?;
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "leaderboard request failed"))?;
    if !resp.status().is_success() {
        return Err((StatusCode::BAD_GATEWAY, "leaderboard API error"));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "leaderboard parse error"))?;
    Ok(axum::Json(body))
}

/// Provider entry for dropdown: only includes providers that have API key set in .env.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentProvider {
    id: String,
    name: String,
    default_model: String,
}

#[derive(Debug, serde::Serialize)]
struct AgentProvidersResponse {
    providers: Vec<AgentProvider>,
}

fn polygon() -> u64 {
    clob_sdk::polygon()
}


async fn api_agent_providers() -> Json<AgentProvidersResponse> {
    let mut providers = Vec::new();
    if std::env::var("OPENROUTER_API_KEY").map(|k| !k.is_empty()).unwrap_or(false) {
        providers.push(AgentProvider {
            id: "openrouter".to_string(),
            name: "OpenRouter".to_string(),
            default_model: std::env::var("OPENROUTER_MODEL")
                .unwrap_or_else(|_| "anthropic/claude-3.5-sonnet".to_string()),
        });
    }
    if std::env::var("OPENAI_API_KEY").map(|k| !k.is_empty()).unwrap_or(false) {
        providers.push(AgentProvider {
            id: "openai".to_string(),
            name: "OpenAI".to_string(),
            default_model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
        });
    }
    if std::env::var("ANTHROPIC_API_KEY").map(|k| !k.is_empty()).unwrap_or(false) {
        providers.push(AgentProvider {
            id: "claude".to_string(),
            name: "Claude.ai".to_string(),
            default_model: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-3-5-sonnet-20241022".to_string()),
        });
    }
    Json(AgentProvidersResponse { providers })
}

/// Request body for agent chat.
#[derive(Debug, serde::Deserialize)]
struct AgentChatRequest {
    message: String,
    /// Provider id: "openrouter" | "openai" | "claude". If missing, uses first available.
    #[serde(default)]
    provider: Option<String>,
    /// Model override for that provider. If missing, uses provider default.
    #[serde(default)]
    model: Option<String>,
}

/// Response body for agent chat.
#[derive(Debug, serde::Serialize)]
struct AgentChatResponse {
    reply: String,
}

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const OPENROUTER_DEFAULT_MODEL: &str = "anthropic/claude-3.5-sonnet";
const OPENAI_DEFAULT_MODEL: &str = "gpt-4o-mini";
const ANTHROPIC_DEFAULT_MODEL: &str = "claude-3-5-sonnet-20241022";

fn parse_llm_error_message(status: u16, body: &str) -> String {
    let json: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let error_msg = json
        .as_ref()
        .and_then(|j| j.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str());
    let code = json
        .as_ref()
        .and_then(|j| j.get("error"))
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str());
    if status == 429 {
        let code_display = code.unwrap_or("unknown");
        if code == Some("rate_limit_exceeded") {
            return "Rate limit hit — too many requests. Wait 30–60 seconds and try again, or use another provider in the dropdown.".to_string();
        }
        if code == Some("insufficient_quota") || error_msg.map(|m| m.contains("quota")).unwrap_or(false) {
            return format!(
                "OpenAI returned \"{}\". Even with a paid key, check: (1) Usage & billing at https://platform.openai.com/usage — ensure you have remaining usage; (2) The key in .env is for the correct org and not revoked; (3) Try another provider in the dropdown.",
                code_display
            );
        }
        return format!("Too many requests ({}). Wait a moment or try another provider in the dropdown.", code_display);
    }
    if let Some(m) = error_msg {
        let trimmed = if m.len() > 280 { format!("{}…", &m[..277]) } else { m.to_string() };
        return format!("LLM API error {}: {}", status, trimmed);
    }
    let trimmed = if body.len() > 280 { format!("{}…", &body[..277]) } else { body.to_string() };
    format!("LLM API error {}: {}", status, trimmed)
}

const OPENROUTER_SYSTEM_PROMPT: &str = r#"You are an autonomous analysis layer using the same method as Mahoraga (Monitor → Analyze). You do NOT Execute: no trades or buy/sell orders, only research and guidance.

Mahoraga's flow: the agent monitors for signals, then each signal is researched by the LLM—analyzing sentiment, timing, catalysts, and red flags. You do that research step. When the user asks a substantive market or position question, treat their question as the "signal" and research it the same way.

For those questions, when it adds clarity, structure your reply as a short research note (use **bold** for section headers):

- **Signal** — One line on what the data or question implies (e.g. "BTC near overbought with sell-side pressure above spot" or "User asking about exposure to trending market").
- **Research** — Your analysis: sentiment, timing, catalysts, red flags. Plain language, same style as Mahoraga's LLM research.
- **Context for you** — How this ties to their positions (entry, PnL, exposure) and bias risk (FOMO, panic, chasing, averaging down).
- **Confidence** — When relevant, give a single confidence or signal-strength note (e.g. "High confidence in holding" or "Medium — wait for confirmation"). Helps them weight your guidance.
- **Guidance** — One clear takeaway (e.g. "holding is safer", "wait for confirmation"). Never "buy" or "sell"; only research and guidance.

Flow: Monitor → Analyze → output (no Execute). For short or simple questions, answer naturally without forcing the structure. Adapt tone: beginners get less raw stats and more explanation; advanced users can get more technical."#;

async fn api_agent_chat(
    State(app): State<AppState>,
    Json(body): Json<AgentChatRequest>,
) -> Result<Json<AgentChatResponse>, (StatusCode, String)> {
    let has_openrouter = std::env::var("OPENROUTER_API_KEY").map(|k| !k.is_empty()).unwrap_or(false);
    let has_openai = std::env::var("OPENAI_API_KEY").map(|k| !k.is_empty()).unwrap_or(false);
    let has_anthropic = std::env::var("ANTHROPIC_API_KEY").map(|k| !k.is_empty()).unwrap_or(false);
    let provider_id = body.provider.as_deref().unwrap_or_else(|| {
        if has_openrouter {
            "openrouter"
        } else if has_openai {
            "openai"
        } else if has_anthropic {
            "claude"
        } else {
            ""
        }
    });
    #[derive(Clone, Copy)]
    enum ProviderKind {
        OpenRouter,
        OpenAI,
        Anthropic,
    }
    let (url, api_key, model, provider_kind) = match provider_id {
        "openrouter" if has_openrouter => {
            let key = std::env::var("OPENROUTER_API_KEY").map_err(|_| {
                (StatusCode::SERVICE_UNAVAILABLE, "OPENROUTER_API_KEY not set".to_string())
            })?;
            let model = body.model.clone().unwrap_or_else(|| {
                std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| OPENROUTER_DEFAULT_MODEL.to_string())
            });
            (OPENROUTER_URL, key, model, ProviderKind::OpenRouter)
        }
        "openai" if has_openai => {
            let key = std::env::var("OPENAI_API_KEY").map_err(|_| {
                (StatusCode::SERVICE_UNAVAILABLE, "OPENAI_API_KEY not set".to_string())
            })?;
            let model = body.model.clone().unwrap_or_else(|| {
                std::env::var("OPENAI_MODEL").unwrap_or_else(|_| OPENAI_DEFAULT_MODEL.to_string())
            });
            (OPENAI_URL, key, model, ProviderKind::OpenAI)
        }
        "claude" if has_anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                (StatusCode::SERVICE_UNAVAILABLE, "ANTHROPIC_API_KEY not set".to_string())
            })?;
            let model = body.model.clone().unwrap_or_else(|| {
                std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| ANTHROPIC_DEFAULT_MODEL.to_string())
            });
            (ANTHROPIC_URL, key, model, ProviderKind::Anthropic)
        }
        _ => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "No provider with API key. Set OPENROUTER_API_KEY, OPENAI_API_KEY, or ANTHROPIC_API_KEY in .env".to_string(),
            ));
        }
    };
    let max_tokens: u32 = std::env::var("OPENROUTER_MAX_TOKENS")
        .or_else(|_| std::env::var("OPENAI_MAX_TOKENS"))
        .or_else(|_| std::env::var("ANTHROPIC_MAX_TOKENS"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512)
        .min(4096);
    let client = app.openrouter_client.clone();
    let message = body.message;
    let url_s = url.to_string();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let req_body = match provider_kind {
            ProviderKind::OpenRouter | ProviderKind::OpenAI => serde_json::json!({
                "model": model,
                "max_tokens": max_tokens,
                "messages": [
                    {"role": "system", "content": OPENROUTER_SYSTEM_PROMPT},
                    {"role": "user", "content": message}
                ]
            }),
            ProviderKind::Anthropic => serde_json::json!({
                "model": model,
                "max_tokens": max_tokens,
                "system": OPENROUTER_SYSTEM_PROMPT,
                "messages": [{"role": "user", "content": message}]
            }),
        };
        let mut req = client
            .post(url_s.as_str())
            .header("Content-Type", "application/json")
            .json(&req_body);
        match provider_kind {
            ProviderKind::OpenRouter => {
                req = req
                    .header("Authorization", format!("Bearer {}", api_key))
                    .header("HTTP-Referer", "https://github.com/frogansol/fast-polymarket-copytrading-bot-rust");
            }
            ProviderKind::OpenAI => {
                req = req.header("Authorization", format!("Bearer {}", api_key));
            }
            ProviderKind::Anthropic => {
                req = req
                    .header("x-api-key", api_key.as_str())
                    .header("anthropic-version", "2023-06-01");
            }
        }
        let result = async {
            let resp = req
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("LLM request failed: {}", e)))?;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            if !status.is_success() {
                let msg = parse_llm_error_message(status.as_u16(), &text);
                return Err((StatusCode::BAD_GATEWAY, msg));
            }
            let json: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("LLM response parse: {}", e)))?;
            let reply = match provider_kind {
                ProviderKind::OpenRouter | ProviderKind::OpenAI => json
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                ProviderKind::Anthropic => json
                    .get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|b| b.get("text"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            };
            Ok(AgentChatResponse { reply })
        };
        let _ = tx.send(result.await);
    });
    let response = rx.await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Agent task dropped".to_string(),
        )
    })?;
    response.map(Json)
}

async fn api_state(State(app): State<AppState>) -> axum::response::Response {
    let state = web_state::get_state(app.web).await;
    let mut res = Json(state).into_response();
    res.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        "no-store, no-cache, must-revalidate, max-age=0"
            .parse()
            .unwrap(),
    );
    res.headers_mut().insert(
        axum::http::header::PRAGMA,
        "no-cache".parse().unwrap(),
    );
    res
}

/// Serve index.html for GET / so the SPA always loads the same entry point.
async fn serve_index(State(app): State<AppState>) -> Result<Response, (StatusCode, &'static str)> {
    use axum::response::IntoResponse;
    let path = app.ui_dir.join("index.html");
    let contents = tokio::fs::read_to_string(&path)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "index.html not found"))?;
    let mut res = Html(contents).into_response();
    res.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        "no-cache, no-store, must-revalidate".parse().unwrap(),
    );
    Ok(res)
}

async fn static_asset_mime(req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut res = next.run(req).await;
    if res.status().is_success() {
        let headers = res.headers_mut();
        if path.ends_with(".wasm") {
            let _ = headers.insert("content-type", "application/wasm".parse().unwrap());
        } else if path.ends_with(".js") {
            let _ = headers.insert(
                "content-type",
                "application/javascript; charset=utf-8".parse().unwrap(),
            );
        }
    }
    res
}

fn spawn_web_server(state: SharedState, notify: NotifyTx, port: u16, ui_dir: PathBuf) {
    let openrouter_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("reqwest OpenRouter client");
    tokio::spawn(async move {
        let app_state = AppState {
            web: state,
            ui_dir: ui_dir.clone(),
            notify,
            openrouter_client,
        };
        let serve_dir = ServeDir::new(&ui_dir);
        let app = Router::new()
            .route("/api/state", get(api_state))
            .route("/api/state/stream", get(sse_state_updates))
            .route("/api/leaderboard", get(api_leaderboard))
            .route("/api/agent/providers", get(api_agent_providers))
            .route("/api/agent/chat", post(api_agent_chat))
            .route("/", get(serve_index))
            .route("/logs", get(serve_index))
            .route("/settings", get(serve_index))
            .route("/toptraders", get(serve_index))
            .route("/agent", get(serve_index))
            .route("/portfolio", get(serve_index))
            .with_state(app_state)
            .fallback_service(serve_dir)
            .layer(middleware::from_fn(static_asset_mime));
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        axum::serve(listener, app).await.expect("serve");
    });
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    let args = CopyArgs::parse();
    let config = Config::load(&args.config)?;

    clob_sdk::ensure_loaded()?;
    let chain_id = polygon();
    let trade_path = if args.trade_config.is_absolute() {
        args.trade_config.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&args.trade_config)
    };
    let copy_config = CopyTradingConfig::load(&trade_path).with_context(|| {
        format!(
            "Load trade.toml (copy targets, filters, exit). Tried: {} (cwd: {})",
            trade_path.display(),
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).display()
        )
    })?;

    let targets = copy_config.target_addresses();
    if targets.is_empty() {
        anyhow::bail!(
            "No copy targets. Set copy.target_address or copy.target_addresses in {}",
            trade_path.display()
        );
    }

    let simulation = args.simulation || copy_config.simulation;
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

    let api_auth = api.clone();
    tokio::spawn(async move {
        if let Err(e) = api_auth.authenticate().await {
            log::error!("CLOB authentication (background) failed: {}", e);
        }
    });

    let wallet = if simulation {
        "simulation".to_string()
    } else {
        api.get_wallet_address().context("Get wallet address")?
    };

    info!(
        "Copy-trading | {} | {} target(s) | wallet: {}",
        if simulation { "SIMULATION" } else { "LIVE" },
        targets.len(),
        if wallet.len() > 20 {
            format!("{}...{}", &wallet[..10], &wallet[wallet.len() - 8..])
        } else {
            wallet.clone()
        }
    );

    let web_state = web_state::new_shared_state();
    web_state::set_status(
        web_state.clone(),
        if simulation { "Sim".to_string() } else { "Live".to_string() },
        targets.len() as u32,
        Some(wallet.clone()),
        Some(targets.clone()),
    )
    .await;
    web_state::set_ui_config(
        web_state.clone(),
        copy_config.ui.delta_highlight_sec,
        copy_config.ui.delta_animation_sec,
    )
    .await;
    let port = copy_config.port;
    let ui_dir = if args.ui_dir.is_absolute() {
        args.ui_dir.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&args.ui_dir)
    };
    let (notify_tx, _) = broadcast::channel(64);
    let entries: Arc<Mutex<HashMap<String, polymarket_trading_bot::copy_trading::Entry>>> =
        Arc::new(Mutex::new(HashMap::new()));

    info!("Serving UI from: {}", ui_dir.display());
    spawn_web_server(web_state.clone(), notify_tx.clone(), port, ui_dir);
    info!("UI: http://localhost:{} (or http://<this-host-ip>:{} from another device)", port, port);
    if !simulation
        && (copy_config.exit.take_profit > 0.0
            || copy_config.exit.stop_loss > 0.0
            || copy_config.exit.trailing_stop > 0.0)
    {
        spawn_exit_loop(
            api.clone(),
            copy_config.clone(),
            wallet.clone(),
            entries.clone(),
        );
        info!("Exit loop started (take_profit/stop_loss/trailing_stop)");
    }
    activity_stream::spawn_activity_stream(
        targets.clone(),
        api.clone(),
        copy_config.clone(),
        web_state.clone(),
        notify_tx.clone(),
        entries.clone(),
        simulation,
    );

    let poll_secs = copy_config.copy.poll_interval_sec.clamp(0.25, 3600.0);
    let poll_interval = std::time::Duration::from_secs_f64(poll_secs);

    let users_to_fetch: Vec<String> = if wallet == "simulation" {
        targets.iter().cloned().collect()
    } else {
        let mut u = vec![wallet.clone()];
        u.extend(targets.iter().cloned());
        u
    };
    let mut initial_done: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let fetch_futures: Vec<_> = users_to_fetch
            .iter()
            .map(|user| {
                let user_lower = user.to_lowercase();
                let api = api.clone();
                async move {
                    let res = api.get_positions(&user_lower).await;
                    (user_lower, res)
                }
            })
            .collect();
        let results = futures_util::future::join_all(fetch_futures).await;

        for (user_lower, positions_result) in results {
            let positions = match positions_result {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("get_positions {}: {}", user_lower, e);
                    continue;
                }
            };
            let pos_list: Vec<_> = positions
                .iter()
                .map(|p| {
                    (
                        p.slug.clone().unwrap_or_else(|| "?".to_string()),
                        p.outcome.clone().unwrap_or_else(|| "?".to_string()),
                        p.size,
                        p.cur_price,
                    )
                })
                .collect();
            web_state::set_positions(web_state.clone(), user_lower.clone(), pos_list).await;

            if !initial_done.contains(&user_lower) {
                info!("INIT | {} | {} position(s)", user_lower, positions.len());
                for p in &positions {
                    let slug = p.slug.as_deref().unwrap_or("?");
                    let outcome = p.outcome.as_deref().unwrap_or("?");
                    web_state::push_trade(
                        web_state.clone(),
                        "POS".to_string(),
                        "—".to_string(),
                        outcome.to_string(),
                        format!("{:.2}", p.size),
                        format!("{:.3}", p.cur_price),
                        slug.to_string(),
                        Some(user_lower.clone()),
                        Some("loaded".to_string()),
                    )
                    .await;
                    let _ = notify_tx.send(());
                }
                initial_done.insert(user_lower);
            }
        }
        tokio::time::sleep(poll_interval).await;
    }
}
