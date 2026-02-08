use std::fmt::Write;

use crate::output::markdown::{
    collapsible_section, effort_bar, persistent_comment_marker, section_emoji,
};

/// A function that generates a link to a file in the PR diff view.
///
/// Parameters: (file, start_line, end_line) → URL string.
/// When None, no links are generated.
pub type LinkGenerator = Box<dyn Fn(&str, i32, Option<i32>) -> String + Send + Sync>;

/// Convert a parsed review YAML response into formatted GitHub markdown.
///
/// `link_gen` optionally provides a function to generate clickable file links.
pub fn format_review_markdown(
    data: &serde_yaml_ng::Value,
    gfm_supported: bool,
    link_gen: Option<&LinkGenerator>,
) -> String {
    let mut out = String::with_capacity(8_000);

    // Header with persistent comment marker
    let marker = persistent_comment_marker("review");
    let _ = writeln!(out, "{marker}");
    let _ = writeln!(out, "## PR Reviewer Guide 🔍\n");

    let review = data.get("review").unwrap_or(data);

    if !review.is_mapping() {
        out.push_str("*No structured review data available.*\n");
        return out;
    }

    if gfm_supported {
        format_review_gfm(review, &mut out, link_gen);
    } else {
        format_review_plain(review, &mut out);
    }

    out
}

/// Format review using GitHub Flavored Markdown (HTML tables).
fn format_review_gfm(
    review: &serde_yaml_ng::Value,
    out: &mut String,
    link_gen: Option<&LinkGenerator>,
) {
    out.push_str("<table>\n");

    let mapping = match review.as_mapping() {
        Some(m) => m,
        None => return,
    };

    for (key, value) in mapping {
        let key_str = key.as_str().unwrap_or_default();

        // Skip empty/null values
        if value.is_null()
            || matches!(value, serde_yaml_ng::Value::String(s) if s.trim().is_empty())
        {
            continue;
        }

        match key_str {
            "estimated_effort_to_review_[1-5]" | "estimated_effort_to_review" => {
                format_effort_row(value, out);
            }
            "score" => {
                format_score_row(value, out);
            }
            "relevant_tests" => {
                format_relevant_tests_row(value, out);
            }
            "possible_issues" => {
                format_simple_row("⚡ Possible issues", value, out);
            }
            "security_concerns" => {
                format_security_row(value, out);
            }
            "key_issues_to_review" => {
                format_key_issues_rows(value, out, link_gen);
            }
            "can_be_split" => {
                format_simple_row("🔀 Can be split", value, out);
            }
            "ticket_compliance_check" => {
                format_simple_row("🎫 Ticket compliance", value, out);
            }
            "todo_sections" => {
                format_todo_sections_row(value, out);
            }
            // Skip internal fields that shouldn't be rendered
            "todo_summary" => {}
            _ => {
                // Generic section
                let emoji = section_emoji(key_str);
                let label = if emoji.is_empty() {
                    key_str.replace('_', " ")
                } else {
                    format!("{emoji} {}", key_str.replace('_', " "))
                };
                format_simple_row(&label, value, out);
            }
        }
    }

    out.push_str("</table>\n");
}

/// Format effort-to-review row with visual bar.
fn format_effort_row(value: &serde_yaml_ng::Value, out: &mut String) {
    let effort = extract_effort_score(value);
    let bar = effort_estimation_bar(effort);
    let emoji = section_emoji("Estimated effort to review [1-5]");

    let _ = writeln!(
        out,
        "<tr><td>{emoji}&nbsp;<strong>Estimated effort to review</strong>: {bar}</td></tr>"
    );
}

/// Format score row.
fn format_score_row(value: &serde_yaml_ng::Value, out: &mut String) {
    let score_str = yaml_value_to_string(value);
    let emoji = section_emoji("Score");

    let _ = writeln!(
        out,
        "<tr><td>{emoji}&nbsp;<strong>Score</strong>: {score_str}</td></tr>"
    );
}

