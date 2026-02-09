use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::config::loader::{get_settings, load_settings, with_settings};
use crate::config::types::Settings;
use crate::git::GitProvider;
use crate::git::github::GithubProvider;
use crate::git::types::CommentId;
use crate::tools;

type HmacSha256 = Hmac<Sha256>;

/// Main webhook handler: POST /api/v1/github_webhooks
///
/// Steps:
/// 1. Verify HMAC-SHA256 signature
/// 2. Parse event type and action
/// 3. Dispatch to appropriate handler in a background task
/// 4. Return 200 immediately
pub async fn handle_github_webhook(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    // 1. Verify signature
    let settings = get_settings();
    let secret = &settings.github.webhook_secret;

    if secret.is_empty() {
        tracing::error!("webhook_secret is not configured — rejecting request for safety");
        return (StatusCode::FORBIDDEN, "webhook secret not configured").into_response();
    }

    {
        let signature = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if let Err(e) = verify_signature(&body, secret, signature) {
            tracing::warn!(error = %e, "webhook signature verification failed");
            return (StatusCode::FORBIDDEN, "signature verification failed").into_response();
        }
    }

    // 2. Parse body and event type
    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse webhook payload");
            return (StatusCode::BAD_REQUEST, "invalid JSON").into_response();
        }
    };

    let action = payload["action"].as_str().unwrap_or("").to_string();

    tracing::info!(event = %event, action = %action, "received webhook");

    // 3. Dispatch in background task
    tokio::spawn(async move {
        if let Err(e) = dispatch_event(&event, &action, &payload).await {
            tracing::error!(event = %event, action = %action, error = %e, "webhook handler failed");
        }
    });

    // 4. Return 200 immediately
    (StatusCode::OK, "ok").into_response()
}

/// Verify the HMAC-SHA256 signature from GitHub.
///
/// Compares the provided `sha256=...` header against the HMAC of the request body.
fn verify_signature(body: &[u8], secret: &str, signature_header: &str) -> Result<(), String> {
    let expected_prefix = "sha256=";
    let signature_hex = signature_header
        .strip_prefix(expected_prefix)
        .ok_or_else(|| "missing sha256= prefix".to_string())?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("invalid HMAC key: {e}"))?;
    mac.update(body);

    let expected =
        hex::decode(signature_hex).map_err(|e| format!("invalid hex in signature: {e}"))?;

    mac.verify_slice(&expected)
        .map_err(|_| "HMAC verification failed".to_string())
}

