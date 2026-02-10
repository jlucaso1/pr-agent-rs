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

        // 3. Detect images in the question
        let image_url = extract_image_url(question);

        // 4. Build template variables
        let mut vars = build_common_vars(&meta, &diff);
        vars.insert("questions".to_string(), Value::from(question.trim()));

        // 5. Render prompts
        let rendered = render_prompt(&settings.pr_questions_prompt, vars)?;

        // 6. Call AI
        let ai = resolve_ai_handler(&self.ai)?;
        let image_urls: Vec<String> = image_url.into_iter().collect();
        let image_ref = if image_urls.is_empty() {
            None
        } else {
            Some(image_urls.as_slice())
        };

        let response = ai
            .chat_completion(
                model,
                &rendered.system,
                &rendered.user,
                Some(settings.config.temperature),
                image_ref,
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

/// Extract image URL from question text.
///
/// Handles two patterns (matching Python):
/// - `![image](url)` — markdown image syntax
/// - Direct `https://...png` or `https://...jpg` URLs
fn extract_image_url(question: &str) -> Option<String> {
    if let Some(marker_pos) = question.find("![image]") {
        // Pattern: "question text ![image](url)"
        // Find the '(' after "![image]" and scan for the matching ')'
        let after_marker = &question[marker_pos + "![image]".len()..];
        let after = after_marker.trim();
        if after.starts_with('(') {
            let inner = after.strip_prefix('(').unwrap();
            // Find the matching closing ')' (handle balanced parens)
            let mut depth = 1u32;
            let mut end = inner.len();
            for (i, ch) in inner.char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let url = inner[..end].trim();
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    } else if question.contains("https://") {
        // Direct image link — extract the URL token up to whitespace
        let after = question.split("https://").nth(1)?;
        let token = format!(
            "https://{}",
            after.split_whitespace().next().unwrap_or(after)
        );
        // Strip common trailing punctuation that isn't part of the URL
        let token = token.trim_end_matches(['.', ',', ';']);
        // Validate extension: extract path before any query/fragment and check suffix
        let path_part = token.split(['?', '#']).next().unwrap_or(token);
        let lower = path_part.to_lowercase();
        if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
            return Some(token.to_string());
        }
    }
    None
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
    fn test_extract_image_url_markdown() {
        let q = "What is this? ![image](https://example.com/img.png)";
        assert_eq!(
            extract_image_url(q),
            Some("https://example.com/img.png".to_string())
        );
    }

    #[test]
    fn test_extract_image_url_direct() {
        let q = "Explain this https://example.com/screenshot.png please";
        assert_eq!(
            extract_image_url(q),
            Some("https://example.com/screenshot.png".to_string())
        );
    }

    #[test]
    fn test_extract_image_url_none() {
        assert_eq!(extract_image_url("What does this PR do?"), None);
    }

    #[test]
    fn test_extract_image_url_non_image_https() {
        assert_eq!(extract_image_url("See https://example.com/docs"), None);
    }

    #[test]
    fn test_extract_image_url_parens_in_url() {
        // URL with balanced parentheses (e.g. Wikipedia)
        let q = "![image](https://example.com/File_(edit).png)";
        assert_eq!(
            extract_image_url(q),
            Some("https://example.com/File_(edit).png".to_string())
        );
    }

    #[test]
    fn test_extract_image_url_query_string() {
        // Direct URL with query params — extension is before '?'
        let q = "See https://example.com/img.png?token=abc123";
        assert_eq!(
            extract_image_url(q),
            Some("https://example.com/img.png?token=abc123".to_string())
        );
    }

    #[test]
    fn test_extract_image_url_trailing_punctuation() {
        // Trailing period should be stripped
        let q = "Look at https://example.com/shot.jpg.";
        assert_eq!(
            extract_image_url(q),
            Some("https://example.com/shot.jpg".to_string())
        );
    }

    #[test]
    fn test_extract_image_url_no_false_positive_contains() {
        // ".png" in a non-extension position should not match
        assert_eq!(extract_image_url("See https://example.com/png-docs"), None);
    }

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
}
