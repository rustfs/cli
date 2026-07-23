//! Retry mechanism with exponential backoff and jitter
//!
//! Implements retry logic for transient failures like network errors and 503 responses.

use std::time::Duration;

use crate::alias::RetryConfig;
use crate::error::{Error, Result};

/// Retry a fallible async operation with exponential backoff
///
/// # Arguments
/// * `config` - Retry configuration
/// * `operation` - Async closure that returns `Result<T>`
/// * `is_retryable` - Closure that determines if an error should trigger retry
///
/// # Example
/// ```ignore
/// let result = retry_with_backoff(
///     &config,
///     || async { client.get_object(path).await },
///     |e| e.is_retryable(),
/// ).await;
/// ```
pub async fn retry_with_backoff<T, F, Fut, R>(
    config: &RetryConfig,
    mut operation: F,
    is_retryable: R,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
    R: Fn(&Error) -> bool,
{
    let mut attempt = 0;

    loop {
        attempt += 1;

        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if attempt >= config.max_attempts || !is_retryable(&e) {
                    return Err(e);
                }

                let backoff = calculate_backoff(config, attempt);
                tracing::debug!(
                    attempt = attempt,
                    backoff_ms = backoff.as_millis(),
                    error = %e,
                    "Retrying after transient error"
                );

                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// Calculate backoff duration with jitter
pub(crate) fn calculate_backoff(config: &RetryConfig, attempt: u32) -> Duration {
    // Exponential backoff: initial * 2^(attempt-1)
    let base_ms = config.initial_backoff_ms * (1u64 << (attempt - 1).min(10));
    let capped_ms = base_ms.min(config.max_backoff_ms);

    // Add jitter: random value between 0 and backoff
    let jitter_ms = rand_jitter(capped_ms);
    Duration::from_millis(capped_ms + jitter_ms)
}

/// Generate pseudo-random jitter without external RNG dependency
fn rand_jitter(max: u64) -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    nanos % max.max(1)
}

/// Check if an error is retryable (transient)
pub fn is_retryable_error(error: &Error) -> bool {
    match error {
        Error::Network(msg) => {
            // Retryable network errors
            let msg_lower = msg.to_lowercase();
            msg_lower.contains("timeout")
                || msg_lower.contains("connection reset")
                || msg_lower.contains("connection refused")
                || msg_lower.contains("503")
                || msg_lower.contains("service unavailable")
                || msg_lower.contains("too many requests")
                || msg_lower.contains("429")
                || msg_lower.contains("request rate")
                || msg_lower.contains("slow down")
        }
        Error::Io(e) => {
            // Retryable I/O errors
            matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            )
        }
        // Non-retryable errors
        Error::Auth(_)
        | Error::NotFound(_)
        | Error::AliasNotFound(_)
        | Error::Conflict(_)
        | Error::InvalidPath(_)
        | Error::Config(_)
        | Error::UnsupportedFeature(_) => false,
        // General errors might be retryable
        Error::General(msg) => {
            let msg_lower = msg.to_lowercase();
            msg_lower.contains("timeout") || msg_lower.contains("temporary")
        }
        _ => false,
    }
}

/// Retry configuration builder for easy customization
#[derive(Debug, Clone)]
pub struct RetryBuilder {
    max_attempts: u32,
    initial_backoff_ms: u64,
    max_backoff_ms: u64,
}

impl RetryBuilder {
    pub fn new() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 10000,
        }
    }

    pub fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    pub fn initial_backoff_ms(mut self, ms: u64) -> Self {
        self.initial_backoff_ms = ms;
        self
    }

    pub fn max_backoff_ms(mut self, ms: u64) -> Self {
        self.max_backoff_ms = ms;
        self
    }

    pub fn build(self) -> RetryConfig {
        RetryConfig {
            max_attempts: self.max_attempts,
            initial_backoff_ms: self.initial_backoff_ms,
            max_backoff_ms: self.max_backoff_ms,
        }
    }
}

impl Default for RetryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_backoff() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 10000,
        };

        // First attempt should have base backoff
        let b1 = calculate_backoff(&config, 1);
        assert!(b1.as_millis() >= 100 && b1.as_millis() < 200);

        // Second attempt doubles
        let b2 = calculate_backoff(&config, 2);
        assert!(b2.as_millis() >= 200 && b2.as_millis() < 400);

        // Third attempt quadruples
        let b3 = calculate_backoff(&config, 3);
        assert!(b3.as_millis() >= 400 && b3.as_millis() < 800);
    }

    #[test]
    fn test_backoff_cap() {
        let config = RetryConfig {
            max_attempts: 10,
            initial_backoff_ms: 1000,
            max_backoff_ms: 5000,
        };

        // Even with many attempts, should not exceed max
        let b = calculate_backoff(&config, 10);
        assert!(b.as_millis() <= 10000); // max + jitter
    }

    #[test]
    fn test_is_retryable_error() {
        // Network errors are retryable
        assert!(is_retryable_error(&Error::Network(
            "connection timeout".to_string()
        )));
        assert!(is_retryable_error(&Error::Network(
            "503 Service Unavailable".to_string()
        )));
        assert!(is_retryable_error(&Error::Network(
            "429 Too Many Requests".to_string()
        )));

        // Auth errors are not retryable
        assert!(!is_retryable_error(&Error::Auth(
            "access denied".to_string()
        )));

        // Not found is not retryable
        assert!(!is_retryable_error(&Error::NotFound(
            "object not found".to_string()
        )));
    }

    #[test]
    fn test_retry_builder() {
        let config = RetryBuilder::new()
            .max_attempts(5)
            .initial_backoff_ms(200)
            .max_backoff_ms(20000)
            .build();

        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.initial_backoff_ms, 200);
        assert_eq!(config.max_backoff_ms, 20000);
    }

    #[tokio::test]
    async fn test_retry_success_first_attempt() {
        let config = RetryConfig::default();
        let mut calls = 0;

        let result = retry_with_backoff(
            &config,
            || {
                calls += 1;
                async { Ok::<_, Error>(42) }
            },
            |_| true,
        )
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn test_retry_success_after_failure() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_backoff_ms: 1, // Fast for tests
            max_backoff_ms: 10,
        };
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count_clone = call_count.clone();

        let result = retry_with_backoff(
            &config,
            || {
                let cc = call_count_clone.clone();
                async move {
                    let count = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if count < 2 {
                        Err(Error::Network("timeout".to_string()))
                    } else {
                        Ok(42)
                    }
                }
            },
            is_retryable_error,
        )
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_exhausted() {
        let config = RetryConfig {
            max_attempts: 2,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
        };
        let mut calls = 0;

        let result: Result<()> = retry_with_backoff(
            &config,
            || {
                calls += 1;
                async { Err(Error::Network("always fails".to_string())) }
            },
            |_| true,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn test_retry_non_retryable() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
        };
        let mut calls = 0;

        let result: Result<()> = retry_with_backoff(
            &config,
            || {
                calls += 1;
                async { Err(Error::NotFound("not found".to_string())) }
            },
            is_retryable_error,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(calls, 1); // Should not retry
    }
}
