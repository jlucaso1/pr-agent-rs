use std::collections::HashMap;

use async_trait::async_trait;
use base64::Engine;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::Client;
use serde::Serialize;
use serde_json::json;

use super::GitProvider;
use super::types::*;
use super::url_parser::{ParsedPrUrl, parse_pr_url};
use crate::config::loader::get_settings;
use crate::error::PrAgentError;

/// Maximum characters in a single comment (GitHub limit ~65536).
const MAX_COMMENT_CHARS: usize = 65000;

/// JWT claims for GitHub App authentication.
#[derive(Debug, Serialize)]
struct GithubAppClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

/// GitHub provider implementation using raw reqwest for full API control.
pub struct GithubProvider {
    /// Raw reqwest client.
    client: Client,
    /// Base URL for the GitHub API (supports Enterprise).
    base_url: String,
    /// Auth token.
    token: String,
    /// Parsed URL info.
    parsed: ParsedPrUrl,
    /// Full repo name "owner/repo".
    repo_full: String,
}

impl GithubProvider {
    /// Create a new GitHub provider from a PR URL.
    ///
    /// Supports both "user" (token) and "app" (JWT + installation token) auth.
    pub async fn new(pr_url: &str) -> Result<Self, PrAgentError> {
        let parsed = parse_pr_url(pr_url)?;
        let settings = get_settings();

        let base_url = settings.github.base_url.clone();
        let timeout = std::time::Duration::from_secs(settings.config.ai_timeout as u64);
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| PrAgentError::Other(format!("failed to build HTTP client: {e}")))?;
        let repo_full = format!("{}/{}", parsed.owner, parsed.repo);

        let token = if settings.github.deployment_type == "app" {
            get_app_installation_token(
                &client,
                &base_url,
                settings.github.app_id,
                &settings.github.private_key,
                &parsed.owner,
            )
            .await?
        } else {
            settings.github.user_token.clone()
        };

