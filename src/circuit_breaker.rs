//! Circuit breaker pattern for resilient API calls.
//! Prevents cascading failures by temporarily stopping requests when failures exceed threshold.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq)]
enum CircuitState {
    Closed,   // Normal operation
    Open,     // Failing, reject requests immediately
    HalfOpen, // Testing if service recovered
}

#[derive(Debug)]
struct CircuitBreakerInner {
    state: CircuitState,
    failure_count: u32,
    success_count: u32,
    last_failure_time: Option<Instant>,
    last_state_change: Instant,
}

/// Circuit breaker for API resilience
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    inner: Arc<RwLock<CircuitBreakerInner>>,
    failure_threshold: u32,
    success_threshold: u32,
    timeout: Duration,
    reset_timeout: Duration,
}

impl CircuitBreaker {
    /// Create a new circuit breaker
    /// - `failure_threshold`: Number of failures before opening circuit
    /// - `success_threshold`: Number of successes in half-open to close circuit
    /// - `timeout`: Time to wait before trying half-open state
    pub fn new(failure_threshold: u32, success_threshold: u32, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(CircuitBreakerInner {
                state: CircuitState::Closed,
                failure_count: 0,
                success_count: 0,
                last_failure_time: None,
                last_state_change: Instant::now(),
            })),
            failure_threshold,
            success_threshold,
            timeout,
            reset_timeout: timeout,
        }
    }

    /// Create a circuit breaker for CLOB API calls
    pub fn for_clob_api() -> Self {
        Self::new(
            5,                          // Open after 5 failures
            2,                          // Close after 2 successes
            Duration::from_secs(30),    // Wait 30s before trying half-open
        )
    }

    /// Check if request is allowed
    pub async fn is_open(&self) -> bool {
        let inner = self.inner.read().await;
        match inner.state {
            CircuitState::Open => {
                // Check if timeout has passed, transition to half-open
                if inner.last_state_change.elapsed() >= self.reset_timeout {
                    drop(inner);
                    let mut inner = self.inner.write().await;
                    if inner.state == CircuitState::Open {
                        inner.state = CircuitState::HalfOpen;
                        inner.success_count = 0;
                        inner.last_state_change = Instant::now();
                        log::info!("Circuit breaker: Open -> HalfOpen (testing recovery)");
                    }
                    false
                } else {
                    true
                }
            }
            CircuitState::HalfOpen | CircuitState::Closed => false,
        }
    }

    /// Record a successful call
    pub async fn record_success(&self) {
        let mut inner = self.inner.write().await;
        match inner.state {
            CircuitState::Closed => {
                // Reset failure count on success
                inner.failure_count = 0;
            }
            CircuitState::HalfOpen => {
                inner.success_count += 1;
                if inner.success_count >= self.success_threshold {
                    inner.state = CircuitState::Closed;
                    inner.failure_count = 0;
                    inner.success_count = 0;
                    inner.last_state_change = Instant::now();
                    log::info!("Circuit breaker: HalfOpen -> Closed (service recovered)");
                }
            }
            CircuitState::Open => {
                // Should not happen, but handle gracefully
            }
        }
    }

    /// Record a failed call
    pub async fn record_failure(&self) {
        let mut inner = self.inner.write().await;
        inner.last_failure_time = Some(Instant::now());
        
        match inner.state {
            CircuitState::Closed => {
                inner.failure_count += 1;
                if inner.failure_count >= self.failure_threshold {
                    inner.state = CircuitState::Open;
                    inner.last_state_change = Instant::now();
                    log::warn!(
                        "Circuit breaker: Closed -> Open ({} failures exceeded threshold)",
                        inner.failure_count
                    );
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open immediately opens circuit
                inner.state = CircuitState::Open;
                inner.failure_count = self.failure_threshold;
                inner.last_state_change = Instant::now();
                log::warn!("Circuit breaker: HalfOpen -> Open (failure during test)");
            }
            CircuitState::Open => {
                // Already open, just update failure time
            }
        }
    }

    /// Get current state (for monitoring)
    pub async fn state(&self) -> String {
        let inner = self.inner.read().await;
        match inner.state {
            CircuitState::Closed => "closed".to_string(),
            CircuitState::Open => "open".to_string(),
            CircuitState::HalfOpen => "half-open".to_string(),
        }
    }

    /// Get failure count (for monitoring)
    pub async fn failure_count(&self) -> u32 {
        let inner = self.inner.read().await;
        inner.failure_count
    }
}