/// Route webhook events to the appropriate handler.
///
/// Route webhook events to the appropriate tool handler.
async fn dispatch_event(
    event: &str,
    action: &str,
    payload: &serde_json::Value,
) -> Result<(), crate::error::PrAgentError> {
    let settings = get_settings();

    match event {
        "pull_request" => {
            let pr_url = extract_pr_url(payload)?;

            // Check ignore filters before processing
            let pr_title = payload["pull_request"]["title"].as_str().unwrap_or("");
            let pr_author = payload["pull_request"]["user"]["login"]
                .as_str()
                .unwrap_or("");
            if should_ignore_pr(&settings, pr_title, pr_author) {
                tracing::info!(pr_url = %pr_url, pr_title, pr_author, "ignoring PR (matched ignore filter)");
                return Ok(());
            }

            if settings
                .github_app
                .handle_pr_actions
                .contains(&action.to_string())
            {
                // New PR opened / reopened / ready_for_review
                tracing::info!(pr_url = %pr_url, action, "handling PR event");
                run_commands(&pr_url, &settings.github_app.pr_commands).await?;
            } else if action == "synchronize" && settings.github_app.handle_push_trigger {
                // New commits pushed
                tracing::info!(pr_url = %pr_url, "handling push trigger");
                run_commands(&pr_url, &settings.github_app.push_commands).await?;
            } else {
                tracing::debug!(action, "ignoring pull_request action");
            }
        }
        "issue_comment" => {
            if action == "edited" {
                // Check for self-review checkbox toggle
                return handle_checkbox_edit(payload).await;
            }

            if action != "created" {
                tracing::debug!(action, "ignoring issue_comment action");
                return Ok(());
            }

            // Only handle comments on PRs (have pull_request key)
            if payload["issue"]["pull_request"].is_null() {
                tracing::debug!("ignoring comment on non-PR issue");
                return Ok(());
            }

            let comment_body = payload["comment"]["body"].as_str().unwrap_or("").trim();

            if !comment_body.starts_with('/') {
                tracing::debug!("ignoring non-command comment");
                return Ok(());
            }

            let pr_url = extract_pr_url_from_issue(payload)?;
            tracing::info!(pr_url = %pr_url, command = comment_body, "handling comment command");

            // Add eyes reaction to the comment
            let comment_id = payload["comment"]["id"].as_u64().unwrap_or(0);
            let provider: Arc<dyn GitProvider> = Arc::new(GithubProvider::new(&pr_url).await?);
            let _ = provider.add_eyes_reaction(comment_id, false).await;

            // Fetch global + repo settings and scope them for this command
            let scoped_settings = fetch_scoped_settings(provider.as_ref(), &settings).await;

            // Parse and dispatch command
            let (command, args) = tools::parse_command(comment_body);
            if let Some(s) = scoped_settings {
                with_settings(s, tools::handle_command(&command, provider, &args)).await?;
            } else {
                tools::handle_command(&command, provider, &args).await?;
            }
        }
        _ => {
            tracing::debug!(event, "ignoring unsupported event type");
        }
    }

    Ok(())
}

/// Check if a PR should be ignored based on title regex patterns or author list.
///
/// Returns true if the PR should be skipped based on configured title/author filters.
fn should_ignore_pr(settings: &Settings, title: &str, author: &str) -> bool {
    for pattern in &settings.config.ignore_pr_title {
        match crate::util::get_or_compile_regex(pattern) {
            Some(re) => {
                if re.is_match(title) {
                    return true;
                }
            }
            None => {
                tracing::warn!(pattern, "invalid ignore_pr_title regex");
            }
        }
    }
    if !author.is_empty()
        && settings
            .config
            .ignore_pr_authors
            .iter()
            .any(|a| a == author)
    {
        return true;
    }
    false
}

/// Fetch an optional TOML settings file, logging success/failure.
async fn fetch_optional_toml(
    enabled: bool,
    fetch: impl std::future::Future<Output = Result<Option<String>, crate::error::PrAgentError>>,
    label: &str,
) -> Option<String> {
    if !enabled {
        return None;
    }
    match fetch.await {
        Ok(Some(toml)) => {
            tracing::info!("loaded {label} .pr_agent.toml for webhook request");
            Some(toml)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "failed to fetch {label} settings");
            None
        }
    }
}

/// Fetch global org-level and repo-level settings, then build a scoped `Arc<Settings>`.
///
/// Returns `Some(settings)` if any overrides were loaded, `None` if neither exists.
async fn fetch_scoped_settings(
    provider: &dyn GitProvider,
    settings: &Settings,
) -> Option<Arc<Settings>> {
    let global_toml = fetch_optional_toml(
        settings.config.use_global_settings_file,
        provider.get_global_settings(),
        "global org-level",
    )
    .await;

    let repo_toml = fetch_optional_toml(
        settings.config.use_repo_settings_file,
        provider.get_repo_settings(),
        "repo-level",
    )
    .await;

    if global_toml.is_some() || repo_toml.is_some() {
        match load_settings(
            &std::collections::HashMap::new(),
            global_toml.as_deref(),
            repo_toml.as_deref(),
        ) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::error!(error = %e, "failed to load scoped settings, using defaults");
                None
            }
        }
    } else {
        None
    }
}

