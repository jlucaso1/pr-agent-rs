pub mod ask;
pub mod ask_line;
pub mod describe;
pub mod image;
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

/// Extract validated image URLs from the PR description and linked issues,
/// respecting `enable_vision` config.
///
/// Collects images from:
/// 1. The PR description itself (markdown images, HTML `<img>` tags, bare URLs)
/// 2. Bodies of issues referenced in the PR description (`#N`, full GitHub URLs)
///
/// **Edge cases handled:**
/// - Skips fetching the PR's own number (GitHub issues API returns PRs too)
/// - Only follows 1 level deep — does NOT recurse into issues referenced by other issues
/// - Individual issue fetch failures are logged and skipped (no hard failure)
/// - Deduplicates images across PR body and all issue bodies
/// - Validates all URLs with HEAD requests (GitHub-hosted URLs are trusted)
/// - Capped at 5 linked issues max to avoid excessive API calls
///
/// Returns `None` when no images are found or vision is disabled,
/// matching the `image_urls: Option<&[String]>` convention used by the AI handler.
pub async fn get_pr_images(
    description: &str,
    provider: &dyn GitProvider,
    pr_number: Option<u64>,
) -> Option<Vec<String>> {
    let settings = get_settings();
    if !settings.config.enable_vision {
        return None;
    }

    // 1. Extract image URLs from PR body
    let mut all_urls = image::extract_image_urls(description);
    let mut seen: std::collections::HashSet<String> = all_urls.iter().cloned().collect();

    // 2. Extract linked issue numbers and fetch their bodies
    let (owner, repo) = provider.repo_owner_and_name();
    if !owner.is_empty() && !repo.is_empty() {
        let issue_numbers = image::extract_linked_issue_numbers(description, &owner, &repo);

        // Filter out the PR's own number (GitHub issues API returns PRs too,
        // so fetching it would just return the same body we already parsed)
        // and enforce the cap of 5 as defense-in-depth.
        let issue_numbers: Vec<u64> = issue_numbers
            .into_iter()
            .filter(|&n| pr_number != Some(n))
            .take(image::MAX_LINKED_ISSUES)
            .collect();

        if !issue_numbers.is_empty() {
            let futures: Vec<_> = issue_numbers
                .iter()
                .map(|&n| provider.get_issue_body(n))
                .collect();
            let results = futures_util::future::join_all(futures).await;

            for (i, result) in results.into_iter().enumerate() {
                match result {
                    Ok((_title, body)) => {
                        for url in image::extract_image_urls(&body) {
                            if seen.insert(url.clone()) {
                                all_urls.push(url);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            issue = issue_numbers[i],
                            error = %e,
                            "failed to fetch linked issue body for image extraction, skipping"
                        );
                    }
                }
            }
        }
    }

    if all_urls.is_empty() {
        return None;
    }

    // 3. Validate all URLs (HEAD requests, GitHub URLs trusted)
    let validated = image::validate_image_urls(all_urls).await;
    if validated.is_empty() {
        None
    } else {
        Some(validated)
    }
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

/// Recognized tool commands.
///
/// The single source of truth for command-name → tool mapping.
/// `resolve_command` maps string aliases to variants; `dispatch` executes them.
/// Adding a new tool here automatically makes it recognized by `is_known_command`.
enum Command {
    Review,
    Describe,
    Improve,
    Ask,
    AskLine,
}

/// Map a command name string to its `Command` variant, if recognized.
fn resolve_command(name: &str) -> Option<Command> {
    match name {
        "review" | "auto_review" | "review_pr" => Some(Command::Review),
        "describe" | "describe_pr" => Some(Command::Describe),
        "improve" | "improve_code" => Some(Command::Improve),
        "ask" => Some(Command::Ask),
        "ask_line" => Some(Command::AskLine),
        _ => None,
    }
}

/// Check whether a command name is one that pr-agent-rs can handle.
///
/// Used by the webhook handler to reject unknown commands early — before
/// creating a provider, adding eyes reactions, or fetching scoped settings.
pub fn is_known_command(name: &str) -> bool {
    resolve_command(name).is_some()
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
    let Some(cmd) = resolve_command(command) else {
        return Err(PrAgentError::Other(format!("unknown command: '{command}'")));
    };
    match cmd {
        Command::Review => review::PRReviewer::new(provider).run().await,
        Command::Describe => describe::PRDescription::new(provider).run().await,
        Command::Improve => improve::PRCodeSuggestions::new(provider).run().await,
        Command::Ask => {
            let question = args.get("_text").map(|s| s.as_str()).unwrap_or("");
            ask::PRAsk::new(provider).run(question).await
        }
        Command::AskLine => ask_line::PRAskLine::new(provider).run(args).await,
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

    // ── is_known_command tests ───────────────────────────────────────

    #[test]
    fn test_is_known_command_all_aliases() {
        // Every alias in resolve_command must be recognized
        for cmd in [
            "review",
            "auto_review",
            "review_pr",
            "describe",
            "describe_pr",
            "improve",
            "improve_code",
            "ask",
            "ask_line",
        ] {
            assert!(is_known_command(cmd), "'{cmd}' should be a known command");
        }
    }

    #[test]
    fn test_is_known_command_rejects_unknown() {
        for cmd in ["qa-verify", "qa-review", "help", "deploy", "", "REVIEW"] {
            assert!(
                !is_known_command(cmd),
                "'{cmd}' should NOT be a known command"
            );
        }
    }
}
