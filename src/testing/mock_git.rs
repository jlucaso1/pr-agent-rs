use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::PrAgentError;
use crate::git::GitProvider;
use crate::git::types::*;

/// Captured calls made to the mock provider, for test assertions.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct MockCalls {
    pub comments: Vec<(String, bool)>,
    pub descriptions: Vec<(String, String)>,
    pub labels: Vec<Vec<String>>,
    pub removed_comments: Vec<String>,
    pub code_suggestions: Vec<Vec<CodeSuggestion>>,
    pub inline_comments: Vec<Vec<InlineComment>>,
    pub edited_comments: Vec<(String, String)>,
    pub auto_approvals: Vec<()>,
}

/// Mock git provider for integration tests.
///
/// Pre-configured with PR metadata and diff files. Captures all publish/remove
/// calls for assertions.
pub struct MockGitProvider {
    pub title: String,
    pub description: String,
    pub branch: String,
    pub commit_messages: String,
    pub diff_files: Vec<FilePatchInfo>,
    pub issue_comments: Vec<IssueComment>,
    pub repo_settings_toml: Option<String>,
    pub global_settings_toml: Option<String>,
    pub calls: Mutex<MockCalls>,
}

impl MockGitProvider {
    pub fn new() -> Self {
        Self {
            title: "Test PR title".into(),
            description: "Test PR description".into(),
            branch: "feature/test".into(),
            commit_messages: "feat: add test feature".into(),
            diff_files: Vec::new(),
            issue_comments: Vec::new(),
            repo_settings_toml: None,
            global_settings_toml: None,
            calls: Mutex::new(MockCalls::default()),
        }
    }

    pub fn with_diff_files(mut self, files: Vec<FilePatchInfo>) -> Self {
        self.diff_files = files;
        self
    }

    pub fn with_pr_description(mut self, title: &str, description: &str) -> Self {
        self.title = title.into();
        self.description = description.into();
        self
    }

    pub fn with_repo_settings(mut self, toml: &str) -> Self {
        self.repo_settings_toml = Some(toml.into());
        self
    }

    pub fn with_global_settings(mut self, toml: &str) -> Self {
        self.global_settings_toml = Some(toml.into());
        self
    }

    pub fn get_calls(&self) -> std::sync::MutexGuard<'_, MockCalls> {
        self.calls.lock().unwrap()
    }
}

#[async_trait]
impl GitProvider for MockGitProvider {
    async fn get_diff_files(&self) -> Result<Vec<FilePatchInfo>, PrAgentError> {
        Ok(self.diff_files.clone())
    }

    async fn get_files(&self) -> Result<Vec<String>, PrAgentError> {
        Ok(self.diff_files.iter().map(|f| f.filename.clone()).collect())
    }

    async fn get_languages(&self) -> Result<HashMap<String, u64>, PrAgentError> {
        Ok(HashMap::new())
    }

    async fn get_pr_branch(&self) -> Result<String, PrAgentError> {
        Ok(self.branch.clone())
    }

    async fn get_pr_base_branch(&self) -> Result<String, PrAgentError> {
        Ok("main".into())
    }

    async fn get_user_id(&self) -> Result<String, PrAgentError> {
        Ok("mock-bot[bot]".into())
    }

    async fn get_pr_description_full(&self) -> Result<(String, String), PrAgentError> {
        Ok((self.title.clone(), self.description.clone()))
    }

    async fn publish_description(&self, title: &str, body: &str) -> Result<(), PrAgentError> {
        self.calls
            .lock()
            .unwrap()
            .descriptions
            .push((title.into(), body.into()));
        Ok(())
    }

    async fn publish_comment(
        &self,
        text: &str,
        is_temporary: bool,
    ) -> Result<Option<CommentId>, PrAgentError> {
        self.calls
            .lock()
            .unwrap()
            .comments
            .push((text.into(), is_temporary));
        Ok(Some(CommentId("mock-comment-1".into())))
    }

    async fn publish_inline_comment(
        &self,
        _body: &str,
        _file: &str,
        _line: &str,
        _original_suggestion: Option<&str>,
    ) -> Result<(), PrAgentError> {
        Ok(())
    }

    async fn publish_inline_comments(
        &self,
        comments: &[InlineComment],
    ) -> Result<(), PrAgentError> {
        self.calls
            .lock()
            .unwrap()
            .inline_comments
            .push(comments.to_vec());
        Ok(())
    }

    async fn remove_initial_comment(&self) -> Result<(), PrAgentError> {
        Ok(())
    }

    async fn remove_comment(&self, comment_id: &CommentId) -> Result<(), PrAgentError> {
        self.calls
            .lock()
            .unwrap()
            .removed_comments
            .push(comment_id.0.clone());
        Ok(())
    }

    async fn publish_code_suggestions(
        &self,
        suggestions: &[CodeSuggestion],
    ) -> Result<bool, PrAgentError> {
        self.calls
            .lock()
            .unwrap()
            .code_suggestions
            .push(suggestions.to_vec());
        Ok(true)
    }

    async fn publish_labels(&self, labels: &[String]) -> Result<(), PrAgentError> {
        self.calls.lock().unwrap().labels.push(labels.to_vec());
        Ok(())
    }

    async fn get_pr_labels(&self) -> Result<Vec<String>, PrAgentError> {
        Ok(vec![])
    }

    async fn add_eyes_reaction(
        &self,
        _comment_id: u64,
        _disable_eyes: bool,
    ) -> Result<Option<u64>, PrAgentError> {
        Ok(None)
    }

    async fn remove_reaction(
        &self,
        _comment_id: u64,
        _reaction_id: u64,
    ) -> Result<(), PrAgentError> {
        Ok(())
    }

    async fn get_commit_messages(&self) -> Result<String, PrAgentError> {
        Ok(self.commit_messages.clone())
    }

    async fn get_repo_settings(&self) -> Result<Option<String>, PrAgentError> {
        Ok(self.repo_settings_toml.clone())
    }

    async fn get_global_settings(&self) -> Result<Option<String>, PrAgentError> {
        Ok(self.global_settings_toml.clone())
    }

    async fn get_issue_comments(&self) -> Result<Vec<IssueComment>, PrAgentError> {
        Ok(self.issue_comments.clone())
    }

    fn is_supported(&self, capability: &str) -> bool {
        capability == "gfm_markdown"
    }

    async fn edit_comment(&self, comment_id: &CommentId, body: &str) -> Result<(), PrAgentError> {
        self.calls
            .lock()
            .unwrap()
            .edited_comments
            .push((comment_id.0.clone(), body.into()));
        Ok(())
    }

    async fn auto_approve(&self) -> Result<bool, PrAgentError> {
        self.calls.lock().unwrap().auto_approvals.push(());
        Ok(true)
    }
}