        Ok(Self {
            client,
            base_url,
            token,
            parsed,
            repo_full,
        })
    }

    /// Send a GitHub API request with automatic retry on rate limits (429).
    ///
    /// Retries up to `ratelimit_retries` times with exponential backoff,
    /// respecting the `Retry-After` header when present.
    async fn api_request_with_retry(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::Response, PrAgentError> {
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), path);
        self.api_request_with_retry_url(method, &url, body).await
    }

    /// Same as `api_request_with_retry` but accepts an absolute URL (for pagination).
    async fn api_request_with_retry_url(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::Response, PrAgentError> {
        let settings = get_settings();
        let max_retries = settings.github.ratelimit_retries;

        for attempt in 0..=max_retries {
            let mut req = self
                .client
                .request(method.clone(), url)
                .bearer_auth(&self.token)
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", "pr-agent-rs");

            if let Some(b) = body {
                req = req.json(b);
            }

            let resp = req.send().await.map_err(PrAgentError::Http)?;

            if resp.status().as_u16() == 429 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(2u64.pow(attempt + 1));

                if attempt < max_retries {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = max_retries,
                        retry_after_secs = retry_after,
                        url,
                        "GitHub API rate limited, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                    continue;
                }
                return Err(PrAgentError::RateLimited {
                    retry_after_secs: retry_after,
                });
            }

            return Ok(resp);
        }

        Err(PrAgentError::GitProvider(
            "GitHub API rate limit retries exhausted".into(),
        ))
    }

    /// Check response status and return a GitProvider error on failure.
    async fn check_response(
        resp: reqwest::Response,
        method: &str,
    ) -> Result<reqwest::Response, PrAgentError> {
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(PrAgentError::GitProvider(format!(
                "GitHub API {method} {status}: {body}"
            )));
        }
        Ok(resp)
    }

    /// Make an authenticated GET request to the GitHub API.
    async fn api_get(&self, path: &str) -> Result<serde_json::Value, PrAgentError> {
        let resp = self
            .api_request_with_retry(reqwest::Method::GET, path, None)
            .await?;
        let resp = Self::check_response(resp, "GET").await?;
        resp.json().await.map_err(PrAgentError::Http)
    }

    /// Make a paginated GET request, collecting all pages of JSON arrays.
    ///
    /// Follows the `Link: <url>; rel="next"` header until no more pages.
    async fn api_get_all_pages(&self, path: &str) -> Result<Vec<serde_json::Value>, PrAgentError> {
        let mut all_items = Vec::new();

        // First request uses the relative path
        let resp = self
            .api_request_with_retry(reqwest::Method::GET, path, None)
            .await?;
        let resp = Self::check_response(resp, "GET").await?;
        let mut next_url = parse_next_link(resp.headers());
        let page: serde_json::Value = resp.json().await.map_err(PrAgentError::Http)?;
        if let Some(arr) = page.as_array() {
            all_items.extend(arr.iter().cloned());
        }

        // Follow pagination links
        while let Some(url) = next_url.take() {
            let resp = self
                .api_request_with_retry_url(reqwest::Method::GET, &url, None)
                .await?;
            let resp = Self::check_response(resp, "GET").await?;
            next_url = parse_next_link(resp.headers());
            let page: serde_json::Value = resp.json().await.map_err(PrAgentError::Http)?;
            if let Some(arr) = page.as_array() {
                all_items.extend(arr.iter().cloned());
            }
        }

        Ok(all_items)
    }

    /// Make an authenticated POST request to the GitHub API.
    async fn api_post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, PrAgentError> {
        let resp = self
            .api_request_with_retry(reqwest::Method::POST, path, Some(body))
            .await?;
        let resp = Self::check_response(resp, "POST").await?;
        resp.json().await.map_err(PrAgentError::Http)
    }

    /// Make an authenticated PATCH request.
    async fn api_patch(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, PrAgentError> {
        let resp = self
            .api_request_with_retry(reqwest::Method::PATCH, path, Some(body))
            .await?;
        let resp = Self::check_response(resp, "PATCH").await?;
        resp.json().await.map_err(PrAgentError::Http)
    }

    /// Make an authenticated DELETE request.
    async fn api_delete(&self, path: &str) -> Result<(), PrAgentError> {
        let resp = self
            .api_request_with_retry(reqwest::Method::DELETE, path, None)
            .await?;
        Self::check_response(resp, "DELETE").await?;
        Ok(())
    }

    /// Get file contents from the repo at a specific ref.
    async fn get_file_content(&self, path: &str, git_ref: &str) -> Result<String, PrAgentError> {
        self.get_file_content_from_repo(&self.repo_full, path, git_ref)
            .await
    }

    /// Get file contents from an arbitrary repo at a specific ref.
    ///
    /// Like `get_file_content()` but allows specifying a different
    /// `repo_full` (e.g. "org/pr-agent-settings" instead of the PR's repo).
    async fn get_file_content_from_repo(
        &self,
        repo_full: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<String, PrAgentError> {
        let api_path = format!("repos/{}/contents/{}?ref={}", repo_full, path, git_ref);
        let resp = self.api_get(&api_path).await?;

        let content = resp["content"]
            .as_str()
            .unwrap_or_default()
            .replace('\n', "");
        let encoding = resp["encoding"].as_str().unwrap_or("");

        if encoding == "base64" {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&content)
                .unwrap_or_default();
            Ok(String::from_utf8_lossy(&decoded).into_owned())
        } else {
            Ok(content)
        }
    }
}

