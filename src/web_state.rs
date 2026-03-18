//! Shared state for the copy-trading UI: logs, status, positions. Served as JSON at /api/state.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Keep only recent activity so the UI stays real-time; trim older entries.
const MAX_LOGS: usize = 50;

#[derive(Debug, Clone, Serialize)]
pub struct TradeLog {
    pub time: String,
    pub tag: String,
    pub side: String,
    pub outcome: String,
    pub size: String,
    pub price: String,
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PositionSummary {
    pub slug: String,
    pub outcome: String,
    pub size: f64,
    pub cur_price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub mode: String,
    pub targets: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_addresses: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiConfig {
    pub delta_highlight_sec: u64,
    pub delta_animation_sec: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BotState {
    pub logs: Vec<TradeLog>,
    pub status: Status,
    pub positions: HashMap<String, Vec<PositionSummary>>,
    pub ui: UiConfig,
}

impl Default for BotState {
    fn default() -> Self {
        Self {
            logs: Vec::new(),
            status: Status {
                mode: String::new(),
                targets: 0,
                wallet: None,
                target_addresses: None,
            },
            positions: HashMap::new(),
            ui: UiConfig {
                delta_highlight_sec: 10,
                delta_animation_sec: 2,
            },
        }
    }
}

/// Global UI state. Clone to get Arc<RwLock<BotState>> for handlers.
pub type SharedState = Arc<RwLock<BotState>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(RwLock::new(BotState::default()))
}

pub async fn get_state(state: SharedState) -> BotState {
    state.read().await.clone()
}

pub async fn set_status(
    state: SharedState,
    mode: String,
    targets: u32,
    wallet: Option<String>,
    target_addresses: Option<Vec<String>>,
) {
    let mut s = state.write().await;
    s.status = Status {
        mode,
        targets,
        wallet,
        target_addresses,
    };
}

pub async fn set_ui_config(state: SharedState, delta_highlight_sec: u64, delta_animation_sec: u64) {
    let mut s = state.write().await;
    s.ui = UiConfig {
        delta_highlight_sec,
        delta_animation_sec,
    };
}

pub async fn push_trade(
    state: SharedState,
    tag: String,
    side: String,
    outcome: String,
    size: String,
    price: String,
    slug: String,
    target_address: Option<String>,
    copy_status: Option<String>,
) {
    let mut s = state.write().await;
    s.logs.push(TradeLog {
        time: chrono::Utc::now().to_rfc3339(),
        tag,
        side,
        outcome,
        size,
        price,
        slug,
        target_address,
        copy_status,
    });
    if s.logs.len() > MAX_LOGS {
        s.logs.remove(0);
    }
}

/// prev_sizes: (user -> (key "slug|outcome" -> size)) for delta
pub async fn set_positions(
    state: SharedState,
    user: String,
    positions: Vec<(String, String, f64, f64)>, // (slug, outcome, size, cur_price)
) {
    let mut prev: HashMap<String, f64> = HashMap::new();
    {
        let s = state.read().await;
        if let Some(existing) = s.positions.get(&user) {
            for p in existing {
                let key = format!("{}|{}", p.slug, p.outcome);
                prev.insert(key, p.size);
            }
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut list = Vec::with_capacity(positions.len());
    for (slug, outcome, size, cur_price) in positions {
        let key = format!("{}|{}", slug, outcome);
        let prev_size = prev.get(&key).copied();
        prev.insert(key.clone(), size);
        let (delta, delta_at) = prev_size
            .map(|ps| {
                let d = size - ps;
                (Some(d), if d != 0.0 { Some(now.clone()) } else { None })
            })
            .unwrap_or((None, None));
        list.push(PositionSummary {
            slug,
            outcome,
            size,
            cur_price,
            delta: if delta.map(|d| d != 0.0).unwrap_or(false) {
                delta
            } else {
                None
            },
            delta_at,
        });
    }
    let mut s = state.write().await;
    s.positions.insert(user, list);
}
