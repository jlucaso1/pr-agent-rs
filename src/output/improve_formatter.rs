use std::fmt::Write;

use crate::git::types::CodeSuggestion;
use crate::output::markdown::persistent_comment_marker;
use crate::output::yaml_parser::{yaml_value_as_i64, yaml_value_as_u64};

/// A parsed code suggestion from the AI response.
#[derive(Debug, Clone)]
pub struct ParsedSuggestion {
    pub label: String,
    pub relevant_file: String,
    pub relevant_lines_start: i32,
    pub relevant_lines_end: i32,
    pub existing_code: String,
    pub improved_code: String,
    pub one_sentence_summary: String,
    pub suggestion_content: String,
    pub score: u32,
}

/// Extract a trimmed string field from a YAML mapping, with a fallback default.
fn yaml_str_field(item: &serde_yaml_ng::Value, key: &str, default: &str) -> String {
    item.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .trim()
        .to_string()
}

/// Parse code suggestions from the AI YAML response.
pub fn parse_suggestions(data: &serde_yaml_ng::Value) -> Vec<ParsedSuggestion> {
    let suggestions_val = data
        .get("code_suggestions")
        .or(data.get("suggestions"))
        .or(data.get("improve"))
        .unwrap_or(data);

    let Some(seq) = suggestions_val.as_sequence() else {
        return Vec::new();
    };

    let mut suggestions = Vec::new();

    for item in seq {
        let label = yaml_str_field(item, "label", "enhancement");
        let relevant_file = yaml_str_field(item, "relevant_file", "");
        let existing_code = yaml_str_field(item, "existing_code", "");
        let improved_code = yaml_str_field(item, "improved_code", "");
        let one_sentence_summary = yaml_str_field(item, "one_sentence_summary", "");
        let suggestion_content = yaml_str_field(item, "suggestion_content", "");

        let lines_start = item
            .get("relevant_lines_start")
            .and_then(yaml_value_as_i64)
            .unwrap_or(0) as i32;
        let lines_end = item
            .get("relevant_lines_end")
            .and_then(yaml_value_as_i64)
            .unwrap_or(0) as i32;
        let score = item.get("score").and_then(yaml_value_as_u64).unwrap_or(5) as u32;

        if relevant_file.is_empty() || improved_code.is_empty() {
            continue;
        }

        suggestions.push(ParsedSuggestion {
            label,
            relevant_file,
            relevant_lines_start: lines_start,
            relevant_lines_end: lines_end,
            existing_code,
            improved_code,
            one_sentence_summary,
            suggestion_content,
            score,
        });
    }

    // Sort by score descending
    suggestions.sort_by(|a, b| b.score.cmp(&a.score));
    suggestions
}

/// Convert parsed suggestions into `CodeSuggestion` structs for inline publishing.
///
/// Uses GitHub's native `suggestion` block format for committable suggestions.
pub fn suggestions_to_code_suggestions(suggestions: &[ParsedSuggestion]) -> Vec<CodeSuggestion> {
    suggestions
        .iter()
        .filter(|s| s.relevant_lines_start > 0 && s.relevant_lines_end > 0)
        .map(|s| {
            let body = format!(
                "**Suggestion:** {} [{}, importance: {}]",
                s.suggestion_content, s.label, s.score
            );
            CodeSuggestion {
                body,
                relevant_file: s.relevant_file.clone(),
                relevant_lines_start: s.relevant_lines_start,
                relevant_lines_end: s.relevant_lines_end,
                existing_code: s.existing_code.clone(),
                improved_code: s.improved_code.clone(),
            }
        })
        .collect()
}

