//! Retry with exponential backoff (US-016).

use anyhow::Result;
use std::time::Duration;
use tracing::warn;

/// Classify whether an error is retryable.
#[allow(dead_code)]
pub fn is_retryable_http_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

pub fn is_retryable_error(err_msg: &str) -> bool {
    let lower = err_msg.to_lowercase();
    lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("dns error")
        || lower.contains("temporarily unavailable")
        || lower.contains("broken pipe")
}

#[allow(dead_code)]
pub fn is_non_retryable_http_status(status: u16) -> bool {
    matches!(status, 400 | 401 | 403 | 404)
}

/// Retry an async operation with exponential backoff.
///
/// - `max_retries`: maximum number of attempts (total, not retries)
/// - `backoff_base_secs`: base delay multiplier (delay = base^attempt seconds)
/// - `label`: name for logging
pub async fn retry_async<F, Fut, T>(
    max_retries: u32,
    backoff_base_secs: u64,
    label: &str,
    f: F,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err = None;
    for attempt in 0..max_retries {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                let err_msg = format!("{:#}", e);
                if !is_retryable_error(&err_msg) && attempt > 0 {
                    // Not retryable — fail immediately
                    return Err(e);
                }
                last_err = Some(e);
                if attempt + 1 < max_retries {
                    let delay = backoff_base_secs.pow(attempt);
                    warn!(
                        "{} failed (attempt {}/{}), retrying in {}s: {}",
                        label,
                        attempt + 1,
                        max_retries,
                        delay,
                        err_msg
                    );
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
    }
    Err(last_err.unwrap())
}

/// Retry a synchronous operation with exponential backoff.
pub fn retry_sync<F, T>(
    max_retries: u32,
    backoff_base_secs: u64,
    label: &str,
    f: F,
) -> Result<T>
where
    F: Fn() -> Result<T>,
{
    let mut last_err = None;
    for attempt in 0..max_retries {
        match f() {
            Ok(val) => return Ok(val),
            Err(e) => {
                let err_msg = format!("{:#}", e);
                if !is_retryable_error(&err_msg) && attempt > 0 {
                    return Err(e);
                }
                last_err = Some(e);
                if attempt + 1 < max_retries {
                    let delay = backoff_base_secs.pow(attempt);
                    warn!(
                        "{} failed (attempt {}/{}), retrying in {}s: {}",
                        label,
                        attempt + 1,
                        max_retries,
                        delay,
                        err_msg
                    );
                    std::thread::sleep(Duration::from_secs(delay));
                }
            }
        }
    }
    Err(last_err.unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn test_is_retryable_http_status() {
        assert!(is_retryable_http_status(429));
        assert!(is_retryable_http_status(500));
        assert!(is_retryable_http_status(503));
        assert!(!is_retryable_http_status(200));
        assert!(!is_retryable_http_status(404));
    }

    #[test]
    fn test_is_non_retryable_http_status() {
        assert!(is_non_retryable_http_status(401));
        assert!(is_non_retryable_http_status(404));
        assert!(!is_non_retryable_http_status(500));
    }

    #[test]
    fn test_is_retryable_error() {
        assert!(is_retryable_error("connection refused (os error 61)"));
        assert!(is_retryable_error("DNS error: lookup failed"));
        assert!(is_retryable_error("request timed out"));
        assert!(!is_retryable_error("file not found"));
    }

    #[test]
    fn test_retry_sync_succeeds_first_try() {
        let result = retry_sync(3, 1, "test", || Ok(42));
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_retry_sync_succeeds_after_retries() {
        let counter = AtomicU32::new(0);
        let result = retry_sync(3, 1, "test", || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                anyhow::bail!("connection refused");
            }
            Ok(42)
        });
        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_retry_sync_exhausts_retries() {
        let result: Result<i32> = retry_sync(2, 1, "test", || {
            anyhow::bail!("connection refused");
        });
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_retry_async_succeeds() {
        let result = retry_async(3, 1, "test", || async { Ok(99) }).await;
        assert_eq!(result.unwrap(), 99);
    }

    #[tokio::test]
    async fn test_retry_async_fails() {
        let result: Result<i32> = retry_async(2, 1, "test", || async {
            anyhow::bail!("timed out");
        })
        .await;
        assert!(result.is_err());
    }
}
