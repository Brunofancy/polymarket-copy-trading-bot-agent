//! Rate limiting utilities for API calls.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Token bucket rate limiter
#[derive(Debug, Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<RateLimiterInner>>,
}

#[derive(Debug)]
struct RateLimiterInner {
    tokens: f64,
    capacity: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a new rate limiter
    /// - `capacity`: Maximum number of tokens
    /// - `refill_rate`: Tokens per second
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RateLimiterInner {
                tokens: capacity,
                capacity,
                refill_rate,
                last_refill: Instant::now(),
            })),
        }
    }

    /// Create a rate limiter for CLOB API (conservative: 10 req/s)
    pub fn for_clob_api() -> Self {
        Self::new(10.0, 10.0)
    }

    /// Create a rate limiter for position polling (2 req/s)
    pub fn for_position_polling() -> Self {
        Self::new(2.0, 2.0)
    }

    /// Try to acquire a token, returns true if successful
    pub async fn try_acquire(&self) -> bool {
        let mut inner = self.inner.lock().await;
        inner.refill();
        if inner.tokens >= 1.0 {
            inner.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Acquire a token, waiting if necessary
    pub async fn acquire(&self) {
        loop {
            if self.try_acquire().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Acquire multiple tokens
    pub async fn acquire_n(&self, n: f64) {
        loop {
            let mut inner = self.inner.lock().await;
            inner.refill();
            if inner.tokens >= n {
                inner.tokens -= n;
                return;
            }
            drop(inner);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl RateLimiterInner {
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill);
        let tokens_to_add = elapsed.as_secs_f64() * self.refill_rate;
        self.tokens = (self.tokens + tokens_to_add).min(self.capacity);
        self.last_refill = now;
    }
}

/// Simple rate limiter that allows N operations per time window
#[derive(Debug, Clone)]
pub struct WindowRateLimiter {
    inner: Arc<Mutex<WindowRateLimiterInner>>,
}

#[derive(Debug)]
struct WindowRateLimiterInner {
    operations: Vec<Instant>,
    max_operations: usize,
    window: Duration,
}

impl WindowRateLimiter {
    /// Create a new window rate limiter
    /// - `max_operations`: Maximum operations allowed
    /// - `window`: Time window
    pub fn new(max_operations: usize, window: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(WindowRateLimiterInner {
                operations: Vec::new(),
                max_operations,
                window,
            })),
        }
    }

    /// Try to acquire, returns true if allowed
    pub async fn try_acquire(&self) -> bool {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        let cutoff = now - inner.window;
        inner.operations.retain(|&t| t > cutoff);
        if inner.operations.len() < inner.max_operations {
            inner.operations.push(now);
            true
        } else {
            false
        }
    }

    /// Acquire, waiting if necessary
    pub async fn acquire(&self) {
        loop {
            if self.try_acquire().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
