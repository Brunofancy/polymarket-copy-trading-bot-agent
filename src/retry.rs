//! Retry utilities with exponential backoff for resilient API calls.

use anyhow::Result;
use std::time::Duration;
use tokio::time::sleep;

/// Retry configuration
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 100,
            max_delay_ms: 5000,
            multiplier: 2.0,
        }
    }
}

impl RetryConfig {
    /// Create a retry config for order placement (more retries for sells)
    pub fn for_order(is_buy: bool) -> Self {
        if is_buy {
            Self {
                max_retries: 2,
                initial_delay_ms: 200,
                max_delay_ms: 2000,
                multiplier: 2.0,
            }
        } else {
            Self {
                max_retries: 3,
                initial_delay_ms: 300,
                max_delay_ms: 3000,
                multiplier: 2.0,
            }
        }
    }

    /// Create a retry config for critical operations
    pub fn critical() -> Self {
        Self {
            max_retries: 5,
            initial_delay_ms: 500,
            max_delay_ms: 10000,
            multiplier: 2.0,
        }
    }
}

/// Retry a function with exponential backoff
pub async fn retry_with_backoff<F, T, E>(config: &RetryConfig, mut f: F) -> Result<T>
where
    F: FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, E>> + Send>>,
    E: std::fmt::Display,
{
    let mut delay_ms = config.initial_delay_ms;
    let mut last_error = None;

    for attempt in 0..=config.max_retries {
        match f().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                last_error = Some(e);
                if attempt < config.max_retries {
                    let delay = Duration::from_millis(delay_ms.min(config.max_delay_ms));
                    log::debug!("Retry attempt {} failed, waiting {:?} before retry", attempt + 1, delay);
                    sleep(delay).await;
                    delay_ms = (delay_ms as f64 * config.multiplier) as u64;
                }
            }
        }
    }

    anyhow::bail!(
        "Operation failed after {} retries: {}",
        config.max_retries + 1,
        last_error.map(|e| e.to_string()).unwrap_or_else(|| "unknown error".to_string())
    )
}

/// Check if an error is retryable
pub fn is_retryable_error(error: &str) -> bool {
    let error_lower = error.to_lowercase();
    error_lower.contains("timeout")
        || error_lower.contains("connection")
        || error_lower.contains("network")
        || error_lower.contains("rate limit")
        || error_lower.contains("429")
        || error_lower.contains("502")
        || error_lower.contains("503")
        || error_lower.contains("504")
}

/// Retry only on retryable errors
pub async fn retry_on_retryable<F, Fut, T>(config: &RetryConfig, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay_ms = config.initial_delay_ms;
    let mut last_error = None;

    for attempt in 0..=config.max_retries {
        match f().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                let error_str = e.to_string();
                if !is_retryable_error(&error_str) {
                    return Err(e);
                }
                last_error = Some(error_str);
                if attempt < config.max_retries {
                    let delay = Duration::from_millis(delay_ms.min(config.max_delay_ms));
                    log::warn!("Retryable error on attempt {}: {}, retrying in {:?}", attempt + 1, error_str, delay);
                    sleep(delay).await;
                    delay_ms = (delay_ms as f64 * config.multiplier) as u64;
                }
            }
        }
    }

    anyhow::bail!(
        "Operation failed after {} retries: {}",
        config.max_retries + 1,
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )
}