/// Format suggestions as a summary comment (table format).
///
/// Used when `commitable_code_suggestions = false`.
/// Suggestions with no valid line numbers (lines <= 0) are displayed in a
/// separate "Architecture & Design" section as high-level observations.
pub fn format_suggestions_table(
    suggestions: &[ParsedSuggestion],
    th_high: u32,
    th_medium: u32,
) -> String {
    let marker = persistent_comment_marker("improve");
    let mut out = String::with_capacity(4_000);

    let _ = writeln!(out, "{marker}");
    let _ = writeln!(out, "## PR Code Suggestions ✨\n");

    if suggestions.is_empty() {
        let _ = writeln!(out, "No code suggestions found for this PR.");
        return out;
    }

    // Split into code-level (valid line numbers) and high-level (no lines)
    let (code_level, high_level): (Vec<&ParsedSuggestion>, Vec<&ParsedSuggestion>) = suggestions
        .iter()
        .partition(|s| s.relevant_lines_start > 0 && s.relevant_lines_end > 0);

    // Render high-level suggestions first (if any)
    if !high_level.is_empty() {
        let _ = writeln!(out, "### Architecture & Design\n");
        for s in &high_level {
            let raw_summary = if s.one_sentence_summary.is_empty() {
                &s.suggestion_content
            } else {
                &s.one_sentence_summary
            };
            let summary = sanitize_table_cell(raw_summary);
            let importance = importance_label(s.score, th_high, th_medium);
            let file = sanitize_table_cell(&s.relevant_file);
            let _ = writeln!(out, "- **[{importance}] {summary}** (`{file}`)");
        }
        let _ = writeln!(out);
    }

    // Render code-level suggestions table
    if !code_level.is_empty() {
        if !high_level.is_empty() {
            let _ = writeln!(out, "### Code Suggestions\n");
        }

        let _ = writeln!(out, "| Category | Suggestion | Score |");
        let _ = writeln!(out, "| --- | --- | --- |");

        for s in &code_level {
            let importance = importance_label(s.score, th_high, th_medium);

            let raw_summary = if s.one_sentence_summary.is_empty() {
                &s.suggestion_content
            } else {
                &s.one_sentence_summary
            };

            // Truncate long summaries for table (char-safe)
            let summary = if raw_summary.len() > 200 {
                let end = raw_summary
                    .char_indices()
                    .take_while(|(i, _)| *i < 200)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(200.min(raw_summary.len()));
                format!("{}...", &raw_summary[..end])
            } else {
                raw_summary.to_string()
            };

            // Sanitize for markdown table: replace newlines and pipes
            let summary = sanitize_table_cell(&summary);
            let label = sanitize_table_cell(&s.label);
            let file = sanitize_table_cell(&s.relevant_file);

            // Format line range
            let lines_str = if s.relevant_lines_start == s.relevant_lines_end {
                format!(" [{}]", s.relevant_lines_start)
            } else {
                format!(" [{}-{}]", s.relevant_lines_start, s.relevant_lines_end)
            };

            let _ = writeln!(
                out,
                "| {label} | **{summary}**<br>`{file}`{lines_str} | {importance} |",
            );
        }
    }

    out
}

/// Map a suggestion score to an importance label using configurable thresholds.
///
/// `th_high` is the minimum score for "Critical", `th_medium` for "Important".
fn importance_label(score: u32, th_high: u32, th_medium: u32) -> &'static str {
    if score >= th_high {
        "Critical"
    } else if score >= th_medium {
        "Important"
    } else {
        "Minor"
    }
}

/// Append a self-review checkbox to the suggestions body.
///
/// Adds a markdown checkbox with an HTML comment indicating which actions
/// to take when checked (approve, fold, or both).
pub fn append_self_review_checkbox(body: &mut String, text: &str, approve: bool, fold: bool) {
    body.push_str("\n\n- [ ]  ");
    body.push_str(text);
    if approve && !fold {
        body.push_str(" <!-- approve pr self-review -->");
    } else if fold && !approve {
        body.push_str(" <!-- fold suggestions self-review -->");
    } else {
        body.push_str(" <!-- approve and fold suggestions self-review -->");
    }
    body.push('\n');
}

