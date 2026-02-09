use std::collections::HashMap;
use std::sync::Arc;

use minijinja::Value;

use crate::ai::AiHandler;
use crate::config::loader::get_settings;
use crate::error::PrAgentError;
use crate::git::GitProvider;
use crate::output::describe_formatter::{FileStats, format_describe_output};
use crate::output::yaml_parser::load_yaml;
use crate::processing::compression::get_pr_diff;
use crate::template::render::render_prompt;
use crate::tools::{
    PrMetadata, build_common_vars, insert_custom_labels_vars, with_progress_comment,
};

/// PR Description tool.
///
/// Fetches diff, calls AI, formats the response as PR title + body,
/// and publishes via publish_description().
pub struct PRDescription {
    provider: Arc<dyn GitProvider>,
    ai: Option<Arc<dyn AiHandler>>,
}

impl PRDescription {
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

    /// Run the full describe pipeline.
    pub async fn run(&self) -> Result<(), PrAgentError> {
        let provider = &self.provider;
        with_progress_comment(provider.as_ref(), "Preparing PR description...", || {
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
        tracing::info!(num_files, "processing changed files for describe");

        let diff_result = get_pr_diff(&mut files, model, true);

        // Build per-file stats for the file walkthrough links (only uses metadata fields).
        // base_file/head_file already released by get_pr_diff internally.
        let file_stats: HashMap<String, FileStats> = files
            .iter()
            .map(|f| {
                let link = self.provider.get_line_link(&f.filename, -1, None);
                let key = f.filename.trim_start_matches('/').to_lowercase();
                (
                    key,
                    FileStats {
                        num_plus_lines: f.num_plus_lines,
                        num_minus_lines: f.num_minus_lines,
                        link,
                    },
                )
            })
            .collect();

        // 3. Build template variables
        let vars = self.build_vars(&meta, &diff_result.diff, num_files);

        // 4. Render prompt
        let rendered = render_prompt(&settings.pr_description_prompt, vars)?;

        // 5. Call AI (with fallback models)
        tracing::info!(model, "calling AI model for describe");
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
            "AI response received"
        );

        // 6. Parse YAML from response
        let yaml_data = load_yaml(&response.content, &[], "type", "pr_files");

        // 7. Format and publish
        // Strip any previous pr-agent:describe content from original body
        // (extract original user-written description)
        let user_description = strip_pr_agent_content(&meta.description);

        if settings.config.publish_output {
            self.publish_description(
                yaml_data.as_ref(),
                &meta.title,
                &user_description,
                &file_stats,
            )
            .await?;
        } else {
            self.print_description(yaml_data.as_ref(), &response.content);
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

        // Describe-specific variables
        vars.insert(
            "extra_instructions".into(),
            Value::from(settings.pr_description.extra_instructions.as_str()),
        );
        insert_custom_labels_vars(&mut vars, &settings);
        vars.insert(
            "enable_semantic_files_types".into(),
            Value::from(settings.pr_description.enable_semantic_files_types),
        );
        vars.insert("related_tickets".into(), Value::from(Vec::<String>::new()));
        vars.insert(
            "include_file_summary_changes".into(),
            Value::from(num_files <= 20),
        );
        vars.insert("duplicate_prompt_examples".into(), Value::from(false));
        vars.insert(
            "enable_pr_diagram".into(),
            Value::from(settings.pr_description.enable_pr_diagram),
        );

        vars
    }

    /// Publish the formatted description to the PR.
    async fn publish_description(
        &self,
        yaml_data: Option<&serde_yaml_ng::Value>,
        original_title: &str,
        original_body: &str,
        file_stats: &HashMap<String, FileStats>,
    ) -> Result<(), PrAgentError> {
        let settings = get_settings();

        let Some(data) = yaml_data else {
            tracing::warn!("could not parse YAML from AI response, skipping publish");
            return Ok(());
        };

        let output = format_describe_output(
            data,
            original_title,
            original_body,
            &settings.pr_description,
            file_stats,
        );

        if settings.pr_description.publish_description_as_comment {
            // Publish as comment instead of editing PR body
            if settings
                .pr_description
                .publish_description_as_comment_persistent
            {
                self.provider
                    .publish_persistent_comment(
                        &output.body,
                        "<!-- pr-agent:describe -->",
                        "",
                        "describe",
                        settings.pr_description.final_update_message,
                    )
                    .await?;
            } else {
                self.provider.publish_comment(&output.body, false).await?;
            }
        } else {
            // Edit PR title and body directly
            self.provider
                .publish_description(&output.title, &output.body)
                .await?;
        }

        // Publish labels if enabled
        if settings.pr_description.publish_labels && !output.labels.is_empty() {
            self.provider.publish_labels(&output.labels).await?;
        }

        Ok(())
    }

    /// Print description to stdout (CLI mode, uses raw body).
    fn print_description(&self, yaml_data: Option<&serde_yaml_ng::Value>, raw_response: &str) {
        match yaml_data {
            Some(data) => {
                println!(
                    "{}",
                    serde_yaml_ng::to_string(data).unwrap_or_else(|_| raw_response.to_string())
                );
            }
            None => {
                eprintln!("Warning: could not parse YAML from AI response, printing raw:");
                println!("{raw_response}");
            }
        }
    }
}

/// Headers that indicate the body was generated by pr-agent.
///
/// Known section headers emitted by pr-agent tools.
const PR_AGENT_HEADERS: &[&str] = &[
    "### **user description**",
    "### **pr type**",
    "### **pr description**",
    "### **pr labels**",
    "### **type**",
    "### **description**",
    "### **labels**",
];

/// Check if a body was generated by pr-agent.
fn is_generated_by_pr_agent(body: &str) -> bool {
    let lower = body.trim_start().to_lowercase();
    // Check for HTML comment marker (Rust pr-agent style)
    if lower.starts_with("<!-- pr-agent:") {
        return true;
    }
    // Check for known section headers (legacy pr-agent style)
    PR_AGENT_HEADERS
        .iter()
        .any(|header| lower.starts_with(header))
}

/// Strip any previous pr-agent generated content from the PR body,
/// returning only the original user-written description.
///
/// Algorithm:
/// 1. If body has `<!-- pr-agent:` marker, return content before it
/// 2. If body starts with a pr-agent header → extract the "User description" section
/// 3. Otherwise → return body as-is
fn strip_pr_agent_content(body: &str) -> String {
    // 1. HTML comment marker (Rust pr-agent style)
    if let Some(pos) = body.find("<!-- pr-agent:") {
        let before = body[..pos].trim();
        // Strip the "---" separator that format_describe_output adds between
        // the user description and the marker.
        let before = before.strip_suffix("---").unwrap_or(before).trim();
        return before.to_string();
    }

    // 2. Check for pr-agent generated headers
    if !is_generated_by_pr_agent(body) {
        return body.to_string();
    }

    let lower = body.to_lowercase();

    // Find the "user description" section
    let user_desc_header = "### **user description**";
    if let Some(start) = lower.find(user_desc_header) {
        let content_start = start + user_desc_header.len();

        // Find where the user description ends (next pr-agent header)
        let mut end_pos = body.len();
        for header in PR_AGENT_HEADERS {
            if *header == user_desc_header {
                continue;
            }
            if let Some(pos) = lower[content_start..].find(header) {
                end_pos = end_pos.min(content_start + pos);
            }
        }

        let user_content = body[content_start..end_pos].trim();
        // Strip trailing separator (___) that pr-agent adds
        let user_content = user_content.strip_suffix("___").unwrap_or(user_content);
        user_content.trim().to_string()
    } else {
        // Body is generated but has no user description section → return empty
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_pr_agent_content_with_marker() {
        let body =
            "User wrote this.\n\n---\n\n<!-- pr-agent:describe -->\n### PR Type\nGenerated stuff";
        // The "---" separator is stripped — only the user's content is recovered
        assert_eq!(strip_pr_agent_content(body), "User wrote this.");
    }

    #[test]
    fn test_strip_pr_agent_content_without_marker() {
        let body = "Just a normal body with no markers.";
        assert_eq!(strip_pr_agent_content(body), body);
    }

    #[test]
    fn test_strip_pr_agent_content_empty() {
        assert_eq!(strip_pr_agent_content(""), "");
    }

    #[test]
    fn test_strip_pr_agent_content_marker_at_start() {
        let body = "<!-- pr-agent:describe -->\nAll generated";
        assert_eq!(strip_pr_agent_content(body), "");
    }

    #[test]
    fn test_strip_pr_agent_content_legacy_format() {
        // Body generated by pr-agent with User description section
        let body = "### **User description**\nUser wrote this.\n\n___\n\n### **PR Type**\nEnhancement\n\n___\n\n### **Description**\n- Generated bullet";
        assert_eq!(strip_pr_agent_content(body), "User wrote this.");
    }

    #[test]
    fn test_strip_pr_agent_content_legacy_format_no_user_desc() {
        // Body generated by pr-agent but without User description section
        let body = "### **PR Type**\nEnhancement\n\n### **Description**\n- Generated";
        assert_eq!(strip_pr_agent_content(body), "");
    }

    #[test]
    fn test_is_generated_by_pr_agent_html_marker() {
        assert!(is_generated_by_pr_agent(
            "<!-- pr-agent:describe -->\nContent"
        ));
    }

    #[test]
    fn test_is_generated_by_pr_agent_legacy_header() {
        assert!(is_generated_by_pr_agent(
            "### **User description**\nContent"
        ));
        assert!(is_generated_by_pr_agent("### **PR Type**\nContent"));
    }

    #[test]
    fn test_is_generated_by_pr_agent_normal_body() {
        assert!(!is_generated_by_pr_agent("Just a normal PR body."));
    }

    /// Integration test: simulates running describe twice on the same PR.
    ///
    /// First run: user has an original PR body, describe formats it with AI content.
    /// Second run: the PR body is now the describe output, describe must recover
    /// the original user description and preserve it in the new output.
    #[test]
    fn test_describe_round_trip_preserves_user_description() {
        use crate::config::types::PrDescriptionConfig;
        use crate::output::describe_formatter::format_describe_output;

        let ai_yaml = r#"
title: "AI generated title"
type: "Enhancement"
description: "AI generated description of changes"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(ai_yaml).unwrap();
        let config = PrDescriptionConfig {
            generate_ai_title: true,
            add_original_user_description: true,
            ..PrDescriptionConfig::default()
        };
        let empty_stats = HashMap::new();

        let user_original_body = "This PR implements feature X.\n\nPlease review carefully.";

        // --- First run: user has their original body ---
        let first_output = format_describe_output(
            &data,
            "Original title",
            user_original_body,
            &config,
            &empty_stats,
        );

        // User body must appear in the output
        assert!(
            first_output.body.contains(user_original_body),
            "first run must include the user's original body"
        );
        assert!(
            first_output.body.contains("<!-- pr-agent:describe -->"),
            "first run must include the marker"
        );

        // User body must be BEFORE the marker so strip_pr_agent_content can find it
        let marker_pos = first_output
            .body
            .find("<!-- pr-agent:describe -->")
            .unwrap();
        let user_pos = first_output.body.find(user_original_body).unwrap();
        assert!(
            user_pos < marker_pos,
            "user description must appear before the pr-agent marker"
        );

        // --- Second run: PR body is now the first_output.body ---
        // strip_pr_agent_content must recover the original user body
        let recovered = strip_pr_agent_content(&first_output.body);
        assert_eq!(
            recovered.trim(),
            user_original_body.trim(),
            "strip_pr_agent_content must recover the original user description after first run"
        );

        // Format again with the recovered user description
        let second_output =
            format_describe_output(&data, "Original title", &recovered, &config, &empty_stats);

        // The user body must still be present in the second output
        assert!(
            second_output.body.contains(user_original_body),
            "second run must still include the user's original body"
        );

        // And recoverable again (third run would also work)
        let recovered_again = strip_pr_agent_content(&second_output.body);
        assert_eq!(
            recovered_again.trim(),
            user_original_body.trim(),
            "strip_pr_agent_content must recover the user description after any number of runs"
        );
    }

    /// Integration test: when add_original_user_description is false,
    /// the marker should be at the start and no user body is embedded.
    #[test]
    fn test_describe_round_trip_no_original_description() {
        use crate::config::types::PrDescriptionConfig;
        use crate::output::describe_formatter::format_describe_output;

        let ai_yaml = r#"
title: "AI title"
type: "Bug fix"
description: "Fixed the bug"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(ai_yaml).unwrap();
        let config = PrDescriptionConfig {
            add_original_user_description: false,
            ..PrDescriptionConfig::default()
        };
        let empty_stats = HashMap::new();

        let output =
            format_describe_output(&data, "Title", "User body here", &config, &empty_stats);

        // User body must NOT be in the output when flag is false
        assert!(
            !output.body.contains("User body here"),
            "user body must not appear when add_original_user_description is false"
        );

        // Marker should be at the start
        assert!(
            output
                .body
                .trim_start()
                .starts_with("<!-- pr-agent:describe -->"),
            "marker should be at the start when no user description"
        );
    }

    /// Integration test: round-trip with the "---" separator.
    /// Ensures the separator between user body and AI content doesn't leak
    /// into the recovered user description.
    #[test]
    fn test_strip_pr_agent_content_does_not_include_separator() {
        use crate::config::types::PrDescriptionConfig;
        use crate::output::describe_formatter::format_describe_output;

        let ai_yaml = r#"
title: "Title"
type: "Enhancement"
description: "Changes"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(ai_yaml).unwrap();
        let config = PrDescriptionConfig {
            add_original_user_description: true,
            ..PrDescriptionConfig::default()
        };
        let empty_stats = HashMap::new();

        let user_body = "Simple description";
        let output = format_describe_output(&data, "Title", user_body, &config, &empty_stats);
        let recovered = strip_pr_agent_content(&output.body);

        // Must not include the "---" separator
        assert!(
            !recovered.ends_with("---"),
            "recovered description must not include the separator: got {:?}",
            recovered
        );
        assert_eq!(recovered.trim(), user_body);
    }

    // ── Integration tests ────────────────────────────────────────────

    use crate::config::loader::with_settings;
    use crate::testing::fixtures::{DESCRIBE_YAML, SAMPLE_PATCH, sample_diff_file};
    use crate::testing::mock_ai::MockAiHandler;
    use crate::testing::mock_git::MockGitProvider;

    #[tokio::test]
    async fn test_describe_pipeline_end_to_end() {
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::new(DESCRIBE_YAML));
        let describer = PRDescription::new_with_ai(provider.clone(), ai.clone());

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("config.publish_output".into(), "true".into());
        overrides.insert("config.publish_output_progress".into(), "false".into());
        overrides.insert("pr_description.generate_ai_title".into(), "true".into());
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());
        with_settings(settings, describer.run()).await.unwrap();

        let calls = provider.get_calls();
        // Default mode publishes via publish_description (title + body)
        assert!(
            !calls.descriptions.is_empty(),
            "should call publish_description"
        );
        let (title, body) = &calls.descriptions[0];
        assert!(
            title.contains("Add debug output"),
            "should use AI-generated title: got {title}"
        );
        assert!(
            body.contains("<!-- pr-agent:describe -->"),
            "body should contain describe marker"
        );
        assert_eq!(ai.get_call_count(), 1, "should call AI exactly once");
    }

    #[tokio::test]
    async fn test_describe_preserves_user_description() {
        let user_body = "My original PR description that should be preserved.";
        let provider = Arc::new(
            MockGitProvider::new()
                .with_pr_description("Original Title", user_body)
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::new(DESCRIBE_YAML));
        let describer = PRDescription::new_with_ai(provider.clone(), ai);

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("config.publish_output".into(), "true".into());
        overrides.insert("config.publish_output_progress".into(), "false".into());
        overrides.insert(
            "pr_description.add_original_user_description".into(),
            "true".into(),
        );
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());
        with_settings(settings, describer.run()).await.unwrap();

        let calls = provider.get_calls();
        let (_, body) = &calls.descriptions[0];
        assert!(
            body.contains(user_body),
            "body should contain original user description"
        );
        // User description should appear before the marker
        let marker_pos = body.find("<!-- pr-agent:describe -->").unwrap();
        let user_pos = body.find(user_body).unwrap();
        assert!(
            user_pos < marker_pos,
            "user description must appear before the pr-agent marker"
        );
    }

