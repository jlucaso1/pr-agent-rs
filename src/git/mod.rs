pub mod github;
pub mod types;
pub mod url_parser;

use std::collections::HashMap;

use async_trait::async_trait;
use types::*;

use crate::error::PrAgentError;

/// Capitalize the first letter of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Trait for git hosting platform providers (GitHub, GitLab, Bitbucket, etc.).
///
/// Methods with default implementations return `Err(Unsupported)` or sensible
/// defaults — providers only implement what their platform supports.
#[async_trait]
#[allow(dead_code)]
pub trait GitProvider: Send + Sync {
    // ── Required ──────────────────────────────────────────────────────

    /// Fetch diff information for each changed file in the PR.
    async fn get_diff_files(&self) -> Result<Vec<FilePatchInfo>, PrAgentError>;

    /// List all changed file paths.
    async fn get_files(&self) -> Result<Vec<String>, PrAgentError>;

    /// Repository language breakdown (language name → byte count).
    async fn get_languages(&self) -> Result<HashMap<String, u64>, PrAgentError>;

    /// Source branch name.
    async fn get_pr_branch(&self) -> Result<String, PrAgentError>;

    /// Target/base branch name.
    async fn get_pr_base_branch(&self) -> Result<String, PrAgentError>;

    /// Current authenticated user/bot identifier.
    async fn get_user_id(&self) -> Result<String, PrAgentError>;

    /// Full PR description: returns (title, body).
    async fn get_pr_description_full(&self) -> Result<(String, String), PrAgentError>;

    /// Update the PR title and description body.
    async fn publish_description(&self, title: &str, body: &str) -> Result<(), PrAgentError>;

    /// Post a comment on the PR. Returns the comment ID if available.
    async fn publish_comment(
        &self,
        text: &str,
        is_temporary: bool,
    ) -> Result<Option<CommentId>, PrAgentError>;

    /// Post an inline comment on a specific file and line.
    async fn publish_inline_comment(
        &self,
        body: &str,
        file: &str,
        line: &str,
        original_suggestion: Option<&str>,
    ) -> Result<(), PrAgentError>;

    /// Post multiple inline comments as an atomic review.
    async fn publish_inline_comments(&self, comments: &[InlineComment])
    -> Result<(), PrAgentError>;

    /// Remove the temporary progress comment.
    async fn remove_initial_comment(&self) -> Result<(), PrAgentError>;

    /// Remove a specific comment by ID.
    async fn remove_comment(&self, comment_id: &CommentId) -> Result<(), PrAgentError>;

    /// Publish code suggestions (inline comments with before/after code blocks).
    async fn publish_code_suggestions(
        &self,
        suggestions: &[CodeSuggestion],
    ) -> Result<bool, PrAgentError>;

    /// Apply labels to the PR.
    async fn publish_labels(&self, labels: &[String]) -> Result<(), PrAgentError>;

    /// Get current PR labels.
    async fn get_pr_labels(&self) -> Result<Vec<String>, PrAgentError>;

    /// Add eyes reaction. Returns reaction ID if successful.
    async fn add_eyes_reaction(
        &self,
        comment_id: u64,
        disable_eyes: bool,
    ) -> Result<Option<u64>, PrAgentError>;

    /// Remove a reaction from a comment.
    async fn remove_reaction(&self, comment_id: u64, reaction_id: u64) -> Result<(), PrAgentError>;

    /// Get concatenated commit messages for the PR.
    async fn get_commit_messages(&self) -> Result<String, PrAgentError>;

    /// Fetch repository-level `.pr_agent.toml` content, if it exists.
    async fn get_repo_settings(&self) -> Result<Option<String>, PrAgentError>;

    /// Fetch organization-level `.pr_agent.toml` from a `pr-agent-settings`
    /// repo in the same org/owner, if it exists.
    ///
    /// Returns `Ok(None)` if the repo or file does not exist.
    async fn get_global_settings(&self) -> Result<Option<String>, PrAgentError> {
        Ok(None)
    }

    /// Get all comments on the PR.
    async fn get_issue_comments(&self) -> Result<Vec<IssueComment>, PrAgentError>;

    // ── Provided defaults ────────────────────────────────────────────

    /// PR URL.
    fn get_pr_url(&self) -> &str {
        ""
    }

    /// Whether this provider supports a named capability.
    fn is_supported(&self, _capability: &str) -> bool {
        false
    }

