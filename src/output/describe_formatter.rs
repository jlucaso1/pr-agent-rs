use std::collections::HashMap;
use std::fmt::Write;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

use crate::config::types::{BoolOrString, PrDescriptionConfig};
use crate::output::markdown::persistent_comment_marker;

/// Formatted describe result ready for publishing.
pub struct DescribeOutput {
    /// AI-generated or original PR title.
    pub title: String,
    /// Formatted PR body.
    pub body: String,
    /// Labels to apply (e.g. "Bug fix", "Enhancement").
    pub labels: Vec<String>,
}

/// Per-file diff statistics and link for the file walkthrough.
pub struct FileStats {
    pub num_plus_lines: i32,
    pub num_minus_lines: i32,
    /// Link to the file in the PR diff page.
    pub link: String,
}

/// Convert parsed describe YAML into a formatted PR title + body.
///
/// Builds the PR body with type, description, diagram, file table, and labels sections.
pub fn format_describe_output(
    data: &serde_yaml_ng::Value,
    original_title: &str,
    original_body: &str,
    config: &PrDescriptionConfig,
    file_stats: &HashMap<String, FileStats>,
) -> DescribeOutput {
    let generate_ai_title = config.generate_ai_title;
    let add_original_description = config.add_original_user_description;
    let enable_semantic_files_types = config.enable_semantic_files_types;
    let marker = persistent_comment_marker("describe");

    // Extract top-level fields
    let ai_title = data
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or(original_title);

    let title = if generate_ai_title {
        ai_title.trim().to_string()
    } else {
        original_title.trim().to_string()
    };

    let pr_type = data
        .get("type")
        .map(|v| {
            // AI may return type as a string or as a list of strings
            if let Some(s) = v.as_str() {
                s.trim().to_string()
            } else if let Some(seq) = v.as_sequence() {
                seq.iter()
                    .filter_map(|item| item.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                String::new()
            }
        })
        .unwrap_or_default();

    let description = data
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Build body
    // The user's original description MUST come BEFORE the marker so that
    // `strip_pr_agent_content()` can recover it on subsequent runs.
    // (It returns everything before `<!-- pr-agent:` .)
    let mut body = String::with_capacity(4_000);

    if add_original_description && !original_body.is_empty() {
        let _ = writeln!(body, "{original_body}");
        let _ = writeln!(body, "\n---\n");
    }

    let _ = writeln!(body, "{marker}");

    if config.enable_pr_type {
        let _ = writeln!(body, "### **PR Type**");
        if !pr_type.is_empty() {
            let _ = writeln!(body, "{pr_type}\n");
        }
    }

    let _ = writeln!(body, "\n___\n");

    let _ = writeln!(body, "### **Description**");
    if !description.is_empty() {
        // Format description as bullet points if it isn't already
        for line in description.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                body.push('\n');
            } else if trimmed.starts_with('-') || trimmed.starts_with('*') {
                let _ = writeln!(body, "{trimmed}");
            } else {
                let _ = writeln!(body, "- {trimmed}");
            }
        }
        body.push('\n');
    }

    let _ = writeln!(body, "\n___\n");

    // Diagram
    if let Some(diagram) = data.get("changes_diagram") {
        let diagram_str = diagram.as_str().unwrap_or("").trim();
        if !diagram_str.is_empty() {
            let _ = writeln!(body, "### Diagram Walkthrough\n");
            // Sanitize mermaid content: quote text with special chars like (){}
            let sanitized = sanitize_mermaid(diagram_str);
            // Preserve existing fences from AI, only add closing if missing.
            if sanitized.starts_with("```") {
                let mut d = sanitized;
                if !d.ends_with("```") {
                    d.push_str("\n```");
                }
                let _ = writeln!(body, "{d}\n");
            } else {
                let _ = writeln!(body, "```mermaid\n{sanitized}\n```\n");
            }
        }
    }

    // Changes walkthrough / PR files
    if enable_semantic_files_types && let Some(files) = data.get("pr_files") {
        let mut walkthrough = String::new();
        format_pr_files(
            files,
            &mut walkthrough,
            &config.collapsible_file_list,
            config.collapsible_file_list_threshold,
            file_stats,
        );
        if !walkthrough.is_empty() {
            let _ = writeln!(
                body,
                "<details> <summary><h3> File Walkthrough</h3></summary>\n"
            );
            body.push_str(&walkthrough);
            let _ = writeln!(body, "\n</details>\n");
        }
    }

    // Labels
    let labels = extract_labels(data, &pr_type);

    DescribeOutput {
        title,
        body,
        labels,
    }
}

