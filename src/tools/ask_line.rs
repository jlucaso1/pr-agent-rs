use std::collections::HashMap;
use std::sync::Arc;

use minijinja::Value;

use crate::ai::AiHandler;
use crate::config::loader::get_settings;
use crate::error::PrAgentError;
use crate::git::GitProvider;
use crate::processing::diff::extract_hunk_lines_from_patch;
use crate::template::render::render_prompt;
use crate::tools::resolve_ai_handler;

/// PR Ask Line tool — answer questions about specific code lines in a PR.
///
/// Uses the diff hunk context and optional conversation history to provide
/// targeted answers about selected lines of code.
pub struct PRAskLine {
    provider: Arc<dyn GitProvider>,
    ai: Option<Arc<dyn AiHandler>>,
}

impl PRAskLine {
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

    /// Run the ask_line pipeline with parsed arguments from the comment command.
    ///
    /// Expected args keys: `line_start`, `line_end`, `side`, `file_name`,
    /// `comment_id`, `_text` (the question).
    pub async fn run(&self, args: &HashMap<String, String>) -> Result<(), PrAgentError> {
        let question = args.get("_text").map(|s| s.as_str()).unwrap_or("");
        if question.trim().is_empty() {
            tracing::info!("empty question, skipping /ask_line");
            return Ok(());
        }

        let file_name = args.get("file_name").map(|s| s.as_str()).unwrap_or("");
        let line_start: usize = args
            .get("line_start")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let line_end: usize = args
            .get("line_end")
            .and_then(|s| s.parse().ok())
            .unwrap_or(line_start);
        let side = args.get("side").map(|s| s.as_str()).unwrap_or("RIGHT");
        let comment_id: u64 = args
            .get("comment_id")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let settings = get_settings();
        let model = &settings.config.model;

        // 1. Get the diff hunk — either from webhook-provided diff_hunk or by fetching files
        let diff_hunk = args.get("_diff_hunk").map(|s| s.as_str()).unwrap_or("");
        let (full_hunk, selected_lines) = if !diff_hunk.is_empty() {
            extract_hunk_lines_from_patch(diff_hunk, file_name, line_start, line_end, side)
        } else {
            // Fallback: fetch diff files and find the matching file
            let files = self.provider.get_diff_files().await?;
            let mut result = (String::new(), String::new());
            for file in &files {
                if file.filename == file_name {
                    result = extract_hunk_lines_from_patch(
                        &file.patch,
                        file_name,
                        line_start,
                        line_end,
                        side,
                    );
                    break;
                }
            }
            result
        };

        if full_hunk.is_empty() {
            tracing::warn!(
                file_name,
                line_start,
                line_end,
                "no hunk found for ask_line"
            );
            return Ok(());
        }

        // 2. Load conversation history if enabled
        let conversation_history =
            if settings.pr_questions.use_conversation_history && comment_id > 0 {
                self.load_conversation_history(comment_id).await
            } else {
                String::new()
            };

        // 3. Build template variables
        let title = self.provider.get_pr_description_full().await?.0;
        let branch = self.provider.get_pr_branch().await?;

        let mut vars: HashMap<String, Value> = HashMap::new();
        vars.insert("title".into(), Value::from(title));
        vars.insert("branch".into(), Value::from(branch));
        vars.insert("full_hunk".into(), Value::from(full_hunk));
        vars.insert("selected_lines".into(), Value::from(selected_lines));
        vars.insert("question".into(), Value::from(question.trim()));
        vars.insert(
            "conversation_history".into(),
            Value::from(conversation_history),
        );

        // 4. Render prompts
        let rendered = render_prompt(&settings.pr_line_questions_prompt, vars)?;

        // 5. Call AI
        let ai = resolve_ai_handler(&self.ai)?;
        let response = ai
            .chat_completion(
                model,
                &rendered.system,
                &rendered.user,
                Some(settings.config.temperature),
                None,
            )
            .await?;

        // 6. Sanitize answer
        let answer = crate::tools::ask::sanitize_answer(&response.content);

        // 7. Publish as reply to the code comment, or as a regular comment
        if comment_id > 0 {
            self.provider.reply_to_comment(comment_id, &answer).await?;
        } else if settings.config.publish_output {
            self.provider.publish_comment(&answer, false).await?;
        }

        Ok(())
    }

    /// Load conversation history from the review thread.
    ///
    /// Fetches all comments in the same review thread and formats them as a
    /// numbered list: "1. username: message"
    async fn load_conversation_history(&self, comment_id: u64) -> String {
        match self.provider.get_review_thread_comments(comment_id).await {
            Ok(comments) => {
                let filtered: Vec<_> = comments
                    .iter()
                    .filter(|c| c.id != comment_id && !c.body.trim().is_empty())
                    .collect();

                if filtered.is_empty() {
                    return String::new();
                }

                tracing::info!(
                    count = filtered.len(),
                    "loaded conversation history from review thread"
                );

                filtered
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let author = if c.user.is_empty() {
                            "Unknown"
                        } else {
                            &c.user
                        };
                        format!("{}. {}: {}", i + 1, author, c.body)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to load conversation history");
                String::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ask_line_args() {
        let mut args = HashMap::new();
        args.insert("line_start".to_string(), "10".to_string());
        args.insert("line_end".to_string(), "15".to_string());
        args.insert("side".to_string(), "RIGHT".to_string());
        args.insert("file_name".to_string(), "src/main.rs".to_string());
        args.insert("comment_id".to_string(), "12345".to_string());
        args.insert(
            "_text".to_string(),
            "What does this function do?".to_string(),
        );

        let line_start: usize = args.get("line_start").unwrap().parse().unwrap();
        let line_end: usize = args.get("line_end").unwrap().parse().unwrap();
        let comment_id: u64 = args.get("comment_id").unwrap().parse().unwrap();

        assert_eq!(line_start, 10);
        assert_eq!(line_end, 15);
        assert_eq!(comment_id, 12345);
        assert_eq!(args.get("file_name").unwrap(), "src/main.rs");
    }
}
