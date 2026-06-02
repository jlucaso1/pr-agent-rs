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
    /// Place after `--` separator: `pr-agent --pr-url=<url> review -- --config.model=gpt-4`
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
            Command::Describe => "describe",
            Command::Improve => "improve",
            Command::Ask => "ask",
            Command::AskLine => "ask_line",
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

/// Normalize a `--section.key=value` (or `--section__key=value`) override token
/// into its `(key, value)` pair, or `None` if it isn't a `key=value` override.
///
/// Strips leading dashes and converts double underscores to dots. The
/// forbidden-key check is intentionally left to the caller, because the CLI
/// errors on a forbidden key while the webhook drops it with a warning.
pub fn parse_override_token(token: &str) -> Option<(String, String)> {
    let stripped = token.trim_start_matches('-').replace("__", ".");
    let (key, value) = stripped.split_once('=')?;
    Some((key.to_string(), value.to_string()))
}

/// Parse the `rest` args into a HashMap of config overrides.
/// Format: `--section.key=value` or `--section__key=value` (double underscores → dots).
fn parse_config_overrides(rest: &[String]) -> Result<HashMap<String, String>, PrAgentError> {
    let mut overrides = HashMap::new();

    for arg in rest {
        // Non-config args (no `=`) are ignored for config; commands inspect rest directly.
        if let Some((key, value)) = parse_override_token(arg) {
            if let Some(forbidden) = check_forbidden_key(&key) {
                return Err(PrAgentError::Other(format!(
                    "forbidden CLI override: '{key}' (matches '{forbidden}')"
                )));
            }
            overrides.insert(key, value);
        }
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
            // Defense-in-depth: reject any command without a dispatch path before
            // doing network/auth work, mirroring the webhook's is_known_command
            // guard. After removing the unimplemented scaffolding variants this is
            // future-proofing — a new CLI variant added without wiring
            // `resolve_command` fails fast instead of after creating the provider.
            let name = cli.command.canonical_name();
            if !tools::is_known_command(name) {
                return Err(PrAgentError::Other(format!(
                    "command '{name}' is not implemented"
                )));
            }

            let url = pr_url
                .ok_or_else(|| PrAgentError::Other(format!("--pr-url is required for {name}")))?;

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

            // Re-initialize global settings with global + repo overrides if either
            // was found, so any non-scoped reads also see the merged config.
            if global_toml.is_some() || repo_toml.is_some() {
                init_settings(
                    &config_overrides,
                    global_toml.as_deref(),
                    repo_toml.as_deref(),
                )?;
            }

            // Thread the global/repo TOML into handle_command so its scoped
            // re-load (when overrides are present) keeps all config layers.
            tools::handle_command(
                cli.command.canonical_name(),
                provider,
                &config_overrides,
                global_toml.as_deref(),
                repo_toml.as_deref(),
            )
            .await?;
        }
    }

    Ok(())
}

/// TCP connect health check for Docker HEALTHCHECK.
async fn health_check() -> Result<(), PrAgentError> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = std::time::Duration::from_secs(5);
    std::net::TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| PrAgentError::Other(format!("health check failed: {e}")))?;
    Ok(())
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
    fn test_parse_override_token() {
        assert_eq!(
            parse_override_token("--config.model=gpt-4"),
            Some(("config.model".into(), "gpt-4".into()))
        );
        // Double underscores become dots.
        assert_eq!(
            parse_override_token("--config__model=gpt-4"),
            Some(("config.model".into(), "gpt-4".into()))
        );
        // Not a key=value override.
        assert_eq!(parse_override_token("review"), None);
        assert_eq!(parse_override_token("--flag"), None);
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

    #[test]
    fn test_all_cli_commands_have_a_dispatch_path() {
        // Every exposed CLI subcommand must either be handled directly in run()
        // (config/serve/health) or be a dispatchable tool command. This guards
        // against re-introducing "announced but always-failing" commands (A1/F1).
        let directly_handled = ["config", "serve", "health"];
        let all = [
            Command::Review,
            Command::AutoReview,
            Command::Describe,
            Command::Improve,
            Command::Ask,
            Command::AskLine,
            Command::Config,
            Command::Serve,
            Command::Health,
        ];
        for cmd in all {
            let name = cmd.canonical_name();
            assert!(
                directly_handled.contains(&name) || tools::is_known_command(name),
                "CLI command '{name}' has no dispatch path"
            );
        }
    }
}
