use regex::Regex;
use std::sync::LazyLock;

/// Regex to extract ```yaml ... ``` code blocks.
static YAML_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"```yaml([\s\S]*?)```(?:\s*$|")"#).unwrap());

/// Default YAML keys that may need fixup for multiline values.
const DEFAULT_KEYS_YAML: &[&str] = &[
    "relevant line:",
    "suggestion content:",
    "relevant file:",
    "existing code:",
    "improved code:",
    "label:",
    "why:",
    "suggestion_summary:",
];

/// Parse YAML from an AI model response, applying progressive fixups if needed.
///
/// Applies a 9-level fallback cascade to handle common AI formatting
/// issues. Returns `None` only if all fallbacks fail.
pub fn load_yaml(
    response_text: &str,
    extra_keys: &[&str],
    first_key: &str,
    last_key: &str,
) -> Option<serde_yaml_ng::Value> {
    let original = response_text.to_string();

    // Strip markdown fences and whitespace
    let cleaned = response_text
        .trim_matches('\n')
        .strip_prefix("yaml")
        .unwrap_or(response_text.trim_matches('\n'))
        .strip_prefix("```yaml")
        .unwrap_or(response_text.trim_matches('\n'))
        .trim()
        .strip_suffix("```")
        .unwrap_or(response_text.trim())
        .to_string();

    // Direct parse attempt
    if let Ok(data) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&cleaned)
        && !data.is_null()
    {
        return Some(data);
    }

    tracing::debug!("initial YAML parse failed, trying fallbacks");

    // Build combined key list
    let mut keys: Vec<&str> = DEFAULT_KEYS_YAML.to_vec();
    keys.extend_from_slice(extra_keys);

    // Run through fallback cascade
    try_fix_yaml(&cleaned, &keys, first_key, last_key, &original)
}

/// Convenience wrapper with no extra keys or key boundaries.
#[allow(dead_code)]
pub fn load_yaml_simple(response_text: &str) -> Option<serde_yaml_ng::Value> {
    load_yaml(response_text, &[], "", "")
}

