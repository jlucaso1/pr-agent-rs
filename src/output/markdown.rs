use std::fmt::Write;

/// Create a collapsible `<details>` section (GitHub Flavored Markdown).
pub fn collapsible_section(summary: &str, body: &str) -> String {
    format!("<details><summary>{summary}</summary>\n\n{body}\n\n</details>\n")
}

/// Wrap text in bold (GitHub HTML style).
#[allow(dead_code)]
pub fn bold(text: &str) -> String {
    format!("<strong>{text}</strong>")
}

/// Emphasize the header portion of a "Header: content" string.
///
/// Everything before the first `: ` is wrapped in bold.
#[allow(dead_code)]
pub fn emphasize_header(text: &str, only_markdown: bool, reference_link: Option<&str>) -> String {
    if let Some(colon_pos) = text.find(": ") {
        let header = &text[..colon_pos + 1]; // includes the colon
        let rest = &text[colon_pos + 1..];
        match (only_markdown, reference_link) {
            (true, Some(link)) => format!("[**{header}**]({link})\n{rest}"),
            (true, None) => format!("**{header}**\n{rest}"),
            (false, Some(link)) => {
                format!("<strong><a href='{link}'>{header}</a></strong><br>{rest}")
            }
            (false, None) => format!("<strong>{header}</strong><br>{rest}"),
        }
    } else {
        text.to_string()
    }
}

/// Build a Markdown table from headers and rows.
#[allow(dead_code)]
pub fn markdown_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = String::new();

    // Header row
    let _ = writeln!(out, "| {} |", headers.join(" | "));

    // Separator
    out.push_str("| ");
    for (i, _) in headers.iter().enumerate() {
        if i > 0 {
            out.push_str(" | ");
        }
        out.push_str("---");
    }
    let _ = writeln!(out, " |");

    // Data rows
    for row in rows {
        let _ = writeln!(out, "| {} |", row.join(" | "));
    }

    out
}

/// Format a list of items as a Markdown bulleted list.
#[allow(dead_code)]
pub fn bullet_list(items: &[String]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(out, "- {item}");
    }
    out
}

/// Build an HTML bulleted list (`<ul>/<li>`).
#[allow(dead_code)]
pub fn html_bullet_list(items: &[String]) -> String {
    let mut out = String::from("<ul>\n");
    for item in items {
        let _ = writeln!(out, "<li>{item}</li>");
    }
    out.push_str("</ul>\n");
    out
}

/// Effort-to-review emoji bar (1–5 scale).
///
/// Maps effort score to emoji indicators.
pub fn effort_bar(effort: u8) -> &'static str {
    match effort.min(5) {
        1 => "1️⃣",
        2 => "2️⃣",
        3 => "3️⃣",
        4 => "4️⃣",
        5 => "5️⃣",
        _ => "🔢",
    }
}

/// Emoji map for review section headers.
///
/// Accepts BOTH the display label ("Score") and the canonical snake_case YAML
/// key ("score"), so call sites passing either form get the right emoji (the
/// generic GFM arm and the plain-text path pass the raw key).
pub fn section_emoji(section: &str) -> &'static str {
    match section {
        "Can be split" | "can_be_split" => "\u{1F500}", // 🔀
        "Key issues to review" | "key_issues_to_review" => "\u{26A1}", // ⚡
        "Recommended focus areas for review" => "\u{26A1}", // ⚡
        "Score" | "score" => "\u{1F3C5}",               // 🏅
        "Relevant tests" | "relevant_tests" => "\u{1F9EA}", // 🧪
        "Focused PR" => "\u{2728}",                     // ✨
        "Relevant ticket" => "\u{1F3AB}",               // 🎫
        "Security concerns" | "security_concerns" => "\u{1F512}", // 🔒
        "Todo sections" | "todo_sections" => "\u{1F4DD}", // 📝
        "Insights from user's answers" | "insights_from_user_answers" => "\u{1F4DD}", // 📝
        "Code feedback" => "\u{1F916}",                 // 🤖
        "Estimated effort to review [1-5]"
        | "estimated_effort_to_review_[1-5]"
        | "estimated_effort_to_review" => "\u{23F1}\u{FE0F}", // ⏱️
        "Contribution time cost estimate" | "contribution_time_cost_estimate" => "\u{23F3}", // ⏳
        "Ticket compliance check" | "ticket_compliance_check" => "\u{1F3AB}", // 🎫
        _ => "",
    }
}

/// Wrap a code snippet in a fenced code block.
#[allow(dead_code)]
pub fn code_block(code: &str, language: &str) -> String {
    format!("```{language}\n{code}\n```")
}

/// Create a persistent comment marker (hidden HTML comment) for finding/updating.
pub fn persistent_comment_marker(tool_name: &str) -> String {
    format!("<!-- pr-agent:{tool_name} -->")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collapsible_section() {
        let result = collapsible_section("Click me", "Hidden content");
        assert!(result.contains("<details>"));
        assert!(result.contains("<summary>Click me</summary>"));
        assert!(result.contains("Hidden content"));
        assert!(result.contains("</details>"));
    }

    #[test]
    fn test_emphasize_header_html() {
        let result = emphasize_header("Score: 85/100", false, None);
        assert_eq!(result, "<strong>Score:</strong><br> 85/100");
    }

    #[test]
    fn test_emphasize_header_markdown() {
        let result = emphasize_header("Score: 85/100", true, None);
        assert_eq!(result, "**Score:**\n 85/100");
    }

    #[test]
    fn test_emphasize_header_with_link() {
        let result = emphasize_header("File: main.rs", false, Some("https://example.com"));
        assert!(result.contains("<a href='https://example.com'>File:</a>"));
    }

    #[test]
    fn test_emphasize_header_no_colon() {
        let result = emphasize_header("No colon here", false, None);
        assert_eq!(result, "No colon here");
    }

    #[test]
    fn test_markdown_table() {
        let headers = &["Name", "Value"];
        let rows = &[
            vec!["key1".into(), "val1".into()],
            vec!["key2".into(), "val2".into()],
        ];
        let result = markdown_table(headers, rows);
        assert!(result.contains("| Name | Value |"));
        assert!(result.contains("| --- | --- |"));
        assert!(result.contains("| key1 | val1 |"));
    }

    #[test]
    fn test_effort_bar() {
        assert_eq!(effort_bar(1), "1️⃣");
        assert_eq!(effort_bar(3), "3️⃣");
        assert_eq!(effort_bar(5), "5️⃣");
        assert_eq!(effort_bar(10), "5️⃣"); // clamped
    }

    #[test]
    fn test_section_emoji() {
        assert_eq!(section_emoji("Security concerns"), "🔒");
        assert_eq!(section_emoji("Score"), "🏅");
        // Canonical snake_case keys map to the same emoji (S3).
        assert_eq!(section_emoji("security_concerns"), "🔒");
        assert_eq!(section_emoji("score"), "🏅");
        assert_eq!(section_emoji("can_be_split"), "🔀");
        assert_eq!(section_emoji("Unknown"), "");
    }

    #[test]
    fn test_persistent_comment_marker() {
        let marker = persistent_comment_marker("review");
        assert_eq!(marker, "<!-- pr-agent:review -->");
    }
}
