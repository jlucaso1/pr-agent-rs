use std::fmt::Write;

use indexmap::IndexMap;

use crate::git::capitalize_first;
use crate::git::types::CodeSuggestion;
use crate::output::markdown::persistent_comment_marker;
use crate::output::yaml_parser::{yaml_str_field, yaml_value_as_i64, yaml_value_as_u64};

/// A function that generates a link to a file range in the PR diff view.
/// Parameters: (file, start_line, end_line) → URL string (possibly empty).
pub type SuggestionLinkGen<'a> = dyn Fn(&str, i32, i32) -> String + 'a;

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
    /// Why the score was given (populated by the self-reflect pass).
    pub score_why: String,
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
            score_why: String::new(),
        });
    }

    // Sort by score descending
    suggestions.sort_by_key(|s| std::cmp::Reverse(s.score));
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
    link_gen: &SuggestionLinkGen,
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

    // Render code-level suggestions as a rich grouped HTML table: each row is a
    // label group; each suggestion is a <details> with its diff (before/after),
    // a link to the lines, and the reflect "Why". Mirrors the Python original.
    if !code_level.is_empty() {
        if !high_level.is_empty() {
            let _ = writeln!(out, "### Code Suggestions\n");
        }

        // Group by label; sort groups by their highest score, items within by score.
        let mut groups: IndexMap<String, Vec<&ParsedSuggestion>> = IndexMap::new();
        for s in &code_level {
            let label = s.label.trim().trim_matches(['\'', '"']).to_string();
            groups.entry(label).or_default().push(s);
        }
        let mut group_vec: Vec<(String, Vec<&ParsedSuggestion>)> = groups.into_iter().collect();
        for (_, items) in group_vec.iter_mut() {
            items.sort_by_key(|s| std::cmp::Reverse(s.score));
        }
        group_vec.sort_by_key(|(_, items)| {
            std::cmp::Reverse(items.iter().map(|s| s.score).max().unwrap_or(0))
        });

        out.push_str("<table>");
        let header = format!("Suggestion{}", "&nbsp; ".repeat(66));
        let _ = write!(
            out,
            "<thead><tr><td><strong>Category</strong></td><td align=left><strong>{header}</strong></td><td align=center><strong>Impact</strong></td></tr>"
        );
        out.push_str("<tbody>");

        for (label, items) in &group_vec {
            let cap_label = capitalize_first(label);
            let n = items.len();
            let _ = writeln!(out, "<tr><td rowspan={n}>{cap_label}</td>");

            for (i, s) in items.iter().enumerate() {
                let range_str = if s.relevant_lines_start == s.relevant_lines_end {
                    format!("[{}]", s.relevant_lines_start)
                } else {
                    format!("[{}-{}]", s.relevant_lines_start, s.relevant_lines_end)
                };
                let link = link_gen(
                    &s.relevant_file,
                    s.relevant_lines_start,
                    s.relevant_lines_end,
                );
                let summary = if s.one_sentence_summary.is_empty() {
                    s.suggestion_content.trim()
                } else {
                    s.one_sentence_summary.trim()
                };
                let file = s.relevant_file.trim();
                let diff_block = code_diff_block(&s.existing_code, &s.improved_code);
                let importance = importance_label(s.score, th_high, th_medium);

                if i == 0 {
                    out.push_str("<td>\n\n");
                } else {
                    out.push_str("<tr><td>\n\n");
                }
                let _ = write!(out, "<details><summary>{summary}</summary>\n\n___\n\n");
                let _ = write!(
                    out,
                    "**{}**\n\n[{file} {range_str}]({link})\n\n{diff_block}\n",
                    s.suggestion_content.trim()
                );
                if !s.score_why.is_empty() {
                    let _ = write!(
                        out,
                        "<details><summary>Suggestion importance[1-10]: {}</summary>\n\n__\n\nWhy: {}\n\n</details>",
                        s.score, s.score_why
                    );
                }
                out.push_str("</details>");
                let _ = write!(out, "</td><td align=center>{importance}\n\n</td></tr>");
            }
        }
        out.push_str("</tbody></table>");
    }

    out
}

