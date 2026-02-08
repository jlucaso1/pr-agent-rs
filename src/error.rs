use thiserror::Error;

#[derive(Error, Debug)]
pub enum PrAgentError {
    #[error("Configuration error: {0}")]
    Config(Box<figment::Error>),

    #[error("Git provider error: {0}")]
    GitProvider(String),

    #[error("AI handler error: {0}")]
    AiHandler(String),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Template rendering error: {0}")]
    Template(#[from] minijinja::Error),

    #[allow(dead_code)]
    #[error("YAML parsing error: {0}")]
    YamlParse(String),

    #[allow(dead_code)]
    #[error("Token budget exceeded: needed {needed}, available {available}")]
    TokenBudget { needed: u32, available: u32 },

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
    #[allow(dead_code)]
    pub fn is_retryable(&self) -> bool {
        match self {
            PrAgentError::Http(e) => {
                e.is_timeout() || e.is_connect() || e.status().is_none_or(|s| s.is_server_error())
            }
            PrAgentError::AiHandler(_) | PrAgentError::RateLimited { .. } => true,
            _ => false,
        }
    }
}