/// Run a list of commands against a PR (e.g. pr_commands or push_commands).
///
/// Fetches global org-level and repo-level `.pr_agent.toml` once, then runs
/// all commands within a scoped settings context.
async fn run_commands(pr_url: &str, commands: &[String]) -> Result<(), crate::error::PrAgentError> {
    let provider: Arc<dyn GitProvider> = Arc::new(GithubProvider::new(pr_url).await?);
    let settings = get_settings();

    // Fetch global + repo settings once for all commands in this PR
    let scoped_settings = fetch_scoped_settings(provider.as_ref(), &settings).await;

    for cmd_str in commands {
        let (command, args) = tools::parse_command(cmd_str);
        let cmd_provider: Arc<dyn GitProvider> = Arc::new(GithubProvider::new(pr_url).await?);

        tracing::info!(command = %command, "running auto-command");
        let result = if let Some(ref s) = scoped_settings {
            with_settings(
                s.clone(),
                tools::handle_command(&command, cmd_provider, &args),
            )
            .await
        } else {
            tools::handle_command(&command, cmd_provider, &args).await
        };
        if let Err(e) = result {
            tracing::error!(command = %command, error = %e, "auto-command failed");
            // Continue with other commands even if one fails
        }
    }
    Ok(())
}

/// Handle an `issue_comment` `edited` event — detect self-review checkbox toggle.
///
/// When the PR author checks the self-review checkbox (added by the improve tool),
/// this handler can auto-approve the PR and/or post a confirmation.
async fn handle_checkbox_edit(
    payload: &serde_json::Value,
) -> Result<(), crate::error::PrAgentError> {
    // Only handle comments on PRs
    if payload["issue"]["pull_request"].is_null() {
        return Ok(());
    }

    let comment_body = payload["comment"]["body"].as_str().unwrap_or("");

    // Check if this comment contains a self-review checkbox marker
    let action = detect_self_review_action(comment_body);
    if action == SelfReviewAction::None {
        return Ok(());
    }

    // Check if the checkbox is actually checked
    if !is_self_review_checked(comment_body) {
        tracing::debug!("self-review checkbox unchecked, ignoring");
        return Ok(());
    }

    // Verify the editor is the PR author
    let sender = payload["sender"]["login"].as_str().unwrap_or("");
    let pr_author = payload["issue"]["user"]["login"].as_str().unwrap_or("");

    if sender.is_empty() || pr_author.is_empty() || sender != pr_author {
        tracing::info!(
            sender,
            pr_author,
            "self-review checkbox checked by non-author, ignoring"
        );
        return Ok(());
    }

    let pr_url = extract_pr_url_from_issue(payload)?;
    tracing::info!(pr_url = %pr_url, sender, action = ?action, "self-review checkbox checked by author");

    let provider: Arc<dyn GitProvider> = Arc::new(GithubProvider::new(&pr_url).await?);

    // Load repo/global settings so flags like approve_pr_on_self_review are respected
    let base_settings = get_settings();
    let settings = fetch_scoped_settings(provider.as_ref(), &base_settings)
        .await
        .unwrap_or(base_settings);

    // Auto-approve if configured
    if matches!(
        action,
        SelfReviewAction::Approve | SelfReviewAction::ApproveAndFold
    ) && settings.pr_code_suggestions.approve_pr_on_self_review
    {
        match provider.auto_approve().await {
            Ok(true) => {
                let _ = provider
                    .publish_comment("PR auto-approved after author self-review.", false)
                    .await;
            }
            Ok(false) => {
                tracing::warn!("auto-approve returned false (unsupported by provider)");
            }
            Err(e) => {
                tracing::error!(error = %e, "auto-approve failed");
                let _ = provider
                    .publish_comment(
                        "Failed to auto-approve PR after self-review. Check bot permissions.",
                        false,
                    )
                    .await;
            }
        }
    }

    // Fold suggestions comment if configured
    if matches!(
        action,
        SelfReviewAction::Fold | SelfReviewAction::ApproveAndFold
    ) && settings.pr_code_suggestions.fold_suggestions_on_self_review
    {
        fold_suggestions_comment(provider.as_ref()).await?;
    }

    Ok(())
}

