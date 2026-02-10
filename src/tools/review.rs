use std::collections::HashMap;
use std::sync::Arc;

use minijinja::Value;

use crate::ai::AiHandler;
use crate::config::loader::get_settings;
use crate::config::types::Settings;
use crate::error::PrAgentError;
use crate::git::GitProvider;
use crate::output::review_formatter::{
    LinkGenerator, extract_effort_score, format_review_markdown, is_value_no, yaml_value_to_string,
};
use crate::output::yaml_parser::load_yaml;
use crate::processing::compression::get_pr_diff;
use crate::template::render::render_prompt;
use crate::tools::{
    PrMetadata, build_common_vars, insert_custom_labels_vars, publish_as_comment,
    with_progress_comment,
};

/// PR Reviewer tool.
///
/// Fetches diff, calls AI, formats the response as markdown,
/// and publishes as a persistent comment.
pub struct PRReviewer {
    provider: Arc<dyn GitProvider>,
    ai: Option<Arc<dyn AiHandler>>,
}

impl PRReviewer {
    pub fn new(provider: Arc<dyn GitProvider>) -> Self {
        Self { provider, ai: None }
    }

    #[cfg(test)]
    pub fn new_with_ai(provider: Arc<dyn GitProvider>, ai: Arc<dyn AiHandler>) -> Self {
        Self {
            provider,
            ai: Some(ai),
        }
    }

    /// Run the full review pipeline.
    pub async fn run(&self) -> Result<(), PrAgentError> {
        let provider = &self.provider;
        with_progress_comment(provider.as_ref(), "Preparing review...", || {
            self.run_inner()
        })
        .await
    }

    async fn run_inner(&self) -> Result<(), PrAgentError> {
        let settings = get_settings();
        let model = &settings.config.model;

        // 1. Fetch PR metadata
        let meta = PrMetadata::fetch(self.provider.as_ref(), &settings).await?;

        // 2. Fetch and process diff
        let mut files = self.provider.get_diff_files().await?;
        let num_files = files.len();
        tracing::info!(num_files, "processing changed files for review");

        let diff_result = get_pr_diff(
            &mut files, model, true, /* add_line_numbers for review */
        );
        drop(files); // release file contents now that diff is built
        tracing::info!(
            tokens = diff_result.token_count,
            files_included = diff_result.files_in_diff.len(),
            remaining = diff_result.remaining_files.len(),
            "diff processed"
        );

        // 3. Build template variables
        let vars = self.build_vars(&meta, &diff_result.diff, num_files);

        // 4. Render prompt
        let rendered = render_prompt(&settings.pr_review_prompt, vars)?;

        // 5. Call AI (with fallback models)
        tracing::info!(model, "calling AI model for review");
        let ai = super::resolve_ai_handler(&self.ai)?;
        let response = crate::ai::chat_completion_with_fallback(
            ai.as_ref(),
            model,
            &settings.config.fallback_models,
            &rendered.system,
            &rendered.user,
            Some(settings.config.temperature),
            None,
        )
        .await?;

        tracing::info!(
            tokens = response.usage.as_ref().map_or(0, |u| u.total_tokens),
            finish_reason = ?response.finish_reason,
            "AI response received"
        );

        // 6. Parse YAML from response
        let yaml_data = load_yaml(
            &response.content,
            &[
                "estimated_effort_to_review_[1-5]:",
                "security_concerns:",
                "key_issues_to_review:",
                "relevant_file:",
                "issue_header:",
                "issue_content:",
                "ticket_compliance_check:",
            ],
            "review",
            "security_concerns",
        );

        // 7. Format and publish
        if settings.config.publish_output {
            self.publish_review(yaml_data.as_ref(), &response.content)
                .await?;
        } else {
            self.print_review(yaml_data.as_ref(), &response.content);
        }

        Ok(())
    }