/// Format the PR files section as a nested HTML table grouped by label.
///
/// The `collapsible` config controls the **per-category** `<details>` nesting
/// (whether each label group is collapsible). The outer `<details>` wrapping
/// is handled by the caller (`format_describe_output`).
fn format_pr_files(
    files: &serde_yaml_ng::Value,
    out: &mut String,
    collapsible: &BoolOrString,
    threshold: u32,
    file_stats: &HashMap<String, FileStats>,
) {
    let file_list = match files.as_sequence() {
        Some(seq) => seq,
        None => return,
    };

    if file_list.is_empty() {
        return;
    }

    // Group files by label (preserves insertion order)
    let mut label_groups: IndexMap<String, Vec<FileEntry>> = IndexMap::new();
    for file in file_list {
        let entry = FileEntry::from_yaml(file);
        if entry.filename.is_empty() {
            continue;
        }
        label_groups
            .entry(entry.label.clone())
            .or_default()
            .push(entry);
    }

    if label_groups.is_empty() {
        return;
    }

    let num_files: usize = label_groups.iter().map(|(_, files)| files.len()).sum();
    let use_collapsible = match collapsible {
        BoolOrString::Bool(b) => *b,
        BoolOrString::Str(s) if s == "adaptive" => num_files as u32 > threshold,
        BoolOrString::Str(_) => true,
    };

    // Build HTML table with label groups
    out.push_str("<table>");
    out.push_str(r#"<thead><tr><th></th><th align="left">Relevant files</th></tr></thead>"#);
    out.push_str("<tbody>");

    for (label, files) in &label_groups {
        let cap_label = capitalize_first(label);
        let _ = write!(out, r#"<tr><td><strong>{cap_label}</strong></td>"#);

        if use_collapsible {
            let _ = write!(
                out,
                r#"<td><details><summary>{} files</summary><table>"#,
                files.len()
            );
        } else {
            out.push_str("<td><table>");
        }

        for entry in files {
            write_file_row(out, entry, file_stats);
        }

        if use_collapsible {
            out.push_str("</table></details></td></tr>");
        } else {
            out.push_str("</table></td></tr>");
        }
    }

    out.push_str("</tr></tbody></table>");
}

/// A single file entry parsed from the AI YAML.
struct FileEntry {
    filename: String,
    changes_title: String,
    changes_summary: String,
    label: String,
}

impl FileEntry {
    fn from_yaml(item: &serde_yaml_ng::Value) -> Self {
        let filename = item
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .replace('\'', "`");
        let changes_title = item
            .get("changes_title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let changes_summary = item
            .get("changes_summary")
            .or_else(|| item.get("changes_content"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let label = item
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();
        Self {
            filename,
            changes_title,
            changes_summary,
            label,
        }
    }

    fn short_name(&self) -> &str {
        self.filename
            .rsplit_once('/')
            .map_or(self.filename.as_str(), |(_, name)| name)
    }
}

/// Write a single file `<tr>` row to the output.
///
/// Writes a single file entry as an HTML table row with optional diff stats link.
fn write_file_row(out: &mut String, entry: &FileEntry, file_stats: &HashMap<String, FileStats>) {
    let short_name = entry.short_name();

    // Build filename_publish with title
    let filename_publish = if !entry.changes_title.is_empty() && entry.changes_title != "..." {
        format!(
            "<strong>{}</strong><dd><code>{}</code></dd>",
            short_name, entry.changes_title
        )
    } else {
        format!("<strong>{short_name}</strong>")
    };

    // Look up diff stats (case-insensitive, strip leading '/')
    let lookup_key = entry.filename.trim_start_matches('/').to_lowercase();
    let (diff_pm, delta_nbsp, link) = if let Some(stats) = file_stats.get(&lookup_key) {
        let mut pm = format!("+{}/-{}", stats.num_plus_lines, stats.num_minus_lines);
        if pm.len() > 12 || pm == "+0/-0" {
            pm = "[link]".to_string();
        }
        let nbsp_count = 8usize.saturating_sub(pm.len());
        let delta = "&nbsp; ".repeat(nbsp_count);
        (pm, delta, stats.link.as_str())
    } else {
        (String::new(), String::new(), "")
    };

    // Build the link cell
    let link_cell = if !link.is_empty() && !diff_pm.is_empty() {
        format!(r#"<a href="{link}">{diff_pm}</a>{delta_nbsp}"#)
    } else {
        String::new()
    };

    if entry.changes_summary.is_empty() {
        // No summary: simple row without description
        let _ = write!(
            out,
            "\n<tr>\n  <td>{filename_publish}</td>\n  <td>{link_cell}</td>\n\n</tr>\n"
        );
    } else {
        // With summary: collapsible details per file
        let desc_br = insert_br_after_x_chars(&entry.changes_summary, 70);
        let _ = write!(
            out,
            "\n<tr>\n  <td>\n    <details>\n      \
             <summary>{filename_publish}</summary>\n<hr>\n\n{}\n\n{desc_br}\n\n\n\
             </details>\n\n\n  </td>\n  <td>{link_cell}</td>\n\n</tr>\n",
            entry.filename
        );
    }
}

/// Insert `<br>` breaks into text to keep visual line length manageable.
///
/// Inserts `<br>` at word boundaries to limit visual line length.
fn insert_br_after_x_chars(text: &str, max_chars: usize) -> String {
    let text = text.replace('\n', "<br>");
    if text.len() <= max_chars {
        return text;
    }
    let mut result = String::new();
    let mut line_len = 0;
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            if line_len + word.len() + 1 > max_chars {
                result.push_str("<br>");
                line_len = 0;
            } else {
                result.push(' ');
                line_len += 1;
            }
        }
        result.push_str(word);
        line_len += word.len();
    }
    result
}

/// Capitalize the first letter of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Extract label strings from the YAML data.
fn extract_labels(data: &serde_yaml_ng::Value, pr_type: &str) -> Vec<String> {
    // From explicit "labels" field
    if let Some(seq) = data.get("labels").and_then(|v| v.as_sequence()) {
        let labels: Vec<String> = seq
            .iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect();
        if !labels.is_empty() {
            return labels;
        }
    }

    // Fallback: split comma-separated pr_type
    pr_type
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Regex matching mermaid edge labels: `|text|` (between arrows and nodes).
/// Captures: (1) = text inside the pipes.
static MERMAID_EDGE_LABEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\|([^"|][^|]*)\|"#).unwrap());

/// Regex matching mermaid node text: `ID[text]` (square brackets after node ID).
/// Captures: (1) = node ID, (2) = text inside the brackets.
static MERMAID_NODE_TEXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(\w+)\[([^"\]]*[(){}][^\]]*)\]"#).unwrap());

/// Characters inside mermaid text that trigger shape parsing and need quoting.
const MERMAID_SPECIAL: &[char] = &['(', ')', '{', '}'];

/// Sanitize mermaid diagram content by quoting text that contains special characters.
///
/// Mermaid interprets `(`, `)`, `{`, `}` as shape delimiters. When AI-generated
/// edge labels or node text contain these (e.g. `.min(1)`), the diagram fails to
/// render. This wraps such text in double quotes, which tells mermaid to treat it
/// as literal text.
fn sanitize_mermaid(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 32);
    for line in text.lines() {
        if !result.is_empty() {
            result.push('\n');
        }
        let mut fixed = line.to_string();
        // 1. Quote edge labels containing special chars: |text| → |"text"|
        if fixed.contains('|') {
            fixed = MERMAID_EDGE_LABEL_RE
                .replace_all(&fixed, |caps: &regex::Captures| {
                    let label = &caps[1];
                    if label.contains(MERMAID_SPECIAL) {
                        format!("|\"{}\"| ", label.trim())
                    } else {
                        caps[0].to_string()
                    }
                })
                .into_owned();
        }
        // 2. Quote node text containing special chars: ID[text()] → ID["text()"]
        fixed = MERMAID_NODE_TEXT_RE
            .replace_all(&fixed, |caps: &regex::Captures| {
                format!("{}[\"{}\" ]", &caps[1], caps[2].trim())
            })
            .into_owned();
        result.push_str(&fixed);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(
        generate_ai_title: bool,
        add_original_description: bool,
        enable_semantic_files_types: bool,
    ) -> PrDescriptionConfig {
        PrDescriptionConfig {
            generate_ai_title,
            add_original_user_description: add_original_description,
            enable_semantic_files_types,
            ..PrDescriptionConfig::default()
        }
    }

    fn empty_stats() -> HashMap<String, FileStats> {
        HashMap::new()
    }

    #[test]
    fn test_format_describe_basic() {
        let yaml_str = r#"
title: "Fix authentication bug in login flow"
type: "Bug fix"
description: |
  Fixed the authentication bug where users could not log in
  Added proper error handling for expired tokens
pr_files:
  - filename: "src/auth.rs"
    changes_title: "Fix token validation"
    changes_summary: "Added expiry check"
    label: "bug fix"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = test_config(true, false, true);
        let result = format_describe_output(&data, "Original title", "", &config, &empty_stats());

        assert_eq!(result.title, "Fix authentication bug in login flow");
        assert!(result.body.contains("Bug fix"));
        assert!(result.body.contains("authentication bug"));
        assert!(result.body.contains("auth.rs"));
        assert!(result.body.contains("<!-- pr-agent:describe -->"));
        assert_eq!(result.labels, vec!["Bug fix"]);
    }

    #[test]
    fn test_format_describe_keep_original_title() {
        let yaml_str = r#"
title: "AI title"
type: "Enhancement"
description: "Some changes"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = test_config(false, false, false);
        let result =
            format_describe_output(&data, "User's original title", "", &config, &empty_stats());

        assert_eq!(result.title, "User's original title");
    }

    #[test]
    fn test_extract_labels() {
        let yaml_str = r#"
labels:
  - "Bug fix"
  - "Tests"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let labels = extract_labels(&data, "");
        assert_eq!(labels, vec!["Bug fix", "Tests"]);
    }

    #[test]
    fn test_extract_labels_from_type() {
        let data = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
        let labels = extract_labels(&data, "Bug fix, Enhancement");
        assert_eq!(labels, vec!["Bug fix", "Enhancement"]);
    }

    #[test]
    fn test_mermaid_diagram_already_fenced() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Test"
changes_diagram: |
  ```mermaid
  graph TD
    A --> B
  ```
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = test_config(false, false, false);
        let result = format_describe_output(&data, "Test", "", &config, &empty_stats());
        // Should NOT have double fences
        assert!(!result.body.contains("```mermaid\n```mermaid"));
        assert!(result.body.contains("```mermaid"));
        assert!(result.body.contains("graph TD"));
    }

    #[test]
    fn test_mermaid_diagram_not_fenced() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Test"
changes_diagram: |
  graph TD
    A --> B
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = test_config(false, false, false);
        let result = format_describe_output(&data, "Test", "", &config, &empty_stats());
        // Should wrap in mermaid fences
        assert!(result.body.contains("```mermaid\ngraph TD"));
    }

    #[test]
    fn test_enable_pr_type_false_hides_section() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Some changes"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = PrDescriptionConfig {
            enable_pr_type: false,
            ..PrDescriptionConfig::default()
        };
        let result = format_describe_output(&data, "Test", "", &config, &empty_stats());
        assert!(!result.body.contains("### **PR Type**"));
    }

    #[test]
    fn test_collapsible_file_list_adaptive_below_threshold() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Test"
pr_files:
  - filename: "src/a.rs"
    changes_title: "Change A"
    label: "fix"
  - filename: "src/b.rs"
    changes_title: "Change B"
    label: "fix"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = PrDescriptionConfig {
            enable_semantic_files_types: true,
            collapsible_file_list: BoolOrString::Str("adaptive".into()),
            collapsible_file_list_threshold: 6,
            ..PrDescriptionConfig::default()
        };
        let result = format_describe_output(&data, "Test", "", &config, &empty_stats());
        // 2 files < threshold 6 → per-category should NOT be collapsible
        // But outer <details> for File Walkthrough is always present
        assert!(result.body.contains("File Walkthrough"));
        assert!(result.body.contains("<strong>Fix</strong>"));
        // Per-category should NOT have <details><summary>N files
        assert!(!result.body.contains("2 files</summary>"));
    }

    #[test]
    fn test_collapsible_file_list_always_true() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Test"
pr_files:
  - filename: "src/a.rs"
    changes_title: "Change A"
    label: "fix"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = PrDescriptionConfig {
            enable_semantic_files_types: true,
            collapsible_file_list: BoolOrString::Bool(true),
            ..PrDescriptionConfig::default()
        };
        let result = format_describe_output(&data, "Test", "", &config, &empty_stats());
        // Per-category should be collapsible
        assert!(result.body.contains("1 files</summary>"));
    }

    #[test]
    fn test_section_separators() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Some changes"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = test_config(false, false, false);
        let result = format_describe_output(&data, "Test", "", &config, &empty_stats());
        assert!(
            result.body.contains("___"),
            "body must contain ___ separators"
        );
    }

    #[test]
    fn test_diagram_header() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Test"
changes_diagram: |
  graph TD
    A --> B
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = test_config(false, false, false);
        let result = format_describe_output(&data, "Test", "", &config, &empty_stats());
        assert!(result.body.contains("### Diagram Walkthrough"));
        assert!(!result.body.contains("### **Changes Diagram**"));
    }

    #[test]
    fn test_grouped_html_table() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Test"
pr_files:
  - filename: "src/auth.rs"
    changes_title: "Fix auth"
    changes_summary: "Fixed authentication"
    label: "bug fix"
  - filename: "src/db.rs"
    changes_title: "Add migration"
    label: "database"
  - filename: "src/api.rs"
    changes_title: "Update endpoint"
    changes_summary: "Changed API response format"
    label: "bug fix"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = PrDescriptionConfig {
            enable_semantic_files_types: true,
            collapsible_file_list: BoolOrString::Bool(true),
            ..PrDescriptionConfig::default()
        };
        let result = format_describe_output(&data, "Test", "", &config, &empty_stats());

        // Should have HTML table structure
        assert!(result.body.contains("<table>"));
        assert!(result.body.contains("<thead>"));
        assert!(result.body.contains("Relevant files"));

        // Should have grouped labels
        assert!(result.body.contains("<strong>Bug fix</strong>"));
        assert!(result.body.contains("<strong>Database</strong>"));

        // Should have per-category collapsible with file counts
        assert!(result.body.contains("2 files</summary>"));
        assert!(result.body.contains("1 files</summary>"));

        // Should have file names in bold
        assert!(result.body.contains("<strong>auth.rs</strong>"));
        assert!(result.body.contains("<strong>db.rs</strong>"));

        // Should have change titles in code tags
        assert!(result.body.contains("<code>Fix auth</code>"));
        assert!(result.body.contains("<code>Add migration</code>"));

        // File with summary should have <details> expandable
        assert!(result.body.contains("Fixed authentication"));
    }

    #[test]
    fn test_file_links_with_stats() {
        let yaml_str = r#"
title: "Test"
type: "Enhancement"
description: "Test"
pr_files:
  - filename: "src/main.rs"
    changes_title: "Main changes"
    label: "enhancement"
"#;
        let data: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml_str).unwrap();
        let config = PrDescriptionConfig {
            enable_semantic_files_types: true,
            ..PrDescriptionConfig::default()
        };

        let mut stats = HashMap::new();
        stats.insert(
            "src/main.rs".to_string(),
            FileStats {
                num_plus_lines: 10,
                num_minus_lines: 5,
                link: "https://github.com/owner/repo/pull/1/files#diff-abc123".to_string(),
            },
        );

        let result = format_describe_output(&data, "Test", "", &config, &stats);
        assert!(result.body.contains("+10/-5"));
        assert!(
            result
                .body
                .contains(r#"<a href="https://github.com/owner/repo/pull/1/files#diff-abc123">"#)
        );
    }

    // ── Mermaid sanitization tests ──────────────────────────────────

    #[test]
    fn test_sanitize_mermaid_edge_label_with_parens() {
        let input = r#"flowchart LR
  G[Schemas] -->|Add .min(1)| H[Prevent errors]"#;
        let result = sanitize_mermaid(input);
        assert!(
            result.contains(r#"|"Add .min(1)"| "#),
            "edge label with parens should be quoted: {result}"
        );
    }

    #[test]
    fn test_sanitize_mermaid_node_text_with_parens() {
        let input = "  A[fn(x)] --> B[result]";
        let result = sanitize_mermaid(input);
        assert!(
            result.contains(r#"A["fn(x)" ]"#),
            "node text with parens should be quoted: {result}"
        );
        // B[result] has no special chars — should NOT be quoted
        assert!(
            result.contains("B[result]"),
            "node text without special chars should be unchanged: {result}"
        );
    }

    #[test]
    fn test_sanitize_mermaid_no_special_chars_unchanged() {
        let input = "flowchart LR\n  A[Start] -->|Do work| B[End]";
        let result = sanitize_mermaid(input);
        assert_eq!(result, input, "no special chars → no changes");
    }

    #[test]
    fn test_sanitize_mermaid_already_quoted_unchanged() {
        // Already double-quoted text should not be re-quoted
        let input = r#"A -->|"already quoted(1)"| B"#;
        let result = sanitize_mermaid(input);
        assert_eq!(result, input, "already-quoted labels should not be changed");
    }

    #[test]
    fn test_sanitize_mermaid_curly_braces_in_edge_label() {
        let input = "A -->|{key: value}| B";
        let result = sanitize_mermaid(input);
        assert!(
            result.contains(r#"|"{key: value}"| "#),
            "edge label with curly braces should be quoted: {result}"
        );
    }

    #[test]
    fn test_sanitize_mermaid_production_failure() {
        // Exact reproduction of the production failure from logs
        let input = r#"flowchart LR
  A[Shared compressPDF] -->|Validation added| B[Prevents corrupted PDFs]
  C[Macer POST/PUT routes] -->|Use uploadFileToR2| D[Consistent file handling]
  E[Transaction callbacks] -->|Fix db→trx| F[Proper isolation]
  G[Payment request schemas] -->|Add .min(1)| H[Prevent empty array errors]
  B --> I[All apps protected]
  D --> I
  F --> I
  H --> I"#;
        let result = sanitize_mermaid(input);
        // The problematic line should now have quoted label
        assert!(
            result.contains(r#"|"Add .min(1)"| "#),
            "production failure line should have quoted edge label: {result}"
        );
        // Lines without special chars should be untouched
        assert!(result.contains("-->|Validation added|"));
        assert!(result.contains("-->|Use uploadFileToR2|"));
    }
}