/// Find the improve suggestions comment and collapse it inside `<details>`.
///
/// Searches PR comments for the `<!-- pr-agent:improve -->` marker, then wraps
/// the entire comment body in a collapsible section via `edit_comment()`.
async fn fold_suggestions_comment(
    provider: &dyn GitProvider,
) -> Result<(), crate::error::PrAgentError> {
    let comments = provider.get_issue_comments().await?;
    for comment in &comments {
        if let Some(folded) = fold_comment_body(&comment.body) {
            provider
                .edit_comment(&CommentId(comment.id.to_string()), &folded)
                .await?;
            tracing::info!(comment_id = comment.id, "folded suggestions comment");
            return Ok(());
        }
    }
    tracing::info!("no improve comment found to fold");
    Ok(())
}

/// Transform an improve comment body into its folded (collapsed) form.
///
/// Returns `Some(new_body)` if the comment should be folded, `None` if it
/// doesn't match or is already folded.
fn fold_comment_body(body: &str) -> Option<String> {
    let marker = "<!-- pr-agent:improve -->";
    if !body.trim_start().starts_with(marker) {
        return None;
    }
    // Already folded — don't double-wrap
    if body.contains("<details><summary>Code suggestions") {
        return None;
    }
    Some(format!(
        "<details><summary>Code suggestions</summary>\n\n{body}\n\n</details>"
    ))
}

/// What action the self-review checkbox indicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelfReviewAction {
    None,
    Approve,
    Fold,
    ApproveAndFold,
}

/// Detect which self-review action is embedded in the comment body.
///
/// Looks for HTML comment markers added by `append_self_review_checkbox()`.
fn detect_self_review_action(body: &str) -> SelfReviewAction {
    if body.contains("<!-- approve and fold suggestions self-review -->") {
        SelfReviewAction::ApproveAndFold
    } else if body.contains("<!-- approve pr self-review -->") {
        SelfReviewAction::Approve
    } else if body.contains("<!-- fold suggestions self-review -->") {
        SelfReviewAction::Fold
    } else {
        SelfReviewAction::None
    }
}

/// Check if the self-review checkbox in the comment body is checked.
///
/// Searches for `- [x]` on the same line as an exact self-review marker.
fn is_self_review_checked(body: &str) -> bool {
    const MARKERS: &[&str] = &[
        "<!-- approve and fold suggestions self-review -->",
        "<!-- approve pr self-review -->",
        "<!-- fold suggestions self-review -->",
    ];
    for line in body.lines() {
        if MARKERS.iter().any(|m| line.contains(m)) {
            let trimmed = line.trim();
            if trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]") {
                return true;
            }
        }
    }
    false
}

/// Extract the PR URL from a pull_request webhook event payload.
fn extract_pr_url(payload: &serde_json::Value) -> Result<String, crate::error::PrAgentError> {
    payload["pull_request"]["html_url"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| {
            crate::error::PrAgentError::Other("missing pull_request.html_url in payload".into())
        })
}

