use std::sync::Arc;

use minijinja::Value;

use crate::ai::AiHandler;
use crate::config::loader::get_settings;
use crate::error::PrAgentError;
use crate::git::GitProvider;
use crate::processing::compression::get_pr_diff;
use crate::template::render::render_prompt;
use crate::tools::{PrMetadata, build_common_vars, resolve_ai_handler, with_progress_comment};

/// PR Ask tool — answer free-form questions about a PR's code changes.
///
/// Fetches the PR diff, renders the question prompt, calls AI,
/// and publishes the answer as a regular comment.
pub struct PRAsk {
    provider: Arc<dyn GitProvider>,
    ai: Option<Arc<dyn AiHandler>>,
}

impl PRAsk {
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

    /// Run the ask pipeline with the given question text.
    pub async fn run(&self, question: &str) -> Result<(), PrAgentError> {
        if question.trim().is_empty() {
            tracing::info!("empty question, skipping /ask");
            return Ok(());
        }

        let provider = &self.provider;
        let q = question.to_string();
        with_progress_comment(provider.as_ref(), "Preparing answer...", || {
            self.run_inner(&q)
        })
        .await
    }

    async fn run_inner(&self, question: &str) -> Result<(), PrAgentError> {
        let settings = get_settings();
        let model = &settings.config.model;

        // 1. Fetch PR metadata
        let meta = PrMetadata::fetch(self.provider.as_ref(), &settings).await?;

        // 2. Fetch and compress diff
        let mut files = self.provider.get_diff_files().await?;
        let diff_result = get_pr_diff(&mut files, model, true);
        drop(files);
        let diff = diff_result.diff;

        // 3. Detect and validate images in the question. Gated on enable_vision
        //    and routed through the shared extractor (markdown/HTML/bare URLs +
        //    HEAD validation), matching /ask_line instead of a weaker inline one.
        let image_urls = if settings.config.enable_vision {
            let urls = crate::tools::image::extract_and_validate_image_urls(question).await;
            if urls.is_empty() { None } else { Some(urls) }
        } else {
            None
        };

        // 4. Build template variables
        let mut vars = build_common_vars(&meta, &diff);
        vars.insert("questions".to_string(), Value::from(question.trim()));

        // 5. Render prompts
        let rendered = render_prompt(&settings.pr_questions_prompt, vars)?;

        // 6. Call AI
        let ai = resolve_ai_handler(&self.ai)?;
        let response = ai
            .chat_completion(
                model,
                &rendered.system,
                &rendered.user,
                Some(settings.config.temperature),
                image_urls.as_deref(),
            )
            .await?;

        // 7. Sanitize and format answer
        let answer = sanitize_answer(&response.content);
        let output = format_ask_output(question, &answer);

        // 8. Publish
        if settings.config.publish_output {
            self.provider.publish_comment(&output, false).await?;
        }

        Ok(())
    }
}

/// Sanitize AI answer to prevent accidental GitHub slash commands.
///
/// GitHub interprets lines starting with `/` as quick actions.
/// We replace `\n/` with `\n /` to prevent that.
pub fn sanitize_answer(answer: &str) -> String {
    let mut sanitized = answer.trim().replace("\n/", "\n /");
    if sanitized.starts_with('/') {
        sanitized.insert(0, ' ');
    }
    sanitized
}

/// Format the final ask output with question and answer headers.
fn format_ask_output(question: &str, answer: &str) -> String {
    // Strip image references from displayed question (clean up "> ![image]..." prefix)
    let display_question = question
        .lines()
        .filter(|line| !line.trim().starts_with("> ![image]"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    format!("### **Ask**\n{display_question}\n\n### **Answer:**\n{answer}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_answer_leading_slash() {
        assert_eq!(sanitize_answer("/approve"), " /approve");
    }

    #[test]
    fn test_sanitize_answer_newline_slash() {
        assert_eq!(sanitize_answer("line1\n/command"), "line1\n /command");
    }

    #[test]
    fn test_sanitize_answer_normal() {
        assert_eq!(sanitize_answer("  normal answer  "), "normal answer");
    }

    #[test]
    fn test_format_ask_output() {
        let output = format_ask_output("What does this do?", "It does X.");
        assert!(output.contains("### **Ask**"));
        assert!(output.contains("What does this do?"));
        assert!(output.contains("### **Answer:**"));
        assert!(output.contains("It does X."));
    }

    #[test]
    fn test_format_ask_output_strips_image_lines() {
        let question = "> ![image](https://img.com/a.png)\nWhat is this?";
        let output = format_ask_output(question, "Answer here.");
        assert!(!output.contains("![image]"));
        assert!(output.contains("What is this?"));
    }

    #[tokio::test]
    async fn test_ask_no_images_when_vision_disabled() {
        // C19: /ask must respect enable_vision. With vision off, an image in the
        // question must NOT be sent to the model (and the gate short-circuits
        // before any HEAD validation, so this is deterministic/offline).
        use crate::config::loader::with_settings;
        use crate::testing::fixtures::{SAMPLE_PATCH, sample_diff_file};
        use crate::testing::mock_ai::MockAiHandler;
        use crate::testing::mock_git::MockGitProvider;

        let provider = Arc::new(
            MockGitProvider::new()
                .with_diff_files(vec![sample_diff_file("src/main.rs", SAMPLE_PATCH)]),
        );
        let ai = Arc::new(MockAiHandler::new("An answer."));
        let ask = PRAsk::new_with_ai(provider.clone(), ai.clone());

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("config.publish_output".into(), "true".into());
        overrides.insert("config.publish_output_progress".into(), "false".into());
        overrides.insert("config.enable_vision".into(), "false".into());
        let settings =
            Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap());

        let question = "What is shown here? ![image](https://example.com/x.png)";
        with_settings(settings, ask.run(question)).await.unwrap();

        let calls = ai.get_recorded_calls();
        assert_eq!(calls.len(), 1, "should call AI once");
        assert!(
            calls[0].image_urls.is_none(),
            "vision disabled → no images passed to AI, got {:?}",
            calls[0].image_urls
        );
    }
}
