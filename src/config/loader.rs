use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use figment::Figment;
use figment::providers::{Env, Format, Toml};

use crate::config::types::Settings;
use crate::error::PrAgentError;

// Embedded default TOML files.
// This makes the binary self-contained while keeping retrocompatibility.
static CONFIGURATION_TOML: &str = include_str!("../../settings/configuration.toml");
static IGNORE_TOML: &str = include_str!("../../settings/ignore.toml");
static LANGUAGE_EXTENSIONS_TOML: &str = include_str!("../../settings/language_extensions.toml");
static CUSTOM_LABELS_TOML: &str = include_str!("../../settings/custom_labels.toml");

// Prompt template TOML files
static PR_REVIEWER_PROMPTS: &str = include_str!("../../settings/pr_reviewer_prompts.toml");
static PR_DESCRIPTION_PROMPTS: &str = include_str!("../../settings/pr_description_prompts.toml");
static PR_CODE_SUGGESTIONS_PROMPTS: &str =
    include_str!("../../settings/code_suggestions/pr_code_suggestions_prompts.toml");
static PR_CODE_SUGGESTIONS_NOT_DECOUPLED: &str =
    include_str!("../../settings/code_suggestions/pr_code_suggestions_prompts_not_decoupled.toml");
static PR_CODE_SUGGESTIONS_REFLECT: &str =
    include_str!("../../settings/code_suggestions/pr_code_suggestions_reflect_prompts.toml");
static PR_QUESTIONS_PROMPTS: &str = include_str!("../../settings/pr_questions_prompts.toml");
static PR_LINE_QUESTIONS_PROMPTS: &str =
    include_str!("../../settings/pr_line_questions_prompts.toml");
static PR_UPDATE_CHANGELOG_PROMPTS: &str =
    include_str!("../../settings/pr_update_changelog_prompts.toml");
static PR_INFORMATION_FROM_USER: &str =
    include_str!("../../settings/pr_information_from_user_prompts.toml");
static PR_HELP_PROMPTS: &str = include_str!("../../settings/pr_help_prompts.toml");
static PR_HELP_DOCS_PROMPTS: &str = include_str!("../../settings/pr_help_docs_prompts.toml");
static PR_HELP_DOCS_HEADINGS: &str =
    include_str!("../../settings/pr_help_docs_headings_prompts.toml");
static PR_EVALUATE_PROMPT_RESPONSE: &str =
    include_str!("../../settings/pr_evaluate_prompt_response.toml");

/// Global settings, re-settable (e.g. after loading repo-level config).
static GLOBAL_SETTINGS: RwLock<Option<Arc<Settings>>> = RwLock::new(None);

tokio::task_local! {
    /// Per-request settings override (used in webhook server mode).
    static REQUEST_SETTINGS: Arc<Settings>;
}

/// Get the current settings.
///
/// In webhook mode, returns the per-request override if set.
/// Otherwise falls back to the global singleton.
pub fn get_settings() -> Arc<Settings> {
    REQUEST_SETTINGS.try_with(Arc::clone).unwrap_or_else(|_| {
        let guard = GLOBAL_SETTINGS.read().unwrap_or_else(|poisoned| {
            tracing::error!("settings RwLock poisoned, recovering inner value");
            poisoned.into_inner()
        });
        match guard.as_ref() {
            Some(s) => s.clone(),
            None => {
                tracing::error!(
                    "get_settings() called before init_settings() — loading defaults as fallback"
                );
                let fallback = Arc::new(load_settings(&HashMap::new(), None, None).unwrap_or_else(|e| {
                    tracing::error!(error = %e, "failed to load fallback settings, using Default");
                    Settings::default()
                }));
                drop(guard);
                let mut write_guard = GLOBAL_SETTINGS
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *write_guard = Some(fallback.clone());
                fallback
            }
        }
    })
}

/// Initialize (or re-initialize) global settings.
///
/// Can be called multiple times — e.g. first for bootstrap, then again
/// after loading repo-level `.pr_agent.toml`.
pub fn init_settings(
    cli_overrides: &HashMap<String, String>,
    global_settings_toml: Option<&str>,
    repo_settings_toml: Option<&str>,
) -> Result<Arc<Settings>, PrAgentError> {
    let settings = Arc::new(load_settings(
        cli_overrides,
        global_settings_toml,
        repo_settings_toml,
    )?);
    *GLOBAL_SETTINGS.write().unwrap_or_else(|poisoned| {
        tracing::error!("settings RwLock poisoned, recovering inner value");
        poisoned.into_inner()
    }) = Some(settings.clone());
    Ok(settings)
}