    /// Find an existing comment by header marker, update it, or create a new one.
    ///
    /// Find-or-create a persistent comment:
    /// 1. Search existing comments for `initial_header` marker
    /// 2. If found: edit in place with updated content + commit header
    /// 3. If not found: create new comment
    async fn publish_persistent_comment(
        &self,
        text: &str,
        initial_header: &str,
        _update_header: &str,
        name: &str,
        final_update_message: bool,
    ) -> Result<(), PrAgentError> {
        let comments = self.get_issue_comments().await?;
        for comment in &comments {
            if comment.body.starts_with(initial_header) {
                tracing::info!(
                    comment_id = comment.id,
                    "updating existing persistent comment"
                );
                let comment_url = comment.url.as_deref().unwrap_or("");

                // Add "updated until commit" header
                let latest_commit_url = self.get_latest_commit_url().await.unwrap_or_default();
                let updated_text = if !latest_commit_url.is_empty() {
                    let cap_name = capitalize_first(name);
                    let updated_header = format!(
                        "{initial_header}\n\n#### ({cap_name} updated until commit {latest_commit_url})\n"
                    );
                    text.replace(initial_header, &updated_header)
                } else {
                    text.to_string()
                };

                self.edit_comment(&CommentId(comment.id.to_string()), &updated_text)
                    .await?;

                // Post notification comment linking to updated persistent comment
                if final_update_message && !comment_url.is_empty() && !latest_commit_url.is_empty()
                {
                    let notification = format!(
                        "**[Persistent {name}]({comment_url})** updated to latest commit {latest_commit_url}"
                    );
                    let _ = self.publish_comment(&notification, false).await;
                }

                return Ok(());
            }
        }
        tracing::info!("creating new persistent comment");
        self.publish_comment(text, false).await?;
        Ok(())
    }

    /// Get URL for the latest commit in the PR.
    async fn get_latest_commit_url(&self) -> Result<String, PrAgentError> {
        Ok(String::new())
    }

    /// Edit an existing comment.
    async fn edit_comment(&self, _comment_id: &CommentId, _body: &str) -> Result<(), PrAgentError> {
        Err(PrAgentError::Unsupported("edit_comment".into()))
    }

    /// Reply to a specific review comment (inline code comment thread).
    async fn reply_to_comment(&self, _comment_id: u64, _body: &str) -> Result<(), PrAgentError> {
        Err(PrAgentError::Unsupported("reply_to_comment".into()))
    }

    /// Get all comments in the same review thread as the given comment.
    async fn get_review_thread_comments(
        &self,
        _comment_id: u64,
    ) -> Result<Vec<IssueComment>, PrAgentError> {
        Err(PrAgentError::Unsupported(
            "get_review_thread_comments".into(),
        ))
    }

    /// Create or update a file in the repo (e.g. for changelog pushes).
    async fn create_or_update_pr_file(
        &self,
        _file_path: &str,
        _branch: &str,
        _contents: &[u8],
        _message: &str,
    ) -> Result<(), PrAgentError> {
        Err(PrAgentError::Unsupported("create_or_update_pr_file".into()))
    }

    /// Auto-approve the PR.
    async fn auto_approve(&self) -> Result<bool, PrAgentError> {
        Ok(false)
    }

    /// Git clone URL for the repository.
    fn get_git_repo_url(&self) -> String {
        String::new()
    }

    /// Get URL linking to specific lines in a file.
    fn get_line_link(&self, _file: &str, _line_start: i32, _line_end: Option<i32>) -> String {
        String::new()
    }

    /// Number of changed files in the PR.
    async fn get_num_of_files(&self) -> Result<usize, PrAgentError> {
        Ok(self.get_diff_files().await?.len())
    }

    /// Get the PR number/ID.
    fn get_pr_id(&self) -> &str {
        ""
    }

    /// Fetch `best_practices.md` content from the repo root.
    ///
    /// Returns the file content truncated to `max_lines_allowed`, or empty
    /// string if the file doesn't exist. Config `best_practices.content`
    /// takes priority over the repo file (checked by the caller).
    async fn get_best_practices(&self) -> Result<String, PrAgentError> {
        Ok(String::new())
    }

    /// Fetch repo metadata files (e.g. AGENTS.MD, CLAUDE.MD).
    ///
    /// Returns concatenated content of all found files with headers,
    /// or empty string if none exist or `add_repo_metadata` is false.
    async fn get_repo_metadata(&self) -> Result<String, PrAgentError> {
        Ok(String::new())
    }
}
