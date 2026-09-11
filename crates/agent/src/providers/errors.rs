use std::time::{Duration, SystemTime};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Provider request timed out")]
    Timeout,
    #[error("Provider connection failed")]
    Connection,
    #[error("Provider returned HTTP {status}")]
    Http {
        status: u16,
        retry_after: Option<Duration>,
    },
    #[error("Invalid provider response: {0}")]
    Protocol(String),
    #[error("Provider response did not complete: {0:?}")]
    Incomplete(super::FinishReason),
}

impl ProviderError {
    pub fn stream(value: &serde_json::Value) -> Self {
        let code = value.get("code");
        let status = code
            .and_then(serde_json::Value::as_u64)
            .and_then(|code| u16::try_from(code).ok())
            .or_else(|| match code.and_then(serde_json::Value::as_str) {
                Some("rate_limit_exceeded") => Some(429),
                Some("server_error") => Some(500),
                _ => None,
            });
        status.map_or_else(
            || Self::Protocol("Provider reported a streaming error".into()),
            |status| Self::Http {
                status,
                retry_after: None,
            },
        )
    }
    pub fn http(response: &reqwest::Response) -> Self {
        Self::Http {
            status: response.status().as_u16(),
            retry_after: response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| retry_after(value, SystemTime::now())),
        }
    }

    pub fn normalize(error: anyhow::Error) -> anyhow::Error {
        if error.downcast_ref::<Self>().is_some() {
            return error;
        }
        if let Some(source) = error.downcast_ref::<reqwest::Error>() {
            if source.is_timeout() {
                return Self::Timeout.into();
            }
            if source.is_connect() || source.is_body() {
                return Self::Connection.into();
            }
            if let Some(status) = source.status() {
                return Self::Http {
                    status: status.as_u16(),
                    retry_after: None,
                }
                .into();
            }
        }
        Self::Protocol(error.to_string()).into()
    }

    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Connection
                | Self::Http {
                    status: 408 | 429 | 500 | 502 | 503 | 504,
                    ..
                }
        )
    }

    /// Respect Retry-After without sleeping unboundedly inside a request loop.
    /// A longer server delay is surfaced to the caller instead of retrying early.
    pub fn retry_delay(&self, attempt: u32, jitter_ms: u64) -> Option<Duration> {
        if !self.retryable() {
            return None;
        }
        let backoff = Duration::from_millis(
            500_u64
                .saturating_mul(
                    1_u64
                        .checked_shl(attempt.saturating_sub(1))
                        .unwrap_or(u64::MAX),
                )
                .min(30_000)
                + jitter_ms.min(250),
        );
        let delay = match self {
            Self::Http {
                retry_after: Some(delay),
                ..
            } => backoff.max(*delay),
            _ => backoff,
        };
        (delay <= Duration::from_mins(1)).then_some(delay)
    }
}

fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_uses_status_headers_and_bounded_jitter() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(retry_after("3", now), Some(Duration::from_secs(3)));
        assert_eq!(
            retry_after(&httpdate::fmt_http_date(now + Duration::from_secs(5)), now),
            Some(Duration::from_secs(5))
        );
        let rate_limit = ProviderError::Http {
            status: 429,
            retry_after: Some(Duration::from_secs(4)),
        };
        assert_eq!(rate_limit.retry_delay(1, 100), Some(Duration::from_secs(4)));
        assert!(
            ProviderError::Http {
                status: 429,
                retry_after: Some(Duration::from_mins(2))
            }
            .retry_delay(1, 0)
            .is_none()
        );
        assert!(!ProviderError::Protocol("error text contains 503 or timeout".into()).retryable());
        assert!(
            !ProviderError::Http {
                status: 401,
                retry_after: None
            }
            .retryable()
        );
    }
}
