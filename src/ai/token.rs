use tiktoken_rs::CoreBPE;

/// Output buffer subtracted from max tokens when deciding if content fits.
pub const OUTPUT_BUFFER_TOKENS_SOFT_THRESHOLD: u32 = 1500;
pub const OUTPUT_BUFFER_TOKENS_HARD_THRESHOLD: u32 = 1000;

// ── Encoder singleton ──────────────────────────────────────────────

/// Returns a shared tiktoken BPE encoder (o200k_base).
/// Initialized once on first call; subsequent calls are free.
fn encoder() -> &'static CoreBPE {
    tiktoken_rs::o200k_base_singleton()
}

// ── Token counting ─────────────────────────────────────────────────

/// Count the number of tokens in `text` using the o200k_base BPE encoder.
pub fn count_tokens(text: &str) -> u32 {
    encoder().encode_ordinary(text).len() as u32
}

/// Clip `text` to fit within `max_tokens`, using a character-ratio estimate.
///
/// Algorithm:
/// 1. Count tokens in `text`.
/// 2. If already within budget, return as-is.
/// 3. Estimate chars-per-token ratio, apply 0.9 safety factor.
/// 4. Truncate to estimated char count.
/// 5. Optionally append a truncation indicator.
pub fn clip_tokens(text: &str, max_tokens: u32, add_three_dots: bool) -> String {
    if text.is_empty() || max_tokens == 0 {
        return String::new();
    }

    let num_input_tokens = count_tokens(text);
    if num_input_tokens <= max_tokens {
        return text.to_string();
    }

    let chars_per_token = text.len() as f64 / num_input_tokens as f64;
    let factor = 0.9;
    let num_output_chars = (factor * chars_per_token * max_tokens as f64) as usize;

    // Truncate on a char boundary
    let truncated = if num_output_chars >= text.len() {
        text
    } else {
        // Find the nearest char boundary at or before num_output_chars
        let end = text
            .char_indices()
            .take_while(|(i, _)| *i < num_output_chars)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        &text[..end]
    };

    if add_three_dots {
        format!("{truncated}\n...(truncated)")
    } else {
        truncated.to_string()
    }
}

// ── Model name normalization ─────────────────────────────────────

/// Strip common provider prefixes (e.g. "openai/", "azure/") for model matching.
fn normalize_model_name(model: &str) -> &str {
    model
        .strip_prefix("openai/")
        .or_else(|| model.strip_prefix("azure/"))
        .unwrap_or(model)
}

// ── Model token limits ─────────────────────────────────────────────

/// Look up the maximum context tokens for a model name.
/// Falls back to `config.max_model_tokens` (default 32000) if unknown.
pub fn get_max_tokens(model: &str) -> u32 {
    let normalized = normalize_model_name(model);

    match normalized {
        // GPT-3.5
        "gpt-3.5-turbo"
        | "gpt-3.5-turbo-0125"
        | "gpt-3.5-turbo-1106"
        | "gpt-3.5-turbo-16k"
        | "gpt-3.5-turbo-16k-0613" => 16_000,
        "gpt-3.5-turbo-0613" => 4_000,

        // GPT-4
        "gpt-4" | "gpt-4-0613" => 8_000,
        "gpt-4-32k" => 32_000,
        "gpt-4-1106-preview"
        | "gpt-4-0125-preview"
        | "gpt-4-turbo-preview"
        | "gpt-4-turbo-2024-04-09"
        | "gpt-4-turbo" => 128_000,

        // GPT-4o
        "gpt-4o"
        | "gpt-4o-2024-05-13"
        | "gpt-4o-mini"
        | "gpt-4o-mini-2024-07-18"
        | "gpt-4o-2024-08-06"
        | "gpt-4o-2024-11-20" => 128_000,

        // GPT-4.5
        "gpt-4.5-preview" | "gpt-4.5-preview-2025-02-27" => 128_000,

        // GPT-4.1
        "gpt-4.1"
        | "gpt-4.1-2025-04-14"
        | "gpt-4.1-mini"
        | "gpt-4.1-mini-2025-04-14"
        | "gpt-4.1-nano"
        | "gpt-4.1-nano-2025-04-14" => 1_047_576,

        // GPT-5
        "gpt-5-nano" | "gpt-5-mini" | "gpt-5" | "gpt-5-2025-08-07" => 200_000,
        "gpt-5.1"
        | "gpt-5.1-2025-11-13"
        | "gpt-5.1-chat-latest"
        | "gpt-5.1-codex"
        | "gpt-5.1-codex-mini" => 200_000,
        "gpt-5.2" | "gpt-5.2-2025-12-11" | "gpt-5.2-codex" => 400_000,
        "gpt-5.2-chat-latest" => 128_000,

        // o-series reasoning models
        "o1-mini" | "o1-mini-2024-09-12" | "o1-preview" | "o1-preview-2024-09-12" => 128_000,
        "o1-2024-12-17" | "o1" | "o3-mini" | "o3-mini-2025-01-31" => 204_800,
        "o3" | "o3-2025-04-16" | "o4-mini" | "o4-mini-2025-04-16" => 200_000,

        // Claude (with anthropic/ prefix already stripped or not)
        s if s.contains("claude-opus-4-5") || s.contains("claude-sonnet-4-5") => 200_000,
        s if s.contains("claude-opus-4-1") => 200_000,
        s if s.contains("claude-opus-4") || s.contains("claude-sonnet-4") => 200_000,
        s if s.contains("claude-haiku-4-5") => 200_000,
        s if s.contains("claude-3-7-sonnet") => 200_000,
        s if s.contains("claude-3-5-sonnet") || s.contains("claude-3-5-haiku") => 100_000,
        s if s.contains("claude-3") => 100_000,
        s if s.contains("claude-2") || s.contains("claude-instant") => 100_000,

        // Gemini
        s if s.starts_with("gemini/") || s.contains("gemini-") => 1_048_576,

        // DeepSeek
        "deepseek/deepseek-chat" => 128_000,
        "deepseek/deepseek-reasoner" => 64_000,

        // Groq
        s if s.starts_with("groq/") => 128_000,

        // xAI
        s if s.starts_with("xai/") => 131_072,

        // Mistral
        "mistral/open-codestral-mamba" => 256_000,
        s if s.starts_with("mistral/") => 128_000,

        // Default fallback
        _ => 0, // caller should use config.max_model_tokens
    }
}