/// Generate a GitHub App JWT and exchange it for an installation access token.
///
/// Flow:
/// 1. Build RS256 JWT with iss=app_id, iat=now-60s, exp=now+10min
/// 2. GET /app/installations → find installation matching the repo owner
/// 3. POST /app/installations/{id}/access_tokens → return the token
async fn get_app_installation_token(
    client: &Client,
    base_url: &str,
    app_id: u64,
    private_key_pem: &str,
    owner: &str,
) -> Result<String, PrAgentError> {
    if app_id == 0 || private_key_pem.is_empty() {
        return Err(PrAgentError::Other(
            "GitHub App auth requires app_id and private_key".into(),
        ));
    }

    // 1. Generate JWT
    let now = chrono::Utc::now().timestamp();
    let claims = GithubAppClaims {
        iat: now - 60,
        exp: now + (10 * 60),
        iss: app_id.to_string(),
    };
    let header = Header::new(Algorithm::RS256);
    let key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).map_err(|_| {
        PrAgentError::Other("invalid GitHub App private key: failed to parse RSA PEM".into())
    })?;
    let jwt = encode(&header, &claims, &key)
        .map_err(|e| PrAgentError::Other(format!("failed to encode JWT: {e}")))?;

    let api_base = base_url.trim_end_matches('/');

    // 2. List installations and find the one matching the owner
    let installations_url = format!("{api_base}/app/installations");
    let resp = client
        .get(&installations_url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "pr-agent-rs")
        .send()
        .await
        .map_err(PrAgentError::Http)?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PrAgentError::GitProvider(format!(
            "failed to list GitHub App installations ({status}): {body}"
        )));
    }

    let installations: serde_json::Value = resp.json().await.map_err(PrAgentError::Http)?;
    let installations_arr = installations.as_array().ok_or_else(|| {
        PrAgentError::GitProvider("unexpected installations response format".into())
    })?;

    let owner_lower = owner.to_lowercase();
    let installation_id = installations_arr
        .iter()
        .find_map(|inst| {
            let account = inst["account"]["login"].as_str().unwrap_or_default();
            if account.to_lowercase() == owner_lower {
                inst["id"].as_u64()
            } else {
                None
            }
        })
        .ok_or_else(|| {
            PrAgentError::GitProvider(format!(
                "no GitHub App installation found for owner '{owner}'"
            ))
        })?;

    tracing::info!(installation_id, owner, "found GitHub App installation");

    // 3. Create installation access token
    let token_url = format!("{api_base}/app/installations/{installation_id}/access_tokens");
    let resp = client
        .post(&token_url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "pr-agent-rs")
        .send()
        .await
        .map_err(PrAgentError::Http)?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PrAgentError::GitProvider(format!(
            "failed to create installation token ({status}): {body}"
        )));
    }

    let token_data: serde_json::Value = resp.json().await.map_err(PrAgentError::Http)?;
    let token = token_data["token"]
        .as_str()
        .ok_or_else(|| PrAgentError::GitProvider("no token in installation response".into()))?
        .to_string();

    tracing::info!("GitHub App installation token obtained successfully");
    Ok(token)
}

