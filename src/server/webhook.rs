use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::config::loader::{get_settings, load_settings, with_settings};
use crate::config::types::Settings;
use crate::error::PrAgentError;
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

            // Bot detection: skip bot PRs (including pr-agent's own events like label changes).
            let sender = payload["sender"]["login"].as_str().unwrap_or("");
            let sender_type = payload["sender"]["type"].as_str().unwrap_or("");
            if settings.github.ignore_bot_pr && sender_type == "Bot" {
                if !sender.contains("pr-agent") {
                    tracing::info!(sender, sender_type, "ignoring PR from bot user");
                }
                return Ok(());
            }

            // Check all ignore filters (title, author, repo, labels, branches)
            if should_ignore_pr(&settings, payload) {
                return Ok(());
            }

            // Handle PR closed/merged event (before state check since closed PRs aren't "open")
            if action == "closed" {
                handle_closed_pr(payload);
                return Ok(());
            }

            // Validate PR state: skip drafts and non-open PRs
            if !check_pull_request_event(action, payload) {
                tracing::info!(pr_url = %pr_url, action, "skipping PR event (draft, not open, or duplicate)");
                return Ok(());
            }

            if settings
                .github_app
                .handle_pr_actions
                .contains(&action.to_string())
            {
                // Check disable_auto_feedback before running auto-commands
                if settings.config.disable_auto_feedback {
                    tracing::info!(pr_url = %pr_url, "auto feedback is disabled, skipping pr_commands");
                    return Ok(());
                }

                tracing::info!(pr_url = %pr_url, action, "handling PR event");
                run_commands(&pr_url, &settings.github_app.pr_commands).await?;
            } else if action == "synchronize" && settings.github_app.handle_push_trigger {
                // Skip merge commits if configured
                if settings.github_app.push_trigger_ignore_merge_commits {
                    let after_sha = payload["after"].as_str().unwrap_or("");
                    let merge_commit_sha = payload["pull_request"]["merge_commit_sha"]
                        .as_str()
                        .unwrap_or("");
                    if !after_sha.is_empty()
                        && !merge_commit_sha.is_empty()
                        && after_sha == merge_commit_sha
                    {
                        tracing::info!(pr_url = %pr_url, after_sha, "skipping merge commit push trigger");
                        return Ok(());
                    }
                }

                // Skip identical before/after SHAs (no-op push)
                let before_sha = payload["before"].as_str().unwrap_or("");
                let after_sha = payload["after"].as_str().unwrap_or("");
                if !before_sha.is_empty() && before_sha == after_sha {
                    tracing::debug!(pr_url = %pr_url, "skipping push trigger: before == after SHA");
                    return Ok(());
                }

                // Push deduplication: limit concurrent tasks per PR
                let _guard = match super::push_dedup::acquire_push_slot(&pr_url).await {
                    Some(guard) => guard,
                    None => {
                        tracing::info!(pr_url = %pr_url, "push trigger deduplicated, skipping");
                        return Ok(());
                    }
                };

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

            let raw_comment = payload["comment"]["body"].as_str().unwrap_or("").trim();

            // Handle image-reply format: "> ![image](url)\n/ask question"
            // When users quote an image and then write /ask, the command isn't at
            // the start. Reformat so /ask comes first with the image appended.
            let comment_body = reformat_image_reply(raw_comment);
            let comment_body = comment_body.as_str();

            if !comment_body.starts_with('/') {
                tracing::debug!("ignoring non-command comment");
                return Ok(());
            }

            // Check if this is a line-level /ask comment (code review comment on specific lines).
            // If so, transform it to /ask_line with the appropriate flags.
            let mut disable_eyes = false;
            let comment_body = if comment_body.contains("/ask")
                && payload["comment"]["subject_type"].as_str() == Some("line")
                && payload["comment"]["pull_request_url"].as_str().is_some()
            {
                disable_eyes = true;
                handle_line_comments(payload, comment_body)
            } else {
                comment_body.to_string()
            };
            let comment_body = comment_body.as_str();

            // Parse command early so we can reject unknown commands before
            // creating a provider, adding eyes reactions, or fetching settings.
            let (command, mut args) = tools::parse_command(comment_body);
            if !tools::is_known_command(&command) {
                tracing::debug!(command, "ignoring unknown command from comment");
                return Ok(());
            }

            // Extract PR URL — from issue or from review comment's pull_request_url
            let pr_url = if let Some(url) = payload["comment"]["pull_request_url"].as_str() {
                url.to_string()
            } else {
                extract_pr_url_from_issue(payload)?
            };
            tracing::info!(pr_url = %pr_url, command = comment_body, "handling comment command");

            // Add eyes reaction to the comment
            let comment_id = payload["comment"]["id"].as_u64().unwrap_or(0);
            let provider: Arc<dyn GitProvider> = Arc::new(GithubProvider::new(&pr_url).await?);
            let _ = provider.add_eyes_reaction(comment_id, disable_eyes).await;

            // Fetch global + repo settings and scope them for this command
            let scoped_settings = fetch_scoped_settings(provider.as_ref(), &settings).await;

            // Inject diff_hunk for ask_line when available
            if command == "ask_line"
                && let Some(diff_hunk) = payload["comment"]["diff_hunk"].as_str()
            {
                args.insert("_diff_hunk".to_string(), diff_hunk.to_string());
            }

            if let Some(s) = scoped_settings {
                with_settings(s, tools::handle_command(&command, provider, &args)).await?;
            } else {
                tools::handle_command(&command, provider, &args).await?;
            }
        }
        "pull_request_review_comment" => {
            if action != "created" {
                tracing::debug!(action, "ignoring pull_request_review_comment action");
                return Ok(());
            }

            let raw_comment = payload["comment"]["body"].as_str().unwrap_or("").trim();
            let comment_body = reformat_image_reply(raw_comment);

            if !comment_body.contains("/ask") {
                tracing::debug!("ignoring review comment without /ask command");
                return Ok(());
            }

            // Extract PR URL from the review comment payload
            let pr_url = payload["comment"]["pull_request_url"]
                .as_str()
                .map(|u| u.to_string())
                .or_else(|| {
                    payload["pull_request"]["url"]
                        .as_str()
                        .map(|u| u.to_string())
                })
                .ok_or_else(|| {
                    PrAgentError::Other("no pull_request_url in review comment".into())
                })?;

            // Transform line comment to /ask_line command
            let transformed = handle_line_comments(payload, &comment_body);
            tracing::info!(
                pr_url = %pr_url,
                command = %transformed,
                "handling line comment command"
            );

            // Add eyes reaction (disabled for line comments to avoid noise)
            let comment_id = payload["comment"]["id"].as_u64().unwrap_or(0);
            let provider: Arc<dyn GitProvider> = Arc::new(GithubProvider::new(&pr_url).await?);
            let _ = provider.add_eyes_reaction(comment_id, true).await;

            let scoped_settings = fetch_scoped_settings(provider.as_ref(), &settings).await;
            let (command, args) = tools::parse_command(&transformed);

            // Inject the diff_hunk from the webhook payload for ask_line
            let mut args = args;
            if let Some(diff_hunk) = payload["comment"]["diff_hunk"].as_str() {
                args.insert("_diff_hunk".to_string(), diff_hunk.to_string());
            }

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

/// Validate a pull_request event payload before processing.
fn check_pull_request_event(action: &str, payload: &serde_json::Value) -> bool {
    let pr = &payload["pull_request"];

    // Skip draft PRs — default to false (non-draft) if field missing
    let is_draft = pr["draft"].as_bool().unwrap_or(false);
    if is_draft {
        return false;
    }

    // Skip non-open PRs
    let state = pr["state"].as_str().unwrap_or("");
    if state != "open" {
        return false;
    }

    // For review_requested and synchronize: skip if created_at == updated_at
    // to avoid double-processing when a PR is first opened (both events fire)
    if action == "review_requested" || action == "synchronize" {
        let created_at = pr["created_at"].as_str().unwrap_or("");
        let updated_at = pr["updated_at"].as_str().unwrap_or("");
        if !created_at.is_empty() && created_at == updated_at {
            tracing::debug!(
                action,
                created_at,
                "skipping: created_at == updated_at (initial PR creation)"
            );
            return false;
        }
    }

    true
}

/// Check if a PR should be ignored based on configured filters.
fn should_ignore_pr(settings: &Settings, payload: &serde_json::Value) -> bool {
    let title = payload["pull_request"]["title"].as_str().unwrap_or("");
    let author = payload["pull_request"]["user"]["login"]
        .as_str()
        .unwrap_or("");

    // 1. Title regex patterns
    for pattern in &settings.config.ignore_pr_title {
        match crate::util::get_or_compile_regex(pattern) {
            Some(re) => {
                if re.is_match(title) {
                    tracing::info!(title, pattern, "ignoring PR: title matches ignore pattern");
                    return true;
                }
            }
            None => {
                tracing::warn!(pattern, "invalid ignore_pr_title regex");
            }
        }
    }

    // 2. Author list
    if !author.is_empty()
        && settings
            .config
            .ignore_pr_authors
            .iter()
            .any(|a| a == author)
    {
        tracing::info!(author, "ignoring PR: author in ignore list");
        return true;
    }

    // 3. Repository full name regex patterns
    let repo_full_name = payload["repository"]["full_name"].as_str().unwrap_or("");
    if !repo_full_name.is_empty() {
        for pattern in &settings.config.ignore_repositories {
            match crate::util::get_or_compile_regex(pattern) {
                Some(re) => {
                    if re.is_match(repo_full_name) {
                        tracing::info!(
                            repo_full_name,
                            pattern,
                            "ignoring PR: repo matches ignore pattern"
                        );
                        return true;
                    }
                }
                None => {
                    tracing::warn!(pattern, "invalid ignore_repositories regex");
                }
            }
        }
    }

    // 4. PR labels (exact match)
    if !settings.config.ignore_pr_labels.is_empty()
        && let Some(labels) = payload["pull_request"]["labels"].as_array()
    {
        for label in labels {
            let label_name = label["name"].as_str().unwrap_or("");
            if settings
                .config
                .ignore_pr_labels
                .iter()
                .any(|l| l == label_name)
            {
                tracing::info!(label_name, "ignoring PR: label in ignore list");
                return true;
            }
        }
    }

    // 5. Source branch regex patterns (head.ref)
    let source_branch = payload["pull_request"]["head"]["ref"]
        .as_str()
        .unwrap_or("");
    if !source_branch.is_empty() {
        for pattern in &settings.config.ignore_pr_source_branches {
            match crate::util::get_or_compile_regex(pattern) {
                Some(re) => {
                    if re.is_match(source_branch) {
                        tracing::info!(
                            source_branch,
                            pattern,
                            "ignoring PR: source branch matches ignore pattern"
                        );
                        return true;
                    }
                }
                None => {
                    tracing::warn!(pattern, "invalid ignore_pr_source_branches regex");
                }
            }
        }
    }

    // 6. Target branch regex patterns (base.ref)
    let target_branch = payload["pull_request"]["base"]["ref"]
        .as_str()
        .unwrap_or("");
    if !target_branch.is_empty() {
        for pattern in &settings.config.ignore_pr_target_branches {
            match crate::util::get_or_compile_regex(pattern) {
                Some(re) => {
                    if re.is_match(target_branch) {
                        tracing::info!(
                            target_branch,
                            pattern,
                            "ignoring PR: target branch matches ignore pattern"
                        );
                        return true;
                    }
                }
                None => {
                    tracing::warn!(pattern, "invalid ignore_pr_target_branches regex");
                }
            }
        }
    }

    false
}

/// Log PR merge statistics when a PR is closed and merged.
///
/// Extracts real statistics from the webhook payload: commits, additions,
/// deletions, changed files, reviewers, comments, and time-to-merge.
fn handle_closed_pr(payload: &serde_json::Value) {
    let pr = &payload["pull_request"];
    let is_merged = pr["merged"].as_bool().unwrap_or(false);
    if !is_merged {
        tracing::debug!("PR closed without merge, skipping analytics");
        return;
    }

    let pr_url = pr["html_url"].as_str().unwrap_or("");
    let title = pr["title"].as_str().unwrap_or("");
    let commits = pr["commits"].as_u64().unwrap_or(0);
    let additions = pr["additions"].as_u64().unwrap_or(0);
    let deletions = pr["deletions"].as_u64().unwrap_or(0);
    let changed_files = pr["changed_files"].as_u64().unwrap_or(0);
    let comments =
        pr["comments"].as_u64().unwrap_or(0) + pr["review_comments"].as_u64().unwrap_or(0);
    let merged_by = pr["merged_by"]["login"].as_str().unwrap_or("");

    // Count requested reviewers
    let reviewers = pr["requested_reviewers"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);

    // Calculate time to merge
    let created_at = pr["created_at"].as_str().unwrap_or("");
    let merged_at = pr["merged_at"].as_str().unwrap_or("");
    let time_to_merge_hours = compute_hours_between(created_at, merged_at);

    tracing::info!(
        pr_url,
        title,
        commits,
        additions,
        deletions,
        changed_files,
        reviewers,
        comments,
        merged_by,
        time_to_merge_hours,
        "PR merged — statistics"
    );
}

/// Compute hours between two ISO 8601 timestamps.
fn compute_hours_between(start: &str, end: &str) -> f64 {
    let Ok(start_dt) = chrono::DateTime::parse_from_rfc3339(start) else {
        return 0.0;
    };
    let Ok(end_dt) = chrono::DateTime::parse_from_rfc3339(end) else {
        return 0.0;
    };
    let duration = end_dt - start_dt;
    duration.num_minutes() as f64 / 60.0
}

/// Transform a line-level `/ask` comment into an `/ask_line` command string.
fn handle_line_comments(payload: &serde_json::Value, comment_body: &str) -> String {
    let comment = &payload["comment"];

    let end_line = comment["line"].as_u64().unwrap_or(0);
    let start_line = comment["start_line"].as_u64().unwrap_or(end_line);
    let start_line = if start_line == 0 {
        end_line
    } else {
        start_line
    };
    let side = comment["side"].as_str().unwrap_or("RIGHT");
    let path = comment["path"].as_str().unwrap_or("");
    let comment_id = comment["id"].as_u64().unwrap_or(0);

    // Extract the question text by stripping the leading /ask command (only the first one)
    let question = comment_body
        .trim_start()
        .strip_prefix("/ask")
        .unwrap_or(comment_body)
        .trim()
        .to_string();

    format!(
        "/ask_line --line_start={start_line} --line_end={end_line} --side={side} --file_name={path} --comment_id={comment_id} {question}"
    )
}

/// Reformat image-reply comment: `> ![image](url)\n/ask question` → `/ask question \n> ![image](url)`.
///
/// When users quote an image and write `/ask` below it, the comment body doesn't
/// start with `/`. This function moves the `/ask` command to the front so it gets
/// recognized as a command. The image quote is appended for the /ask tool to detect.
fn reformat_image_reply(comment: &str) -> String {
    if comment.starts_with('/') || !comment.contains("/ask") {
        return comment.to_string();
    }

    if comment.trim().starts_with("> ![image]")
        && let Some(pos) = comment.find("/ask")
    {
        let before = comment[..pos].trim().trim_start_matches('>').trim();
        let after = &comment[pos..];
        let reformatted = format!("{after} \n{before}");
        tracing::info!("reformatted image-reply comment so /ask is at the beginning");
        return reformatted;
    }

    comment.to_string()
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

    /// Helper: build a minimal PR payload for should_ignore_pr tests.
    fn make_pr_payload(title: &str, author: &str) -> serde_json::Value {
        serde_json::json!({
            "pull_request": {
                "title": title,
                "user": { "login": author },
                "labels": [],
                "head": { "ref": "feature/test" },
                "base": { "ref": "main" }
            },
            "repository": { "full_name": "owner/repo" }
        })
    }

    #[test]
    fn test_should_ignore_pr_title_regex() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_title = vec![r"^\[Auto\]".into(), r"^Auto".into()];

        assert!(should_ignore_pr(
            &settings,
            &make_pr_payload("[Auto] Update deps", "user1")
        ));
        assert!(should_ignore_pr(
            &settings,
            &make_pr_payload("Auto merge from main", "user1")
        ));
        assert!(!should_ignore_pr(
            &settings,
            &make_pr_payload("Fix authentication bug", "user1")
        ));
    }

    #[test]
    fn test_should_ignore_pr_author() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_authors = vec!["dependabot[bot]".into(), "renovate[bot]".into()];

        assert!(should_ignore_pr(
            &settings,
            &make_pr_payload("Update deps", "dependabot[bot]")
        ));
        assert!(should_ignore_pr(
            &settings,
            &make_pr_payload("Update deps", "renovate[bot]")
        ));
        assert!(!should_ignore_pr(
            &settings,
            &make_pr_payload("Update deps", "human-dev")
        ));
    }

    #[test]
    fn test_should_ignore_pr_empty_filters() {
        let settings = Settings::default();
        // Default has ignore_pr_title patterns but a normal title won't match
        assert!(!should_ignore_pr(
            &settings,
            &make_pr_payload("Normal PR title", "user1")
        ));
    }

    #[test]
    fn test_should_ignore_pr_repository() {
        let mut settings = Settings::default();
        settings.config.ignore_repositories = vec![r"^org/internal-".into()];

        let mut payload = make_pr_payload("My PR", "user1");
        payload["repository"]["full_name"] = serde_json::json!("org/internal-tools");
        assert!(should_ignore_pr(&settings, &payload));

        let payload = make_pr_payload("My PR", "user1"); // default: owner/repo
        assert!(!should_ignore_pr(&settings, &payload));
    }

    #[test]
    fn test_should_ignore_pr_labels() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_labels = vec!["do-not-review".into(), "wip".into()];

        let mut payload = make_pr_payload("My PR", "user1");
        payload["pull_request"]["labels"] = serde_json::json!([
            { "name": "enhancement" },
            { "name": "do-not-review" }
        ]);
        assert!(should_ignore_pr(&settings, &payload));

        let mut payload = make_pr_payload("My PR", "user1");
        payload["pull_request"]["labels"] = serde_json::json!([
            { "name": "enhancement" }
        ]);
        assert!(!should_ignore_pr(&settings, &payload));
    }

    #[test]
    fn test_should_ignore_pr_source_branch() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_source_branches = vec![r"^dependabot/".into()];

        let mut payload = make_pr_payload("My PR", "user1");
        payload["pull_request"]["head"]["ref"] = serde_json::json!("dependabot/npm/lodash-4.17.21");
        assert!(should_ignore_pr(&settings, &payload));

        let payload = make_pr_payload("My PR", "user1"); // default: feature/test
        assert!(!should_ignore_pr(&settings, &payload));
    }

    #[test]
    fn test_should_ignore_pr_target_branch() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_target_branches = vec![r"^release/".into()];

        let mut payload = make_pr_payload("My PR", "user1");
        payload["pull_request"]["base"]["ref"] = serde_json::json!("release/v2.0");
        assert!(should_ignore_pr(&settings, &payload));

        let payload = make_pr_payload("My PR", "user1"); // default: main
        assert!(!should_ignore_pr(&settings, &payload));
    }

    #[test]
    fn test_check_pull_request_event_draft() {
        let payload = serde_json::json!({
            "pull_request": { "draft": true, "state": "open",
                "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T01:00:00Z" }
        });
        assert!(!check_pull_request_event("opened", &payload));
    }

    #[test]
    fn test_check_pull_request_event_closed() {
        let payload = serde_json::json!({
            "pull_request": { "draft": false, "state": "closed",
                "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T01:00:00Z" }
        });
        assert!(!check_pull_request_event("opened", &payload));
    }

    #[test]
    fn test_check_pull_request_event_open_non_draft() {
        let payload = serde_json::json!({
            "pull_request": { "draft": false, "state": "open",
                "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T01:00:00Z" }
        });
        assert!(check_pull_request_event("opened", &payload));
    }

    #[test]
    fn test_check_pull_request_event_sync_created_eq_updated() {
        // When created_at == updated_at, synchronize should be skipped
        // (avoids double-processing on initial PR creation)
        let payload = serde_json::json!({
            "pull_request": { "draft": false, "state": "open",
                "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T00:00:00Z" }
        });
        assert!(!check_pull_request_event("synchronize", &payload));
        assert!(!check_pull_request_event("review_requested", &payload));
        // But opened should still be allowed
        assert!(check_pull_request_event("opened", &payload));
    }

    #[test]
    fn test_check_pull_request_event_sync_different_timestamps() {
        let payload = serde_json::json!({
            "pull_request": { "draft": false, "state": "open",
                "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-02T00:00:00Z" }
        });
        assert!(check_pull_request_event("synchronize", &payload));
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
        assert!(!should_ignore_pr(
            &settings,
            &make_pr_payload("Some PR title", "user1")
        ));
    }

    #[test]
    fn test_should_ignore_pr_empty_author() {
        let mut settings = Settings::default();
        settings.config.ignore_pr_authors = vec!["bot".into()];
        // Empty author should not match
        assert!(!should_ignore_pr(&settings, &make_pr_payload("Title", "")));
    }

    /// dispatch_event should return Ok(()) without attempting network calls
    /// when the PR is a draft — the draft check short-circuits before run_commands.
    #[tokio::test]
    async fn test_dispatch_event_skips_draft_pr() {
        let payload = serde_json::json!({
            "action": "opened",
            "sender": { "login": "testuser", "type": "User" },
            "repository": { "full_name": "owner/repo" },
            "pull_request": {
                "html_url": "https://github.com/owner/repo/pull/1",
                "title": "My PR",
                "draft": true,
                "state": "open",
                "labels": [],
                "user": { "login": "testuser" },
                "head": { "ref": "feat/test" },
                "base": { "ref": "main" },
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T01:00:00Z"
            }
        });

        // Should succeed (skip) without trying to connect to GitHub
        let result = dispatch_event("pull_request", "opened", &payload).await;
        assert!(result.is_ok(), "draft PR should be skipped silently");
    }

    /// dispatch_event should also skip PRs that are not in "open" state.
    #[tokio::test]
    async fn test_dispatch_event_skips_closed_pr() {
        let payload = serde_json::json!({
            "action": "reopened",
            "sender": { "login": "testuser", "type": "User" },
            "repository": { "full_name": "owner/repo" },
            "pull_request": {
                "html_url": "https://github.com/owner/repo/pull/1",
                "title": "My PR",
                "draft": false,
                "state": "closed",
                "labels": [],
                "user": { "login": "testuser" },
                "head": { "ref": "feat/test" },
                "base": { "ref": "main" },
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T01:00:00Z"
            }
        });

        let result = dispatch_event("pull_request", "reopened", &payload).await;
        assert!(result.is_ok(), "closed PR should be skipped silently");
    }

    /// dispatch_event should skip PRs from bot users when ignore_bot_pr is true.
    #[tokio::test]
    async fn test_dispatch_event_skips_bot_pr() {
        let payload = serde_json::json!({
            "action": "opened",
            "sender": { "login": "dependabot[bot]", "type": "Bot" },
            "repository": { "full_name": "owner/repo" },
            "pull_request": {
                "html_url": "https://github.com/owner/repo/pull/1",
                "title": "Bump lodash",
                "draft": false,
                "state": "open",
                "labels": [],
                "user": { "login": "dependabot[bot]" },
                "head": { "ref": "dependabot/npm/lodash" },
                "base": { "ref": "main" },
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T01:00:00Z"
            }
        });

        let result = dispatch_event("pull_request", "opened", &payload).await;
        assert!(result.is_ok(), "bot PR should be skipped silently");
    }

    /// dispatch_event should also skip pr-agent bot events (e.g. label changes).
    #[tokio::test]
    async fn test_dispatch_event_skips_pr_agent_bot_labeled_event() {
        let payload = serde_json::json!({
            "action": "labeled",
            "sender": { "login": "pr-agent-app[bot]", "type": "Bot" },
            "repository": { "full_name": "owner/repo" },
            "pull_request": {
                "html_url": "https://github.com/owner/repo/pull/1",
                "title": "My Feature",
                "draft": false,
                "state": "open",
                "labels": [{ "name": "Enhancement" }],
                "user": { "login": "developer" },
                "head": { "ref": "feat/test" },
                "base": { "ref": "main" },
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T02:00:00Z"
            }
        });

        // Should be silently ignored — bot's own label changes must not re-trigger review
        let result = dispatch_event("pull_request", "labeled", &payload).await;
        assert!(
            result.is_ok(),
            "pr-agent bot labeled event should be skipped"
        );
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

    // ── Image-reply reformat tests ──────────────────────────────────

    #[test]
    fn test_reformat_image_reply_basic() {
        let comment = "> ![image](https://img.com/a.png)\n/ask What is this?";
        let result = reformat_image_reply(comment);
        assert!(
            result.starts_with("/ask"),
            "should start with /ask, got: {result}"
        );
        assert!(result.contains("What is this?"));
        assert!(result.contains("![image](https://img.com/a.png)"));
    }

    #[test]
    fn test_reformat_image_reply_already_starts_with_slash() {
        let comment = "/ask What does this PR do?";
        let result = reformat_image_reply(comment);
        assert_eq!(result, comment, "should return unchanged");
    }

    #[test]
    fn test_reformat_image_reply_no_ask() {
        let comment = "> ![image](https://img.com/a.png)\nsome other text";
        let result = reformat_image_reply(comment);
        assert_eq!(result, comment, "should return unchanged without /ask");
    }

    #[test]
    fn test_reformat_image_reply_no_image() {
        let comment = "some text /ask question";
        let result = reformat_image_reply(comment);
        assert_eq!(result, comment, "should return unchanged without image");
    }

    // ── Line comment transformation tests ───────────────────────────

    #[test]
    fn test_handle_line_comments_basic() {
        let payload = serde_json::json!({
            "comment": {
                "id": 12345,
                "line": 20,
                "start_line": 15,
                "side": "RIGHT",
                "path": "src/main.rs",
                "diff_hunk": "@@ -10,5 +10,7 @@ fn main()"
            }
        });

        let result = handle_line_comments(&payload, "/ask What does this do?");
        assert!(result.starts_with("/ask_line"));
        assert!(result.contains("--line_start=15"));
        assert!(result.contains("--line_end=20"));
        assert!(result.contains("--side=RIGHT"));
        assert!(result.contains("--file_name=src/main.rs"));
        assert!(result.contains("--comment_id=12345"));
        assert!(result.contains("What does this do?"));
    }

    #[test]
    fn test_handle_line_comments_no_start_line() {
        let payload = serde_json::json!({
            "comment": {
                "id": 100,
                "line": 42,
                "start_line": null,
                "side": "LEFT",
                "path": "lib.rs"
            }
        });

        let result = handle_line_comments(&payload, "/ask Why was this removed?");
        // When start_line is null, it should default to end_line
        assert!(result.contains("--line_start=42"));
        assert!(result.contains("--line_end=42"));
        assert!(result.contains("--side=LEFT"));
    }

    #[test]
    fn test_handle_line_comments_question_containing_ask() {
        // Question text contains "/ask" — only the leading one should be stripped
        let payload = serde_json::json!({
            "comment": {
                "id": 999,
                "line": 5,
                "start_line": 5,
                "side": "RIGHT",
                "path": "main.rs"
            }
        });

        let result = handle_line_comments(&payload, "/ask why does /ask appear here?");
        assert!(
            result.contains("why does /ask appear here?"),
            "inner /ask should be preserved, got: {result}"
        );
    }

    // ── PR merge analytics tests ────────────────────────────────────

    #[test]
    fn test_compute_hours_between() {
        let hours = compute_hours_between("2025-01-01T00:00:00Z", "2025-01-01T02:30:00Z");
        assert!((hours - 2.5).abs() < 0.01);
    }

    #[test]
    fn test_compute_hours_between_invalid() {
        assert_eq!(
            compute_hours_between("invalid", "2025-01-01T00:00:00Z"),
            0.0
        );
        assert_eq!(compute_hours_between("", ""), 0.0);
    }

    #[test]
    fn test_handle_closed_pr_merged() {
        // Should not panic, just logs
        let payload = serde_json::json!({
            "pull_request": {
                "html_url": "https://github.com/o/r/pull/1",
                "title": "Add feature",
                "merged": true,
                "commits": 3,
                "additions": 100,
                "deletions": 20,
                "changed_files": 5,
                "comments": 2,
                "review_comments": 4,
                "merged_by": { "login": "reviewer" },
                "requested_reviewers": [{"login": "r1"}, {"login": "r2"}],
                "created_at": "2025-01-01T00:00:00Z",
                "merged_at": "2025-01-02T12:00:00Z"
            }
        });
        // Just verify it doesn't panic
        handle_closed_pr(&payload);
    }

    #[test]
    fn test_handle_closed_pr_not_merged() {
        let payload = serde_json::json!({
            "pull_request": {
                "merged": false
            }
        });
        // Should return early without panic
        handle_closed_pr(&payload);
    }

    // ── Unknown command early-rejection tests ────────────────────────

    /// dispatch_event should silently ignore unknown `/` commands in issue
    /// comments — no provider creation, no eyes reaction, no error.
    #[tokio::test]
    async fn test_dispatch_event_ignores_unknown_slash_command() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": {
                "pull_request": {
                    "html_url": "https://github.com/owner/repo/pull/1"
                }
            },
            "comment": {
                "id": 42,
                "body": "/qa-verify"
            }
        });

        // Should return Ok(()) without attempting any network calls
        let result = dispatch_event("issue_comment", "created", &payload).await;
        assert!(
            result.is_ok(),
            "unknown command /qa-verify should be silently ignored, got: {:?}",
            result,
        );
    }

    /// Known commands like /review should NOT be rejected (they will fail due
    /// to missing network, but that's expected — we only verify they aren't
    /// short-circuited by the unknown-command check).
    #[tokio::test]
    async fn test_dispatch_event_does_not_reject_known_command() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": {
                "pull_request": {
                    "html_url": "https://github.com/owner/repo/pull/1"
                }
            },
            "comment": {
                "id": 42,
                "body": "/review"
            }
        });

        // Known command should NOT return Ok(()) early — it will try to
        // create a GithubProvider and fail because there's no real GitHub.
        // An error here proves it got past the unknown-command gate.
        let result = dispatch_event("issue_comment", "created", &payload).await;
        assert!(
            result.is_err(),
            "/review should proceed past the gate and fail on provider creation"
        );
    }
}
