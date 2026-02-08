pub mod openai;
pub mod token;
pub mod types;

use crate::error::PrAgentError;
use async_trait::async_trait;
use types::{ChatResponse, ModelCapabilities};

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
