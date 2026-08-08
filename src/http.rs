//! Shared HTTP transport for the two public Polymarket APIs: a client-side throttle, a
//! jittered retry policy for transient failures, and error messages that say what was
//! being fetched and from where.
//!
//! Both clients return the raw response body so that response *parsing* stays a pure
//! function, testable against fixtures without network access.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::Rng;
use serde::Serialize;
use thiserror::Error;

use crate::config::ApiConfig;

const BASE_BACKOFF_MS: u64 = 250;
const MAX_BACKOFF_MS: u64 = 8_000;
/// Response bodies are logged/echoed on error; keep the excerpt short.
const ERROR_BODY_EXCERPT: usize = 400;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("could not build the HTTP client: {source}")]
    Build {
        #[source]
        source: reqwest::Error,
    },
    #[error("{method} {url} failed after {attempts} attempt(s): {source}")]
    Transport {
        method: &'static str,
        url: String,
        attempts: u32,
        #[source]
        source: reqwest::Error,
    },
    #[error("{method} {url} returned HTTP {status}: {body}")]
    Status {
        method: &'static str,
        url: String,
        status: u16,
        body: String,
    },
    #[error("could not parse the response from {url}: {source}")]
    Decode {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    /// The endpoint answered and the body was valid JSON, but nothing usable came out of
    /// it. Polymarket always has active events, so this is a response-shape mismatch until
    /// proven otherwise — and it must say so in one line, because the daemon's only visible
    /// symptom is an endless discovery retry. (2026-07-30: the live `/events/keyset`
    /// envelope key turned out to be `events`, which this parser did not accept.)
    #[error("discovery parsed {events} events from {url} — response shape mismatch? ({detail})")]
    EmptyDiscovery {
        url: String,
        events: usize,
        detail: String,
    },
}

pub struct HttpClient {
    client: reqwest::Client,
    min_interval: Duration,
    max_retries: u32,
    /// Earliest instant at which the next request may leave. Guarded by a std mutex that
    /// is never held across an await.
    next_allowed: Mutex<Instant>,
}

impl HttpClient {
    pub fn new(cfg: &ApiConfig) -> Result<Self, ApiError> {
        let client = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            // Two ceilings, because they bound two different failures (M7.1). `timeout`
            // covers the whole request; `connect_timeout` covers DNS + TCP + TLS on its
            // own, which is the phase that stretched to minutes during a live DNS outage
            // and dragged the daemon's loop tick with it.
            .timeout(Duration::from_secs(cfg.request_timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            .build()
            .map_err(|source| ApiError::Build { source })?;
        Ok(Self {
            client,
            min_interval: Duration::from_millis(cfg.min_request_interval_ms),
            max_retries: cfg.max_retries,
            next_allowed: Mutex::new(Instant::now()),
        })
    }

    /// Reserve the next slot in the request budget and sleep until it opens.
    async fn throttle(&self) {
        if self.min_interval.is_zero() {
            return;
        }
        let wait = {
            // A poisoned mutex here only means another task panicked mid-reservation;
            // throttling is best-effort, so recover rather than propagate.
            let mut slot = self
                .next_allowed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            let start = (*slot).max(now);
            *slot = start + self.min_interval;
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    pub async fn get_json(&self, url: &str, query: &[(&str, String)]) -> Result<String, ApiError> {
        self.send("GET", url, |c| c.get(url).query(query)).await
    }

    pub async fn post_json<B: Serialize>(&self, url: &str, body: &B) -> Result<String, ApiError> {
        self.send("POST", url, |c| c.post(url).json(body)).await
    }

    async fn send<F>(&self, method: &'static str, url: &str, build: F) -> Result<String, ApiError>
    where
        F: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            self.throttle().await;

            let result = build(&self.client).send().await;
            match result {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return response.text().await.map_err(|source| ApiError::Transport {
                            method,
                            url: url.to_string(),
                            attempts: attempt,
                            source,
                        });
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if !retryable || attempt > self.max_retries {
                        let body = response.text().await.unwrap_or_default();
                        return Err(ApiError::Status {
                            method,
                            url: url.to_string(),
                            status: status.as_u16(),
                            body: excerpt(&body),
                        });
                    }
                    tracing::warn!(
                        %url, status = status.as_u16(), attempt,
                        "retryable HTTP status, backing off"
                    );
                }
                Err(source) => {
                    if attempt > self.max_retries {
                        return Err(ApiError::Transport {
                            method,
                            url: url.to_string(),
                            attempts: attempt,
                            source,
                        });
                    }
                    tracing::warn!(%url, attempt, error = %source, "request failed, retrying");
                }
            }

            tokio::time::sleep(backoff(attempt)).await;
        }
    }
}

/// Exponential backoff with full jitter on the increment.
fn backoff(attempt: u32) -> Duration {
    let exp = BASE_BACKOFF_MS
        .saturating_mul(1u64 << attempt.min(6))
        .min(MAX_BACKOFF_MS);
    let jitter = {
        let mut rng = rand::thread_rng();
        rng.gen_range(0..=exp / 2)
    };
    Duration::from_millis(exp + jitter)
}

fn excerpt(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= ERROR_BODY_EXCERPT {
        return trimmed.to_string();
    }
    let cut = trimmed
        .char_indices()
        .take_while(|(i, _)| *i < ERROR_BODY_EXCERPT)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}…", &trimmed[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_stays_bounded() {
        let a = backoff(1);
        let b = backoff(5);
        assert!(a >= Duration::from_millis(500));
        assert!(b <= Duration::from_millis(MAX_BACKOFF_MS + MAX_BACKOFF_MS / 2));
        assert!(b > a);
    }

    #[test]
    fn excerpt_truncates_without_splitting_chars() {
        let long = "é".repeat(500);
        let e = excerpt(&long);
        assert!(e.ends_with('…'));
        assert!(e.chars().count() < 500);
        assert_eq!(excerpt("  short  "), "short");
    }
}