/// Render a ```diff block showing the change from `existing` to `improved`.
fn code_diff_block(existing: &str, improved: &str) -> String {
    let diff = similar::TextDiff::from_lines(existing.trim_end(), improved.trim_end());
    let mut out = String::from("```diff\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => '-',
            similar::ChangeTag::Insert => '+',
            similar::ChangeTag::Equal => ' ',
        };
        out.push(sign);
        out.push_str(change.value().trim_end_matches('\n'));
        out.push('\n');
    }
    out.push_str("```");
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
            score_why: String::new(),
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
            score_why: String::new(),
        }];

        let result =
            format_suggestions_table(&suggestions, 9, 7, &|_: &str, _: i32, _: i32| String::new());
        assert!(result.contains("PR Code Suggestions"));
        assert!(result.contains("<!-- pr-agent:improve -->"));
        assert!(result.contains("Improve performance"));
        assert!(result.contains("Important"));
    }

    #[test]
    fn test_format_suggestions_table_empty() {
        let result = format_suggestions_table(&[], 9, 7, &|_: &str, _: i32, _: i32| String::new());
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
            score_why: String::new(),
        }];

        let result =
            format_suggestions_table(&suggestions, 9, 7, &|_: &str, _: i32, _: i32| String::new());
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
                score_why: String::new(),
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
                score_why: String::new(),
            },
        ];

        let result =
            format_suggestions_table(&suggestions, 9, 7, &|_: &str, _: i32, _: i32| String::new());
        // Both sections present
        assert!(result.contains("Architecture & Design"));
        assert!(result.contains("Code Suggestions"));
        // High-level in bullet list
        assert!(result.contains("[Important] Consider splitting module"));
        // Code-level in the rich HTML table: capitalized label, range, diff block.
        assert!(result.contains("<table>"));
        assert!(result.contains(">Bug</td>"));
        assert!(result.contains("[10-15]"));
        assert!(result.contains("```diff"));
    }

    #[test]
    fn test_format_suggestions_table_rich_features() {
        let suggestions = vec![ParsedSuggestion {
            label: "bug".into(),
            relevant_file: "src/main.rs".into(),
            relevant_lines_start: 10,
            relevant_lines_end: 12,
            existing_code: "let x = 1;\nlet y = 2;".into(),
            improved_code: "let x = 1;\nlet y = 3;".into(),
            one_sentence_summary: "Fix the value".into(),
            suggestion_content: "Change y to 3".into(),
            score: 8,
            score_why: "Avoids a subtle bug".into(),
        }];
        let link_gen = |file: &str, s: i32, e: i32| format!("https://gh/{file}#L{s}-L{e}");
        let result = format_suggestions_table(&suggestions, 9, 7, &link_gen);

        // Collapsible summary + before/after diff.
        assert!(result.contains("<details><summary>Fix the value</summary>"));
        assert!(result.contains("```diff"));
        assert!(
            result.contains("-let y = 2;"),
            "diff shows the removed line: {result}"
        );
        assert!(result.contains("+let y = 3;"), "diff shows the added line");
        assert!(
            result.contains(" let x = 1;"),
            "diff keeps the context line"
        );
        // Line link.
        assert!(result.contains("https://gh/src/main.rs#L10-L12"));
        // Reflect "Why".
        assert!(result.contains("Why: Avoids a subtle bug"));
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
            score_why: String::new(),
        }];

        let result =
            format_suggestions_table(&suggestions, 9, 7, &|_: &str, _: i32, _: i32| String::new());
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
            score_why: String::new(),
        }];

        let result =
            format_suggestions_table(&suggestions, 9, 7, &|_: &str, _: i32, _: i32| String::new());
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
