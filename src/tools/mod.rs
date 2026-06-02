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
        // These fetches are independent, so run them concurrently. Error
        // handling is preserved per field: the first three propagate (`?`),
        // the last two degrade to a default. best_practices still prefers the
        // configured content and only fetches when it is empty.
        let bp_from_config = !settings.best_practices.content.is_empty();

        let (desc_res, branch_res, commits_res, bp_res, repo_res) = tokio::join!(
            provider.get_pr_description_full(),
            provider.get_pr_branch(),
            provider.get_commit_messages(),
            async {
                if bp_from_config {
                    Ok(settings.best_practices.content.clone())
                } else {
                    provider.get_best_practices().await
                }
            },
            provider.get_repo_metadata(),
        );

        let (title, description) = desc_res?;
        let branch = branch_res?;
        let commit_messages = commits_res?;
        let best_practices = bp_res.unwrap_or_default();
        let repo_metadata = repo_res.unwrap_or_default();

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

/// Call the AI with the configured fallback models, reading model/fallbacks/
/// temperature from settings. Shared by review/describe/improve (which all use
/// the fallback path). NOT used by ask/ask_line, which intentionally call the
/// model directly without fallback.
pub(crate) async fn ai_call_with_fallback(
    ai: &dyn AiHandler,
    settings: &Settings,
    model: &str,
    system: &str,
    user: &str,
    image_urls: Option<&[String]>,
) -> Result<crate::ai::types::ChatResponse, PrAgentError> {
    crate::ai::chat_completion_with_fallback(
        ai,
        model,
        &settings.config.fallback_models,
        system,
        user,
        Some(settings.config.temperature),
        image_urls,
    )
    .await
}

