pub mod describe;
pub mod improve;
pub mod review;

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use minijinja::Value;

use crate::config::loader::{get_settings, load_settings, with_settings};
use crate::config::types::{CustomLabelEntry, Settings};
use crate::error::PrAgentError;
use crate::git::GitProvider;

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
    let mut vars = HashMap::new();
    vars.insert("title".into(), Value::from(meta.title.as_str()));
    vars.insert("branch".into(), Value::from(meta.branch.as_str()));
    vars.insert("description".into(), Value::from(meta.description.as_str()));
    vars.insert("language".into(), Value::from(""));
    vars.insert("diff".into(), Value::from(diff));
    vars.insert(
        "commit_messages_str".into(),
        Value::from(meta.commit_messages.as_str()),
    );
    vars.insert(
        "best_practices_content".into(),
        Value::from(meta.best_practices.as_str()),
    );
    vars.insert(
        "repo_metadata".into(),
        Value::from(meta.repo_metadata.as_str()),
    );
    vars
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

/// Parse a "/command --arg=value" string into (command_name, args_overrides).
///
/// Splits on whitespace and extracts `--key=value` pairs as config overrides.
pub fn parse_command(input: &str) -> (String, HashMap<String, String>) {
    let trimmed = input.trim();
    let mut parts = trimmed.split_whitespace();
    let command = parts
        .next()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_lowercase();

    let mut overrides = HashMap::new();
    for part in parts {
        let stripped = part.trim_start_matches('-');
        // Convert double underscore to dot (e.g., pr_reviewer__num_code_suggestions → pr_reviewer.num_code_suggestions)
        let stripped = stripped.replace("__", ".");
        if let Some((key, value)) = stripped.split_once('=') {
            overrides.insert(key.to_string(), value.to_string());
        }
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
    // If there are per-command args, scope them as settings overrides
    if !args.is_empty() {
        let current = get_settings();
        // Rebuild settings: start from current base, apply these args as CLI overrides
        let scoped =
            Arc::new(load_settings(args, None, None).unwrap_or_else(|_| (*current).clone()));
        return with_settings(scoped, dispatch(command, provider)).await;
    }

    dispatch(command, provider).await
}

async fn dispatch(command: &str, provider: Arc<dyn GitProvider>) -> Result<(), PrAgentError> {
    match command {
        "review" | "auto_review" | "review_pr" => review::PRReviewer::new(provider).run().await,
        "describe" | "describe_pr" => describe::PRDescription::new(provider).run().await,
        "improve" | "improve_code" => improve::PRCodeSuggestions::new(provider).run().await,
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
}