/// Extract an i64 from a YAML value, trying numeric first then string parse.
pub fn yaml_value_as_i64(value: &serde_yaml_ng::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Extract a u64 from a YAML value, trying numeric first then string parse.
pub fn yaml_value_as_u64(value: &serde_yaml_ng::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 9-level fallback cascade to handle common AI YAML formatting issues.
fn try_fix_yaml(
    text: &str,
    keys: &[&str],
    first_key: &str,
    last_key: &str,
    original: &str,
) -> Option<serde_yaml_ng::Value> {
    // ── Fallback 1: Add literal block scalar (|-) for known keys ──
    if let Some(data) = fallback_add_block_scalar(text, keys) {
        tracing::info!("YAML parsed after adding |- block scalars");
        return Some(data);
    }

    // ── Fallback 1.5: Replace | with |2 (indent indicator) ──
    if let Some(data) = fallback_pipe_to_pipe2(text) {
        tracing::info!("YAML parsed after replacing | with |2");
        return Some(data);
    }

    // ── Fallback 2: Extract ```yaml...``` code block ──
    if let Some(data) = fallback_extract_yaml_block(text, original) {
        tracing::info!("YAML parsed after extracting yaml code block");
        return Some(data);
    }

    // ── Fallback 3: Remove curly brackets ──
    if let Some(data) = fallback_remove_curly_brackets(text) {
        tracing::info!("YAML parsed after removing curly brackets");
        return Some(data);
    }

    // ── Fallback 4: Extract by first_key / last_key boundaries ──
    if !first_key.is_empty()
        && !last_key.is_empty()
        && let Some(data) = fallback_extract_by_keys(text, first_key, last_key)
    {
        tracing::info!("YAML parsed after extracting by key boundaries");
        return Some(data);
    }

    // ── Fallback 5: Remove leading '+' characters ──
    if let Some(data) = fallback_remove_leading_plus(text) {
        tracing::info!("YAML parsed after removing leading '+' characters");
        return Some(data);
    }

    // ── Fallback 6: Replace tabs with spaces ──
    if text.contains('\t')
        && let Some(data) = fallback_replace_tabs(text)
    {
        tracing::info!("YAML parsed after replacing tabs with spaces");
        return Some(data);
    }

    // ── Fallback 7: Fix code block indentation ──
    if let Some(data) = fallback_fix_code_indent(text, keys) {
        tracing::info!("YAML parsed after fixing code block indentation");
        return Some(data);
    }

    // ── Fallback 8: Remove pipe characters from start ──
    if let Some(data) = fallback_remove_leading_pipe(text) {
        tracing::info!("YAML parsed after removing leading pipe chars");
        return Some(data);
    }

    tracing::error!("all YAML fallbacks exhausted");
    None
}

/// Try to parse, returning Some if successful and non-null.
fn try_parse(text: &str) -> Option<serde_yaml_ng::Value> {
    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) {
        Ok(val) if !val.is_null() => Some(val),
        _ => None,
    }
}

// ── Fallback implementations ────────────────────────────────────────

/// Fallback 1: For each known key, add `|-\n        ` block scalar indicator.
fn fallback_add_block_scalar(text: &str, keys: &[&str]) -> Option<serde_yaml_ng::Value> {
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    for line in &mut lines {
        for key in keys {
            if line.contains(key) && !line.contains('|') {
                *line = line.replace(key, &format!("{key} |\n        "));
            }
        }
    }
    try_parse(&lines.join("\n"))
}

/// Fallback 1.5: Replace `|\n` with `|2\n` for proper indent handling.
fn fallback_pipe_to_pipe2(text: &str) -> Option<serde_yaml_ng::Value> {
    let replaced = text.replace("|\n", "|2\n");
    if let Some(data) = try_parse(&replaced) {
        return Some(data);
    }

    // Nested fix: add indent for lines with '}' at indent level 2
    let mut lines: Vec<String> = replaced.lines().map(|l| l.to_string()).collect();
    for line in &mut lines {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent == 2 && !line.contains("|2") && line.contains('}') {
            *line = format!("    {}", trimmed);
        }
    }
    try_parse(&lines.join("\n"))
}

/// Fallback 2: Extract YAML from ```yaml ... ``` code blocks.
fn fallback_extract_yaml_block(text: &str, original: &str) -> Option<serde_yaml_ng::Value> {
    // Try on modified text first, then original
    for source in [text, original] {
        if let Some(caps) = YAML_BLOCK_RE.captures(source) {
            let inner = caps.get(1).map_or("", |m| m.as_str());
            let cleaned = inner.trim();
            if let Some(data) = try_parse(cleaned) {
                return Some(data);
            }
        }
    }
    None
}

/// Fallback 3: Remove surrounding curly brackets.
fn fallback_remove_curly_brackets(text: &str) -> Option<serde_yaml_ng::Value> {
    let stripped = text
        .trim()
        .strip_prefix('{')
        .unwrap_or(text.trim())
        .strip_suffix('}')
        .unwrap_or(text.trim())
        .trim_end_matches(":\n")
        .trim();
    try_parse(stripped)
}

/// Fallback 4: Extract YAML between first_key and last_key boundaries.
fn fallback_extract_by_keys(
    text: &str,
    first_key: &str,
    last_key: &str,
) -> Option<serde_yaml_ng::Value> {
    let first_pattern = format!("\n{first_key}:");
    let index_start = text
        .find(&first_pattern)
        .or_else(|| text.find(&format!("{first_key}:")))?;

    let last_pattern = format!("{last_key}:");
    let index_last = text.rfind(&last_pattern)?;

    let index_end = text[index_last..]
        .find("\n\n")
        .map_or(text.len(), |i| index_last + i);

    let slice = &text[index_start..index_end];
    let cleaned = slice
        .trim()
        .strip_prefix("```yaml")
        .unwrap_or(slice.trim())
        .strip_suffix("```")
        .unwrap_or(slice.trim())
        .trim();

    try_parse(cleaned)
}

/// Fallback 5: Replace leading '+' with space (AI sometimes adds diff markers).
fn fallback_remove_leading_plus(text: &str) -> Option<serde_yaml_ng::Value> {
    use std::fmt::Write;
    let mut fixed = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            fixed.push('\n');
        }
        if let Some(rest) = line.strip_prefix('+') {
            let _ = write!(fixed, " {rest}");
        } else {
            fixed.push_str(line);
        }
    }
    try_parse(&fixed)
}

/// Fallback 6: Replace tabs with 4 spaces.
fn fallback_replace_tabs(text: &str) -> Option<serde_yaml_ng::Value> {
    try_parse(&text.replace('\t', "    "))
}

/// Regex to detect a YAML key pattern (word chars followed by colon).
static YAML_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*\s*:").unwrap());