/// Format the relevant tests row as an HTML table row.
fn format_relevant_tests_row(value: &serde_yaml_ng::Value, out: &mut String) {
    let emoji = section_emoji("Relevant tests");
    let text = yaml_value_to_string(value);

    if is_value_no(&text) {
        let _ = writeln!(
            out,
            "<tr><td>{emoji}&nbsp;<strong>No relevant tests</strong></td></tr>"
        );
    } else {
        let _ = writeln!(
            out,
            "<tr><td>{emoji}&nbsp;<strong>PR contains tests</strong></td></tr>"
        );
    }
}

/// Format todo sections as HTML table rows.
fn format_todo_sections_row(value: &serde_yaml_ng::Value, out: &mut String) {
    let text = yaml_value_to_string(value);

    if is_value_no(&text) {
        let _ = writeln!(
            out,
            "<tr><td>✅&nbsp;<strong>No TODO sections</strong></td></tr>"
        );
    } else {
        let emoji = section_emoji("Todo sections");
        let _ = writeln!(
            out,
            "<tr><td>{emoji}&nbsp;<strong>TODO sections</strong><br><br>{text}</td></tr>"
        );
    }
}

/// Format security concerns with collapsible details.
fn format_security_row(value: &serde_yaml_ng::Value, out: &mut String) {
    let text = yaml_value_to_string(value);
    let emoji = section_emoji("Security concerns");

    if is_value_no(&text) {
        let _ = writeln!(
            out,
            "<tr><td>{emoji}&nbsp;<strong>No security concerns identified</strong></td></tr>"
        );
    } else {
        let details = collapsible_section("Security concerns", &text);
        let _ = writeln!(out, "<tr><td>{emoji}&nbsp;{details}</td></tr>");
    }
}