/// Extract the PR URL from an issue_comment webhook event payload.
fn extract_pr_url_from_issue(
    payload: &serde_json::Value,
) -> Result<String, crate::error::PrAgentError> {
    // The issue_comment event has issue.pull_request.html_url
    payload["issue"]["pull_request"]["html_url"]
        .as_str()
        .map(String::from)
        .or_else(|| {
            // Fallback: construct from issue URL
            payload["issue"]["html_url"].as_str().map(String::from)
        })
        .ok_or_else(|| {
            crate::error::PrAgentError::Other(
                "missing pull request URL in issue_comment payload".into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_signature_valid() {
        let body = b"test payload";
        let secret = "mysecret";

        // Compute expected signature
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let result = mac.finalize();
        let signature = format!("sha256={}", hex::encode(result.into_bytes()));

        assert!(verify_signature(body, secret, &signature).is_ok());
    }

    #[test]
    fn test_verify_signature_invalid() {
        let body = b"test payload";
        let secret = "mysecret";
        let bad_sig = "sha256=0000000000000000000000000000000000000000000000000000000000000000";

        assert!(verify_signature(body, secret, bad_sig).is_err());
    }

    #[test]
    fn test_verify_signature_missing_prefix() {
        let body = b"test payload";
        let secret = "mysecret";

        assert!(verify_signature(body, secret, "invalid").is_err());
    }

    #[test]
    fn test_extract_pr_url() {
        let payload = serde_json::json!({
            "pull_request": {
                "html_url": "https://github.com/owner/repo/pull/1"
            }
        });
        let url = extract_pr_url(&payload).unwrap();
        assert_eq!(url, "https://github.com/owner/repo/pull/1");
    }

    #[test]
    fn test_detect_self_review_action_approve() {
        let body = "- [ ]  I reviewed <!-- approve pr self-review -->";
        assert_eq!(detect_self_review_action(body), SelfReviewAction::Approve);
    }

    #[test]
    fn test_detect_self_review_action_fold() {
        let body = "- [ ]  I reviewed <!-- fold suggestions self-review -->";
        assert_eq!(detect_self_review_action(body), SelfReviewAction::Fold);
    }

    #[test]
    fn test_detect_self_review_action_both() {
        let body = "- [ ]  I reviewed <!-- approve and fold suggestions self-review -->";
        assert_eq!(
            detect_self_review_action(body),
            SelfReviewAction::ApproveAndFold
        );
    }

    #[test]
    fn test_detect_self_review_action_none() {
        let body = "Just a normal comment";
        assert_eq!(detect_self_review_action(body), SelfReviewAction::None);
    }

    #[test]
    fn test_is_self_review_checked_true() {
        let body = "Some table\n\n- [x]  I reviewed <!-- approve pr self-review -->\n";
        assert!(is_self_review_checked(body));
    }

    #[test]
    fn test_is_self_review_checked_uppercase() {
        let body = "Some table\n\n- [X]  I reviewed <!-- approve pr self-review -->\n";
        assert!(is_self_review_checked(body));
    }

    #[test]
    fn test_is_self_review_checked_false() {
        let body = "Some table\n\n- [ ]  I reviewed <!-- approve pr self-review -->\n";
        assert!(!is_self_review_checked(body));
    }

    #[test]
    fn test_is_self_review_checked_no_marker() {
        let body = "Some table\n\n- [x] regular checkbox\n";
        assert!(!is_self_review_checked(body));
    }

    #[test]
    fn test_should_ignore_pr_title_regex() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_title = vec![r"^\[Auto\]".into(), r"^Auto".into()];

        assert!(should_ignore_pr(&settings, "[Auto] Update deps", "user1"));
        assert!(should_ignore_pr(&settings, "Auto merge from main", "user1"));
        assert!(!should_ignore_pr(
            &settings,
            "Fix authentication bug",
            "user1"
        ));
    }

    #[test]
    fn test_should_ignore_pr_author() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_authors = vec!["dependabot[bot]".into(), "renovate[bot]".into()];

        assert!(should_ignore_pr(
            &settings,
            "Update deps",
            "dependabot[bot]"
        ));
        assert!(should_ignore_pr(&settings, "Update deps", "renovate[bot]"));
        assert!(!should_ignore_pr(&settings, "Update deps", "human-dev"));
    }

    #[test]
    fn test_should_ignore_pr_empty_filters() {
        let settings = Settings::default();
        // Default has ignore_pr_title patterns but a normal title won't match
        assert!(!should_ignore_pr(&settings, "Normal PR title", "user1"));
    }

    #[test]
    fn test_extract_pr_url_from_issue() {
        let payload = serde_json::json!({
            "issue": {
                "html_url": "https://github.com/owner/repo/pull/1",
                "pull_request": {
                    "html_url": "https://github.com/owner/repo/pull/1"
                }
            }
        });
        let url = extract_pr_url_from_issue(&payload).unwrap();
        assert_eq!(url, "https://github.com/owner/repo/pull/1");
    }

    #[test]
    fn test_is_self_review_checked_false_positive_marker() {
        // A comment containing "<!-- approved by reviewer -->" should NOT trigger
        let body = "- [x] Fix applied <!-- approved by reviewer -->\n";
        assert!(
            !is_self_review_checked(body),
            "similar-looking marker must not trigger"
        );
    }

    #[test]
    fn test_detect_self_review_action_no_false_positive() {
        // Markers that look similar but aren't ours
        let body = "<!-- approved by CI --> some text <!-- folding section -->";
        assert_eq!(detect_self_review_action(body), SelfReviewAction::None);
    }

    #[test]
    fn test_fold_comment_body_leading_whitespace() {
        let body = "  <!-- pr-agent:improve -->\n## PR Code Suggestions\n\n| table |";
        let folded = fold_comment_body(body);
        assert!(folded.is_some(), "should fold despite leading whitespace");
        let folded = folded.unwrap();
        assert!(folded.starts_with("<details><summary>Code suggestions</summary>"));
        assert!(folded.contains(body));
    }

    #[test]
    fn test_fold_comment_body_basic() {
        let body = "<!-- pr-agent:improve -->\n## PR Code Suggestions\n\n| table |";
        let folded = fold_comment_body(body).unwrap();
        assert!(folded.starts_with("<details><summary>Code suggestions</summary>"));
        assert!(folded.ends_with("</details>"));
        assert!(folded.contains(body));
    }

    #[test]
    fn test_fold_comment_body_already_folded() {
        let body = "<details><summary>Code suggestions</summary>\n\n<!-- pr-agent:improve -->\n## PR Code Suggestions\n\n</details>";
        assert!(
            fold_comment_body(body).is_none(),
            "already folded comment must return None"
        );
    }

    #[test]
    fn test_fold_comment_body_not_improve_comment() {
        let body = "<!-- pr-agent:review -->\n## PR Reviewer Guide";
        assert!(
            fold_comment_body(body).is_none(),
            "non-improve comment must return None"
        );
        assert!(
            fold_comment_body("Just a regular comment").is_none(),
            "regular comment must return None"
        );
    }

    #[tokio::test]
    async fn test_fetch_scoped_settings_with_global_only() {
        use crate::testing::mock_git::MockGitProvider;
        let provider = MockGitProvider::new().with_global_settings(
            r#"
[pr_reviewer]
num_max_findings = 42
"#,
        );
        let base = Settings::default();
        let scoped = fetch_scoped_settings(&provider, &base).await;
        assert!(scoped.is_some());
        assert_eq!(scoped.unwrap().pr_reviewer.num_max_findings, 42);
    }

    #[tokio::test]
    async fn test_fetch_scoped_settings_with_repo_only() {
        use crate::testing::mock_git::MockGitProvider;
        let provider = MockGitProvider::new().with_repo_settings(
            r#"
[pr_reviewer]
num_max_findings = 7
"#,
        );
        let base = Settings::default();
        let scoped = fetch_scoped_settings(&provider, &base).await;
        assert!(scoped.is_some());
        assert_eq!(scoped.unwrap().pr_reviewer.num_max_findings, 7);
    }

    #[tokio::test]
    async fn test_fetch_scoped_settings_repo_overrides_global() {
        use crate::testing::mock_git::MockGitProvider;
        let provider = MockGitProvider::new()
            .with_global_settings(
                r#"
[pr_reviewer]
num_max_findings = 42
extra_instructions = "Org rule"
"#,
            )
            .with_repo_settings(
                r#"
[pr_reviewer]
num_max_findings = 3
"#,
            );
        let base = Settings::default();
        let scoped = fetch_scoped_settings(&provider, &base).await;
        assert!(scoped.is_some());
        let s = scoped.unwrap();
        // Repo wins over global
        assert_eq!(s.pr_reviewer.num_max_findings, 3);
        // Global preserved for non-overlapping keys
        assert_eq!(s.pr_reviewer.extra_instructions, "Org rule");
    }

    #[tokio::test]
    async fn test_fetch_scoped_settings_returns_none_when_no_overrides() {
        use crate::testing::mock_git::MockGitProvider;
        let provider = MockGitProvider::new();
        let base = Settings::default();
        let scoped = fetch_scoped_settings(&provider, &base).await;
        assert!(scoped.is_none());
    }

    #[tokio::test]
    async fn test_fetch_scoped_settings_disabled_flags() {
        use crate::testing::mock_git::MockGitProvider;
        let provider = MockGitProvider::new()
            .with_global_settings("[pr_reviewer]\nnum_max_findings = 42")
            .with_repo_settings("[pr_reviewer]\nnum_max_findings = 7");
        let mut base = Settings::default();
        base.config.use_global_settings_file = false;
        base.config.use_repo_settings_file = false;
        let scoped = fetch_scoped_settings(&provider, &base).await;
        assert!(scoped.is_none(), "should skip when flags are disabled");
    }

    #[test]
    fn test_extract_pr_url_missing_field() {
        let payload = serde_json::json!({ "pull_request": {} });
        let result = extract_pr_url(&payload);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing pull_request.html_url")
        );
    }

    #[test]
    fn test_extract_pr_url_from_issue_fallback() {
        // When pull_request.html_url is missing, should fallback to issue.html_url
        let payload = serde_json::json!({
            "issue": {
                "html_url": "https://github.com/owner/repo/pull/42",
                "pull_request": {}
            }
        });
        let url = extract_pr_url_from_issue(&payload).unwrap();
        assert_eq!(url, "https://github.com/owner/repo/pull/42");
    }

    #[test]
    fn test_extract_pr_url_from_issue_missing_both() {
        let payload = serde_json::json!({ "issue": {} });
        let result = extract_pr_url_from_issue(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_signature_invalid_hex() {
        let result = verify_signature(b"body", "secret", "sha256=not-hex-data!");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid hex"));
    }

    #[test]
    fn test_should_ignore_pr_invalid_regex_does_not_crash() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_title = vec!["[invalid".into()]; // unclosed bracket
        // Should not panic — invalid regex is skipped with warning
        assert!(!should_ignore_pr(&settings, "Some PR title", "user1"));
    }

    #[test]
    fn test_should_ignore_pr_empty_author() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_authors = vec!["bot".into()];
        // Empty author should not match
        assert!(!should_ignore_pr(&settings, "Title", ""));
    }

    #[test]
    fn test_fold_comment_body_preserves_marker_and_content() {
        let body = "<!-- pr-agent:improve -->\n## PR Code Suggestions ✨\n\n| Category | Suggestion | Score |\n| --- | --- | --- |\n| bug | Fix null check | Important |\n\n- [ ]  I reviewed <!-- approve and fold suggestions self-review -->";
        let folded = fold_comment_body(body).unwrap();

        // Original marker preserved inside details
        assert!(folded.contains("<!-- pr-agent:improve -->"));
        // Table content preserved
        assert!(folded.contains("| Category | Suggestion | Score |"));
        assert!(folded.contains("Fix null check"));
        // Checkbox preserved
        assert!(folded.contains("<!-- approve and fold suggestions self-review -->"));
    }
}
