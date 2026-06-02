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

    let Some(mapping) = review.as_mapping() else {
        return;
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
                format_can_be_split_row(value, out);
            }
            "contribution_time_cost_estimate" => {
                format_contribution_time_cost_row(value, out);
            }
            "ticket_compliance_check" => {
                format_ticket_compliance_row(section_emoji("Ticket compliance check"), value, out);
            }
            "todo_sections" => {
                format_todo_sections_row(value, out, link_gen);
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

/// Format the `todo_sections` row — `Union[List[TodoSection], str]`. When the
/// PR has TODO comments the AI returns a list (each item: relevant_file,
/// line_number, content); otherwise the string "No". Mirrors Python
/// `format_todo_items`: render a capped `<ul>` of linked file refs. Without the
/// list branch, `yaml_value_to_string` flattened the items into raw YAML.
fn format_todo_sections_row(
    value: &serde_yaml_ng::Value,
    out: &mut String,
    link_gen: Option<&LinkGenerator>,
) {
    // "No todos" is signalled either by an empty list or by the "No" string
    // sentinel. An empty `<ul>` would render a misleading TODO header, so treat
    // an empty sequence the same as "No".
    let no_todos = match value.as_sequence() {
        Some(seq) => seq.is_empty(),
        None => is_value_no(&yaml_value_to_string(value)),
    };
    if no_todos {
        let _ = writeln!(
            out,
            "<tr><td>✅&nbsp;<strong>No TODO sections</strong></td></tr>"
        );
        return;
    }

    let emoji = section_emoji("Todo sections");
    let _ = write!(
        out,
        "<tr><td>{emoji}&nbsp;<strong>TODO sections</strong>\n<br><br>\n"
    );
    out.push_str(&format_todo_items(value, link_gen));
    out.push_str("</td></tr>\n");
}

/// Maximum TODO items to display (mirrors Python `MAX_ITEMS`).
const MAX_TODO_ITEMS: usize = 5;

/// Render the TODO items as a capped `<ul>` list (or a single `<p>` when the
/// value is one item rather than a list). Mirrors Python `format_todo_items`.
fn format_todo_items(value: &serde_yaml_ng::Value, link_gen: Option<&LinkGenerator>) -> String {
    let mut out = String::new();
    match value.as_sequence() {
        Some(items) => {
            out.push_str("<ul>\n");
            for item in items.iter().take(MAX_TODO_ITEMS) {
                let _ = writeln!(out, "<li>{}</li>", format_todo_item(item, link_gen));
            }
            out.push_str("</ul>\n");
        }
        None => {
            let _ = writeln!(out, "<p>{}</p>", format_todo_item(value, link_gen));
        }
    }
    out
}

/// Render one TODO item as `<a href=link>file [line]</a>: content`.
/// Mirrors Python `format_todo_item`.
fn format_todo_item(item: &serde_yaml_ng::Value, link_gen: Option<&LinkGenerator>) -> String {
    let relevant_file = item
        .get("relevant_file")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let line_str = item
        .get("line_number")
        .map(yaml_value_to_string)
        .unwrap_or_default();
    let content = item
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    let mut file_ref = format!("{relevant_file} [{line_str}]");
    let line_num: i32 = line_str.parse().unwrap_or(0);
    if !relevant_file.is_empty()
        && let Some(link_fn) = link_gen
    {
        let link = link_fn(relevant_file, line_num, None);
        if !link.is_empty() {
            file_ref = format!("<a href='{link}'>{file_ref}</a>");
        }
    }

    if content.is_empty() {
        file_ref
    } else {
        format!("{file_ref}: {content}")
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

        // Build the issue entry in GFM format. All issues are within the same
        // <td>, not separate rows.
        //
        // NOTE (C27): `header`/`body`/`file` are AI-controlled and interpolated
        // into HTML without escaping. This deliberately matches the Python
        // original (which also emits these verbatim) and relies on GitHub's
        // markdown sanitizer to neutralize any HTML. The `href` uses a link
        // generated from file/line + commit SHA, not raw AI text. Treat these
        // fields as untrusted display content.
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

/// Format the `can_be_split` row — a `List[SubPR]` where each item has a
/// `title` and `relevant_files`. Mirrors Python `process_can_be_split`: render
/// a collapsible `<details>` per sub-PR theme, or "No multiple PR themes" when
/// the list is missing, empty, or has a single theme. Without this, the value
/// was serialized as a raw YAML blob into the table cell.
fn format_can_be_split_row(value: &serde_yaml_ng::Value, out: &mut String) {
    let emoji = section_emoji("Can be split");
    let splits = value.as_sequence();
    let is_no = match splits {
        None => true,
        Some(seq) => seq.len() <= 1,
    };

    out.push_str("<tr><td>");
    if is_no {
        let _ = write!(out, "{emoji} <strong>No multiple PR themes</strong>\n\n");
    } else {
        let _ = write!(
            out,
            "{emoji} <strong>Multiple PR themes</strong><br><br>\n\n"
        );
        for split in splits.into_iter().flatten() {
            let title = split.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let _ = write!(
                out,
                "<details><summary>\nSub-PR theme: <b>{title}</b></summary>\n\n___\n\nRelevant files:\n\n"
            );
            if let Some(files) = split.get("relevant_files").and_then(|v| v.as_sequence()) {
                for file in files {
                    if let Some(f) = file.as_str() {
                        let _ = writeln!(out, "- {f}");
                    }
                }
            }
            out.push_str("___\n\n</details>\n\n");
        }
    }
    out.push_str("</td></tr>\n");
}

/// Normalize a requirement bucket: AI placeholders such as "None.", "No", or
/// "N/A" mean "no items", so collapse them to empty before deriving compliance.
/// Trailing sentence punctuation is stripped first so "None." matches.
fn normalize_requirement_bucket(raw: &str) -> String {
    let stripped = raw.trim().trim_end_matches(['.', '!']).trim();
    if is_value_no(stripped) || stripped.eq_ignore_ascii_case("n/a") {
        String::new()
    } else {
        raw.trim().to_string()
    }
}

/// Format the `ticket_compliance_check` row — a `List[TicketCompliance]`, each
/// item carrying `ticket_url` plus compliant / non-compliant / needs-human-
/// verification requirement bullet lists. Mirrors Python `ticket_markdown_logic`:
/// derive a per-ticket compliance level, an aggregate emoji, and render a
/// readable block per ticket. Without this the nested list was flattened by
/// `yaml_value_to_string` into one unreadable paragraph (raw YAML field names
/// and `|` block-scalar markers included).
fn format_ticket_compliance_row(emoji: &str, value: &serde_yaml_ng::Value, out: &mut String) {
    let Some(tickets) = value.as_sequence() else {
        return;
    };

    let mut compliance_str = String::new();
    let mut levels: Vec<&str> = Vec::new();

    for ticket in tickets {
        let field = |k: &str| {
            ticket
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let ticket_url = field("ticket_url");
        // Requirement buckets: the model often emits a textual placeholder like
        // "None." / "No" instead of leaving the field empty. Normalize those to
        // empty so a fulfilled ticket isn't mislabeled "Partially compliant".
        let fully = normalize_requirement_bucket(&field("fully_compliant_requirements"));
        let not = normalize_requirement_bucket(&field("not_compliant_requirements"));
        let needs = normalize_requirement_bucket(&field("requires_further_human_verification"));

        // A ticket with no compliant/non-compliant items carries no signal.
        if fully.is_empty() && not.is_empty() {
            continue;
        }

        let level = if !fully.is_empty() {
            if !not.is_empty() {
                "Partially compliant"
            } else if needs.is_empty() {
                "Fully compliant"
            } else {
                "PR Code Verified"
            }
        } else {
            "Not compliant"
        };
        levels.push(level);

        let mut explanation = String::new();
        if !fully.is_empty() {
            let _ = write!(explanation, "Compliant requirements:\n\n{fully}\n\n");
        }
        if !not.is_empty() {
            let _ = write!(explanation, "Non-compliant requirements:\n\n{not}\n\n");
        }
        if !needs.is_empty() {
            let _ = write!(
                explanation,
                "Requires further human verification:\n\n{needs}\n\n"
            );
        }

        // Link text is the trailing path segment (the ticket id), as in Python.
        let ticket_id = ticket_url.rsplit('/').next().unwrap_or("");
        let heading = if ticket_url.is_empty() {
            level.to_string()
        } else {
            format!("[{ticket_id}]({ticket_url}) - {level}")
        };
        let _ = write!(compliance_str, "\n\n**{heading}**\n\n{explanation}\n\n");
    }

    // Nothing renderable → skip the whole row (don't emit an empty header).
    if compliance_str.trim().is_empty() {
        return;
    }

    let compliance_emoji = aggregate_compliance_emoji(&levels);
    let _ = write!(
        out,
        "<tr><td>\n\n**{emoji} Ticket compliance analysis {compliance_emoji}**\n\n{compliance_str}</td></tr>\n"
    );
}

/// Derive the overall compliance emoji from the per-ticket levels.
/// Mirrors the aggregation in Python `ticket_markdown_logic`.
fn aggregate_compliance_emoji(levels: &[&str]) -> &'static str {
    if levels.is_empty() {
        return "";
    }
    let all = |target: &str| levels.iter().all(|&l| l == target);
    let any = |target: &str| levels.contains(&target);

    if all("Fully compliant") || all("PR Code Verified") {
        "✅"
    } else if any("Not compliant") {
        if any("Fully compliant") || any("PR Code Verified") {
            "🔶" // mix of compliant and non-compliant
        } else {
            "❌"
        }
    } else if any("Partially compliant") {
        "🔶"
    } else {
        "✅"
    }
}

/// Format the `contribution_time_cost_estimate` row (best/average/worst case).
/// Mirrors Python: render the three cases inline, expanding the `m` suffix to
/// " minutes". Without this, the mapping was serialized as a raw YAML blob.
fn format_contribution_time_cost_row(value: &serde_yaml_ng::Value, out: &mut String) {
    let emoji = section_emoji("Contribution time cost estimate");
    let case = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .replace('m', " minutes")
    };
    let _ = writeln!(
        out,
        "<tr><td>{emoji}&nbsp;<strong>Contribution time estimate</strong> (best, average, worst case): {} | {} | {}</td></tr>",
        case("best_case"),
        case("average_case"),
        case("worst_case")
    );
}

/// Format review using plain markdown (no HTML tables).
// NOTE (C33): this plain (non-GFM) path renders structured fields like
// `key_issues_to_review`/`can_be_split` via `yaml_value_to_string`, which
// serializes a list of objects back to a raw YAML blob. It is only reached for
// providers that report no `gfm_markdown` support — none today (GitHub is the
// only provider and always supports GFM), so this is effectively unreachable.
// If a non-GFM provider is ever added, these fields need dedicated plain-text
// rendering mirroring the GFM helpers above.
fn format_review_plain(review: &serde_yaml_ng::Value, out: &mut String) {
    let Some(mapping) = review.as_mapping() else {
        return;
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
    use serde_yaml_ng::Value;
    match value {
        Value::String(s) => s.trim().to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        Value::Sequence(seq) if seq.is_empty() => String::new(),
        Value::Sequence(seq) => seq
            .iter()
            .map(yaml_value_to_string)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Mapping(_) => serde_yaml_ng::to_string(value)
            .unwrap_or_default()
            .trim()
            .to_string(),
        Value::Tagged(tagged) => yaml_value_to_string(&tagged.value),
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
    fn test_todo_sections_empty_list_shows_no_todos() {
        // An empty List[TodoSection] means "no todos" — must render the same as
        // the "No" sentinel, not an empty <ul> under a TODO header.
        let yaml_str = "review:\n  todo_sections: []\n";
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);
        assert!(result.contains("No TODO sections"), "got: {result}");
        assert!(!result.contains("<ul>"), "no empty TODO list: {result}");
    }

    #[test]
    fn test_todo_sections_list_renders_items() {
        // When require_todo_scan is on, the AI returns a List[TodoSection].
        // It must render as a readable <ul>, not a flattened YAML blob.
        let yaml_str = r#"
review:
  todo_sections:
    - relevant_file: "src/auth.rs"
      line_number: 42
      content: "handle token refresh"
    - relevant_file: "src/db.rs"
      line_number: 7
      content: "add migration"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);

        assert!(result.contains("<strong>TODO sections</strong>"));
        assert!(result.contains("<ul>"));
        assert!(result.contains("src/auth.rs [42]: handle token refresh"));
        assert!(result.contains("src/db.rs [7]: add migration"));
        // Regression guard: no leaked raw YAML field names.
        assert!(!result.contains("relevant_file:"), "leaked field: {result}");
        assert!(!result.contains("line_number:"), "leaked field: {result}");
    }

    #[test]
    fn test_todo_sections_list_caps_at_five() {
        // More than MAX_TODO_ITEMS (5) items are truncated.
        let mut yaml_str = String::from("review:\n  todo_sections:\n");
        for i in 0..8 {
            yaml_str.push_str(&format!(
                "    - relevant_file: \"f{i}.rs\"\n      line_number: {i}\n      content: \"todo {i}\"\n"
            ));
        }
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml_str).unwrap();
        let result = format_review_markdown(&data, true, None);
        assert!(result.contains("todo 4"), "keeps first 5: {result}");
        assert!(!result.contains("todo 5"), "drops the 6th item: {result}");
        assert_eq!(
            result.matches("<li>").count(),
            5,
            "exactly 5 items rendered"
        );
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

    #[test]
    fn test_can_be_split_multiple_themes() {
        let yaml = "\
review:
  can_be_split:
    - title: Refactor auth
      relevant_files:
        - src/auth.rs
        - src/token.rs
    - title: Add tests
      relevant_files:
        - tests/auth.rs
";
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
        let result = format_review_markdown(&data, true, None);

        assert!(result.contains("Multiple PR themes"));
        assert!(result.contains("Sub-PR theme: <b>Refactor auth</b>"));
        assert!(result.contains("<details>"));
        assert!(result.contains("- src/auth.rs"));
        assert!(result.contains("- tests/auth.rs"));
        // Must NOT leak raw YAML mapping syntax into the cell.
        assert!(!result.contains("relevant_files:"));
    }

    #[test]
    fn test_can_be_split_single_theme_is_no() {
        let yaml = "\
review:
  can_be_split:
    - title: Only theme
      relevant_files:
        - src/a.rs
";
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
        let result = format_review_markdown(&data, true, None);
        assert!(result.contains("No multiple PR themes"));
        assert!(!result.contains("<details>"));
    }

    #[test]
    fn test_contribution_time_cost_estimate() {
        let yaml = "\
review:
  contribution_time_cost_estimate:
    best_case: \"45m\"
    average_case: \"5h\"
    worst_case: \"30m\"
";
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
        let result = format_review_markdown(&data, true, None);

        assert!(result.contains("Contribution time estimate"));
        assert!(result.contains("45 minutes"));
        assert!(result.contains("5h"));
        assert!(result.contains("30 minutes"));
        // Must NOT leak the raw YAML keys.
        assert!(!result.contains("best_case:"));
    }

    #[test]
    fn test_ticket_compliance_renders_structured_block() {
        // Reproduces the production scenario where the nested list was flattened
        // into one paragraph (raw `ticket_url: |` keys and `|` markers leaked).
        let yaml = "\
review:
  ticket_compliance_check:
    - ticket_url: |
        https://github.com/acme/repo/issues/2504
      ticket_requirements: |
        * Migrate db.transaction() calls
      fully_compliant_requirements: |
        * Migrated the travel-requests router
        * Migrated fleet-requests REST routes
      not_compliant_requirements: |
        None.
      requires_further_human_verification: |
        * Test file updates are not included in this diff
";
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
        let result = format_review_markdown(&data, true, None);

        // Renders the analysis header and a per-ticket linked heading.
        assert!(
            result.contains("Ticket compliance analysis"),
            "has the analysis header: {result}"
        );
        assert!(
            result.contains("[2504](https://github.com/acme/repo/issues/2504)"),
            "links the ticket by id: {result}"
        );
        // Renders the readable requirement sub-sections.
        assert!(result.contains("Compliant requirements:"));
        assert!(result.contains("Migrated the travel-requests router"));
        assert!(result.contains("Requires further human verification:"));

        // The "None." placeholder must be normalized to empty, so this ticket
        // (fully compliant + needs human verification, no real non-compliant
        // items) classifies as PR Code Verified with a ✅ aggregate — NOT
        // "Partially compliant" with a 🔶.
        assert!(
            result.contains("PR Code Verified"),
            "placeholder 'None.' must not count as non-compliant: {result}"
        );
        assert!(
            result.contains("Ticket compliance analysis ✅"),
            "aggregate emoji should be ✅: {result}"
        );
        assert!(
            !result.contains("Partially compliant"),
            "must not be mislabeled partially compliant: {result}"
        );
        assert!(
            !result.contains("Non-compliant requirements:"),
            "normalized 'None.' must not render a non-compliant section: {result}"
        );

        // CRITICAL regression guard: must NOT leak the raw YAML field names or
        // block-scalar markers that the flattened rendering produced.
        assert!(
            !result.contains("ticket_url:"),
            "raw ticket_url key leaked: {result}"
        );
        assert!(
            !result.contains("fully_compliant_requirements:"),
            "raw field key leaked: {result}"
        );
        assert!(
            !result.contains("ticket_requirements:"),
            "raw field key leaked: {result}"
        );
    }

    #[test]
    fn test_ticket_compliance_aggregate_emoji_levels() {
        // Fully compliant (no non-compliant, no further verification) → ✅
        assert_eq!(aggregate_compliance_emoji(&["Fully compliant"]), "✅");
        // A non-compliant ticket alone → ❌
        assert_eq!(aggregate_compliance_emoji(&["Not compliant"]), "❌");
        // Mix of compliant + non-compliant → partial 🔶
        assert_eq!(
            aggregate_compliance_emoji(&["Fully compliant", "Not compliant"]),
            "🔶"
        );
        // Partially compliant present → 🔶
        assert_eq!(aggregate_compliance_emoji(&["Partially compliant"]), "🔶");
        // PR Code Verified across the board → ✅
        assert_eq!(aggregate_compliance_emoji(&["PR Code Verified"]), "✅");
        // No levels → no emoji.
        assert_eq!(aggregate_compliance_emoji(&[]), "");
    }

    #[test]
    fn test_ticket_compliance_skips_empty_requirements() {
        // A ticket with neither compliant nor non-compliant items carries no
        // signal and must be dropped (no empty header row).
        let yaml = "\
review:
  ticket_compliance_check:
    - ticket_url: |
        https://github.com/acme/repo/issues/9
      ticket_requirements: |
        * Something
      fully_compliant_requirements: |
      not_compliant_requirements: |
";
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
        let result = format_review_markdown(&data, true, None);
        assert!(
            !result.contains("Ticket compliance analysis"),
            "empty ticket should not render a row: {result}"
        );
    }
}
