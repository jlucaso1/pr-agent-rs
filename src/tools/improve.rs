use std::collections::HashMap;
use std::sync::Arc;

use minijinja::Value;

use crate::ai::AiHandler;
use crate::config::loader::get_settings;
use crate::error::PrAgentError;
use crate::git::GitProvider;
use crate::output::improve_formatter::{
    ParsedSuggestion, append_self_review_checkbox, format_suggestions_table, parse_suggestions,
    suggestions_to_code_suggestions,
};
use crate::output::yaml_parser::{load_yaml, yaml_value_as_i64, yaml_value_as_u64};
use futures_util::future::join_all;

use crate::processing::compression::get_pr_diff_multiple_patches;
use crate::template::render::render_prompt;
use crate::tools::{PrMetadata, build_common_vars, publish_as_comment, with_progress_comment};

/// PR Code Suggestions tool.
///
/// Fetches diff, calls AI, and formats the response as inline code
/// suggestions or a summary table.
pub struct PRCodeSuggestions {
    provider: Arc<dyn GitProvider>,
    ai: Option<Arc<dyn AiHandler>>,
}

impl PRCodeSuggestions {
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

    /// Run the full improve pipeline.
    pub async fn run(&self) -> Result<(), PrAgentError> {
        let provider = &self.provider;
        with_progress_comment(provider.as_ref(), "Preparing code suggestions...", || {
            self.run_inner()
        })
        .await
    }

    async fn run_inner(&self) -> Result<(), PrAgentError> {
        let settings = get_settings();
        let model = &settings.config.model;

        // 1. Fetch PR metadata
        let meta = PrMetadata::fetch(self.provider.as_ref(), &settings).await?;

        // 2. Fetch and split diff into batches (extended mode).
        let mut files = self.provider.get_diff_files().await?;
        let num_files = files.len();
        tracing::info!(num_files, "processing changed files for improve");

        let max_calls = settings.pr_code_suggestions.max_number_of_calls as usize;

        // Generate batches without line numbers (for the suggestion prompt)
        let batches_no_lines = get_pr_diff_multiple_patches(&mut files, model, false, max_calls);
        // Generate batches with line numbers (for the reflect prompt).
        // filter_files is idempotent so this operates on the already-filtered set.
        let batches_with_lines = get_pr_diff_multiple_patches(&mut files, model, true, max_calls);

        if batches_no_lines.is_empty() {
            tracing::info!("no diff content, skipping improve");
            return Ok(());
        }

        let ai = super::resolve_ai_handler(&self.ai)?;
        let num_batches = batches_no_lines.len();
        tracing::info!(num_batches, num_files, "processing PR in extended mode");

        // 3. Process batches (parallel or sequential)
        let all_suggestions = if settings.pr_code_suggestions.parallel_calls && num_batches > 1 {
            let futures: Vec<_> = batches_no_lines
                .iter()
                .zip(batches_with_lines.iter())
                .enumerate()
                .map(|(i, (batch, batch_lines))| {
                    self.process_single_batch(
                        ai.as_ref(),
                        model,
                        &meta,
                        &batch.patches,
                        &batch_lines.patches,
                        i,
                    )
                })
                .collect();
            let results = join_all(futures).await;
            results
                .into_iter()
                .enumerate()
                .flat_map(|(i, r)| match r {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(batch = i, error = %e, "batch failed");
                        Vec::new()
                    }
                })
                .collect::<Vec<_>>()
        } else {
            let mut all = Vec::new();
            for (i, (batch, batch_lines)) in batches_no_lines
                .iter()
                .zip(batches_with_lines.iter())
                .enumerate()
            {
                match self
                    .process_single_batch(
                        ai.as_ref(),
                        model,
                        &meta,
                        &batch.patches,
                        &batch_lines.patches,
                        i,
                    )
                    .await
                {
                    Ok(suggestions) => all.extend(suggestions),
                    Err(e) => tracing::error!(batch = i, error = %e, "batch failed"),
                }
            }
            all
        };

        // 4. Filter by score threshold, sort, deduplicate
        let score_threshold = settings
            .pr_code_suggestions
            .suggestions_score_threshold
            .max(1);
        let mut suggestions: Vec<ParsedSuggestion> = all_suggestions
            .into_iter()
            .filter(|s| s.score >= score_threshold && s.score > 0)
            .collect();
        suggestions.sort_by(|a, b| b.score.cmp(&a.score));

