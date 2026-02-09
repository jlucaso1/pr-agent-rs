use std::collections::HashMap;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::config::loader::init_settings;
use crate::error::PrAgentError;
use crate::git::github::GithubProvider;
use crate::tools;

/// PR-Agent: AI-powered code review and PR analysis tool.
#[derive(Parser, Debug)]
#[command(name = "pr-agent", version, about)]
pub struct Cli {
    /// The URL of the PR to review.
    #[arg(long)]
    pub pr_url: Option<String>,

    /// The URL of the issue to review.
    #[arg(long)]
    pub issue_url: Option<String>,

    #[command(subcommand)]
    pub command: Command,

    /// Extra arguments passed as config overrides (--section.key=value).
    /// Place after `--` separator: `pr-agent review --pr_url=<url> -- --config.model=gpt-4`
    #[arg(last = true, allow_hyphen_values = true, global = true)]
    pub rest: Vec<String>,
}

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum Command {
    /// Add a review with summary and suggestions.
    #[command(alias = "review_pr")]
    Review,
    /// Automatic review (triggered by CI/webhooks).
    AutoReview,
    /// Answer mode (for issue comments).
    Answer,
    /// Modify PR title and description.
    #[command(alias = "describe_pr")]
    Describe,
    /// Suggest code improvements.
    #[command(alias = "improve_code")]
    Improve,
    /// Ask a question about the PR.
    #[command(alias = "ask_question")]
    Ask,
    /// Ask questions at specific lines.
    AskLine,
    /// Update changelog based on PR.
    UpdateChangelog,
    /// Add documentation.
    AddDocs,
    /// Generate PR labels.
    GenerateLabels,
    /// Get help on issues/PRs.
    HelpDocs,
    /// Find similar issues.
    SimilarIssue,
    /// View/manage configuration.
    #[command(alias = "settings")]
    Config,
    /// Start the webhook server.
    Serve,
    /// Check if the server is healthy (for Docker HEALTHCHECK).
    Health,
}

impl Command {
    /// Return the canonical tool name used in config and prompts.
    pub fn canonical_name(&self) -> &'static str {
        match self {
            Command::Review => "review",
            Command::AutoReview => "auto_review",
            Command::Answer => "answer",
            Command::Describe => "describe",
            Command::Improve => "improve",
            Command::Ask => "ask",
            Command::AskLine => "ask_line",
            Command::UpdateChangelog => "update_changelog",
            Command::AddDocs => "add_docs",
            Command::GenerateLabels => "generate_labels",
            Command::HelpDocs => "help_docs",
            Command::SimilarIssue => "similar_issue",
            Command::Config => "config",
            Command::Serve => "serve",
            Command::Health => "health",
        }
    }
}

/// Forbidden config keys that cannot be overridden via CLI args or webhook comments.
///
/// These are security-sensitive — exposing them to untrusted input (PR comments)
/// could allow secrets exfiltration or provider redirection.
pub const FORBIDDEN_OVERRIDE_KEYS: &[&str] = &[
    "shared_secret",
    "user",
    "system",
    "enable_comment_approval",
    "enable_manual_approval",
    "enable_auto_approval",
    "approve_pr_on_self_review",
    "base_url",
    "url",
    "app_name",
    "secret_provider",
    "git_provider",
    "skip_keys",
    "openai.key",
    "analytics_folder",
    "uri",
    "app_id",
    "webhook_secret",
    "bearer_token",
    "personal_access_token",
    "override_deployment_type",
    "private_key",
    "local_cache_path",
    "enable_local_cache",
    "jira_base_url",
    "api_base",
    "api_type",
    "api_version",
];

/// Check if a config key is forbidden for override.
///
/// Returns `Some(matched_forbidden_key)` if the key matches, `None` if allowed.
pub fn check_forbidden_key(key: &str) -> Option<&'static str> {
    let key_lower = key.to_lowercase();
    let segments: Vec<&str> = key_lower.split('.').collect();
    FORBIDDEN_OVERRIDE_KEYS
        .iter()
        .find(|&&forbidden| key_lower == forbidden || segments.contains(&forbidden))
        .copied()
}

/// Parse the `rest` args into a HashMap of config overrides.
/// Format: `--section.key=value` or `--section__key=value` (double underscores → dots).
fn parse_config_overrides(rest: &[String]) -> Result<HashMap<String, String>, PrAgentError> {
    let mut overrides = HashMap::new();

    for arg in rest {
        let stripped = arg.trim_start_matches('-');
        if stripped.is_empty() {
            continue;
        }

        // Convert double underscore to dot (e.g., pr_reviewer__num_code_suggestions → pr_reviewer.num_code_suggestions)
        let stripped = stripped.replace("__", ".");

        if let Some((key, value)) = stripped.split_once('=') {
            if let Some(forbidden) = check_forbidden_key(key) {
                return Err(PrAgentError::Other(format!(
                    "forbidden CLI override: '{key}' (matches '{forbidden}')"
                )));
            }

            overrides.insert(key.to_string(), value.to_string());
        }
        // Non-config args (no `=`) are ignored for config; commands can inspect rest directly
    }

    Ok(overrides)
}

