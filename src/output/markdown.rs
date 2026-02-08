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
    let _ = writeln!(
        out,
        "| {} |",
        headers
            .iter()
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    );

    // Data rows
    for row in rows {
        let _ = writeln!(out, "| {} |", row.join(" | "));
    }

    out
}

/// Format a list of items as a Markdown bulleted list.
#[allow(dead_code)]
pub fn bullet_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
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
pub fn section_emoji(section: &str) -> &'static str {
    match section {
        "Can be split" => "\u{1F500}",                            // 🔀
        "Key issues to review" => "\u{26A1}",                     // ⚡
        "Recommended focus areas for review" => "\u{26A1}",       // ⚡
        "Score" => "\u{1F3C5}",                                   // 🏅
        "Relevant tests" => "\u{1F9EA}",                          // 🧪
        "Focused PR" => "\u{2728}",                               // ✨
        "Relevant ticket" => "\u{1F3AB}",                         // 🎫
        "Security concerns" => "\u{1F512}",                       // 🔒
        "Todo sections" => "\u{1F4DD}",                           // 📝
        "Insights from user's answers" => "\u{1F4DD}",            // 📝
        "Code feedback" => "\u{1F916}",                           // 🤖
        "Estimated effort to review [1-5]" => "\u{23F1}\u{FE0F}", // ⏱️
        "Contribution time cost estimate" => "\u{23F3}",          // ⏳
        "Ticket compliance check" => "\u{1F3AB}",                 // 🎫
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
        assert_eq!(section_emoji("Unknown"), "");
    }

    #[test]
    fn test_persistent_comment_marker() {
        let marker = persistent_comment_marker("review");
        assert_eq!(marker, "<!-- pr-agent:review -->");
    }
}