    #[tokio::test]
    async fn test_describe_strips_previous_agent_content() {
        // Simulate a PR body that already has pr-agent content from a previous run
        let prev_body = "User wrote this.\n\n---\n\n<!-- pr-agent:describe -->\n### **PR Type**\nOld generated content";
        let provider = Arc::new(
            MockGitProvider::new()
                .with_pr_description("Old Title", prev_body)
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::new(DESCRIBE_YAML));
        let describer = PRDescription::new_with_ai(provider.clone(), ai);

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("config.publish_output".into(), "true".into());
        overrides.insert("config.publish_output_progress".into(), "false".into());
        overrides.insert(
            "pr_description.add_original_user_description".into(),
            "true".into(),
        );
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());
        with_settings(settings, describer.run()).await.unwrap();

        let calls = provider.get_calls();
        let (_, body) = &calls.descriptions[0];
        assert!(
            body.contains("User wrote this."),
            "should preserve original user text"
        );
        assert!(
            !body.contains("Old generated content"),
            "should strip previous agent content"
        );
    }

    #[tokio::test]
    async fn test_describe_as_comment_mode() {
        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::new(DESCRIBE_YAML));
        let describer = PRDescription::new_with_ai(provider.clone(), ai);

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("config.publish_output".into(), "true".into());
        overrides.insert("config.publish_output_progress".into(), "false".into());
        overrides.insert(
            "pr_description.publish_description_as_comment".into(),
            "true".into(),
        );
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());
        with_settings(settings, describer.run()).await.unwrap();

        let calls = provider.get_calls();
        // Should publish as comment, not as description
        assert!(
            calls.descriptions.is_empty(),
            "should NOT call publish_description in comment mode"
        );
        assert!(
            !calls.comments.is_empty(),
            "should publish as comment instead"
        );
    }
}
