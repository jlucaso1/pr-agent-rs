use thiserror::Error;

#[derive(Error, Debug)]
pub enum PrAgentError {
    #[error("Configuration error: {0}")]
    Config(Box<figment::Error>),

    #[error("Git provider error: {0}")]
    GitProvider(String),

    #[error("AI handler error: {0}")]
    AiHandler(String),

    /// A non-retryable AI client error (HTTP 4xx other than 429), e.g. bad
    /// request or auth failure. Kept distinct from [`Self::AiHandler`] so the
    /// retry loop doesn't waste attempts on it.
    #[error("AI client error: {0}")]
    AiClientError(String),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Template rendering error: {0}")]
    Template(#[from] minijinja::Error),

    #[error("Unsupported operation: {0}")]
    Unsupported(String),

    #[error("Rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML deserialization error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("{0}")]
    Other(String),
}

impl From<figment::Error> for PrAgentError {
    fn from(err: figment::Error) -> Self {
        PrAgentError::Config(Box::new(err))
    }
}

impl PrAgentError {
    /// Whether an operation that produced this error is worth retrying.
    ///
    /// Network/transport failures and server (5xx) errors are transient;
    /// client (4xx) errors and rate limits are not (the latter is handled with
    /// an explicit backoff at the call site, not by blind retry).
    pub fn is_retryable(&self) -> bool {
        match self {
            PrAgentError::Http(e) => {
                e.is_timeout() || e.is_connect() || e.status().is_none_or(|s| s.is_server_error())
            }
            // AiHandler is used for transient/5xx AI failures.
            PrAgentError::AiHandler(_) => true,
            // Client (4xx) errors and rate limits are not blindly retried.
            PrAgentError::AiClientError(_) | PrAgentError::RateLimited { .. } => false,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_retryable() {
        // S7: transient/5xx is retryable; client errors and rate limits are not.
        assert!(PrAgentError::AiHandler("server error".into()).is_retryable());
        assert!(!PrAgentError::AiClientError("bad request".into()).is_retryable());
        assert!(
            !PrAgentError::RateLimited {
                retry_after_secs: 1
            }
            .is_retryable()
        );
        assert!(!PrAgentError::Other("misc".into()).is_retryable());
    }
}