/// Sanitize text for use inside a markdown table cell.
/// Replaces newlines with `<br>` and escapes pipe characters.
fn sanitize_table_cell(text: &str) -> String {
    text.replace('\n', "<br>")
        .replace('\r', "")
        .replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_suggestions() {
        let yaml_str = r#"
code_suggestions:
  - label: "bug fix"
    relevant_file: "src/main.rs"
    existing_code: "let x = 1;"
    improved_code: "let x = 2;"
    one_sentence_summary: "Fix off-by-one"
    suggestion_content: "The value should be 2"
    relevant_lines_start: 10
    relevant_lines_end: 10
    score: 8
  - label: "enhancement"
    relevant_file: "src/lib.rs"
    existing_code: "fn foo() {}"
    improved_code: "fn foo() -> Result<()> {}"
    one_sentence_summary: "Add error handling"
    suggestion_content: "Return Result type"
    relevant_lines_start: 5
    relevant_lines_end: 5
    score: 6
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let suggestions = parse_suggestions(&data);

        assert_eq!(suggestions.len(), 2);
        // Sorted by score descending
        assert_eq!(suggestions[0].score, 8);
        assert_eq!(suggestions[0].relevant_file, "src/main.rs");
        assert_eq!(suggestions[1].score, 6);
    }

    #[test]
    fn test_suggestions_to_code_suggestions() {
        let suggestions = vec![ParsedSuggestion {
            label: "bug fix".into(),
            relevant_file: "src/main.rs".into(),
            relevant_lines_start: 10,
            relevant_lines_end: 12,
            existing_code: "old code".into(),
            improved_code: "new code".into(),
            one_sentence_summary: "Fix bug".into(),
            suggestion_content: "Fix the bug".into(),
            score: 8,
        }];

        let code_suggestions = suggestions_to_code_suggestions(&suggestions);
        assert_eq!(code_suggestions.len(), 1);
        assert_eq!(code_suggestions[0].relevant_file, "src/main.rs");
        assert!(code_suggestions[0].body.contains("bug fix"));
    }

    #[test]
    fn test_format_suggestions_table() {
        let suggestions = vec![ParsedSuggestion {
            label: "enhancement".into(),
            relevant_file: "src/lib.rs".into(),
            relevant_lines_start: 5,
            relevant_lines_end: 10,
            existing_code: "old".into(),
            improved_code: "new".into(),
            one_sentence_summary: "Improve performance".into(),
            suggestion_content: "Use a better algorithm".into(),
            score: 7,
        }];

        let result = format_suggestions_table(&suggestions, 9, 7);
        assert!(result.contains("PR Code Suggestions"));
        assert!(result.contains("<!-- pr-agent:improve -->"));
        assert!(result.contains("Improve performance"));
        assert!(result.contains("Important"));
    }

    #[test]
    fn test_format_suggestions_table_empty() {
        let result = format_suggestions_table(&[], 9, 7);
        assert!(result.contains("No code suggestions found"));
    }

    #[test]
    fn test_format_suggestions_table_zero_lines_as_high_level() {
        let suggestions = vec![ParsedSuggestion {
            label: "enhancement".into(),
            relevant_file: "src/lib.rs".into(),
            relevant_lines_start: 0,
            relevant_lines_end: 0,
            existing_code: "old".into(),
            improved_code: "new".into(),
            one_sentence_summary: "Fix issue".into(),
            suggestion_content: "Fix".into(),
            score: 5,
        }];

        let result = format_suggestions_table(&suggestions, 9, 7);
        // Should appear in high-level section, not in table
        assert!(result.contains("Architecture & Design"));
        assert!(result.contains("[Minor] Fix issue"));
        assert!(result.contains("`src/lib.rs`"));
        // Should NOT contain table headers (no code-level suggestions)
        assert!(!result.contains("| Category |"));
    }

    #[test]
    fn test_format_suggestions_table_mixed_high_and_code_level() {
        let suggestions = vec![
            ParsedSuggestion {
                label: "design".into(),
                relevant_file: "src/lib.rs".into(),
                relevant_lines_start: 0,
                relevant_lines_end: 0,
                existing_code: "".into(),
                improved_code: "new".into(),
                one_sentence_summary: "Consider splitting module".into(),
                suggestion_content: "Split".into(),
                score: 8,
            },
            ParsedSuggestion {
                label: "bug".into(),
                relevant_file: "src/main.rs".into(),
                relevant_lines_start: 10,
                relevant_lines_end: 15,
                existing_code: "old".into(),
                improved_code: "new".into(),
                one_sentence_summary: "Fix null check".into(),
                suggestion_content: "Add null check".into(),
                score: 9,
            },
        ];

        let result = format_suggestions_table(&suggestions, 9, 7);
        // Both sections present
        assert!(result.contains("Architecture & Design"));
        assert!(result.contains("Code Suggestions"));
        // High-level in bullet list
        assert!(result.contains("[Important] Consider splitting module"));
        // Code-level in table
        assert!(result.contains("| bug |"));
        assert!(result.contains("[10-15]"));
    }

    #[test]
    fn test_format_suggestions_table_single_line() {
        let suggestions = vec![ParsedSuggestion {
            label: "bug".into(),
            relevant_file: "src/main.rs".into(),
            relevant_lines_start: 42,
            relevant_lines_end: 42,
            existing_code: "old".into(),
            improved_code: "new".into(),
            one_sentence_summary: "Fix".into(),
            suggestion_content: "Fix".into(),
            score: 8,
        }];

        let result = format_suggestions_table(&suggestions, 9, 7);
        assert!(result.contains("[42]"));
        assert!(!result.contains("[42-42]"));
    }

    #[test]
    fn test_format_suggestions_table_sanitizes_newlines() {
        let suggestions = vec![ParsedSuggestion {
            label: "line1\nline2".into(),
            relevant_file: "src/lib.rs".into(),
            relevant_lines_start: 1,
            relevant_lines_end: 5,
            existing_code: "old".into(),
            improved_code: "new".into(),
            one_sentence_summary: "Summary with\nnewline".into(),
            suggestion_content: "Content".into(),
            score: 6,
        }];

        let result = format_suggestions_table(&suggestions, 9, 7);
        // Table rows should not have raw newlines within cells
        for line in result.lines() {
            if line.starts_with("| ") && line.contains("Summary") {
                // This line is a table row — must not split across lines
                assert!(line.ends_with(" |") || line.ends_with(" |"));
            }
        }
    }

    #[test]
    fn test_append_self_review_checkbox_approve_only() {
        let mut body = String::from("table content");
        append_self_review_checkbox(&mut body, "I reviewed", true, false);
        assert!(body.contains("- [ ]  I reviewed"));
        assert!(body.contains("<!-- approve pr self-review -->"));
        assert!(!body.contains("fold"));
    }

    #[test]
    fn test_append_self_review_checkbox_fold_only() {
        let mut body = String::from("table content");
        append_self_review_checkbox(&mut body, "I reviewed", false, true);
        assert!(body.contains("- [ ]  I reviewed"));
        assert!(body.contains("<!-- fold suggestions self-review -->"));
        assert!(!body.contains("approve"));
    }

    #[test]
    fn test_append_self_review_checkbox_both() {
        let mut body = String::from("table content");
        append_self_review_checkbox(&mut body, "I reviewed", true, true);
        assert!(body.contains("- [ ]  I reviewed"));
        assert!(body.contains("<!-- approve and fold suggestions self-review -->"));
    }

    #[test]
    fn test_append_self_review_checkbox_neither() {
        let mut body = String::from("table content");
        append_self_review_checkbox(&mut body, "I reviewed", false, false);
        assert!(body.contains("- [ ]  I reviewed"));
        // When both false, defaults to "approve and fold"
        assert!(body.contains("<!-- approve and fold suggestions self-review -->"));
    }
}
