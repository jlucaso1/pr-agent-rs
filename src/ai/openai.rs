use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use super::AiHandler;
use super::token::{
    get_max_tokens_with_fallback, is_no_temperature_model, is_user_message_only_model,
    supports_reasoning_effort,
};
use super::types::{ChatResponse, FinishReason, ModelCapabilities, Usage};
use crate::config::loader::get_settings;
use crate::error::PrAgentError;

/// Number of retry attempts for transient API errors (not rate limits).
const MODEL_RETRIES: u32 = 2;

/// OpenAI-compatible chat completions handler.
///
/// Works with: OpenAI, Azure OpenAI, Ollama, Groq, DeepSeek, DeepInfra,
/// xAI, OpenRouter, Mistral — any provider exposing the `/v1/chat/completions` API.
pub struct OpenAiCompatibleHandler {
    client: Client,
    base_url: String,
    api_key: String,
    #[allow(dead_code)]
    deployment_id: String,
}

impl OpenAiCompatibleHandler {
    /// Create a new handler from the current settings.
    pub fn from_settings() -> Result<Self, PrAgentError> {
        let settings = get_settings();
        let api_key = settings.openai.key.clone();
        let base_url = if settings.openai.api_base.is_empty() {
            "https://api.openai.com/v1".to_string()
        } else {
            settings.openai.api_base.clone()
        };
        let deployment_id = settings.openai.deployment_id.clone();
        let timeout_secs = settings.config.ai_timeout as u64;

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(PrAgentError::Http)?;

        Ok(Self {
            client,
            base_url,
            api_key,
            deployment_id,
        })
    }

    /// Build the request body for the chat completions API.
    fn build_request_body(
        &self,
        model: &str,
        system: &str,
        user: &str,
        temperature: Option<f32>,
        image_urls: Option<&[String]>,
    ) -> serde_json::Value {
        let settings = get_settings();
        let caps = self.capabilities(model);

        // Build messages
        let mut messages = Vec::new();

        let (sys_msg, usr_msg) = if !caps.supports_system_message {
            // Combine system + user into a single user message
            (String::new(), format!("{system}\n\n\n{user}"))
        } else {
            (system.to_string(), user.to_string())
        };

        if !sys_msg.is_empty() {
            messages.push(json!({"role": "system", "content": sys_msg}));
        }

        // Handle images if present
        let has_images = image_urls.is_some_and(|urls| !urls.is_empty());
        if has_images {
            // SAFETY: has_images is only true when image_urls.is_some_and(|urls| !urls.is_empty())
            let urls = match image_urls {
                Some(urls) => urls,
                None => &[],
            };
            let mut content = vec![json!({"type": "text", "text": usr_msg})];
            for url in urls {
                content.push(json!({
                    "type": "image_url",
                    "image_url": {"url": url}
                }));
            }
            messages.push(json!({"role": "user", "content": content}));
        } else {
            messages.push(json!({"role": "user", "content": usr_msg}));
        }

        let mut body = json!({
            "model": model,
            "messages": messages,
        });

        // Temperature
        if caps.supports_temperature && !settings.config.custom_reasoning_model {
            if let Some(temp) = temperature {
                body["temperature"] = json!(temp);
            } else {
                body["temperature"] = json!(settings.config.temperature);
            }
        }

        // Reasoning effort (for o3/o4-mini models)
        if caps.reasoning_effort.is_some() {
            // When reasoning effort is set, remove temperature
            if let Some(obj) = body.as_object_mut() {
                obj.remove("temperature");
            }
            body["reasoning_effort"] = json!(caps.reasoning_effort);
        }

        // Seed
        let seed = settings.config.seed;
        if seed >= 0 {
            body["seed"] = json!(seed);
        }

        body
    }

    /// Send a single request and parse the response. No retry logic here.
    async fn send_completion(
        &self,
        body: &serde_json::Value,
    ) -> Result<ChatResponse, PrAgentError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut req = self.client.post(&url).json(body);

        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req.send().await.map_err(PrAgentError::Http)?;

