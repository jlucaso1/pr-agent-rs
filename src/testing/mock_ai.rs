use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::ai::AiHandler;
use crate::ai::types::{ChatResponse, FinishReason, ModelCapabilities, Usage};
use crate::error::PrAgentError;

/// Mock AI handler that returns pre-configured responses in order.
///
/// Supports single-response (review/describe) and multi-response (improve's
/// suggestion + reflect passes) flows.
pub struct MockAiHandler {
    responses: Mutex<VecDeque<String>>,
    pub call_count: Mutex<usize>,
}

impl MockAiHandler {
    /// Create a mock that returns the same response for every call.
    pub fn new(response: impl Into<String>) -> Self {
        let mut q = VecDeque::new();
        q.push_back(response.into());
        Self {
            responses: Mutex::new(q),
            call_count: Mutex::new(0),
        }
    }

    /// Create a mock that returns responses in order (one per call).
    pub fn with_responses(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            call_count: Mutex::new(0),
        }
    }

    pub fn get_call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }
}

#[async_trait]
impl AiHandler for MockAiHandler {
    fn deployment_id(&self) -> &str {
        "mock"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn chat_completion(
        &self,
        _model: &str,
        _system: &str,
        _user: &str,
        _temperature: Option<f32>,
        _image_urls: Option<&[String]>,
    ) -> Result<ChatResponse, PrAgentError> {
        let mut count = self.call_count.lock().unwrap();
        *count += 1;

        let mut responses = self.responses.lock().unwrap();
        // If only one response left, clone it (reusable); otherwise pop front.
        let content = if responses.len() == 1 {
            responses.front().unwrap().clone()
        } else {
            responses
                .pop_front()
                .ok_or_else(|| PrAgentError::AiHandler("no more mock responses".into()))?
        };

        Ok(ChatResponse {
            content,
            finish_reason: FinishReason::Stop,
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 200,
                total_tokens: 300,
            }),
        })
    }
}
