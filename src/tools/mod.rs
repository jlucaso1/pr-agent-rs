pub mod ask;
pub mod ask_line;
pub mod describe;
pub mod improve;
pub mod review;

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use minijinja::Value;

use crate::ai::AiHandler;
use crate::ai::openai::OpenAiCompatibleHandler;
use crate::config::loader::{get_settings, load_settings, with_settings};
use crate::config::types::{CustomLabelEntry, Settings};
use crate::error::PrAgentError;
use crate::git::GitProvider;

/// Resolve the AI handler: use the injected one or create from settings.
pub fn resolve_ai_handler(
    injected: &Option<Arc<dyn AiHandler>>,
) -> Result<Arc<dyn AiHandler>, PrAgentError> {
    match injected {
        Some(ai) => Ok(ai.clone()),
        None => Ok(Arc::new(OpenAiCompatibleHandler::from_settings()?)),
    }
}

/// Common PR metadata fetched once and shared across tool pipelines.
///
/// Bundles the fields that all tools (review, describe, improve) need,
/// eliminating the 9-parameter `build_vars` signatures.
pub struct PrMetadata {
    pub title: String,
    pub description: String,
    pub branch: String,
    pub commit_messages: String,
    pub best_practices: String,
    pub repo_metadata: String,
}

impl PrMetadata {
    /// Fetch all common PR metadata from the provider and settings.
    ///
    /// This consolidates the identical metadata-fetching code that was
    /// duplicated across review, describe, and improve tools.
    pub async fn fetch(
        provider: &dyn GitProvider,
        settings: &Settings,
    ) -> Result<Self, PrAgentError> {
        let (title, description) = provider.get_pr_description_full().await?;
        let branch = provider.get_pr_branch().await?;
        let commit_messages = provider.get_commit_messages().await?;

        let best_practices = {
            let bp = &settings.best_practices.content;
            if !bp.is_empty() {
                bp.clone()
            } else {
                provider.get_best_practices().await.unwrap_or_default()
            }
        };
        let repo_metadata = provider.get_repo_metadata().await.unwrap_or_default();

        Ok(Self {
            title,
            description,
            branch,
            commit_messages,
            best_practices,
            repo_metadata,
        })
    }
}

/// Run a tool's inner logic wrapped with progress comment lifecycle.
///
/// If `publish_output_progress` is enabled, creates a progress comment before
/// running `inner`, then removes it afterward (even on error).
pub async fn with_progress_comment<F, Fut>(
    provider: &dyn GitProvider,
    message: &str,
    inner: F,
) -> Result<(), PrAgentError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), PrAgentError>>,
{
    let settings = get_settings();

    let progress_comment_id = if settings.config.publish_output_progress {
        provider.publish_comment(message, true).await.ok().flatten()
    } else {
        None
    };

    let result = inner().await;

    if let Some(ref id) = progress_comment_id {
        let _ = provider.remove_comment(id).await;
    }

    result
}

/// Build the custom labels class string for prompt templates.
///
/// Produces the prompt-friendly label class format:
/// ```text
/// Label('gn-florestal', description='Changes to gn-florestal')
/// Label('database', description='Changes to database schemas')
/// ```
pub fn build_custom_labels_class(labels: &HashMap<String, CustomLabelEntry>) -> String {
    let mut out = String::new();
    for (name, entry) in labels {
        let _ = writeln!(
            out,
            "Label('{}', description='{}')",
            name, entry.description
        );
    }
    out
}

/// Build the template variables shared by all tools (review, describe, improve).
///
/// Returns a `HashMap` pre-populated with the 8 variables that every tool needs.
/// Each tool then extends this map with its own tool-specific variables.
pub fn build_common_vars(meta: &PrMetadata, diff: &str) -> HashMap<String, Value> {
    [
        ("title", meta.title.as_str()),
        ("branch", meta.branch.as_str()),
        ("description", meta.description.as_str()),
        ("language", ""),
        ("diff", diff),
        ("commit_messages_str", meta.commit_messages.as_str()),
        ("best_practices_content", meta.best_practices.as_str()),
        ("repo_metadata", meta.repo_metadata.as_str()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), Value::from(v)))
    .collect()
}

/// Insert custom-labels template variables into the vars map.
///
/// Shared by review and describe, which both need `enable_custom_labels`,
/// `custom_labels_class`, and `custom_labels` template variables.
pub fn insert_custom_labels_vars(vars: &mut HashMap<String, Value>, settings: &Settings) {
    let has_custom_labels = !settings.custom_labels.is_empty();
    vars.insert(
        "enable_custom_labels".into(),
        Value::from(has_custom_labels),
    );
    vars.insert(
        "custom_labels_class".into(),
        Value::from(if has_custom_labels {
            build_custom_labels_class(&settings.custom_labels)
        } else {
            String::new()
        }),
    );
    vars.insert("custom_labels".into(), Value::from(""));
}