#[async_trait]
impl GitProvider for GithubProvider {
    async fn get_diff_files(&self) -> Result<Vec<FilePatchInfo>, PrAgentError> {
        let pr_path = format!("repos/{}/pulls/{}", self.repo_full, self.parsed.pr_number);
        let pr_data = self.api_get(&pr_path).await?;

        let base_sha = pr_data["base"]["sha"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let head_sha = pr_data["head"]["sha"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let compare_path = format!(
            "repos/{}/compare/{}...{}",
            self.repo_full, base_sha, head_sha
        );
        let compare_data = self.api_get(&compare_path).await?;

        let files = compare_data["files"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut diff_files = Vec::with_capacity(files.len());

        for file in &files {
            let filename = file["filename"].as_str().unwrap_or_default().to_string();
            let status = file["status"].as_str().unwrap_or("modified");
            let patch = file["patch"].as_str().unwrap_or_default().to_string();
            let previous_filename = file["previous_filename"].as_str().map(String::from);

            let edit_type = match status {
                "added" => EditType::Added,
                "removed" => EditType::Deleted,
                "renamed" => EditType::Renamed,
                "modified" | "changed" => EditType::Modified,
                _ => EditType::Unknown,
            };

            let (plus_lines, minus_lines) = count_patch_lines(&patch);

            let base_file = if edit_type != EditType::Added {
                let ref_name = if edit_type == EditType::Renamed {
                    previous_filename.as_deref().unwrap_or(&filename)
                } else {
                    &filename
                };
                self.get_file_content(ref_name, &base_sha)
                    .await
                    .unwrap_or_default()
            } else {
                String::new()
            };

            let head_file = if edit_type != EditType::Deleted {
                self.get_file_content(&filename, &head_sha)
                    .await
                    .unwrap_or_default()
            } else {
                String::new()
            };

            let mut info = FilePatchInfo::new(base_file, head_file, patch, filename);
            info.edit_type = edit_type;
            info.old_filename = previous_filename;
            info.num_plus_lines = plus_lines;
            info.num_minus_lines = minus_lines;

            diff_files.push(info);
        }

        Ok(diff_files)
    }

    async fn get_files(&self) -> Result<Vec<String>, PrAgentError> {
        let path = format!(
            "repos/{}/pulls/{}/files?per_page=100",
            self.repo_full, self.parsed.pr_number
        );
        let items = self.api_get_all_pages(&path).await?;
        let files = items
            .iter()
            .filter_map(|f| f["filename"].as_str().map(String::from))
            .collect();
        Ok(files)
    }

    async fn get_languages(&self) -> Result<HashMap<String, u64>, PrAgentError> {
        let path = format!("repos/{}/languages", self.repo_full);
        let data = self.api_get(&path).await?;
        Ok(data
            .as_object()
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get_pr_branch(&self) -> Result<String, PrAgentError> {
        let path = format!("repos/{}/pulls/{}", self.repo_full, self.parsed.pr_number);
        let data = self.api_get(&path).await?;
        Ok(data["head"]["ref"].as_str().unwrap_or_default().to_string())
    }

    async fn get_pr_base_branch(&self) -> Result<String, PrAgentError> {
        let path = format!("repos/{}/pulls/{}", self.repo_full, self.parsed.pr_number);
        let data = self.api_get(&path).await?;
        Ok(data["base"]["ref"].as_str().unwrap_or_default().to_string())
    }

    async fn get_user_id(&self) -> Result<String, PrAgentError> {
        let data = self.api_get("user").await?;
        Ok(data["login"].as_str().unwrap_or_default().to_string())
    }

    async fn get_pr_description_full(&self) -> Result<(String, String), PrAgentError> {
        let path = format!("repos/{}/pulls/{}", self.repo_full, self.parsed.pr_number);
        let data = self.api_get(&path).await?;
        let title = data["title"].as_str().unwrap_or_default().to_string();
        let body = data["body"].as_str().unwrap_or_default().to_string();
        Ok((title, body))
    }

    async fn publish_description(&self, title: &str, body: &str) -> Result<(), PrAgentError> {
        let path = format!("repos/{}/pulls/{}", self.repo_full, self.parsed.pr_number);
        self.api_patch(&path, &json!({"title": title, "body": body}))
            .await?;
        Ok(())
    }

    async fn publish_comment(
        &self,
        text: &str,
        _is_temporary: bool,
    ) -> Result<Option<CommentId>, PrAgentError> {
        let truncated = if text.len() > MAX_COMMENT_CHARS {
            // Find the largest char boundary at or before MAX_COMMENT_CHARS
            let mut end = MAX_COMMENT_CHARS;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            text
        };
        let path = format!(
            "repos/{}/issues/{}/comments",
            self.repo_full, self.parsed.pr_number
        );
        let resp = self.api_post(&path, &json!({"body": truncated})).await?;
        let id = resp["id"].as_u64().map(|id| CommentId(id.to_string()));
        Ok(id)
    }

    async fn publish_inline_comment(
        &self,
        body: &str,
        file: &str,
        line: &str,
        _original_suggestion: Option<&str>,
    ) -> Result<(), PrAgentError> {
        let path = format!(
            "repos/{}/pulls/{}/reviews",
            self.repo_full, self.parsed.pr_number
        );
        let mut comment = json!({
            "body": body,
            "path": file,
            "side": "RIGHT",
        });
        if let Ok(line_num) = line.parse::<u64>()
            && line_num > 0
        {
            comment["line"] = json!(line_num);
        }
        let review_body = json!({
            "event": "COMMENT",
            "comments": [comment]
        });
        self.api_post(&path, &review_body).await?;
        Ok(())
    }

    async fn publish_inline_comments(
        &self,
        comments: &[InlineComment],
    ) -> Result<(), PrAgentError> {
        if comments.is_empty() {
            return Ok(());
        }

        let pr_path = format!("repos/{}/pulls/{}", self.repo_full, self.parsed.pr_number);
        let pr_data = self.api_get(&pr_path).await?;
        let head_sha = pr_data["head"]["sha"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let review_comments: Vec<serde_json::Value> = comments
            .iter()
            .map(|c| {
                let mut comment = json!({
                    "body": c.body,
                    "path": c.path,
                    "line": c.line,
                    "side": c.side,
                });
                if let Some(start) = c.start_line {
                    comment["start_line"] = json!(start);
                    comment["start_side"] = json!(&c.side);
                }
                comment
            })
            .collect();

        let path = format!(
            "repos/{}/pulls/{}/reviews",
            self.repo_full, self.parsed.pr_number
        );
        let review_body = json!({
            "commit_id": head_sha,
            "event": "COMMENT",
            "comments": review_comments,
        });

        match self.api_post(&path, &review_body).await {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "bulk review failed, trying individual comments");
                for comment in comments {
                    let single = json!({
                        "commit_id": head_sha,
                        "event": "COMMENT",
                        "comments": [{
                            "body": comment.body,
                            "path": comment.path,
                            "line": comment.line,
                            "side": comment.side,
                        }],
                    });
                    if let Err(e) = self.api_post(&path, &single).await {
                        tracing::warn!(path = comment.path, error = %e, "individual comment failed");
                    }
                }
                Ok(())
            }
        }
    }

    async fn remove_initial_comment(&self) -> Result<(), PrAgentError> {
        Ok(())
    }

    async fn remove_comment(&self, comment_id: &CommentId) -> Result<(), PrAgentError> {
        let path = format!("repos/{}/issues/comments/{}", self.repo_full, comment_id.0);
        self.api_delete(&path).await
    }

    async fn publish_code_suggestions(
        &self,
        suggestions: &[CodeSuggestion],
    ) -> Result<bool, PrAgentError> {
        if suggestions.is_empty() {
            return Ok(false);
        }

        let pr_path = format!("repos/{}/pulls/{}", self.repo_full, self.parsed.pr_number);
        let pr_data = self.api_get(&pr_path).await?;
        let head_sha = pr_data["head"]["sha"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let comments: Vec<serde_json::Value> = suggestions
            .iter()
            .map(|s| {
                let body = format!("{}\n\n```suggestion\n{}\n```", s.body, s.improved_code);
                let mut comment = json!({
                    "body": body,
                    "path": s.relevant_file,
                    "line": s.relevant_lines_end,
                    "side": "RIGHT",
                });
                if s.relevant_lines_start != s.relevant_lines_end {
                    comment["start_line"] = json!(s.relevant_lines_start);
                    comment["start_side"] = json!("RIGHT");
                }
                comment
            })
            .collect();

        let path = format!(
            "repos/{}/pulls/{}/reviews",
            self.repo_full, self.parsed.pr_number
        );
        let body = json!({
            "commit_id": head_sha,
            "event": "COMMENT",
            "comments": comments,
        });

        self.api_post(&path, &body).await?;
        Ok(true)
    }

    async fn publish_labels(&self, labels: &[String]) -> Result<(), PrAgentError> {
        let path = format!(
            "repos/{}/issues/{}/labels",
            self.repo_full, self.parsed.pr_number
        );
        self.api_post(&path, &json!({"labels": labels})).await?;
        Ok(())
    }

    async fn get_pr_labels(&self) -> Result<Vec<String>, PrAgentError> {
        let path = format!(
            "repos/{}/issues/{}/labels",
            self.repo_full, self.parsed.pr_number
        );
        let data = self.api_get(&path).await?;
        let labels = data
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(labels)
    }

    async fn add_eyes_reaction(
        &self,
        comment_id: u64,
        disable_eyes: bool,
    ) -> Result<Option<u64>, PrAgentError> {
        if disable_eyes {
            return Ok(None);
        }
        let path = format!(
            "repos/{}/issues/comments/{}/reactions",
            self.repo_full, comment_id
        );
        let resp = self.api_post(&path, &json!({"content": "eyes"})).await?;
        Ok(resp["id"].as_u64())
    }

    async fn remove_reaction(&self, comment_id: u64, reaction_id: u64) -> Result<(), PrAgentError> {
        let path = format!(
            "repos/{}/issues/comments/{}/reactions/{}",
            self.repo_full, comment_id, reaction_id
        );
        self.api_delete(&path).await
    }

    async fn get_commit_messages(&self) -> Result<String, PrAgentError> {
        let path = format!(
            "repos/{}/pulls/{}/commits?per_page=100",
            self.repo_full, self.parsed.pr_number
        );
        let items = self.api_get_all_pages(&path).await?;
        let messages: Vec<String> = items
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                c["commit"]["message"]
                    .as_str()
                    .map(|m| format!("{}. {}", i + 1, m))
            })
            .collect();
        Ok(messages.join("\n"))
    }

    async fn get_repo_settings(&self) -> Result<Option<String>, PrAgentError> {
        match self.get_file_content(".pr_agent.toml", "HEAD").await {
            Ok(content) if !content.is_empty() => Ok(Some(content)),
            _ => Ok(None),
        }
    }

    async fn get_global_settings(&self) -> Result<Option<String>, PrAgentError> {
        let global_repo = format!("{}/pr-agent-settings", self.parsed.owner);
        tracing::debug!(repo = %global_repo, "checking for org-level global settings");
        match self
            .get_file_content_from_repo(&global_repo, ".pr_agent.toml", "HEAD")
            .await
        {
            Ok(content) if !content.is_empty() => {
                tracing::info!(repo = %global_repo, "loaded global org-level .pr_agent.toml");
                Ok(Some(content))
            }
            Ok(_) => Ok(None),
            Err(e) => {
                tracing::info!(
                    repo = %global_repo,
                    error = %e,
                    "no org-level pr-agent-settings repo found, continuing without global config"
                );
                Ok(None)
            }
        }
    }

    async fn get_issue_comments(&self) -> Result<Vec<IssueComment>, PrAgentError> {
        let path = format!(
            "repos/{}/issues/{}/comments?per_page=100",
            self.repo_full, self.parsed.pr_number
        );
        let items = self.api_get_all_pages(&path).await?;
        let comments = items
            .iter()
            .filter_map(|c| {
                Some(IssueComment {
                    id: c["id"].as_u64()?,
                    body: c["body"].as_str().unwrap_or_default().to_string(),
                    user: c["user"]["login"].as_str().unwrap_or_default().to_string(),
                    created_at: c["created_at"].as_str().unwrap_or_default().to_string(),
                    url: c["html_url"].as_str().map(|s| s.to_string()),
                })
            })
            .collect();
        Ok(comments)
    }

    fn is_supported(&self, capability: &str) -> bool {
        matches!(
            capability,
            "gfm_markdown" | "labels" | "reactions" | "code_suggestions" | "inline_comments"
        )
    }

    async fn edit_comment(&self, comment_id: &CommentId, body: &str) -> Result<(), PrAgentError> {
        let path = format!("repos/{}/issues/comments/{}", self.repo_full, comment_id.0);
        self.api_patch(&path, &json!({"body": body})).await?;
        Ok(())
    }

    async fn get_latest_commit_url(&self) -> Result<String, PrAgentError> {
        let path = format!(
            "repos/{}/pulls/{}/commits?per_page=100",
            self.repo_full, self.parsed.pr_number
        );
        let items = self.api_get_all_pages(&path).await?;
        let url = items
            .last()
            .and_then(|c| c["html_url"].as_str())
            .unwrap_or_default();
        Ok(url.to_string())
    }

    async fn get_best_practices(&self) -> Result<String, PrAgentError> {
        let settings = get_settings();

        // Config content takes priority — caller should check before calling,
        // but guard here too.
        if !settings.best_practices.content.is_empty() {
            return Ok(String::new());
        }

        match self.get_file_content("best_practices.md", "HEAD").await {
            Ok(content) if !content.is_empty() => {
                let max_lines = settings.best_practices.max_lines_allowed as usize;
                let truncated: String = content
                    .lines()
                    .take(max_lines)
                    .collect::<Vec<_>>()
                    .join("\n");
                tracing::info!(
                    lines = truncated.lines().count(),
                    max = max_lines,
                    "loaded best_practices.md from repo"
                );
                Ok(truncated)
            }
            _ => Ok(String::new()),
        }
    }

    async fn get_repo_metadata(&self) -> Result<String, PrAgentError> {
        let settings = get_settings();

        if !settings.config.add_repo_metadata {
            return Ok(String::new());
        }

        let file_list = &settings.config.add_repo_metadata_file_list;
        let mut combined = String::new();

        for filename in file_list {
            match self.get_file_content(filename, "HEAD").await {
                Ok(content) if !content.is_empty() => {
                    if !combined.is_empty() {
                        combined.push_str("\n\n");
                    }
                    combined.push_str(&format!("## From {}:\n{}", filename, content));
                    tracing::info!(file = %filename, "loaded repo metadata file");
                }
                _ => {
                    tracing::debug!(file = %filename, "repo metadata file not found, skipping");
                }
            }
        }

        Ok(combined)
    }

    async fn auto_approve(&self) -> Result<bool, PrAgentError> {
        let path = format!(
            "repos/{}/pulls/{}/reviews",
            self.repo_full, self.parsed.pr_number
        );
        let body = json!({ "event": "APPROVE" });
        match self.api_post(&path, &body).await {
            Ok(_) => {
                tracing::info!("PR auto-approved");
                Ok(true)
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to auto-approve PR");
                Err(e)
            }
        }
    }

    fn get_line_link(&self, file: &str, line_start: i32, line_end: Option<i32>) -> String {
        // Convert API URL back to web URL for links
        let web_base = self
            .base_url
            .replace("api.github.com", "github.com")
            .replace("/api/v3", "");

        // All links point to the PR files diff view
        use sha2::{Digest, Sha256};
        let hash = hex::encode(Sha256::digest(file.as_bytes()));

        if line_start == -1 {
            // PR files tab link without line anchor
            return format!(
                "{}/{}/pull/{}/files#diff-{}",
                web_base, self.repo_full, self.parsed.pr_number, hash
            );
        }

        // PR files tab link with line anchor(s)
        let base = format!(
            "{}/{}/pull/{}/files#diff-{}R{}",
            web_base, self.repo_full, self.parsed.pr_number, hash, line_start
        );
        match line_end {
            Some(end) if end != line_start => format!("{base}-R{end}"),
            _ => base,
        }
    }
}

/// Parse the `Link` header to find the `rel="next"` URL.
fn parse_next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get("link")?.to_str().ok()?;
    for part in link.split(',') {
        let part = part.trim();
        if part.contains(r#"rel="next""#) {
            // Extract URL between < and >
            let start = part.find('<')? + 1;
            let end = part.find('>')?;
            return Some(part[start..end].to_string());
        }
    }
    None
}

/// Count added (+) and removed (-) lines in a unified diff patch.
fn count_patch_lines(patch: &str) -> (i32, i32) {
    let mut plus = 0i32;
    let mut minus = 0i32;
    for line in patch.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            plus += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            minus += 1;
        }
    }
    (plus, minus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_patch_lines() {
        let patch = "\
@@ -1,5 +1,6 @@
 unchanged
-removed line
+added line
+another added
 context
";
        let (plus, minus) = count_patch_lines(patch);
        assert_eq!(plus, 2);
        assert_eq!(minus, 1);
    }

    #[test]
    fn test_count_patch_lines_empty() {
        let (plus, minus) = count_patch_lines("");
        assert_eq!(plus, 0);
        assert_eq!(minus, 0);
    }

    #[test]
    fn test_parse_next_link() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "link",
            r#"<https://api.github.com/repos/owner/repo/pulls/1/files?per_page=100&page=2>; rel="next", <https://api.github.com/repos/owner/repo/pulls/1/files?per_page=100&page=3>; rel="last""#
                .parse()
                .unwrap(),
        );
        let next = parse_next_link(&headers);
        assert_eq!(
            next.unwrap(),
            "https://api.github.com/repos/owner/repo/pulls/1/files?per_page=100&page=2"
        );
    }

    #[test]
    fn test_parse_next_link_no_next() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "link",
            r#"<https://api.github.com/repos/owner/repo/pulls/1/files?page=1>; rel="first""#
                .parse()
                .unwrap(),
        );
        assert!(parse_next_link(&headers).is_none());
    }

    #[test]
    fn test_parse_next_link_no_header() {
        let headers = reqwest::header::HeaderMap::new();
        assert!(parse_next_link(&headers).is_none());
    }
}