        // 5. Format and publish
        if settings.config.publish_output {
            self.publish_suggestions(&suggestions, false).await?;
        } else {
            self.print_suggestions(&suggestions);
        }

        Ok(())
    }

    /// Process a single diff batch: AI call + reflect pass.
    ///
    /// Returns scored (but unfiltered) suggestions for this batch.
    async fn process_single_batch(
        &self,
        ai: &dyn AiHandler,
        model: &str,
        meta: &PrMetadata,
        diff: &str,
        diff_with_lines: &str,
        batch_index: usize,
    ) -> Result<Vec<ParsedSuggestion>, PrAgentError> {
        let settings = get_settings();

        // 1. Build template variables
        let vars = self.build_vars(meta, diff);

        // 2. Render prompt
        let rendered = render_prompt(&settings.pr_code_suggestions_prompt, &vars)?;

        // 3. Call AI (generate suggestions, with fallback models)
        tracing::info!(model, batch = batch_index, "calling AI model for improve");
        let response = crate::ai::chat_completion_with_fallback(
            ai,
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
            batch = batch_index,
            "AI response received (improve pass 1)"
        );

        // 4. Parse YAML
        let yaml_data = load_yaml(&response.content, &[], "code_suggestions", "improved_code");
        let mut suggestions = yaml_data
            .as_ref()
            .map(parse_suggestions)
            .unwrap_or_default();

        if suggestions.is_empty() {
            return Ok(suggestions);
        }

        // 5. Self-reflect pass (per-batch)
        match self
            .self_reflect_on_suggestions(ai, model, &suggestions, diff_with_lines, &settings)
            .await
        {
            Ok(feedback) => {
                apply_reflect_feedback(&mut suggestions, &feedback);
                tracing::info!(
                    count = suggestions.len(),
                    batch = batch_index,
                    "applied reflect feedback to suggestions"
                );
            }
            Err(e) => {
                tracing::warn!(batch = batch_index, error = %e, "reflect pass failed, using default scores");
                for s in &mut suggestions {
                    if s.score == 0 {
                        s.score = 7;
                    }
                }
            }
        }

        Ok(suggestions)
    }

    /// Self-reflect on suggestions: second AI call to score and locate them.
    ///
    /// Second AI call to score and locate each suggestion in the diff.
    async fn self_reflect_on_suggestions(
        &self,
        ai: &dyn AiHandler,
        model: &str,
        suggestions: &[ParsedSuggestion],
        diff_with_lines: &str,
        settings: &crate::config::types::Settings,
    ) -> Result<Vec<ReflectFeedback>, PrAgentError> {
        // Build suggestion string for the self-reflect prompt
        let mut suggestion_str = String::new();
        for (i, s) in suggestions.iter().enumerate() {
            use std::fmt::Write;
            let _ = writeln!(
                suggestion_str,
                "suggestion {}: {{'relevant_file': '{}', 'suggestion_content': '{}', 'existing_code': '{}', 'improved_code': '{}', 'one_sentence_summary': '{}', 'label': '{}'}}",
                i + 1,
                s.relevant_file,
                s.suggestion_content.replace('\'', "\\'"),
                s.existing_code.replace('\'', "\\'"),
                s.improved_code.replace('\'', "\\'"),
                s.one_sentence_summary.replace('\'', "\\'"),
                s.label,
            );
        }

        // Build template variables for reflect prompt
        let mut vars = HashMap::new();
        vars.insert("diff".into(), Value::from(diff_with_lines));
        vars.insert(
            "suggestion_str".into(),
            Value::from(suggestion_str.as_str()),
        );
        vars.insert(
            "num_code_suggestions".into(),
            Value::from(suggestions.len() as u32),
        );
        vars.insert("is_ai_metadata".into(), Value::from(false));
        vars.insert(
            "duplicate_prompt_examples".into(),
            Value::from(settings.config.duplicate_prompt_examples),
        );

        // Render reflect prompt
        let rendered = render_prompt(&settings.pr_code_suggestions_reflect_prompt, &vars)?;

        // Call AI (second pass -- reflect, with fallback models)
        tracing::info!(model, "calling AI model for improve reflect pass");
        let response = crate::ai::chat_completion_with_fallback(
            ai,
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
            "AI response received (improve pass 2 - reflect)"
        );

        // Parse reflect YAML
        let reflect_yaml = load_yaml(
            &response.content,
            &[],
            "code_suggestions",
            "suggestion_score",
        );

        let feedback = reflect_yaml
            .as_ref()
            .map(parse_reflect_response)
            .unwrap_or_else(|| {
                tracing::warn!("could not parse reflect YAML response");
                Vec::new()
            });

        Ok(feedback)
    }

    fn build_vars(&self, meta: &PrMetadata, diff: &str) -> HashMap<String, Value> {
        let settings = get_settings();
        let mut vars = build_common_vars(meta, diff);

        // Improve-specific variables
        // The template uses diff_no_line_numbers (diff is generated without line numbers for improve)
        vars.insert("diff_no_line_numbers".into(), Value::from(diff));
        vars.insert(
            "extra_instructions".into(),
            Value::from(settings.pr_code_suggestions.extra_instructions.as_str()),
        );
        vars.insert(
            "num_code_suggestions".into(),
            Value::from(settings.pr_code_suggestions.num_code_suggestions_per_chunk),
        );
        vars.insert(
            "focus_only_on_problems".into(),
            Value::from(settings.pr_code_suggestions.focus_only_on_problems),
        );
        vars.insert("is_ai_metadata".into(), Value::from(false));
        vars.insert(
            "duplicate_prompt_examples".into(),
            Value::from(settings.config.duplicate_prompt_examples),
        );
        vars.insert(
            "date".into(),
            Value::from(chrono::Utc::now().format("%Y-%m-%d").to_string()),
        );

        vars
    }

    /// Publish suggestions to the PR.
    ///
    /// Three modes:
    /// 1. **Dual publishing** (`dual_publishing_score_threshold > -1`): publish
    ///    high-scoring suggestions as inline committable comments AND all
    ///    suggestions as a summary table.
    /// 2. **Inline-only** (`commitable_code_suggestions = true`): publish as
    ///    inline GitHub code suggestions; fall back to table on failure.
    /// 3. **Table-only** (default): publish as persistent comment table.
    async fn publish_suggestions(
        &self,
        suggestions: &[ParsedSuggestion],
        reflect_failed: bool,
    ) -> Result<(), PrAgentError> {
        let settings = get_settings();

        if suggestions.is_empty() {
            tracing::info!("no code suggestions to publish");
            return Ok(());
        }

        tracing::info!(count = suggestions.len(), "publishing code suggestions");

        let threshold = settings.pr_code_suggestions.dual_publishing_score_threshold;

        if threshold > -1 {
            // Dual publishing mode: inline high-scoring + table for all
            let threshold_u32 = threshold.max(0) as u32;
            let high_scoring: Vec<ParsedSuggestion> = suggestions
                .iter()
                .filter(|s| s.score >= threshold_u32)
                .cloned()
                .collect();

            if !high_scoring.is_empty() {
                let code_suggestions = suggestions_to_code_suggestions(&high_scoring);
                if !code_suggestions.is_empty() {
                    match self
                        .provider
                        .publish_code_suggestions(&code_suggestions)
                        .await
                    {
                        Ok(_) => {
                            tracing::info!(
                                count = code_suggestions.len(),
                                threshold = threshold_u32,
                                "published inline suggestions (dual mode)"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to publish inline suggestions in dual mode");
                        }
                    }
                }
            }

            // Always publish the full table as well
            self.publish_table(suggestions, reflect_failed).await?;
        } else if settings.pr_code_suggestions.commitable_code_suggestions {
            // Inline-only mode
            let code_suggestions = suggestions_to_code_suggestions(suggestions);
            if code_suggestions.is_empty() {
                tracing::warn!(
                    total = suggestions.len(),
                    "all suggestions filtered out (missing line numbers), falling back to table mode"
                );
                self.publish_table(suggestions, reflect_failed).await?;
            } else {
                match self
                    .provider
                    .publish_code_suggestions(&code_suggestions)
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to publish inline suggestions, falling back to table mode");
                        self.publish_table(suggestions, reflect_failed).await?;
                    }
                }
            }
        } else {
            // Table-only mode
            self.publish_table(suggestions, reflect_failed).await?;
        }

        Ok(())
    }

    /// Publish suggestions as a formatted table (persistent or regular comment).
    async fn publish_table(
        &self,
        suggestions: &[ParsedSuggestion],
        reflect_failed: bool,
    ) -> Result<(), PrAgentError> {
        let settings = get_settings();
        let mut table = format_suggestions_table(
            suggestions,
            settings.pr_code_suggestions.new_score_mechanism_th_high,
            settings.pr_code_suggestions.new_score_mechanism_th_medium,
        );

        if reflect_failed {
            table.push_str("\n> **Note:** Suggestion scoring may be less accurate (self-review pass was unavailable).\n");
        }

        if settings
            .pr_code_suggestions
            .demand_code_suggestions_self_review
        {
            append_self_review_checkbox(
                &mut table,
                &settings
                    .pr_code_suggestions
                    .code_suggestions_self_review_text,
                settings.pr_code_suggestions.approve_pr_on_self_review,
                settings.pr_code_suggestions.fold_suggestions_on_self_review,
            );
        }

        publish_as_comment(
            self.provider.as_ref(),
            &table,
            "improve",
            settings.pr_code_suggestions.persistent_comment,
            false,
        )
        .await
    }

    /// Print suggestions to stdout (CLI mode).
    fn print_suggestions(&self, suggestions: &[ParsedSuggestion]) {
        if suggestions.is_empty() {
            println!("No code suggestions found.");
        } else {
            let settings = get_settings();
            let table = format_suggestions_table(
                suggestions,
                settings.pr_code_suggestions.new_score_mechanism_th_high,
                settings.pr_code_suggestions.new_score_mechanism_th_medium,
            );
            println!("{table}");
        }
    }
}