/// Look up the maximum context tokens for a model, falling back to the
/// configured `max_model_tokens` if the model is unknown.
pub fn get_max_tokens_with_fallback(model: &str, config_max: u32) -> u32 {
    let known = get_max_tokens(model);
    if known > 0 { known } else { config_max }
}

/// Check if a model does NOT support the temperature parameter.
pub fn is_no_temperature_model(model: &str) -> bool {
    let normalized = normalize_model_name(model);

    matches!(
        normalized,
        "deepseek/deepseek-reasoner"
            | "o1-mini"
            | "o1-mini-2024-09-12"
            | "o1-preview"
            | "o1-2024-12-17"
            | "o1"
            | "o3-mini"
            | "o3-mini-2025-01-31"
            | "o3"
            | "o3-2025-04-16"
            | "o4-mini"
            | "o4-mini-2025-04-16"
            | "gpt-5.1-codex"
            | "gpt-5.1-codex-mini"
            | "gpt-5.2-codex"
            | "gpt-5-mini"
    )
}

/// Check if a model requires combining system+user into a single user message.
pub fn is_user_message_only_model(model: &str) -> bool {
    let normalized = normalize_model_name(model);

    matches!(
        normalized,
        "deepseek/deepseek-reasoner" | "o1-mini" | "o1-mini-2024-09-12" | "o1-preview"
    )
}

/// Check if a model supports the `reasoning_effort` parameter.
pub fn supports_reasoning_effort(model: &str) -> bool {
    let normalized = normalize_model_name(model);

    matches!(
        normalized,
        "o3-mini"
            | "o3-mini-2025-01-31"
            | "o3"
            | "o3-2025-04-16"
            | "o4-mini"
            | "o4-mini-2025-04-16"
    )
}

/// Check if a model requires streaming (e.g. some API providers require it).
#[allow(dead_code)]
pub fn requires_streaming(model: &str) -> bool {
    let normalized = normalize_model_name(model);

    matches!(normalized, "openai/qwq-plus")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_tokens() {
        let tokens = count_tokens("Hello, world!");
        assert!(tokens > 0);
        assert!(tokens < 10);
    }

    #[test]
    fn test_count_tokens_empty() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn test_clip_tokens_within_budget() {
        let text = "Hello, world!";
        let result = clip_tokens(text, 100, true);
        assert_eq!(result, text);
    }

    #[test]
    fn test_clip_tokens_over_budget() {
        let text = "word ".repeat(1000);
        let result = clip_tokens(&text, 10, true);
        assert!(result.len() < text.len());
        assert!(result.ends_with("...(truncated)"));
    }

    #[test]
    fn test_clip_tokens_empty() {
        assert_eq!(clip_tokens("", 100, true), "");
        assert_eq!(clip_tokens("hello", 0, true), "");
    }

    #[test]
    fn test_get_max_tokens() {
        assert_eq!(get_max_tokens("gpt-4"), 8_000);
        assert_eq!(get_max_tokens("gpt-4o"), 128_000);
        assert_eq!(get_max_tokens("gpt-4.1"), 1_047_576);
        assert_eq!(get_max_tokens("gpt-5.2-2025-12-11"), 400_000);
        assert_eq!(get_max_tokens("o3-mini"), 204_800);
        assert_eq!(
            get_max_tokens("anthropic/claude-sonnet-4-5-20250929"),
            200_000
        );
        assert_eq!(get_max_tokens("gemini/gemini-2.5-pro"), 1_048_576);
        assert_eq!(get_max_tokens("deepseek/deepseek-chat"), 128_000);
        assert_eq!(get_max_tokens("unknown-model"), 0);
    }

    #[test]
    fn test_model_capabilities() {
        assert!(is_no_temperature_model("o3-mini"));
        assert!(!is_no_temperature_model("gpt-4o"));
        assert!(is_user_message_only_model("o1-mini"));
        assert!(!is_user_message_only_model("gpt-4o"));
        assert!(supports_reasoning_effort("o3-mini"));
        assert!(!supports_reasoning_effort("gpt-4o"));
    }
}