/// Print a raw AI response to stdout when its YAML couldn't be parsed (CLI mode).
pub(crate) fn print_raw_fallback(raw_response: &str) {
    eprintln!("Warning: could not parse YAML from AI response, printing raw:");
    println!("{raw_response}");
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

/// Which configured model a tool should use.
#[derive(Clone, Copy)]
pub(crate) enum ModelKind {
    /// A cheaper/faster model for lighter tasks (describe, ask, ask_line).
    Weak,
    /// A reasoning model (e.g. improve's self-reflect pass).
    Reasoning,
}

/// Resolve the model for a task: the kind-specific model (`model_weak` /
/// `model_reasoning`) when configured, otherwise the default `config.model`.
/// Mirrors the Python `get_model`.
pub(crate) fn select_model(kind: ModelKind) -> String {
    let cfg = &get_settings().config;
    match kind {
        ModelKind::Weak if !cfg.model_weak.is_empty() => cfg.model_weak.clone(),
        ModelKind::Reasoning if !cfg.model_reasoning.is_empty() => cfg.model_reasoning.clone(),
        _ => cfg.model.clone(),
    }
}

/// Append the response-language instruction to a tool's `extra_instructions`
/// when a non-default `response_language` is configured, mirroring the Python
/// original (which injects it into every section's extra_instructions). The
/// instruction is deduplicated so repeated runs don't stack it.
pub(crate) fn with_response_language(extra_instructions: &str) -> String {
    let lang = &get_settings().config.response_language;
    if lang.is_empty() || lang.eq_ignore_ascii_case("en-us") {
        return extra_instructions.to_string();
    }
    let lang_instruction = format!(
        "Your response MUST be written in the language corresponding to locale code: '{lang}'. This is crucial."
    );
    if extra_instructions.contains(&lang_instruction) {
        return extra_instructions.to_string();
    }
    if extra_instructions.is_empty() {
        lang_instruction
    } else {
        format!("{extra_instructions}\n======\n\nIn addition, {lang_instruction}")
    }
}

/// Fetch GitHub issues linked in the PR description and build the
/// `related_tickets` template list for review ticket-compliance analysis.
///
/// GitHub-only (no Jira/Azure work-items), capped at [`image::MAX_LINKED_ISSUES`],
/// and the PR's own number is skipped. Returns an empty list when nothing is
/// linked, so the `{% if related_tickets %}` prompt blocks stay off.
pub(crate) async fn fetch_related_tickets(
    provider: &dyn GitProvider,
    description: &str,
    pr_number: Option<u64>,
) -> Vec<Value> {
    let (owner, repo) = provider.repo_owner_and_name();
    if owner.is_empty() || repo.is_empty() {
        return Vec::new();
    }
    let issue_numbers: Vec<u64> = image::extract_linked_issue_numbers(description, &owner, &repo)
        .into_iter()
        .filter(|&n| pr_number != Some(n))
        .take(image::MAX_LINKED_ISSUES)
        .collect();
    if issue_numbers.is_empty() {
        return Vec::new();
    }

    #[derive(serde::Serialize)]
    struct RelatedTicket {
        ticket_url: String,
        title: String,
        body: String,
        labels: String,
    }

    let futures: Vec<_> = issue_numbers
        .iter()
        .map(|&n| provider.get_issue(n))
        .collect();
    let results = futures_util::future::join_all(futures).await;

    let mut tickets = Vec::new();
    for (i, result) in results.into_iter().enumerate() {
        let n = issue_numbers[i];
        match result {
            Ok((title, body, labels)) => tickets.push(Value::from_serialize(&RelatedTicket {
                ticket_url: format!("https://github.com/{owner}/{repo}/issues/{n}"),
                title,
                body,
                labels: labels.join(", "),
            })),
            Err(e) => {
                tracing::warn!(issue = n, error = %e, "failed to fetch linked ticket, skipping")
            }
        }
    }
    tickets
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

/// Keep only labels that were added by the user.
///
/// Filters out the standard PR-type set (bug fix / tests / enhancement /
/// documentation / other) and, when custom labels are configured, any label
/// whose name matches a configured custom label. Mirrors the Python
/// `get_user_labels`. Used before a `publish_labels` PUT (which replaces *all*
/// labels) so user-applied labels are preserved instead of clobbered.
pub fn get_user_labels(current_labels: &[String], settings: &Settings) -> Vec<String> {
    const STANDARD: [&str; 5] = ["bug fix", "tests", "enhancement", "documentation", "other"];
    let has_custom = !settings.custom_labels.is_empty();
    current_labels
        .iter()
        .filter(|label| {
            let lower = label.to_lowercase();
            if STANDARD.contains(&lower.as_str()) {
                return false;
            }
            // Custom-label match is case-insensitive too, to stay consistent
            // with the standard-set check above.
            if has_custom
                && settings
                    .custom_labels
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case(label))
            {
                return false;
            }
            true
        })
        .cloned()
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
        publish_persistent_comment(provider, content, &marker, tool_name, final_update_message)
            .await?;
    } else {
        provider.publish_comment(content, false).await?;
    }
    Ok(())
}

/// Find an existing persistent comment by its header marker and update it in
/// place (appending an "updated until commit" header and an optional
/// notification), or create a new one if none exists.
///
/// This is pr-agent's *usage* of the provider (business orchestration), so it
/// lives here as a free function over `&dyn GitProvider` rather than as a
/// GitProvider trait default — the trait stays about provider capabilities.
pub async fn publish_persistent_comment(
    provider: &dyn GitProvider,
    text: &str,
    initial_header: &str,
    name: &str,
    final_update_message: bool,
) -> Result<(), PrAgentError> {
    use crate::git::capitalize_first;
    use crate::git::types::CommentId;

    let comments = provider.get_issue_comments().await?;
    for comment in &comments {
        if comment.body.starts_with(initial_header) {
            tracing::info!(
                comment_id = comment.id,
                "updating existing persistent comment"
            );
            let comment_url = comment.url.as_deref().unwrap_or("");

            // Add "updated until commit" header
            let latest_commit_url = provider.get_latest_commit_url().await.unwrap_or_default();
            let updated_text = if !latest_commit_url.is_empty() {
                let cap_name = capitalize_first(name);
                let updated_header = format!(
                    "{initial_header}\n\n#### ({cap_name} updated until commit {latest_commit_url})\n"
                );
                text.replace(initial_header, &updated_header)
            } else {
                text.to_string()
            };

            provider
                .edit_comment(&CommentId(comment.id.to_string()), &updated_text)
                .await?;

            // Post notification comment linking to the updated persistent comment
            if final_update_message && !comment_url.is_empty() && !latest_commit_url.is_empty() {
                let notification = format!(
                    "**[Persistent {name}]({comment_url})** updated to latest commit {latest_commit_url}"
                );
                let _ = provider.publish_comment(&notification, false).await;
            }

            return Ok(());
        }
    }
    tracing::info!("creating new persistent comment");
    provider.publish_comment(text, false).await?;
    Ok(())
}