/// Run an async block with per-request settings override.
pub async fn with_settings<F, T>(settings: Arc<Settings>, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    REQUEST_SETTINGS.scope(settings, f).await
}

/// Build the full configuration by merging layers:
///
/// 1. Embedded TOML defaults (`settings/configuration.toml`, etc.)
/// 2. Secrets file from filesystem (`.secrets.toml`, optional)
/// 3. Global org-level `.pr_agent.toml` (from `pr-agent-settings` repo, optional)
/// 4. Repo-level `.pr_agent.toml` (fetched from git provider, optional)
/// 5. CLI argument overrides (`--section.key=value`)
/// 6. Environment variables (highest precedence for secrets)
pub fn load_settings(
    cli_overrides: &HashMap<String, String>,
    global_settings_toml: Option<&str>,
    repo_settings_toml: Option<&str>,
) -> Result<Settings, PrAgentError> {
    // Layer 1: embedded defaults
    let mut figment = Figment::new()
        .merge(Toml::string(CONFIGURATION_TOML))
        .merge(Toml::string(IGNORE_TOML))
        .merge(Toml::string(LANGUAGE_EXTENSIONS_TOML))
        .merge(Toml::string(CUSTOM_LABELS_TOML))
        // Prompt templates
        .merge(Toml::string(PR_REVIEWER_PROMPTS))
        .merge(Toml::string(PR_DESCRIPTION_PROMPTS))
        .merge(Toml::string(PR_CODE_SUGGESTIONS_PROMPTS))
        .merge(Toml::string(PR_CODE_SUGGESTIONS_NOT_DECOUPLED))
        .merge(Toml::string(PR_CODE_SUGGESTIONS_REFLECT))
        .merge(Toml::string(PR_QUESTIONS_PROMPTS))
        .merge(Toml::string(PR_LINE_QUESTIONS_PROMPTS))
        .merge(Toml::string(PR_UPDATE_CHANGELOG_PROMPTS))
        .merge(Toml::string(PR_INFORMATION_FROM_USER))
        .merge(Toml::string(PR_HELP_PROMPTS))
        .merge(Toml::string(PR_HELP_DOCS_PROMPTS))
        .merge(Toml::string(PR_HELP_DOCS_HEADINGS))
        .merge(Toml::string(PR_EVALUATE_PROMPT_RESPONSE));

    // Layer 2: secrets file (optional, from filesystem)
    figment = figment.merge(Toml::file(".secrets.toml"));
    figment = figment.merge(Toml::file("settings/.secrets.toml"));

    // Layer 3: global org-level .pr_agent.toml (from pr-agent-settings repo, optional)
    if let Some(global_toml) = global_settings_toml {
        figment = figment.merge(Toml::string(global_toml));
    }

    // Layer 4: repo-level .pr_agent.toml (provided as string from git provider)
    if let Some(repo_toml) = repo_settings_toml {
        figment = figment.merge(Toml::string(repo_toml));
    }

    // Layer 5: CLI argument overrides (--pr_reviewer.num_max_findings=5)
    for (key, value) in cli_overrides {
        // Figment doesn't have a direct "set key" method for arbitrary dotted keys,
        // so we build a TOML fragment: `[section]\nkey = value`
        if let Some(toml_fragment) = cli_override_to_toml(key, value) {
            figment = figment.merge(Toml::string(&toml_fragment));
        }
    }

    // Layer 6a: Well-known env var aliases (underscore-separated names)
    figment = figment.merge(
        Env::raw()
            .map(|key| match key.as_str() {
                "OPENAI_API_KEY" | "OPENAI_KEY" => "openai.key".into(),
                "GITHUB_TOKEN" | "GITHUB_USER_TOKEN" => "github.user_token".into(),
                "ANTHROPIC_API_KEY" => "anthropic.key".into(),
                _ => key.into(),
            })
            .only(&[
                "OPENAI_API_KEY",
                "OPENAI_KEY",
                "GITHUB_TOKEN",
                "GITHUB_USER_TOKEN",
                "ANTHROPIC_API_KEY",
            ]),
    );

    // Layer 6b: Dynaconf-compatible SECTION.KEY env vars
    // Maps CONFIG.MODEL → config.model, OPENAI.KEY → openai.key, etc.
    //
    // We handle ALL dotted env vars here as TOML fragments instead of using
    // Figment's Env provider, because Env treats all values as strings and
    // cannot deserialize array syntax like ['item'] into Vec<T> fields.
    for (key, value) in std::env::vars() {
        if !key.contains('.') {
            continue;
        }
        let value_trimmed = value.trim();
        let lower = key.to_lowercase();
        let Some((section, field)) = lower.split_once('.') else {
            continue;
        };

        // Reject keys with metacharacters that could break out of the fragment.
        if !is_safe_toml_section(section) || !is_safe_toml_field(field) {
            tracing::warn!("ignoring dotted env var with unsafe key: {key}");
            continue;
        }

        let is_array = value_trimmed.starts_with('[') && value_trimmed.ends_with(']');

        let fragment = if is_array {
            let toml_val = normalize_toml_array(value_trimmed);
            format!("[{section}]\n{field} = {toml_val}")
        } else {
            // Scalar value — detect type for proper TOML encoding
            let toml_value = encode_toml_scalar(value_trimmed);
            format!("[{section}]\n{field} = {toml_value}")
        };
        figment = figment.merge(Toml::string(&fragment));
    }

    let settings: Settings = figment.extract()?;
    Ok(settings)
}