/// Fallback 7: Fix unindented block scalar content.
///
/// When the AI returns `key: |\ncontent` without indenting the content,
/// YAML parsing fails. This adds indentation to content lines following
/// `key: |` until the next YAML key at the same or lower indentation level.
fn fallback_fix_code_indent(text: &str, _keys: &[&str]) -> Option<serde_yaml_ng::Value> {
    use std::fmt::Write;
    let mut result = String::with_capacity(text.len());
    let mut in_block_scalar = false;
    let mut key_indent: usize = 0;

    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        let trimmed = line.trim_end();
        let line_indent = line.len() - line.trim_start().len();
        let trimmed_start = trimmed.trim_start();

        if in_block_scalar {
            // Check if this line ends the block scalar:
            // non-empty, at indent <= key_indent, looks like a YAML key or list item
            let is_yaml_key = !trimmed_start.is_empty()
                && line_indent <= key_indent
                && (YAML_KEY_RE.is_match(trimmed_start) || trimmed_start.starts_with("- "));

            if is_yaml_key {
                in_block_scalar = false;
                result.push_str(line);
            } else {
                // Indent content so it's deeper than the key
                let _ = write!(result, "{:width$}{line}", "", width = key_indent + 2);
            }
        } else {
            result.push_str(line);
        }

        // Check if this line starts a block scalar (ends with `: |` or `: |-`)
        if !in_block_scalar && (trimmed.ends_with(": |") || trimmed.ends_with(": |-")) {
            in_block_scalar = true;
            key_indent = line_indent;
        }
    }

    try_parse(&result)
}

/// Fallback 8: Remove leading pipe characters.
fn fallback_remove_leading_pipe(text: &str) -> Option<serde_yaml_ng::Value> {
    let stripped = text.trim_start_matches(['|', '\n']);
    try_parse(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_yaml_direct() {
        let yaml = "key: value\nlist:\n  - item1\n  - item2";
        let data = load_yaml_simple(yaml).unwrap();
        assert_eq!(data["key"].as_str().unwrap(), "value");
    }

    #[test]
    fn test_load_yaml_with_markdown_fences() {
        let yaml = "```yaml\nkey: value\n```";
        let data = load_yaml_simple(yaml).unwrap();
        assert_eq!(data["key"].as_str().unwrap(), "value");
    }

    #[test]
    fn test_load_yaml_with_tabs() {
        let yaml = "key:\n\t- item1\n\t- item2";
        let data = load_yaml_simple(yaml).unwrap();
        assert!(data["key"].is_sequence());
    }

    #[test]
    fn test_load_yaml_with_leading_plus() {
        // Leading + on each line (common AI artifact)
        let yaml = "items:\n+  - first\n+  - second";
        let data = load_yaml_simple(yaml).unwrap();
        assert!(data["items"].is_sequence());
    }

    #[test]
    fn test_load_yaml_with_curly_brackets() {
        let yaml = "{key: value, other: data}";
        let data = load_yaml_simple(yaml).unwrap();
        assert_eq!(data["key"].as_str().unwrap(), "value");
    }

    #[test]
    fn test_load_yaml_extract_by_keys() {
        let text = "Some preamble\n\nfirst_key: hello\nsecond_key: world\n\nsome epilogue";
        let data = load_yaml(text, &[], "first_key", "second_key").unwrap();
        assert_eq!(data["first_key"].as_str().unwrap(), "hello");
        assert_eq!(data["second_key"].as_str().unwrap(), "world");
    }

    #[test]
    fn test_load_yaml_empty_returns_none() {
        assert!(load_yaml_simple("").is_none());
    }

    #[test]
    fn test_load_yaml_garbage_returns_none() {
        assert!(load_yaml_simple("{{{{not yaml at all!!!!").is_none());
    }

    #[test]
    fn test_fallback_pipe_to_pipe2() {
        // YAML with | block scalar
        let yaml = "code: |\n  line1\n  line2";
        let data = load_yaml_simple(yaml).unwrap();
        assert!(data["code"].as_str().is_some());
    }

    #[test]
    fn test_load_yaml_unindented_block_scalar() {
        // AI sometimes returns block scalar content without indentation
        let yaml = r#"type: Enhancement
description: |
Fix the login bug
Added error handling
title: |
Fix authentication
pr_files:
- filename: src/auth.rs
  label: bug fix"#;
        let data = load_yaml(yaml, &[], "type", "pr_files").unwrap();
        assert_eq!(data["type"].as_str().unwrap(), "Enhancement");
        assert!(data["description"].as_str().unwrap().contains("login bug"));
        assert!(data["title"].as_str().unwrap().contains("authentication"));
        assert!(data["pr_files"].is_sequence());
    }

    #[test]
    fn test_load_yaml_nested_code_fences_in_block_scalar() {
        // AI returns mermaid diagram inside a block scalar with code fences
        let yaml = "```yaml\ntype: Enhancement\ndescription: |\nSome changes\nchanges_diagram: |\n```mermaid\ngraph TD\n  A --> B\n```\npr_files:\n- filename: foo.rs\n  label: fix\n```";
        let data = load_yaml(yaml, &[], "type", "pr_files").unwrap();
        assert_eq!(data["type"].as_str().unwrap(), "Enhancement");
        assert!(
            data["changes_diagram"]
                .as_str()
                .unwrap()
                .contains("mermaid")
        );
        assert!(data["pr_files"].is_sequence());
    }
}