/// Publish tool output as either a persistent comment or a regular comment.
///
/// Shared by review and improve, which both follow the same pattern:
/// if persistent_comment is enabled → publish_persistent_comment with marker;
/// otherwise → publish_comment.
pub async fn publish_as_comment(
    provider: &dyn GitProvider,
    content: &str,
    tool_name: &str,
    persistent: bool,
    final_update_message: bool,
) -> Result<(), PrAgentError> {
    if persistent {
        let marker = format!("<!-- pr-agent:{tool_name} -->");
        provider
            .publish_persistent_comment(content, &marker, "", tool_name, final_update_message)
            .await?;
    } else {
        provider.publish_comment(content, false).await?;
    }
    Ok(())
}

/// Parse a "/command --arg=value text" string into (command_name, args_overrides).
///
/// Splits on whitespace and extracts `--key=value` pairs as config overrides.
/// Non-flag words (without `--` prefix or without `=`) are collected into
/// the `_text` key — used by /ask and /ask_line for the question text.
/// Security-sensitive keys (secrets, auth, URLs) are dropped with a warning log.
pub fn parse_command(input: &str) -> (String, HashMap<String, String>) {
    let trimmed = input.trim();
    let mut parts = trimmed.split_whitespace();
    let command = parts
        .next()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_lowercase();

    let mut overrides = HashMap::new();
    let mut text_parts: Vec<&str> = Vec::new();
    for part in parts {
        if part.starts_with('-') && part.contains('=') {
            let stripped = part.trim_start_matches('-');
            // Convert double underscore to dot
            let stripped = stripped.replace("__", ".");
            if let Some((key, value)) = stripped.split_once('=') {
                if let Some(forbidden) = crate::cli::check_forbidden_key(key) {
                    tracing::warn!(
                        key,
                        forbidden,
                        "dropping forbidden override from comment command"
                    );
                    continue;
                }
                overrides.insert(key.to_string(), value.to_string());
            }
        } else {
            text_parts.push(part);
        }
    }

    if !text_parts.is_empty() {
        overrides.insert("_text".to_string(), text_parts.join(" "));
    }

    (command, overrides)
}

/// Dispatch a command to the appropriate tool.
///
/// If `args` contains per-command overrides (from `/command --key=value` parsing),
/// creates a scoped settings override for this command execution.
pub async fn handle_command(
    command: &str,
    provider: Arc<dyn GitProvider>,
    args: &HashMap<String, String>,
) -> Result<(), PrAgentError> {
    // Separate config overrides (key=value flags) from tool data (_text, _diff_hunk, etc.)
    let config_overrides: HashMap<String, String> = args
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // If there are per-command config overrides, scope them as settings overrides
    if !config_overrides.is_empty() {
        let current = get_settings();
        let scoped = Arc::new(match load_settings(&config_overrides, None, None) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    ?config_overrides,
                    "failed to apply command config overrides, using current settings"
                );
                (*current).clone()
            }
        });
        return with_settings(scoped, dispatch(command, provider, args)).await;
    }

    dispatch(command, provider, args).await
}

