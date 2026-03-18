//! Metrics collection for trading bot performance tracking.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TradeMetrics {
    pub total_trades: u64,
    pub successful_trades: u64,
    pub failed_trades: u64,
    pub total_volume_usd: f64,
    pub average_trade_size_usd: f64,
    pub last_trade_time: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiMetrics {
    pub total_calls: u64,
    pub successful_calls: u64,
    pub failed_calls: u64,
    pub average_latency_ms: f64,
    pub last_call_time: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceMetrics {
    pub trades: TradeMetrics,
    pub api: ApiMetrics,
    pub uptime_seconds: u64,
    pub websocket_reconnects: u64,
    pub last_update: String,
}

#[derive(Debug)]
pub struct MetricsCollector {
    inner: Arc<RwLock<MetricsCollectorInner>>,
    start_time: Instant,
}

#[derive(Debug)]
struct MetricsCollectorInner {
    trades: TradeMetrics,
    api: ApiMetrics,
    websocket_reconnects: u64,
    latency_samples: Vec<Duration>,
}

impl Default for TradeMetrics {
    fn default() -> Self {
        Self {
            total_trades: 0,
            successful_trades: 0,
            failed_trades: 0,
            total_volume_usd: 0.0,
            average_trade_size_usd: 0.0,
            last_trade_time: None,
        }
    }
}

impl Default for ApiMetrics {
    fn default() -> Self {
        Self {
            total_calls: 0,
            successful_calls: 0,
            failed_calls: 0,
            average_latency_ms: 0.0,
            last_call_time: None,
        }
    }
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MetricsCollectorInner {
                trades: TradeMetrics::default(),
                api: ApiMetrics::default(),
                websocket_reconnects: 0,
                latency_samples: Vec::new(),
            })),
            start_time: Instant::now(),
        }
    }

    pub async fn record_trade(&self, success: bool, volume_usd: f64) {
        let mut inner = self.inner.write().await;
        inner.trades.total_trades += 1;
        if success {
            inner.trades.successful_trades += 1;
            inner.trades.total_volume_usd += volume_usd;
        } else {
            inner.trades.failed_trades += 1;
        }
        if inner.trades.total_trades > 0 {
            inner.trades.average_trade_size_usd = inner.trades.total_volume_usd / inner.trades.successful_trades as f64;
        }
        inner.trades.last_trade_time = Some(chrono::Utc::now().to_rfc3339());
    }

    pub async fn record_api_call(&self, success: bool, latency: Duration) {
        let mut inner = self.inner.write().await;
        inner.api.total_calls += 1;
        if success {
            inner.api.successful_calls += 1;
            inner.latency_samples.push(latency);
            // Keep only last 100 samples for average calculation
            if inner.latency_samples.len() > 100 {
                inner.latency_samples.remove(0);
            }
            let total_ms: f64 = inner.latency_samples.iter().map(|d| d.as_secs_f64() * 1000.0).sum();
            inner.api.average_latency_ms = total_ms / inner.latency_samples.len() as f64;
        } else {
            inner.api.failed_calls += 1;
        }
        inner.api.last_call_time = Some(chrono::Utc::now().to_rfc3339());
    }

    pub async fn record_websocket_reconnect(&self) {
        let mut inner = self.inner.write().await;
        inner.websocket_reconnects += 1;
    }

    pub async fn get_metrics(&self) -> PerformanceMetrics {
        let inner = self.inner.read().await;
        PerformanceMetrics {
            trades: inner.trades.clone(),
            api: inner.api.clone(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            websocket_reconnects: inner.websocket_reconnects,
            last_update: chrono::Utc::now().to_rfc3339(),
        }
    }

    pub fn success_rate(&self) -> impl std::future::Future<Output = f64> + Send {
        let inner = Arc::clone(&self.inner);
        async move {
            let inner = inner.read().await;
            if inner.trades.total_trades == 0 {
                return 0.0;
            }
            inner.trades.successful_trades as f64 / inner.trades.total_trades as f64 * 100.0
        }
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}
