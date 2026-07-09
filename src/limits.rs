//! Guardrails: per-principal rate limiting and size checks (§11).

use crate::errors::{ApiError, ApiResult};
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use std::num::NonZeroU32;

/// Per-principal request rate limiter (§11). Structured so per-route limits can
/// be layered on later (needed by the proxy module, §16).
pub struct RateGuard {
    limiter: Option<RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>>,
    clock: DefaultClock,
}

impl RateGuard {
    /// `per_minute == 0` disables limiting.
    pub fn new(per_minute: u32) -> Self {
        let limiter = NonZeroU32::new(per_minute).map(|q| RateLimiter::keyed(Quota::per_minute(q)));
        Self {
            limiter,
            clock: DefaultClock::default(),
        }
    }

    /// Allow or reject a request for `key`. On rejection, returns the number of
    /// seconds the caller should wait (for the `Retry-After` header).
    pub fn check(&self, key: &str) -> Result<(), u64> {
        match &self.limiter {
            None => Ok(()),
            Some(l) => match l.check_key(&key.to_string()) {
                Ok(()) => Ok(()),
                Err(neg) => {
                    let wait = neg.wait_time_from(self.clock.now());
                    Err(wait.as_secs() + 1)
                }
            },
        }
    }
}

/// Reject documents larger than `max_document_bytes` (§11). `max == 0` disables.
pub fn check_document_size(doc: &serde_json::Value, max: u64) -> ApiResult<()> {
    if max == 0 {
        return Ok(());
    }
    let bytes = serde_json::to_vec(doc).map(|v| v.len()).unwrap_or(0) as u64;
    if bytes > max {
        return Err(ApiError::payload_too_large(
            "document exceeds max_document_bytes",
        ));
    }
    Ok(())
}