async fn dispatch(
    command: &str,
    provider: Arc<dyn GitProvider>,
    args: &HashMap<String, String>,
) -> Result<(), PrAgentError> {
    match command {
        "review" | "auto_review" | "review_pr" => review::PRReviewer::new(provider).run().await,
        "describe" | "describe_pr" => describe::PRDescription::new(provider).run().await,
        "improve" | "improve_code" => improve::PRCodeSuggestions::new(provider).run().await,
        "ask" => {
            let question = args.get("_text").map(|s| s.as_str()).unwrap_or("");
            ask::PRAsk::new(provider).run(question).await
        }
        "ask_line" => ask_line::PRAskLine::new(provider).run(args).await,
        _ => Err(PrAgentError::Other(format!("unknown command: '{command}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_command_simple() {
        let (cmd, args) = parse_command("/review");
        assert_eq!(cmd, "review");
        assert!(args.is_empty());
    }

    #[test]
    fn test_parse_command_with_args() {
        let (cmd, args) =
            parse_command("/describe --pr_description.publish_labels=true --config.model=gpt-4");
        assert_eq!(cmd, "describe");
        assert_eq!(args.get("pr_description.publish_labels").unwrap(), "true");
        assert_eq!(args.get("config.model").unwrap(), "gpt-4");
    }

    #[test]
    fn test_parse_command_double_underscore() {
        let (cmd, args) = parse_command("/improve --pr_code_suggestions__extra_instructions=test");
        assert_eq!(cmd, "improve");
        assert_eq!(
            args.get("pr_code_suggestions.extra_instructions").unwrap(),
            "test"
        );
    }

    #[test]
    fn test_parse_command_with_leading_slash() {
        let (cmd, _) = parse_command("review");
        assert_eq!(cmd, "review");
    }

    #[test]
    fn test_parse_command_drops_forbidden_keys() {
        let (cmd, args) = parse_command("/review --openai.key=sk-secret --config.model=gpt-4");
        assert_eq!(cmd, "review");
        assert!(
            !args.contains_key("openai.key"),
            "forbidden key should be dropped"
        );
        assert_eq!(args.get("config.model").unwrap(), "gpt-4");
    }

    #[test]
    fn test_parse_command_drops_forbidden_segment() {
        let (_, args) = parse_command("/review --github.base_url=http://evil.com");
        assert!(
            !args.contains_key("github.base_url"),
            "forbidden segment 'base_url' should be dropped"
        );
    }

    #[test]
    fn test_build_common_vars_populates_all_keys() {
        let meta = PrMetadata {
            title: "My Title".into(),
            description: "My Desc".into(),
            branch: "feat/test".into(),
            commit_messages: "commit 1\ncommit 2".into(),
            best_practices: "Use Rust idioms".into(),
            repo_metadata: "CLAUDE.md content".into(),
        };

        let vars = build_common_vars(&meta, "the-diff-content");

        assert_eq!(vars["title"].to_string(), "My Title");
        assert_eq!(vars["branch"].to_string(), "feat/test");
        assert_eq!(vars["description"].to_string(), "My Desc");
        assert_eq!(vars["diff"].to_string(), "the-diff-content");
        assert_eq!(
            vars["commit_messages_str"].to_string(),
            "commit 1\ncommit 2"
        );
        assert_eq!(
            vars["best_practices_content"].to_string(),
            "Use Rust idioms"
        );
        assert_eq!(vars["repo_metadata"].to_string(), "CLAUDE.md content");
        assert_eq!(vars["language"].to_string(), "");
    }

    #[test]
    fn test_build_custom_labels_class_formats_correctly() {
        let mut labels = HashMap::new();
        labels.insert(
            "bug-fix".into(),
            CustomLabelEntry {
                description: "Bug fix changes".into(),
            },
        );

        let result = build_custom_labels_class(&labels);
        assert!(result.contains("Label('bug-fix', description='Bug fix changes')"));
    }

    #[test]
    fn test_build_custom_labels_class_empty() {
        let labels = HashMap::new();
        let result = build_custom_labels_class(&labels);
        assert!(result.is_empty());
    }

    #[test]
    fn test_insert_custom_labels_vars_with_labels() {
        let mut vars = HashMap::new();
        let mut settings = Settings::default();
        settings.custom_labels.insert(
            "perf".into(),
            CustomLabelEntry {
                description: "Performance".into(),
            },
        );

        insert_custom_labels_vars(&mut vars, &settings);

        assert_eq!(vars["enable_custom_labels"].to_string(), "true");
        let class_str = vars["custom_labels_class"].to_string();
        assert!(class_str.contains("perf"));
    }

    #[test]
    fn test_insert_custom_labels_vars_without_labels() {
        let mut vars = HashMap::new();
        let settings = Settings::default();

        insert_custom_labels_vars(&mut vars, &settings);

        assert_eq!(vars["enable_custom_labels"].to_string(), "false");
        assert_eq!(vars["custom_labels_class"].to_string(), "");
    }

    #[tokio::test]
    async fn test_dispatch_unknown_command_returns_error() {
        use crate::testing::mock_git::MockGitProvider;

        let provider = Arc::new(MockGitProvider::new());
        let args = HashMap::new();
        let result = dispatch("unknown_command", provider, &args).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown command"),
            "should mention unknown command, got: {err}"
        );
    }

    #[test]
    fn test_parse_command_empty_input() {
        let (cmd, args) = parse_command("");
        assert_eq!(cmd, "");
        assert!(args.is_empty());
    }

    #[test]
    fn test_parse_command_whitespace_only() {
        let (cmd, args) = parse_command("   ");
        assert_eq!(cmd, "");
        assert!(args.is_empty());
    }

    #[test]
    fn test_parse_command_no_value() {
        // --flag without =value becomes text (not a config override)
        let (cmd, args) = parse_command("/review --verbose");
        assert_eq!(cmd, "review");
        assert!(
            !args.contains_key("verbose"),
            "flag without = should not be a config override"
        );
        assert_eq!(
            args.get("_text").unwrap(),
            "--verbose",
            "non-flag parts collected as _text"
        );
    }

    #[test]
    fn test_parse_command_ask_with_question() {
        let (cmd, args) = parse_command("/ask What does this PR do?");
        assert_eq!(cmd, "ask");
        assert_eq!(args.get("_text").unwrap(), "What does this PR do?");
    }

    #[test]
    fn test_parse_command_ask_line_with_flags_and_text() {
        let (cmd, args) = parse_command(
            "/ask_line --line_start=10 --line_end=15 --side=RIGHT --file_name=src/main.rs --comment_id=123 What is this?",
        );
        assert_eq!(cmd, "ask_line");
        assert_eq!(args.get("line_start").unwrap(), "10");
        assert_eq!(args.get("line_end").unwrap(), "15");
        assert_eq!(args.get("side").unwrap(), "RIGHT");
        assert_eq!(args.get("file_name").unwrap(), "src/main.rs");
        assert_eq!(args.get("comment_id").unwrap(), "123");
        assert_eq!(args.get("_text").unwrap(), "What is this?");
    }
}