        if !resp.status().is_success() {
            let status = resp.status();

            if status.as_u16() == 429 {
                // Parse Retry-After header if available, default to 60s
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(60);
                return Err(PrAgentError::RateLimited {
                    retry_after_secs: retry_after,
                });
            }

            let body_text = resp.text().await.unwrap_or_default();
            return Err(PrAgentError::AiHandler(format!(
                "API returned {status}: {body_text}"
            )));
        }

        let api_resp: ApiResponse = resp.json().await.map_err(PrAgentError::Http)?;

        let choice = api_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| PrAgentError::AiHandler("no choices in response".into()))?;

        let content = choice.message.content.unwrap_or_default();

        let finish_reason = choice
            .finish_reason
            .as_deref()
            .map(FinishReason::from)
            .unwrap_or_default();

        let usage = api_resp.usage.map(|u| Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        });

        Ok(ChatResponse {
            content,
            finish_reason,
            usage,
        })
    }
}

#[async_trait]
impl AiHandler for OpenAiCompatibleHandler {
    fn deployment_id(&self) -> &str {
        &self.deployment_id
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let settings = get_settings();
        let max_tokens = get_max_tokens_with_fallback(model, settings.config.max_model_tokens);

        let reasoning_effort = supports_reasoning_effort(model)
            .then(|| &settings.config.reasoning_effort)
            .filter(|e| !e.is_empty())
            .cloned();

        ModelCapabilities {
            supports_system_message: !is_user_message_only_model(model),
            supports_temperature: !is_no_temperature_model(model),
            supports_images: true, // Most OpenAI-compatible models support vision
            requires_streaming: false,
            reasoning_effort,
            max_tokens,
        }
    }

    async fn chat_completion(
        &self,
        model: &str,
        system: &str,
        user: &str,
        temperature: Option<f32>,
        image_urls: Option<&[String]>,
    ) -> Result<ChatResponse, PrAgentError> {
        let body = self.build_request_body(model, system, user, temperature, image_urls);

        // Retry logic: retry on transient errors with exponential backoff
        let mut last_err = None;
        for attempt in 0..=MODEL_RETRIES {
            match self.send_completion(&body).await {
                Ok(resp) => return Ok(resp),
                Err(e @ PrAgentError::RateLimited { .. }) => {
                    // Don't retry rate limits — propagate immediately
                    return Err(e);
                }
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = MODEL_RETRIES + 1,
                        error = %e,
                        "AI request failed, retrying"
                    );
                    last_err = Some(e);

                    // Exponential backoff: 2s, 4s, 8s, ...
                    if attempt < MODEL_RETRIES {
                        let delay = std::time::Duration::from_secs(2u64.pow(attempt + 1));
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| PrAgentError::AiHandler("all retries exhausted".into())))
    }
}

// ── API response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
    usage: Option<ApiUsage>,
}

#[derive(Debug, Deserialize)]
struct ApiChoice {
    message: ApiMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

// ── Fallback model support ─────────────────────────────────────────

/// Run an async function with fallback models. If the primary model fails,
/// tries each fallback in order.
#[allow(dead_code)]
pub async fn retry_with_fallback_models<F, Fut>(
    handler: &OpenAiCompatibleHandler,
    primary_model: &str,
    fallback_models: &[String],
    f: F,
) -> Result<ChatResponse, PrAgentError>
where
    F: Fn(&OpenAiCompatibleHandler, &str) -> Fut,
    Fut: std::future::Future<Output = Result<ChatResponse, PrAgentError>>,
{
    // Try primary first
    match f(handler, primary_model).await {
        Ok(resp) => return Ok(resp),
        Err(e) => {
            if fallback_models.is_empty() {
                return Err(e);
            }
            tracing::warn!(model = primary_model, error = %e, "primary model failed, trying fallbacks");
        }
    }

    // Try fallbacks
    for (i, fallback) in fallback_models.iter().enumerate() {
        match f(handler, fallback).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                tracing::warn!(
                    model = fallback.as_str(),
                    attempt = i + 2,
                    error = %e,
                    "fallback model failed"
                );
                if i == fallback_models.len() - 1 {
                    return Err(e);
                }
            }
        }
    }

    Err(PrAgentError::AiHandler(
        "all models (primary + fallbacks) failed".into(),
    ))
}
