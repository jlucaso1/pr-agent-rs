pub mod openai;
pub mod token;
pub mod types;

use crate::error::PrAgentError;
use async_trait::async_trait;
use types::ChatResponse;
pub use types::ModelCapabilities;

/// Trait for AI/LLM provider handlers.
///
/// Implementors handle a single provider family (e.g. OpenAI-compatible endpoints).
/// Object-safe for dynamic dispatch via `Arc<dyn AiHandler>`.
#[async_trait]
pub trait AiHandler: Send + Sync {
    /// Unique deployment identifier (e.g. Azure deployment name). May be empty.
    #[allow(dead_code)]
    fn deployment_id(&self) -> &str;

    /// Query capabilities for a specific model (system message support, temperature, etc.).
    fn capabilities(&self, model: &str) -> ModelCapabilities;

    /// Send a chat completion request.
    async fn chat_completion(
        &self,
        model: &str,
        system: &str,
        user: &str,
        temperature: Option<f32>,
        image_urls: Option<&[String]>,
    ) -> Result<ChatResponse, PrAgentError>;
}

/// Try the primary model first, then each fallback in order.
///
/// Each model attempt uses the handler's built-in retry logic (exponential backoff).
/// If all models fail, returns the last error.
pub async fn chat_completion_with_fallback(
    handler: &dyn AiHandler,
    primary_model: &str,
    fallback_models: &[String],
    system: &str,
    user: &str,
    temperature: Option<f32>,
    image_urls: Option<&[String]>,
) -> Result<ChatResponse, PrAgentError> {
    // Try primary model
    match handler
        .chat_completion(primary_model, system, user, temperature, image_urls)
        .await
    {
        Ok(resp) => return Ok(resp),
        Err(e) => {
            if fallback_models.is_empty() {
                return Err(e);
            }
            tracing::warn!(
                model = primary_model,
                error = %e,
                "primary model failed, trying fallbacks"
            );
        }
    }

    // Try each fallback sequentially
    let mut last_err = PrAgentError::AiHandler("no fallback models configured".into());
    for (i, fallback) in fallback_models.iter().enumerate() {
        tracing::info!(
            model = fallback.as_str(),
            attempt = i + 2,
            "trying fallback model"
        );
        match handler
            .chat_completion(fallback, system, user, temperature, image_urls)
            .await
        {
            Ok(resp) => {
                tracing::info!(model = fallback.as_str(), "fallback model succeeded");
                return Ok(resp);
            }
            Err(e) => {
                tracing::warn!(
                    model = fallback.as_str(),
                    attempt = i + 2,
                    error = %e,
                    "fallback model failed"
                );
                last_err = e;
            }
        }
    }

    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use types::{FinishReason, Usage};

    /// Mock AI handler that fails for specific models and tracks all attempted models.
    struct FallbackTestHandler {
        /// Models that should fail when called.
        failing_models: HashSet<String>,
        /// Record of which models were attempted, in order.
        attempted_models: Mutex<Vec<String>>,
    }

    impl FallbackTestHandler {
        fn new(failing: &[&str]) -> Self {
            Self {
                failing_models: failing.iter().map(|s| s.to_string()).collect(),
                attempted_models: Mutex::new(Vec::new()),
            }
        }

        fn attempted(&self) -> Vec<String> {
            self.attempted_models.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AiHandler for FallbackTestHandler {
        fn deployment_id(&self) -> &str {
            "test"
        }
        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn chat_completion(
            &self,
            model: &str,
            _system: &str,
            _user: &str,
            _temperature: Option<f32>,
            _image_urls: Option<&[String]>,
        ) -> Result<ChatResponse, PrAgentError> {
            self.attempted_models
                .lock()
                .unwrap()
                .push(model.to_string());
            if self.failing_models.contains(model) {
                Err(PrAgentError::AiHandler(format!(
                    "model {model} unavailable"
                )))
            } else {
                Ok(ChatResponse {
                    content: format!("response from {model}"),
                    finish_reason: FinishReason::Stop,
                    usage: Some(Usage {
                        prompt_tokens: 10,
                        completion_tokens: 20,
                        total_tokens: 30,
                    }),
                })
            }
        }
    }

    #[tokio::test]
    async fn test_fallback_primary_succeeds_no_fallback_tried() {
        let handler = FallbackTestHandler::new(&[]);
        let fallbacks = vec!["fallback-1".into()];
        let resp = chat_completion_with_fallback(
            &handler, "primary", &fallbacks, "sys", "usr", None, None,
        )
        .await
        .unwrap();

        assert_eq!(resp.content, "response from primary");
        assert_eq!(handler.attempted(), vec!["primary"]);
    }

    #[tokio::test]
    async fn test_fallback_primary_fails_fallback_succeeds() {
        let handler = FallbackTestHandler::new(&["primary"]);
        let fallbacks = vec!["fallback-1".into()];
        let resp = chat_completion_with_fallback(
            &handler, "primary", &fallbacks, "sys", "usr", None, None,
        )
        .await
        .unwrap();

        assert_eq!(resp.content, "response from fallback-1");
        assert_eq!(handler.attempted(), vec!["primary", "fallback-1"]);
    }

    #[tokio::test]
    async fn test_fallback_first_fallback_fails_second_succeeds() {
        let handler = FallbackTestHandler::new(&["primary", "fallback-1"]);
        let fallbacks = vec!["fallback-1".into(), "fallback-2".into()];
        let resp = chat_completion_with_fallback(
            &handler, "primary", &fallbacks, "sys", "usr", None, None,
        )
        .await
        .unwrap();

        assert_eq!(resp.content, "response from fallback-2");
        assert_eq!(
            handler.attempted(),
            vec!["primary", "fallback-1", "fallback-2"]
        );
    }

    #[tokio::test]
    async fn test_fallback_all_models_fail_returns_last_error() {
        let handler = FallbackTestHandler::new(&["primary", "fallback-1", "fallback-2"]);
        let fallbacks = vec!["fallback-1".into(), "fallback-2".into()];
        let err = chat_completion_with_fallback(
            &handler, "primary", &fallbacks, "sys", "usr", None, None,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("fallback-2"),
            "should return last model's error, got: {err}"
        );
        assert_eq!(
            handler.attempted(),
            vec!["primary", "fallback-1", "fallback-2"]
        );
    }

    #[tokio::test]
    async fn test_fallback_no_fallbacks_returns_primary_error() {
        let handler = FallbackTestHandler::new(&["primary"]);
        let fallbacks: Vec<String> = vec![];
        let err = chat_completion_with_fallback(
            &handler, "primary", &fallbacks, "sys", "usr", None, None,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("primary"),
            "should return primary error, got: {err}"
        );
        assert_eq!(handler.attempted(), vec!["primary"]);
    }
}