pub async fn run() -> Result<(), PrAgentError> {
    let cli = Cli::parse();

    // Health check runs before any settings init — fast, lightweight.
    if cli.command == Command::Health {
        return health_check().await;
    }

    let config_overrides = parse_config_overrides(&cli.rest)?;

    // Bootstrap settings (no repo/global settings yet — need provider to fetch them)
    let settings = init_settings(&config_overrides, None, None)?;

    let pr_url = cli.pr_url.as_deref().or(cli.issue_url.as_deref());

    tracing::info!(
        command = cli.command.canonical_name(),
        pr_url = pr_url,
        overrides = config_overrides.len(),
        model = %settings.config.model,
        "starting pr-agent"
    );

    match cli.command {
        Command::Config => {
            println!("Model: {}", settings.config.model);
            println!("Temperature: {}", settings.config.temperature);
            println!("Git provider: {}", settings.config.git_provider);
            println!("Max model tokens: {}", settings.config.max_model_tokens);
        }
        Command::Serve => {
            crate::server::start_server().await?;
        }
        _ => {
            let url = pr_url.ok_or_else(|| {
                PrAgentError::Other(format!(
                    "--pr-url is required for {}",
                    cli.command.canonical_name()
                ))
            })?;

            let provider: Arc<dyn crate::git::GitProvider> =
                Arc::new(GithubProvider::new(url).await?);

            // Load global org-level and repo-level .pr_agent.toml if enabled
            let global_toml = if settings.config.use_global_settings_file {
                match provider.get_global_settings().await {
                    Ok(Some(toml)) => {
                        tracing::info!("loaded global org-level .pr_agent.toml");
                        Some(toml)
                    }
                    Ok(None) => {
                        tracing::debug!("no global org-level .pr_agent.toml found");
                        None
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to fetch global settings, continuing without");
                        None
                    }
                }
            } else {
                None
            };

            let repo_toml = if settings.config.use_repo_settings_file {
                match provider.get_repo_settings().await {
                    Ok(Some(toml)) => {
                        tracing::info!("loaded repo-level .pr_agent.toml");
                        Some(toml)
                    }
                    Ok(None) => {
                        tracing::debug!("no repo-level .pr_agent.toml found");
                        None
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to fetch repo settings, continuing without");
                        None
                    }
                }
            } else {
                None
            };

            // Re-initialize settings with global + repo overrides if either was found
            if global_toml.is_some() || repo_toml.is_some() {
                init_settings(
                    &config_overrides,
                    global_toml.as_deref(),
                    repo_toml.as_deref(),
                )?;
            }

            tools::handle_command(cli.command.canonical_name(), provider, &config_overrides)
                .await?;
        }
    }

    Ok(())
}

/// Lightweight health check: GET http://127.0.0.1:$PORT/ with a 5s timeout.
///
/// Used by Docker HEALTHCHECK in distroless images where curl is unavailable.
async fn health_check() -> Result<(), PrAgentError> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| PrAgentError::Other(format!("health check failed: {e}")))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| PrAgentError::Other(format!("health check failed: {e}")))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(PrAgentError::Other(format!(
            "health check failed: status {}",
            resp.status()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_overrides() {
        let args = vec![
            "--pr_reviewer.num_max_findings=10".into(),
            "--config.temperature=0.5".into(),
            "--config__model=gpt-4".into(), // double underscore
        ];
        let overrides = parse_config_overrides(&args).unwrap();
        assert_eq!(overrides.get("pr_reviewer.num_max_findings").unwrap(), "10");
        assert_eq!(overrides.get("config.temperature").unwrap(), "0.5");
        assert_eq!(overrides.get("config.model").unwrap(), "gpt-4");
    }

    #[test]
    fn test_forbidden_overrides() {
        let args = vec!["--openai.key=sk-secret".into()];
        let result = parse_config_overrides(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("forbidden"));
    }

    #[test]
    fn test_command_canonical_names() {
        assert_eq!(Command::Review.canonical_name(), "review");
        assert_eq!(Command::AutoReview.canonical_name(), "auto_review");
        assert_eq!(Command::Describe.canonical_name(), "describe");
        assert_eq!(Command::Improve.canonical_name(), "improve");
        assert_eq!(Command::Ask.canonical_name(), "ask");
        assert_eq!(Command::Config.canonical_name(), "config");
    }
}