/// Parsed feedback from the reflect/self-review AI call.
#[derive(Debug)]
struct ReflectFeedback {
    relevant_lines_start: i32,
    relevant_lines_end: i32,
    suggestion_score: u32,
}

/// Parse the reflect response YAML into feedback items.
fn parse_reflect_response(data: &serde_yaml_ng::Value) -> Vec<ReflectFeedback> {
    let suggestions_val = data.get("code_suggestions").unwrap_or(data);

    let Some(seq) = suggestions_val.as_sequence() else {
        return Vec::new();
    };

    seq.iter()
        .map(|item| {
            let lines_start = item
                .get("relevant_lines_start")
                .and_then(yaml_value_as_i64)
                .unwrap_or(-1) as i32;
            let lines_end = item
                .get("relevant_lines_end")
                .and_then(yaml_value_as_i64)
                .unwrap_or(-1) as i32;
            let score = item
                .get("suggestion_score")
                .or_else(|| item.get("score"))
                .and_then(yaml_value_as_u64)
                .unwrap_or(7) as u32;

            ReflectFeedback {
                relevant_lines_start: lines_start,
                relevant_lines_end: lines_end,
                suggestion_score: score,
            }
        })
        .collect()
}

/// Merge reflect feedback (scores + line numbers) into parsed suggestions.
///
/// Merges reflect feedback (scores + corrected line numbers) into parsed suggestions.
fn apply_reflect_feedback(suggestions: &mut [ParsedSuggestion], feedback: &[ReflectFeedback]) {
    if feedback.len() != suggestions.len() {
        tracing::warn!(
            suggestions = suggestions.len(),
            feedback = feedback.len(),
            "reflect feedback count mismatch, applying partial"
        );
    }

    for (i, suggestion) in suggestions.iter_mut().enumerate() {
        if let Some(fb) = feedback.get(i) {
            suggestion.score = fb.suggestion_score;

            // Only update line numbers if the suggestion doesn't already have valid ones
            if suggestion.relevant_lines_start <= 0 || suggestion.relevant_lines_end <= 0 {
                suggestion.relevant_lines_start = fb.relevant_lines_start;
                suggestion.relevant_lines_end = fb.relevant_lines_end;
            }

            // If line numbers are still invalid, zero out the score
            if suggestion.relevant_lines_start < 0 || suggestion.relevant_lines_end < 0 {
                suggestion.score = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_reflect_response() {
        let yaml_str = r#"
code_suggestions:
  - suggestion_summary: "Use descriptive name"
    relevant_file: "src/main.rs"
    relevant_lines_start: 10
    relevant_lines_end: 12
    suggestion_score: 8
    why: "Important for readability"
  - suggestion_summary: "Add error handling"
    relevant_file: "src/lib.rs"
    relevant_lines_start: 5
    relevant_lines_end: 5
    suggestion_score: 3
    why: "Minor improvement"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let feedback = parse_reflect_response(&data);

        assert_eq!(feedback.len(), 2);
        assert_eq!(feedback[0].relevant_lines_start, 10);
        assert_eq!(feedback[0].relevant_lines_end, 12);
        assert_eq!(feedback[0].suggestion_score, 8);
        assert_eq!(feedback[1].relevant_lines_start, 5);
        assert_eq!(feedback[1].suggestion_score, 3);
    }

    #[test]
    fn test_apply_reflect_feedback() {
        let mut suggestions = vec![
            ParsedSuggestion {
                label: "bug".into(),
                relevant_file: "src/main.rs".into(),
                relevant_lines_start: 0,
                relevant_lines_end: 0,
                existing_code: "old".into(),
                improved_code: "new".into(),
                one_sentence_summary: "Fix bug".into(),
                suggestion_content: "Fix the bug".into(),
                score: 5,
            },
            ParsedSuggestion {
                label: "enhancement".into(),
                relevant_file: "src/lib.rs".into(),
                relevant_lines_start: 0,
                relevant_lines_end: 0,
                existing_code: "old".into(),
                improved_code: "new".into(),
                one_sentence_summary: "Improve".into(),
                suggestion_content: "Improve this".into(),
                score: 5,
            },
        ];

        let feedback = vec![
            ReflectFeedback {
                relevant_lines_start: 10,
                relevant_lines_end: 12,
                suggestion_score: 9,
            },
            ReflectFeedback {
                relevant_lines_start: 5,
                relevant_lines_end: 5,
                suggestion_score: 3,
            },
        ];

        apply_reflect_feedback(&mut suggestions, &feedback);

        assert_eq!(suggestions[0].score, 9);
        assert_eq!(suggestions[0].relevant_lines_start, 10);
        assert_eq!(suggestions[0].relevant_lines_end, 12);
        assert_eq!(suggestions[1].score, 3);
        assert_eq!(suggestions[1].relevant_lines_start, 5);
    }

    #[test]
    fn test_apply_reflect_feedback_negative_lines_zeroes_score() {
        let mut suggestions = vec![ParsedSuggestion {
            label: "bug".into(),
            relevant_file: "src/main.rs".into(),
            relevant_lines_start: 0,
            relevant_lines_end: 0,
            existing_code: "old".into(),
            improved_code: "new".into(),
            one_sentence_summary: "Fix".into(),
            suggestion_content: "Fix".into(),
            score: 5,
        }];

        let feedback = vec![ReflectFeedback {
            relevant_lines_start: -1,
            relevant_lines_end: -1,
            suggestion_score: 8,
        }];

        apply_reflect_feedback(&mut suggestions, &feedback);

        // Score should be zeroed because lines are invalid
        assert_eq!(suggestions[0].score, 0);
    }

    #[test]
    fn test_apply_reflect_feedback_mismatch_count() {
        let mut suggestions = vec![
            ParsedSuggestion {
                label: "bug".into(),
                relevant_file: "src/main.rs".into(),
                relevant_lines_start: 0,
                relevant_lines_end: 0,
                existing_code: "old".into(),
                improved_code: "new".into(),
                one_sentence_summary: "Fix".into(),
                suggestion_content: "Fix".into(),
                score: 5,
            },
            ParsedSuggestion {
                label: "enhancement".into(),
                relevant_file: "src/lib.rs".into(),
                relevant_lines_start: 0,
                relevant_lines_end: 0,
                existing_code: "old".into(),
                improved_code: "new".into(),
                one_sentence_summary: "Improve".into(),
                suggestion_content: "Improve".into(),
                score: 7,
            },
        ];

        // Only one feedback item
        let feedback = vec![ReflectFeedback {
            relevant_lines_start: 10,
            relevant_lines_end: 12,
            suggestion_score: 9,
        }];

        apply_reflect_feedback(&mut suggestions, &feedback);

        // First suggestion updated
        assert_eq!(suggestions[0].score, 9);
        assert_eq!(suggestions[0].relevant_lines_start, 10);
        // Second suggestion unchanged
        assert_eq!(suggestions[1].score, 7);
        assert_eq!(suggestions[1].relevant_lines_start, 0);
    }

    // ── Integration tests ────────────────────────────────────────────

    use crate::config::loader::with_settings;
    use crate::config::types::Settings;
    use crate::testing::fixtures::{
        IMPROVE_YAML_PASS1, IMPROVE_YAML_PASS2_REFLECT, SAMPLE_PATCH, sample_diff_file,
    };
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
    async fn test_improve_pipeline_end_to_end() {
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        // Two responses: first for suggestions, second for reflect
        let ai = Arc::new(MockAiHandler::with_responses(vec![
            IMPROVE_YAML_PASS1.into(),
            IMPROVE_YAML_PASS2_REFLECT.into(),
        ]));
        let improver = PRCodeSuggestions::new_with_ai(provider.clone(), ai.clone());

        let settings = test_settings();
        with_settings(settings, improver.run()).await.unwrap();

        let calls = provider.get_calls();
        // Should publish a comment (table mode by default)
        assert!(
            !calls.comments.is_empty(),
            "should publish suggestions comment"
        );
        let comment = &calls.comments[0].0;
        assert!(
            comment.contains("<!-- pr-agent:improve -->"),
            "comment should contain improve marker"
        );
        assert!(
            comment.contains("magic number") || comment.contains("named constant"),
            "comment should contain suggestion content"
        );
        // Two AI calls: one for suggestions, one for reflect
        assert_eq!(
            ai.get_call_count(),
            2,
            "should call AI twice (suggest + reflect)"
        );
    }

    #[tokio::test]
    async fn test_improve_reflect_failure_uses_default_scores() {
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        // First call returns valid suggestions, second returns garbage (reflect fails)
        let ai = Arc::new(MockAiHandler::with_responses(vec![
            IMPROVE_YAML_PASS1.into(),
            "not valid yaml at all".into(),
        ]));
        let improver = PRCodeSuggestions::new_with_ai(provider.clone(), ai.clone());

        let settings = test_settings();
        with_settings(settings, improver.run()).await.unwrap();

        let calls = provider.get_calls();
        // Should still publish suggestions even though reflect failed
        assert!(
            !calls.comments.is_empty(),
            "should publish suggestions even when reflect fails"
        );
        // AI should have been called twice (suggestion + failed reflect)
        assert_eq!(ai.get_call_count(), 2);
    }

    #[tokio::test]
    async fn test_improve_empty_diff() {
        let provider = Arc::new(MockGitProvider::new()); // no diff files
        let ai = Arc::new(MockAiHandler::new(IMPROVE_YAML_PASS1));
        let improver = PRCodeSuggestions::new_with_ai(provider.clone(), ai.clone());

        let settings = test_settings();
        with_settings(settings, improver.run()).await.unwrap();

        // With no diff, AI should NOT be called
        assert_eq!(ai.get_call_count(), 0, "should not call AI with empty diff");
        let calls = provider.get_calls();
        assert!(calls.comments.is_empty(), "should not publish when no diff");
    }

    #[tokio::test]
    async fn test_improve_high_level_suggestions() {
        // Suggestions with lines 0-0 should appear as "Architecture & Design" bullet list
        let high_level_yaml = r#"```yaml
code_suggestions:
  - relevant_file: |
      src/main.rs
    language: |
      Rust
    suggestion_content: |
      Consider splitting this module into separate files
    existing_code: |
      // entire module
    improved_code: |
      // split into lib.rs and main.rs
    one_sentence_summary: |
      Split module for better organization
    relevant_lines_start: 0
    relevant_lines_end: 0
    label: |
      best practice
```"#;
        let reflect_yaml = r#"```yaml
code_suggestions:
  - relevant_file: |
      src/main.rs
    suggestion_content: |
      Consider splitting this module into separate files
    existing_code: |
      // entire module
    improved_code: |
      // split into lib.rs and main.rs
    one_sentence_summary: |
      Split module for better organization
    relevant_lines_start: 0
    relevant_lines_end: 0
    label: |
      best practice
    score: 8
```"#;
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::with_responses(vec![
            high_level_yaml.into(),
            reflect_yaml.into(),
        ]));
        let improver = PRCodeSuggestions::new_with_ai(provider.clone(), ai);

        let settings = test_settings();
        with_settings(settings, improver.run()).await.unwrap();

        let calls = provider.get_calls();
        assert!(
            !calls.comments.is_empty(),
            "should publish high-level suggestions"
        );
        let comment = &calls.comments[0].0;
        assert!(
            comment.contains("Architecture") || comment.contains("General"),
            "high-level suggestions (lines 0-0) should appear in Architecture/General section: got first 500 chars: {}",
            &comment[..comment.len().min(500)]
        );
    }
}