    fn build_vars(
        &self,
        meta: &PrMetadata,
        diff: &str,
        num_files: usize,
    ) -> HashMap<String, Value> {
        let settings = get_settings();
        let mut vars = build_common_vars(meta, diff);

        // Review-specific variables
        vars.insert("num_pr_files".into(), Value::from(num_files));
        vars.insert(
            "num_max_findings".into(),
            Value::from(settings.pr_reviewer.num_max_findings),
        );
        vars.insert(
            "require_score".into(),
            Value::from(settings.pr_reviewer.require_score_review),
        );
        vars.insert(
            "require_tests".into(),
            Value::from(settings.pr_reviewer.require_tests_review),
        );
        vars.insert(
            "require_estimate_effort_to_review".into(),
            Value::from(settings.pr_reviewer.require_estimate_effort_to_review),
        );
        vars.insert(
            "require_estimate_contribution_time_cost".into(),
            Value::from(settings.pr_reviewer.require_estimate_contribution_time_cost),
        );
        vars.insert(
            "require_can_be_split_review".into(),
            Value::from(settings.pr_reviewer.require_can_be_split_review),
        );
        vars.insert(
            "require_security_review".into(),
            Value::from(settings.pr_reviewer.require_security_review),
        );
        vars.insert(
            "require_todo_scan".into(),
            Value::from(settings.pr_reviewer.require_todo_scan),
        );
        vars.insert(
            "require_ticket_analysis_review".into(),
            Value::from(settings.pr_reviewer.require_ticket_analysis_review),
        );
        vars.insert("question_str".into(), Value::from(""));
        vars.insert("answer_str".into(), Value::from(""));
        vars.insert(
            "extra_instructions".into(),
            Value::from(settings.pr_reviewer.extra_instructions.as_str()),
        );
        insert_custom_labels_vars(&mut vars, &settings);
        vars.insert("is_ai_metadata".into(), Value::from(false));
        vars.insert("related_tickets".into(), Value::from(Vec::<String>::new()));
        vars.insert("duplicate_prompt_examples".into(), Value::from(false));
        vars.insert(
            "date".into(),
            Value::from(chrono::Utc::now().format("%Y-%m-%d").to_string()),
        );

        vars
    }

    /// Publish the formatted review to the PR.
    async fn publish_review(
        &self,
        yaml_data: Option<&serde_yaml_ng::Value>,
        raw_response: &str,
    ) -> Result<(), PrAgentError> {
        let settings = get_settings();
        let gfm_supported = self.provider.is_supported("gfm_markdown");

        // Build link generator from provider
        let provider = self.provider.clone();
        let link_gen: LinkGenerator = Box::new(move |file: &str, start: i32, end: Option<i32>| {
            provider.get_line_link(file, start, end)
        });

        let markdown = match yaml_data {
            Some(data) => format_review_markdown(data, gfm_supported, Some(&link_gen)),
            None => {
                tracing::warn!("could not parse YAML from AI response, publishing raw");
                format!("## PR Reviewer Guide 🔍\n\n{}\n", raw_response)
            }
        };

        publish_as_comment(
            self.provider.as_ref(),
            &markdown,
            "review",
            settings.pr_reviewer.persistent_comment,
            settings.pr_reviewer.final_update_message,
        )
        .await?;

        // Publish review labels (effort / security) if enabled
        if let Some(data) = yaml_data {
            self.publish_review_labels(data, &settings).await?;
        }

        Ok(())
    }

    /// Extract and publish review labels (effort score, security concern) from AI response.
    async fn publish_review_labels(
        &self,
        data: &serde_yaml_ng::Value,
        settings: &Settings,
    ) -> Result<(), PrAgentError> {
        let review = data.get("review").unwrap_or(data);
        let mut labels = Vec::new();

        if settings.pr_reviewer.enable_review_labels_effort
            && let Some(effort_val) = review
                .get("estimated_effort_to_review_[1-5]")
                .or_else(|| review.get("estimated_effort_to_review"))
        {
            let effort = extract_effort_score(effort_val);
            labels.push(format!("Review effort [1-5]: {effort}"));
        }

        if settings.pr_reviewer.enable_review_labels_security
            && let Some(sec_val) = review.get("security_concerns")
        {
            let text = yaml_value_to_string(sec_val);
            if !is_value_no(&text) {
                labels.push("Security concern".to_string());
            }
        }

        if !labels.is_empty() {
            tracing::info!(?labels, "publishing review labels");
            self.provider.publish_labels(&labels).await?;
        }

        Ok(())
    }