/// Format key issues to review as individual rows with file links.
///
/// Formats the "key issues to review" section as linked HTML rows.
fn format_key_issues_rows(
    value: &serde_yaml_ng::Value,
    out: &mut String,
    link_gen: Option<&LinkGenerator>,
) {
    let emoji = section_emoji("Key issues to review");

    let issues = match value.as_sequence() {
        Some(seq) => seq,
        None => {
            let text = yaml_value_to_string(value);
            if is_value_no(&text) {
                let _ = writeln!(
                    out,
                    "<tr><td>{emoji}&nbsp;<strong>No major issues detected</strong></td></tr>"
                );
            } else if !text.is_empty() {
                let _ = writeln!(
                    out,
                    "<tr><td>{emoji}&nbsp;<strong>Recommended focus areas for review</strong><br>{text}</td></tr>"
                );
            }
            return;
        }
    };

    if issues.is_empty() {
        let _ = writeln!(
            out,
            "<tr><td>{emoji}&nbsp;<strong>No major issues detected</strong></td></tr>"
        );
        return;
    }

    let _ = write!(
        out,
        "<tr><td>{emoji}&nbsp;<strong>Recommended focus areas for review</strong><br><br>\n\n"
    );

    for issue in issues {
        // Support both field name variants: issue_header/issue_content and header/content
        // .trim() all values to strip YAML trailing newlines
        let header = issue
            .get("issue_header")
            .or(issue.get("header"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .unwrap_or("Issue");
        // Rename "Possible Bug" to "Possible Issue" for display
        let header = if header.eq_ignore_ascii_case("possible bug") {
            "Possible Issue"
        } else {
            header
        };

        let body = issue
            .get("issue_content")
            .or(issue.get("content"))
            .or(issue.get("details"))
            .or(issue.get("suggestion"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .unwrap_or("");
        let file = issue
            .get("relevant_file")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .unwrap_or("");

        // Prefer start_line/end_line; fall back to relevant_line
        let start_line_str = issue
            .get("start_line")
            .map(yaml_value_to_string)
            .unwrap_or_default();
        let end_line_str = issue
            .get("end_line")
            .map(yaml_value_to_string)
            .unwrap_or_default();
        let start_line_num: i32 = start_line_str.parse().unwrap_or(0);
        let end_line_num: i32 = end_line_str.parse().unwrap_or(0);

        let line_display = if !start_line_str.is_empty()
            && !end_line_str.is_empty()
            && start_line_str != end_line_str
        {
            format!("{start_line_str}-{end_line_str}")
        } else if !start_line_str.is_empty() {
            start_line_str.clone()
        } else {
            issue
                .get("relevant_line")
                .map(yaml_value_to_string)
                .unwrap_or_default()
        };

        // Generate link if provider is available
        let reference_link: Option<String> = if !file.is_empty() {
            link_gen.map(|link_fn| {
                let end = if end_line_num > 0 && end_line_num != start_line_num {
                    Some(end_line_num)
                } else {
                    None
                };
                link_fn(file, start_line_num, end)
            })
        } else {
            None
        };

        // Build the issue entry in GFM format
        // All issues are within the same <td>, not separate rows
        let header_html = match &reference_link {
            Some(link) if !link.is_empty() => {
                format!("<a href='{link}'><strong>{header}</strong></a>")
            }
            _ => format!("<strong>{header}</strong>"),
        };

        let file_info = if !file.is_empty() {
            if !line_display.is_empty() {
                format!("<br><code>{file}</code> (line {line_display})")
            } else {
                format!("<br><code>{file}</code>")
            }
        } else {
            String::new()
        };

        let body_html = if !body.is_empty() {
            format!("<br>{body}")
        } else {
            String::new()
        };

        let _ = writeln!(out, "{header_html}{file_info}{body_html}\n");
    }

    let _ = writeln!(out, "</td></tr>");
}

/// Format a simple key-value row. Skips "No"/"None"/"False" values.
fn format_simple_row(label: &str, value: &serde_yaml_ng::Value, out: &mut String) {
    let text = yaml_value_to_string(value);
    if text.is_empty() || is_value_no(&text) {
        return;
    }
    let _ = writeln!(out, "<tr><td><strong>{label}</strong>: {text}</td></tr>");
}

/// Format review using plain markdown (no HTML tables).
fn format_review_plain(review: &serde_yaml_ng::Value, out: &mut String) {
    let mapping = match review.as_mapping() {
        Some(m) => m,
        None => return,
    };

    for (key, value) in mapping {
        let key_str = key.as_str().unwrap_or_default();
        let emoji = section_emoji(key_str);
        let text = yaml_value_to_string(value);

        if text.is_empty() {
            continue;
        }

        if emoji.is_empty() {
            let _ = writeln!(out, "**{key_str}**: {text}\n");
        } else {
            let _ = writeln!(out, "{emoji} **{key_str}**: {text}\n");
        }
    }
}

/// Create effort estimation visual bar.
fn effort_estimation_bar(effort: u8) -> String {
    let effort = effort.clamp(1, 5);
    let filled = effort as usize;
    let empty = 5 - filled;
    let bar_emoji = effort_bar(effort);
    let visual: String = "🔵".repeat(filled) + &"⚪".repeat(empty);
    format!("{bar_emoji} ({visual})")
}

/// Extract numeric effort score from various YAML formats.
pub(crate) fn extract_effort_score(value: &serde_yaml_ng::Value) -> u8 {
    // Could be "3", 3, "3/5", "3 - because..."
    let text = yaml_value_to_string(value);
    text.chars()
        .find(|c| c.is_ascii_digit())
        .and_then(|c| c.to_digit(10))
        .map(|d| d as u8)
        .unwrap_or(3)
}

/// Check if a value represents "no" (handles "no", "none", empty, etc.).
pub(crate) fn is_value_no(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    t.is_empty() || t == "no" || t == "none" || t == "false"
}

/// Convert a YAML value to a trimmed display string.
pub(crate) fn yaml_value_to_string(value: &serde_yaml_ng::Value) -> String {
    match value {
        serde_yaml_ng::Value::String(s) => s.trim().to_string(),
        serde_yaml_ng::Value::Bool(b) => b.to_string(),
        serde_yaml_ng::Value::Number(n) => n.to_string(),
        serde_yaml_ng::Value::Null => String::new(),
        serde_yaml_ng::Value::Sequence(seq) if seq.is_empty() => String::new(),
        serde_yaml_ng::Value::Sequence(seq) => seq
            .iter()
            .map(yaml_value_to_string)
            .collect::<Vec<_>>()
            .join(", "),
        serde_yaml_ng::Value::Mapping(_) => {
            // For mappings, try to produce something readable
            serde_yaml_ng::to_string(value)
                .unwrap_or_default()
                .trim()
                .to_string()
        }
        serde_yaml_ng::Value::Tagged(tagged) => yaml_value_to_string(&tagged.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effort_estimation_bar() {
        let bar = effort_estimation_bar(3);
        assert!(bar.contains("🔵🔵🔵⚪⚪"));
        assert!(bar.contains("3️⃣"));
    }

    #[test]
    fn test_extract_effort_score() {
        assert_eq!(
            extract_effort_score(&serde_yaml_ng::Value::String("3".into())),
            3
        );
        assert_eq!(
            extract_effort_score(&serde_yaml_ng::Value::String(
                "4 - moderate complexity".into()
            )),
            4
        );
        assert_eq!(
            extract_effort_score(&serde_yaml_ng::Value::Number(2.into())),
            2
        );
    }

    #[test]
    fn test_format_review_markdown_basic() {
        let yaml_str = r#"
review:
  estimated_effort_to_review_[1-5]: 3
  relevant_tests: "No"
  security_concerns: "No"
  key_issues_to_review:
    - issue_header: "Error Handling"
      issue_content: "Missing error check"
      relevant_file: "src/main.rs"
      start_line: 42
      end_line: 42
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);

        assert!(result.contains("PR Reviewer Guide"));
        assert!(result.contains("<!-- pr-agent:review -->"));
        assert!(result.contains("Estimated effort to review"));
        assert!(result.contains("🔵🔵🔵⚪⚪"));
        assert!(result.contains("Error Handling"));
        assert!(result.contains("src/main.rs"));
        // "No" for relevant_tests should show "No relevant tests"
        assert!(result.contains("No relevant tests"));
        // "No" for security should show "No security concerns identified"
        assert!(result.contains("No security concerns identified"));
    }

    #[test]
    fn test_format_review_markdown_no_issues() {
        let yaml_str = r#"
review:
  estimated_effort_to_review_[1-5]: 1
  security_concerns: "No"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);

        assert!(result.contains("No security concerns identified"));
    }

    #[test]
    fn test_yaml_value_to_string_trims() {
        // YAML block scalars have trailing newlines
        assert_eq!(
            yaml_value_to_string(&serde_yaml_ng::Value::String("hello\n".into())),
            "hello"
        );
        assert_eq!(
            yaml_value_to_string(&serde_yaml_ng::Value::String("  spaced  ".into())),
            "spaced"
        );
    }

    #[test]
    fn test_relevant_tests_yes_shows_contains() {
        let yaml_str = r#"
review:
  relevant_tests: "Yes"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);
        assert!(result.contains("PR contains tests"));
        assert!(!result.contains("Relevant tests: Yes"));
    }

    #[test]
    fn test_todo_sections_no_shows_no_todos() {
        let yaml_str = r#"
review:
  todo_sections: "No"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);
        assert!(result.contains("No TODO sections"));
        assert!(!result.contains("todo_sections"));
    }

    #[test]
    fn test_key_issues_with_canonical_field_names() {
        let yaml_str = r#"
review:
  key_issues_to_review:
    - issue_header: "Possible Bug"
      issue_content: "Null pointer dereference when input is empty"
      relevant_file: "src/parser.rs"
      start_line: 15
      end_line: 20
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);

        assert!(result.contains("Possible Issue"));
        assert!(!result.contains("Possible Bug"));
        assert!(result.contains("Null pointer dereference"));
        assert!(result.contains("src/parser.rs"));
        assert!(result.contains("15-20"));
    }

    #[test]
    fn test_key_issues_with_legacy_field_names() {
        let yaml_str = r#"
review:
  key_issues_to_review:
    - header: "Performance"
      content: "Slow query detected"
      relevant_file: "src/db.rs"
      relevant_line: "100"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);

        assert!(result.contains("Performance"));
        assert!(result.contains("Slow query detected"));
        assert!(result.contains("src/db.rs"));
        assert!(result.contains("100"));
    }

    #[test]
    fn test_is_value_no() {
        assert!(is_value_no("No"));
        assert!(is_value_no("no"));
        assert!(is_value_no("None"));
        assert!(is_value_no("false"));
        assert!(is_value_no(""));
        assert!(is_value_no("  no  "));
        assert!(!is_value_no("Yes"));
        assert!(!is_value_no("Some value"));
    }
}