/// Encode a scalar value as a TOML literal (bool/int/float) or escaped string.
///
/// Non-finite floats (`nan`, `inf`, `-inf`) are intentionally NOT treated as
/// literals: `value.parse::<f64>()` accepts them, but emitting them bare would
/// let untrusted input (e.g. a webhook comment `--config.temperature=nan`)
/// poison the request body. They fall through to the quoted-string branch,
/// where figment rejects them when extracting a numeric field.
fn encode_toml_scalar(value: &str) -> String {
    let is_literal = value == "true"
        || value == "false"
        || value.parse::<i64>().is_ok()
        || value.parse::<f64>().map(f64::is_finite).unwrap_or(false);
    if is_literal {
        value.to_string()
    } else {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        format!("\"{escaped}\"")
    }
}

/// Whether a TOML section name is safe to interpolate into a `[section]`
/// header. Rejects anything that could break out of the header (brackets,
/// quotes, whitespace, newlines, dots).
fn is_safe_toml_section(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether a TOML field key is safe to interpolate before ` = value`. Dots are
/// allowed (valid TOML dotted keys for nested fields); brackets, quotes,
/// `=`, whitespace and newlines are not.
fn is_safe_toml_field(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

/// Normalize a `[...]` array env-var value into a valid TOML array.
///
/// Docker/Coolify often single-quote and backslash-escape array values
/// (`[\'a\', \'b\']`). We split at top-level commas (commas inside quotes do
/// NOT split), then strip one matching pair of surrounding quotes per element
/// and re-encode it with [`encode_toml_scalar`]. A blanket `'` → `"` replace
/// (the previous approach) corrupted values like `["it's_a_file.txt"]`.
fn normalize_toml_array(value: &str) -> String {
    let inner = value
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(value);
    // Un-escape Docker-style escaped quotes before tokenizing.
    let inner = inner.replace("\\'", "'").replace("\\\"", "\"");

    // Split at top-level commas, keeping quotes so they can be stripped per element.
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in inner.chars() {
        match quote {
            Some(q) => {
                current.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    current.push(ch);
                }
                ',' => segments.push(std::mem::take(&mut current)),
                _ => current.push(ch),
            },
        }
    }
    segments.push(current);

    let encoded: Vec<String> = segments
        .iter()
        .map(|seg| seg.trim())
        .filter(|seg| !seg.is_empty())
        .map(|seg| encode_toml_scalar(strip_matching_quotes(seg)))
        .collect();
    format!("[{}]", encoded.join(", "))
}

/// Strip a single matching pair of surrounding single/double quotes, preserving
/// the inner content verbatim (so apostrophes inside a double-quoted value stay).
fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Convert a CLI override like "pr_reviewer.num_max_findings=5" into a TOML fragment.
fn cli_override_to_toml(key: &str, value: &str) -> Option<String> {
    let (section, field) = match key.split_once('.') {
        Some(pair) => pair,
        None => {
            tracing::warn!("ignoring CLI override with no section: {key}={value}");
            return None;
        }
    };
    if !is_safe_toml_section(section) || !is_safe_toml_field(field) {
        tracing::warn!("ignoring CLI override with unsafe key: {key}");
        return None;
    }
    let toml_value = encode_toml_scalar(value);
    Some(format!("[{section}]\n{field} = {toml_value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mutex to serialize tests that modify environment variables.
    // `load_settings()` iterates ALL dotted env vars via `std::env::vars()`,
    // so concurrent tests setting env vars will contaminate each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_encode_toml_scalar_rejects_non_finite_floats() {
        // C14: nan/inf must NOT be emitted as bare TOML literals.
        assert_eq!(encode_toml_scalar("nan"), "\"nan\"");
        assert_eq!(encode_toml_scalar("inf"), "\"inf\"");
        assert_eq!(encode_toml_scalar("-inf"), "\"-inf\"");
        // Finite numbers and bools are still literals.
        assert_eq!(encode_toml_scalar("0.2"), "0.2");
        assert_eq!(encode_toml_scalar("42"), "42");
        assert_eq!(encode_toml_scalar("true"), "true");
        assert_eq!(encode_toml_scalar("gpt-4"), "\"gpt-4\"");
    }

    #[test]
    fn test_normalize_toml_array_preserves_apostrophes() {
        // C15: a double-quoted value containing an apostrophe must survive
        // (the old blanket ' → " replace corrupted it into invalid TOML).
        assert_eq!(
            normalize_toml_array("[\"it's_a_file.txt\"]"),
            "[\"it's_a_file.txt\"]"
        );
        // Single-quoted and escaped (Docker-style) inputs normalize to double quotes.
        assert_eq!(normalize_toml_array("['a', 'b']"), "[\"a\", \"b\"]");
        assert_eq!(normalize_toml_array("[\\'a\\', \\'b\\']"), "[\"a\", \"b\"]");
        // Empty array.
        assert_eq!(normalize_toml_array("[]"), "[]");
    }

    #[test]
    fn test_normalize_toml_array_roundtrips_as_valid_toml() {
        // The result must parse as TOML (the previous corruption broke load).
        let frag = format!("[ignore]\nglob = {}", normalize_toml_array("[\"a's.txt\"]"));
        let parsed: toml::Value = toml::from_str(&frag).expect("should be valid TOML");
        let arr = parsed["ignore"]["glob"].as_array().unwrap();
        assert_eq!(arr[0].as_str().unwrap(), "a's.txt");
    }

    #[test]
    fn test_toml_key_safety_validation() {
        // C13: only safe key segments are interpolated.
        assert!(is_safe_toml_section("pr_reviewer"));
        assert!(!is_safe_toml_section("evil]\n[other"));
        assert!(!is_safe_toml_section("a.b")); // section has no dots
        assert!(is_safe_toml_field("num_max_findings"));
        assert!(is_safe_toml_field("nested.key")); // dotted fields allowed
        assert!(!is_safe_toml_field("x = 1\n[evil"));
        assert!(!is_safe_toml_field("k\"]"));

        // cli_override_to_toml rejects unsafe keys.
        assert!(cli_override_to_toml("config.model", "gpt-4").is_some());
        assert!(cli_override_to_toml("config.x]\n[evil", "1").is_none());
    }

    #[test]
    fn test_load_default_settings() {
        let _guard = ENV_LOCK.lock().unwrap();
        let settings =
            load_settings(&HashMap::new(), None, None).expect("should load default settings");

        // Verify values match the configuration.toml defaults
        assert_eq!(settings.config.model, "gpt-5.2-2025-12-11");
        assert_eq!(settings.config.git_provider, "github");
        assert!(settings.config.publish_output);
        assert_eq!(settings.config.ai_timeout, 120);
        assert_eq!(settings.config.temperature, 0.2);
        assert_eq!(settings.config.max_model_tokens, 32_000);
        assert_eq!(settings.config.patch_extra_lines_before, 5);
        assert_eq!(settings.config.patch_extra_lines_after, 1);
        assert_eq!(settings.config.large_patch_policy, "clip");

        // Tool configs
        assert!(settings.pr_reviewer.require_tests_review);
        assert!(settings.pr_reviewer.require_security_review);
        assert_eq!(settings.pr_reviewer.num_max_findings, 3);
        assert!(!settings.pr_description.publish_labels);
        assert!(settings.pr_description.enable_pr_diagram);
        assert!(settings.pr_code_suggestions.focus_only_on_problems);

        // GitHub config
        assert_eq!(settings.github.deployment_type, "user");
        assert_eq!(settings.github.ratelimit_retries, 5);
        assert_eq!(settings.github.base_url, "https://api.github.com");
    }

    #[test]
    fn test_cli_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("pr_reviewer.num_max_findings".into(), "10".into());
        overrides.insert("config.temperature".into(), "0.5".into());

        let settings = load_settings(&overrides, None, None).expect("should load with overrides");

        assert_eq!(settings.pr_reviewer.num_max_findings, 10);
        assert!((settings.config.temperature - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_repo_settings_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let repo_toml = r#"
[pr_reviewer]
num_max_findings = 7
extra_instructions = "Focus on security"
"#;
        let settings = load_settings(&HashMap::new(), None, Some(repo_toml))
            .expect("should merge repo settings");

        assert_eq!(settings.pr_reviewer.num_max_findings, 7);
        assert_eq!(settings.pr_reviewer.extra_instructions, "Focus on security");
        // Other values should remain at defaults
        assert!(settings.pr_reviewer.require_tests_review);
    }

    #[test]
    fn test_global_settings_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let global_toml = r#"
[pr_reviewer]
num_max_findings = 20
extra_instructions = "Org-wide: check licenses"
"#;
        let settings = load_settings(&HashMap::new(), Some(global_toml), None)
            .expect("should merge global settings");

        assert_eq!(settings.pr_reviewer.num_max_findings, 20);
        assert_eq!(
            settings.pr_reviewer.extra_instructions,
            "Org-wide: check licenses"
        );
    }

    #[test]
    fn test_repo_overrides_global_settings() {
        let _guard = ENV_LOCK.lock().unwrap();
        let global_toml = r#"
[pr_reviewer]
num_max_findings = 20
extra_instructions = "Org-wide: check licenses"
"#;
        let repo_toml = r#"
[pr_reviewer]
num_max_findings = 5
"#;
        let settings = load_settings(&HashMap::new(), Some(global_toml), Some(repo_toml))
            .expect("should merge both");

        // Repo overrides global
        assert_eq!(settings.pr_reviewer.num_max_findings, 5);
        // Global value preserved when repo doesn't override it
        assert_eq!(
            settings.pr_reviewer.extra_instructions,
            "Org-wide: check licenses"
        );
    }

    #[test]
    fn test_cli_overrides_repo_and_global() {
        let _guard = ENV_LOCK.lock().unwrap();
        let global_toml = r#"
[pr_reviewer]
num_max_findings = 20
"#;
        let repo_toml = r#"
[pr_reviewer]
num_max_findings = 5
"#;
        let mut cli = HashMap::new();
        cli.insert("pr_reviewer.num_max_findings".into(), "99".into());

        let settings = load_settings(&cli, Some(global_toml), Some(repo_toml))
            .expect("should merge all layers");

        // CLI wins over both
        assert_eq!(settings.pr_reviewer.num_max_findings, 99);
    }

    // All env var tests acquire ENV_LOCK. The `unsafe` blocks are required
    // because modifying env vars is inherently process-global.

    #[test]
    fn test_dotted_env_var_simple_string() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("CONFIG.MODEL", "openai/test-model-env") };
        let settings =
            load_settings(&HashMap::new(), None, None).expect("should load with env override");
        assert_eq!(settings.config.model, "openai/test-model-env");
        unsafe { std::env::remove_var("CONFIG.MODEL") };
    }

    #[test]
    fn test_dotted_env_var_array_double_quoted() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("CONFIG.FALLBACK_MODELS", r#"["openai/test-fallback"]"#);
        }
        let settings =
            load_settings(&HashMap::new(), None, None).expect("should load array env var");
        assert_eq!(
            settings.config.fallback_models,
            vec!["openai/test-fallback"]
        );
        unsafe { std::env::remove_var("CONFIG.FALLBACK_MODELS") };
    }

    #[test]
    fn test_dotted_env_var_array_single_quoted() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("IGNORE.GLOB", "['pnpm-lock.yaml']") };
        let settings =
            load_settings(&HashMap::new(), None, None).expect("should load single-quoted array");
        assert!(
            settings.ignore.glob.contains(&"pnpm-lock.yaml".to_string()),
            "glob should contain pnpm-lock.yaml, got: {:?}",
            settings.ignore.glob
        );
        unsafe { std::env::remove_var("IGNORE.GLOB") };
    }

    #[test]
    fn test_dotted_env_var_array_docker_escaped_double_quotes() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Docker/Coolify may backslash-escape double quotes: [\"val\"]
        unsafe {
            std::env::set_var("IGNORE.GLOB", r#"[\"pnpm-lock.yaml\"]"#);
        }
        let settings = load_settings(&HashMap::new(), None, None)
            .expect("should handle Docker-escaped double-quoted array");
        assert!(
            settings.ignore.glob.contains(&"pnpm-lock.yaml".to_string()),
            "glob should contain pnpm-lock.yaml from Docker-escaped env, got: {:?}",
            settings.ignore.glob
        );
        unsafe { std::env::remove_var("IGNORE.GLOB") };
    }

    #[test]
    fn test_dotted_env_var_array_docker_escaped_single_quotes() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Coolify escapes single quotes too: [\'val\'] — this is the actual
        // production failure mode when the user types ['pnpm-lock.yaml']
        unsafe {
            std::env::set_var("IGNORE.GLOB", r"[\'pnpm-lock.yaml\']");
        }
        let settings = load_settings(&HashMap::new(), None, None)
            .expect("should handle Docker-escaped single-quoted array");
        assert!(
            settings.ignore.glob.contains(&"pnpm-lock.yaml".to_string()),
            "glob should contain pnpm-lock.yaml from escaped single-quoted env, got: {:?}",
            settings.ignore.glob
        );
        unsafe { std::env::remove_var("IGNORE.GLOB") };
    }

    #[test]
    fn test_dotted_env_var_array_docker_escaped_multi() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Multiple items with Docker/JSON escaping
        unsafe {
            std::env::set_var(
                "CONFIG.FALLBACK_MODELS",
                r#"[\"openai/gpt-4\", \"openai/gpt-3.5\"]"#,
            );
        }
        let settings = load_settings(&HashMap::new(), None, None)
            .expect("should handle multi-item Docker-escaped array");
        assert_eq!(
            settings.config.fallback_models,
            vec!["openai/gpt-4", "openai/gpt-3.5"]
        );
        unsafe { std::env::remove_var("CONFIG.FALLBACK_MODELS") };
    }

    #[test]
    fn test_dotted_env_var_bool() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("GITHUB_APP.HANDLE_PUSH_TRIGGER", "true") };
        let settings =
            load_settings(&HashMap::new(), None, None).expect("should load bool env var");
        assert!(settings.github_app.handle_push_trigger);
        unsafe { std::env::remove_var("GITHUB_APP.HANDLE_PUSH_TRIGGER") };
    }

    #[test]
    fn test_dotted_env_var_int() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("CONFIG.MAX_MODEL_TOKENS", "128000") };
        let settings = load_settings(&HashMap::new(), None, None).expect("should load int env var");
        assert_eq!(settings.config.max_model_tokens, 128_000);
        unsafe { std::env::remove_var("CONFIG.MAX_MODEL_TOKENS") };
    }

    #[test]
    fn test_dotted_env_var_multiline_private_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fake_key = "-----BEGIN RSA PRIVATE KEY-----\nMIIBogIBAAJBALR\ntest123\n-----END RSA PRIVATE KEY-----";
        unsafe { std::env::set_var("GITHUB.PRIVATE_KEY", fake_key) };
        let settings =
            load_settings(&HashMap::new(), None, None).expect("should load multiline env var");
        assert!(
            settings
                .github
                .private_key
                .contains("BEGIN RSA PRIVATE KEY"),
            "private key should contain full PEM, got: {:?}",
            &settings.github.private_key[..50.min(settings.github.private_key.len())]
        );
        assert!(settings.github.private_key.contains("test123"));
        unsafe { std::env::remove_var("GITHUB.PRIVATE_KEY") };
    }

    #[test]
    fn test_cli_override_to_toml_types() {
        assert_eq!(
            cli_override_to_toml("config.model", "gpt-4"),
            Some("[config]\nmodel = \"gpt-4\"".into())
        );
        assert_eq!(
            cli_override_to_toml("pr_reviewer.num_max_findings", "10"),
            Some("[pr_reviewer]\nnum_max_findings = 10".into())
        );
        assert_eq!(
            cli_override_to_toml("config.publish_output", "false"),
            Some("[config]\npublish_output = false".into())
        );
    }
}