/// Marker header for the "invalid repo configuration" persistent comment.
const CONFIG_ERROR_HEADER: &str = "❌ **PR-Agent failed to apply repo settings**";

/// Validate a repo-level `.pr_agent.toml` before it is merged into the config.
///
/// If the file fails to parse as TOML, publish a persistent comment to the PR
/// describing the error (so the author can fix it) and return `None` so the
/// caller proceeds with the remaining config layers instead of crashing.
/// A `None`/empty input passes through unchanged. Mirrors the Python
/// `handle_configurations_errors`.
pub async fn validate_repo_settings_toml(
    provider: &dyn GitProvider,
    repo_toml: Option<String>,
) -> Option<String> {
    let toml_str = repo_toml?;
    if toml_str.trim().is_empty() {
        return Some(toml_str);
    }
    match toml::from_str::<toml::Value>(&toml_str) {
        Ok(_) => Some(toml_str),
        Err(e) => {
            let body = format!(
                "{CONFIG_ERROR_HEADER}\n\nThe configuration file needs to be a valid \
                 [TOML](https://qodo-merge-docs.qodo.ai/usage-guide/configuration_options/), \
                 please fix it.\n\n___\n\n**Error message:**\n`{e}`\n\n\
                 <details><summary>Configuration content:</summary>\n\n\
                 ```toml\n{toml_str}\n```\n\n</details>"
            );
            tracing::warn!(error = %e, "repo .pr_agent.toml is invalid; reporting to PR");
            if let Err(pub_err) = publish_persistent_comment(
                provider,
                &body,
                CONFIG_ERROR_HEADER,
                "configuration error",
                false,
            )
            .await
            {
                tracing::warn!(error = %pub_err, "failed to publish config-error comment");
            }
            None
        }
    }
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
        if part.starts_with('-')
            && part.contains('=')
            && let Some((key, value)) = crate::cli::parse_override_token(part)
        {
            if let Some(forbidden) = crate::cli::check_forbidden_key(&key) {
                tracing::warn!(
                    key,
                    forbidden,
                    "dropping forbidden override from comment command"
                );
                continue;
            }
            overrides.insert(key, value);
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
    Help,
}

/// Map a command name string to its `Command` variant, if recognized.
fn resolve_command(name: &str) -> Option<Command> {
    match name {
        "review" | "auto_review" | "review_pr" => Some(Command::Review),
        "describe" | "describe_pr" => Some(Command::Describe),
        "improve" | "improve_code" => Some(Command::Improve),
        "ask" => Some(Command::Ask),
        "ask_line" => Some(Command::AskLine),
        "help" => Some(Command::Help),
        _ => None,
    }
}

/// Build the static `/help` message listing the supported commands.
fn build_help_message() -> String {
    let mut out = String::with_capacity(512);
    out.push_str("## PR-Agent Commands 🤖\n\n");
    out.push_str("| Command | Description |\n");
    out.push_str("|---------|-------------|\n");
    out.push_str("| `/review` | Review the PR: summary, key issues, and effort estimate |\n");
    out.push_str("| `/describe` | Generate a PR title, type, and description |\n");
    out.push_str("| `/improve` | Suggest committable code improvements |\n");
    out.push_str("| `/ask <question>` | Ask a free-form question about the PR |\n");
    out.push_str(
        "| `/ask_line <question>` | Ask about specific lines (reply to a line comment) |\n",
    );
    out.push_str("| `/help` | Show this help message |\n");
    out
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
/// `global_toml` / `repo_toml` are the org-level and repo-level `.pr_agent.toml`
/// contents already fetched by the caller. They are threaded all the way down so
/// that, when `args` also carries per-command overrides (`/command --key=value`),
/// the scoped re-load merges ALL layers (defaults → secrets → global → repo →
/// overrides → env). Previously the override re-load passed `None, None`, which
/// silently discarded the repo/global config whenever any override was present.
pub async fn handle_command(
    command: &str,
    provider: Arc<dyn GitProvider>,
    args: &HashMap<String, String>,
    global_toml: Option<&str>,
    repo_toml: Option<&str>,
) -> Result<(), PrAgentError> {
    // Separate config overrides (key=value flags) from tool data (_text, _diff_hunk, etc.)
    let config_overrides: HashMap<String, String> = args
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    match build_scoped_settings(&config_overrides, global_toml, repo_toml) {
        Some(scoped) => with_settings(scoped, dispatch(command, provider, args)).await,
        None => dispatch(command, provider, args).await,
    }
}

/// Build the scoped settings for a command execution, merging per-command
/// overrides on TOP of the global + repo `.pr_agent.toml` layers.
///
/// Returns `None` when there is nothing to scope (no overrides and no
/// repo/global TOML), in which case the caller dispatches against the ambient
/// settings. Including `global_toml`/`repo_toml` here is the fix for the bug
/// where any per-command override silently discarded the repo/global config.
fn build_scoped_settings(
    config_overrides: &HashMap<String, String>,
    global_toml: Option<&str>,
    repo_toml: Option<&str>,
) -> Option<Arc<Settings>> {
    if config_overrides.is_empty() && global_toml.is_none() && repo_toml.is_none() {
        return None;
    }

    Some(Arc::new(
        match load_settings(config_overrides, global_toml, repo_toml) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    ?config_overrides,
                    "failed to apply scoped settings, using current settings"
                );
                (*get_settings()).clone()
            }
        },
    ))
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
        Command::Help => {
            // Static help table — no AI call, mirrors the Python PRHelpMessage
            // else-branch.
            provider
                .publish_comment(&build_help_message(), false)
                .await
                .map(|_| ())
        }
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
    fn test_build_help_message_and_known() {
        // F2: /help is a known command and lists every supported command.
        assert!(is_known_command("help"));
        let msg = build_help_message();
        for cmd in [
            "/review",
            "/describe",
            "/improve",
            "/ask",
            "/ask_line",
            "/help",
        ] {
            assert!(msg.contains(cmd), "help should mention {cmd}");
        }
    }

    #[tokio::test]
    async fn test_dispatch_help_publishes_comment() {
        use crate::testing::mock_git::MockGitProvider;
        let provider = Arc::new(MockGitProvider::new());
        dispatch("help", provider.clone(), &HashMap::new())
            .await
            .unwrap();
        let calls = provider.get_calls();
        assert!(!calls.comments.is_empty(), "/help should publish a comment");
        assert!(calls.comments[0].0.contains("PR-Agent Commands"));
    }

    #[test]
    fn test_scoped_settings_overrides_preserve_repo_toml() {
        // Regression for C3: a per-command override must NOT discard the
        // repo-level `.pr_agent.toml`. Both layers have to survive.
        let mut overrides = HashMap::new();
        overrides.insert("config.temperature".to_string(), "0.9".to_string());
        let repo_toml = "[pr_reviewer]\nnum_max_findings = 7\n";

        let scoped = build_scoped_settings(&overrides, None, Some(repo_toml))
            .expect("overrides present → must produce scoped settings");

        // The CLI/comment override is applied...
        assert!((scoped.config.temperature - 0.9).abs() < 1e-6);
        // ...AND the repo toml is preserved (previously dropped to its default of 3).
        assert_eq!(scoped.pr_reviewer.num_max_findings, 7);
    }

    #[test]
    fn test_scoped_settings_none_when_nothing_to_scope() {
        // No overrides and no repo/global TOML → dispatch against ambient settings.
        assert!(build_scoped_settings(&HashMap::new(), None, None).is_none());
    }

    #[tokio::test]
    async fn test_select_model() {
        use crate::config::loader::with_settings;

        // Defaults: weak/reasoning empty → fall back to config.model.
        let settings =
            Arc::new(crate::config::loader::load_settings(&HashMap::new(), None, None).unwrap());
        let default_model = settings.config.model.clone();
        let (weak, reasoning) = with_settings(settings, async {
            (
                select_model(ModelKind::Weak),
                select_model(ModelKind::Reasoning),
            )
        })
        .await;
        assert_eq!(weak, default_model);
        assert_eq!(reasoning, default_model);

        // Configured → the specific model is used.
        let mut overrides = HashMap::new();
        overrides.insert("config.model_weak".to_string(), "gpt-4o-mini".to_string());
        overrides.insert("config.model_reasoning".to_string(), "o3-mini".to_string());
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());
        let (weak, reasoning) = with_settings(settings, async {
            (
                select_model(ModelKind::Weak),
                select_model(ModelKind::Reasoning),
            )
        })
        .await;
        assert_eq!(weak, "gpt-4o-mini");
        assert_eq!(reasoning, "o3-mini");
    }

    #[tokio::test]
    async fn test_with_response_language() {
        use crate::config::loader::with_settings;

        // Default en-US → unchanged.
        let settings =
            Arc::new(crate::config::loader::load_settings(&HashMap::new(), None, None).unwrap());
        let unchanged = with_settings(settings, async { with_response_language("base") }).await;
        assert_eq!(unchanged, "base");

        // Non-default language → appended, with dedup on re-application.
        let mut overrides = HashMap::new();
        overrides.insert("config.response_language".to_string(), "pt-BR".to_string());
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());
        let (from_empty, with_base, reapplied) = with_settings(settings, async {
            let from_empty = with_response_language("");
            let with_base = with_response_language("Focus on X");
            let reapplied = with_response_language(&with_base);
            (from_empty, with_base, reapplied)
        })
        .await;

        assert!(
            from_empty.contains("pt-BR"),
            "empty base gets the instruction"
        );
        assert!(
            with_base.starts_with("Focus on X"),
            "existing instructions preserved"
        );
        assert!(with_base.contains("pt-BR"), "language instruction appended");
        assert_eq!(reapplied, with_base, "re-applying must not duplicate");
    }

    #[tokio::test]
    async fn test_pr_metadata_fetch_populates_fields() {
        // P2: the concurrent fetch still returns the correct fields.
        use crate::testing::mock_git::MockGitProvider;
        let provider = MockGitProvider::new().with_pr_description("My Title", "My Desc");
        let settings = crate::config::loader::load_settings(&HashMap::new(), None, None).unwrap();

        let meta = PrMetadata::fetch(&provider, &settings).await.unwrap();
        assert_eq!(meta.title, "My Title");
        assert_eq!(meta.description, "My Desc");
        assert_eq!(meta.branch, "feature/test");
    }

    #[tokio::test]
    async fn test_pr_metadata_prefers_configured_best_practices() {
        // P2: the best_practices gate is preserved — configured content wins and
        // the provider is not consulted for it.
        use crate::testing::mock_git::MockGitProvider;
        let provider = MockGitProvider::new();
        let mut overrides = HashMap::new();
        overrides.insert("best_practices.content".to_string(), "MY BP".to_string());
        let settings = crate::config::loader::load_settings(&overrides, None, None).unwrap();

        let meta = PrMetadata::fetch(&provider, &settings).await.unwrap();
        assert_eq!(meta.best_practices, "MY BP");
    }

    #[test]
    fn test_scoped_settings_repo_toml_only() {
        // Repo TOML with no overrides must still scope (so the layer applies).
        let repo_toml = "[pr_reviewer]\nnum_max_findings = 5\n";
        let scoped = build_scoped_settings(&HashMap::new(), None, Some(repo_toml))
            .expect("repo toml present → must produce scoped settings");
        assert_eq!(scoped.pr_reviewer.num_max_findings, 5);
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

    #[test]
    fn test_get_user_labels_filters_standard_set() {
        let settings = Settings::default();
        let current = vec![
            "Bug fix".to_string(),
            "Enhancement".to_string(),
            "needs-review".to_string(),
            "priority/high".to_string(),
        ];
        let user = get_user_labels(&current, &settings);
        // Standard PR-type labels dropped (case-insensitive); user labels kept.
        assert_eq!(user, vec!["needs-review", "priority/high"]);
    }

    #[test]
    fn test_get_user_labels_filters_custom_labels() {
        let mut settings = Settings::default();
        settings.custom_labels.insert(
            "Performance".into(),
            CustomLabelEntry {
                description: "Perf".into(),
            },
        );
        let current = vec![
            "Performance".to_string(),
            // Differs only by case from the configured custom label — must
            // still be filtered out.
            "performance".to_string(),
            "tests".to_string(),
            "keep-me".to_string(),
        ];
        let user = get_user_labels(&current, &settings);
        assert_eq!(user, vec!["keep-me"]);
    }

    #[tokio::test]
    async fn test_validate_repo_settings_toml_valid_passes_through() {
        use crate::testing::mock_git::MockGitProvider;

        let provider = MockGitProvider::new();
        let toml = "[pr_reviewer]\nnum_max_findings = 5\n".to_string();
        let result = validate_repo_settings_toml(&provider, Some(toml.clone())).await;
        assert_eq!(result, Some(toml));
        // No error comment published for valid TOML.
        assert!(provider.get_calls().comments.is_empty());
    }

    #[tokio::test]
    async fn test_validate_repo_settings_toml_invalid_reports_and_drops() {
        use crate::testing::mock_git::MockGitProvider;

        let provider = MockGitProvider::new();
        // Missing closing bracket / dangling key — invalid TOML.
        let bad = "[pr_reviewer\nnum_max_findings = ".to_string();
        let result = validate_repo_settings_toml(&provider, Some(bad)).await;
        // Invalid TOML is dropped so the run continues with defaults.
        assert_eq!(result, None);
        // An error comment was published to the PR.
        let calls = provider.get_calls();
        assert_eq!(calls.comments.len(), 1);
        assert!(
            calls.comments[0]
                .0
                .contains("failed to apply repo settings"),
            "comment should explain the config error: {}",
            calls.comments[0].0
        );
        assert!(
            calls.comments[0].0.contains("```toml"),
            "comment should include the offending config content"
        );
    }

    #[tokio::test]
    async fn test_validate_repo_settings_toml_none_and_empty() {
        use crate::testing::mock_git::MockGitProvider;

        let provider = MockGitProvider::new();
        assert_eq!(validate_repo_settings_toml(&provider, None).await, None);
        assert_eq!(
            validate_repo_settings_toml(&provider, Some(String::new())).await,
            Some(String::new())
        );
        assert!(provider.get_calls().comments.is_empty());
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
        // Note: "help" IS known (see test_build_help_message_and_known).
        for cmd in ["qa-verify", "qa-review", "deploy", "", "REVIEW"] {
            assert!(
                !is_known_command(cmd),
                "'{cmd}' should NOT be a known command"
            );
        }
    }
}
