use serde::{Deserialize, Serialize};

/// Response from an AI chat completion call.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    #[default]
    Unknown,
}

impl From<&str> for FinishReason {
    fn from(s: &str) -> Self {
        match s {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "content_filter" => Self::ContentFilter,
            "tool_calls" => Self::ToolCalls,
            _ => Self::Unknown,
        }
    }
}

/// Token usage information returned by the API.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Per-model capability flags. The handler consults these before building the
/// HTTP request body, centralizing model-specific quirks in one place instead
/// of scattered if/else checks.
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    pub supports_system_message: bool,
    pub supports_temperature: bool,
    #[allow(dead_code)]
    pub supports_images: bool,
    #[allow(dead_code)]
    pub requires_streaming: bool,
    pub reasoning_effort: Option<String>,
    #[allow(dead_code)]
    pub max_tokens: u32,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_system_message: true,
            supports_temperature: true,
            supports_images: false,
            requires_streaming: false,
            reasoning_effort: None,
            max_tokens: 32_000,
        }
    }
}