    /// Print review to stdout (CLI mode).
    fn print_review(&self, yaml_data: Option<&serde_yaml_ng::Value>, raw_response: &str) {
        match yaml_data {
            Some(data) => {
                let formatted = format_review_markdown(data, true, None);
                println!("{formatted}");
            }
            None => {
                eprintln!("Warning: could not parse YAML from AI response, printing raw:");
                println!("{raw_response}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::loader::with_settings;
    use crate::testing::fixtures::{REVIEW_YAML, SAMPLE_PATCH, sample_diff_file};
    use crate::testing::mock_ai::MockAiHandler;
    use crate::testing::mock_git::MockGitProvider;

    fn test_settings() -> Arc<Settings> {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("config.publish_output".into(), "true".into());
        overrides.insert("config.publish_output_progress".into(), "false".into());
        Arc::new(
            crate::config::loader::load_settings(&overrides, None, None)
                .expect("should load test settings"),
        )
    }

    #[tokio::test]
    async fn test_review_pipeline_end_to_end() {
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::new(REVIEW_YAML));
        let reviewer = PRReviewer::new_with_ai(provider.clone(), ai.clone());

        let settings = test_settings();
        with_settings(settings, reviewer.run()).await.unwrap();

        let calls = provider.get_calls();
        // Should publish a comment (persistent comment via publish_comment fallback)
        assert!(!calls.comments.is_empty(), "should publish a comment");
        let comment = &calls.comments[0].0;
        assert!(
            comment.contains("<!-- pr-agent:review -->"),
            "comment should contain review marker"
        );
        assert!(
            comment.contains("PR Reviewer Guide"),
            "comment should contain review header"
        );
        assert!(
            comment.contains("Potential null pointer"),
            "comment should contain the key issue"
        );
        assert_eq!(ai.get_call_count(), 1, "should call AI exactly once");
    }

    #[tokio::test]
    async fn test_review_handles_malformed_yaml() {
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        // YAML that parses but has no "review" key — should still produce output
        let ai = Arc::new(MockAiHandler::new("```yaml\nunrelated_key: value\n```"));
        let reviewer = PRReviewer::new_with_ai(provider.clone(), ai);

        let settings = test_settings();
        with_settings(settings, reviewer.run()).await.unwrap();

        let calls = provider.get_calls();
        assert!(!calls.comments.is_empty(), "should still publish a comment");
        let comment = &calls.comments[0].0;
        // Even with wrong YAML structure, review formatter produces output
        assert!(
            comment.contains("PR Reviewer Guide"),
            "should contain review header even with malformed data"
        );
    }

    #[tokio::test]
    async fn test_review_publishes_labels_when_enabled() {
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::new(REVIEW_YAML));
        let reviewer = PRReviewer::new_with_ai(provider.clone(), ai);

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("config.publish_output".into(), "true".into());
        overrides.insert("config.publish_output_progress".into(), "false".into());
        overrides.insert(
            "pr_reviewer.enable_review_labels_effort".into(),
            "true".into(),
        );
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());

        with_settings(settings, reviewer.run()).await.unwrap();

        let calls = provider.get_calls();
        assert!(!calls.labels.is_empty(), "should publish effort labels");
        let labels = &calls.labels[0];
        assert!(
            labels.iter().any(|l| l.contains("Review effort")),
            "should include effort score label"
        );
    }

    #[tokio::test]
    async fn test_review_empty_diff() {
        let provider = Arc::new(MockGitProvider::new()); // no diff files
        let ai = Arc::new(MockAiHandler::new(REVIEW_YAML));
        let reviewer = PRReviewer::new_with_ai(provider.clone(), ai.clone());

        let settings = test_settings();
        // Should still succeed even with empty diff
        with_settings(settings, reviewer.run()).await.unwrap();
        // AI is still called (with empty diff)
        assert_eq!(ai.get_call_count(), 1);
    }

    #[tokio::test]
    async fn test_progress_comment_lifecycle() {
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::new(REVIEW_YAML));
        let reviewer = PRReviewer::new_with_ai(provider.clone(), ai);

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("config.publish_output".into(), "true".into());
        overrides.insert("config.publish_output_progress".into(), "true".into());
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());
        with_settings(settings, reviewer.run()).await.unwrap();

        let calls = provider.get_calls();
        // Should have a temporary progress comment that was then removed
        let has_temp_comment = calls.comments.iter().any(|(_, is_temp)| *is_temp);
        assert!(
            has_temp_comment,
            "should create a temporary progress comment"
        );
        assert!(
            !calls.removed_comments.is_empty(),
            "should remove the progress comment after completion"
        );
    }
}
