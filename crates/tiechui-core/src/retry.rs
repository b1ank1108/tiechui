use std::time::Duration;

use tiechui_providers::{ErrorKind, ProviderError};

/// `retries` counts attempts already consumed by this model request, not the turn.
pub trait RetryPolicy: Send + Sync {
    fn delay(&self, error: &ProviderError, retries: u32) -> Option<Duration>;
    fn max_retries(&self) -> Option<u32> {
        None
    }
}

pub struct BoundedRetry {
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for BoundedRetry {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy for BoundedRetry {
    fn max_retries(&self) -> Option<u32> {
        Some(self.max_retries)
    }
    fn delay(&self, error: &ProviderError, retries: u32) -> Option<Duration> {
        if retries >= self.max_retries
            || error
                .retry_after
                .is_some_and(|delay| delay > self.max_delay)
            || !matches!(
                error.kind,
                ErrorKind::RateLimit
                    | ErrorKind::Overloaded
                    | ErrorKind::TransientNetwork
                    | ErrorKind::AmbiguousRequest
            )
        {
            return None;
        }
        if error.kind == ErrorKind::AmbiguousRequest {
            return Some(Duration::from_secs(2).max(error.retry_after.unwrap_or_default()));
        }
        let backoff = self
            .initial_delay
            .saturating_mul(2u32.saturating_pow(retries))
            .min(self.max_delay);
        Some(backoff.max(error.retry_after.unwrap_or_default()))
    }
}
