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
    // Strip markdown fences and whitespace — trim once, reuse the slice
    let trimmed = response_text.trim_matches('\n');
    let stripped = trimmed
        .strip_prefix("yaml")
        .or_else(|| trimmed.strip_prefix("```yaml"))
        .unwrap_or(trimmed)
        .trim();
    let cleaned = stripped.strip_suffix("```").unwrap_or(stripped).trim();

    // Direct parse attempt — zero allocations on the happy path
    if let Ok(data) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(cleaned)
        && !data.is_null()
    {
        return Some(data);
    }

    tracing::debug!("initial YAML parse failed, trying fallbacks");

    // Build combined key list
    let mut keys: Vec<&str> = DEFAULT_KEYS_YAML.to_vec();
    keys.extend_from_slice(extra_keys);

    // Run through fallback cascade (pass original text for fallback 2's code-block extraction)
    try_fix_yaml(cleaned, &keys, first_key, last_key, response_text)
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

    // ── Fallback 9: Fix orphan continuation lines ──
    // When the AI returns a long value that wraps to the next line at column 0,
    // it breaks YAML. This indents those orphan lines to make them valid
    // plain-scalar continuations of the previous line's value.
    if let Some(data) = fallback_fix_orphan_continuation_lines(text) {
        tracing::info!("YAML parsed after fixing orphan continuation lines");
        return Some(data);
    }

    // ── Fallback 10: Quote keys containing brackets ──
    // Keys like `estimated_effort_to_review_[1-5]` can confuse some YAML parsers
    // because `[1-5]` looks like a flow sequence. Quote them to be safe.
    if text.contains('[')
        && let Some(data) = fallback_quote_bracket_keys(text)
    {
        tracing::info!("YAML parsed after quoting bracket-containing keys");
        return Some(data);
    }

    let preview = if text.len() > 500 {
        format!("{}...(truncated {} chars)", &text[..500], text.len() - 500)
    } else {
        text.to_string()
    };
    tracing::error!(response = %preview, "all YAML fallbacks exhausted");
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
///
/// Single pass: scans each line for a matching key and splices in the block
/// scalar marker without collecting lines into a `Vec<String>`.
fn fallback_add_block_scalar(text: &str, keys: &[&str]) -> Option<serde_yaml_ng::Value> {
    let mut result = String::with_capacity(text.len() + keys.len() * 16);
    let mut changed = false;
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        // Find the first matching key in this line (skip if line already has '|')
        if !line.contains('|')
            && let Some((key, pos)) = keys.iter().find_map(|k| line.find(k).map(|p| (*k, p)))
        {
            result.push_str(&line[..pos + key.len()]);
            result.push_str(" |\n        ");
            result.push_str(line[pos + key.len()..].trim_start());
            changed = true;
            continue;
        }
        result.push_str(line);
    }
    if changed { try_parse(&result) } else { None }
}

/// Fallback 1.5: Replace `|\n` with `|2\n` for proper indent handling.
fn fallback_pipe_to_pipe2(text: &str) -> Option<serde_yaml_ng::Value> {
    let replaced = text.replace("|\n", "|2\n");
    if let Some(data) = try_parse(&replaced) {
        return Some(data);
    }

    // Nested fix: add indent for lines with '}' at indent level 2
    let mut result = String::with_capacity(replaced.len() + 64);
    let mut changed = false;
    for (i, line) in replaced.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent == 2 && !line.contains("|2") && line.contains('}') {
            result.push_str("    ");
            result.push_str(trimmed);
            changed = true;
        } else {
            result.push_str(line);
        }
    }
    if changed { try_parse(&result) } else { None }
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

/// Fallback 9: Fix orphan continuation lines.
///
/// When the AI returns a long value that wraps to the next line at column 0 without
/// indentation, the YAML parser fails. This detects such "orphan" lines (at indent 0,
/// not YAML keys, not list items) and indents them to be valid plain-scalar continuations
/// of the previous line's value.
///
/// Single O(n) pass — tracks the previous non-empty line's indent incrementally
/// instead of scanning backwards.
fn fallback_fix_orphan_continuation_lines(text: &str) -> Option<serde_yaml_ng::Value> {
    use std::fmt::Write;
    let mut result = String::with_capacity(text.len() + 128);
    let mut changed = false;
    let mut prev_nonempty_indent: usize = 0;

    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        let trimmed = line.trim_start();
        let line_indent = line.len() - trimmed.len();

        // Detect orphan lines: at indent 0, not empty, not a YAML key or list item
        if i > 0
            && !trimmed.is_empty()
            && line_indent == 0
            && prev_nonempty_indent >= 2
            && !YAML_KEY_RE.is_match(trimmed)
            && !trimmed.starts_with("- ")
            && !trimmed.starts_with("---")
            && !trimmed.starts_with("...")
            && !trimmed.starts_with('#')
        {
            // Indent as a continuation: deeper than the mapping key's indent
            let _ = write!(
                result,
                "{:width$}{trimmed}",
                "",
                width = prev_nonempty_indent + 2
            );
            changed = true;
            // Don't update prev_nonempty_indent — the orphan's logical indent
            // is the one we just assigned, but for consecutive orphans we still
            // want to use the original anchor indent.
            continue;
        }

        result.push_str(line);

        // Track the indent of the last non-empty line
        if !trimmed.is_empty() {
            prev_nonempty_indent = line_indent;
        }
    }

    if changed { try_parse(&result) } else { None }
}

/// Regex for bracket-containing YAML keys: captures leading indent + key with brackets + colon.
static BRACKET_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*)([\w]+(?:_\[[^\]]*\])+[\w]*)(\s*:.*)$").unwrap());

/// Fallback 10: Quote YAML keys that contain square brackets.
fn fallback_quote_bracket_keys(text: &str) -> Option<serde_yaml_ng::Value> {
    use std::fmt::Write;
    let mut result = String::with_capacity(text.len() + 32);
    let mut changed = false;
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        if let Some(caps) = BRACKET_KEY_RE.captures(line) {
            let indent = caps.get(1).map_or("", |m| m.as_str());
            let key = caps.get(2).map_or("", |m| m.as_str());
            let rest = caps.get(3).map_or("", |m| m.as_str());
            let _ = write!(result, r#"{indent}"{key}"{rest}"#);
            changed = true;
        } else {
            result.push_str(line);
        }
    }
    if changed { try_parse(&result) } else { None }
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
    fn test_load_yaml_review_long_issue_content() {
        // Reproduces production failure: AI returns a long issue_content that wraps across
        // lines without block scalar indicator or indentation.
        let yaml = r#"review:
  estimated_effort_to_review_[1-5]: 2
  score: 90
  relevant_tests: yes
  key_issues_to_review:
    - relevant_file: apps/web/src/app/(app)/subscription/page.tsx
      issue_header: Undefined variable 'isLoading'
      issue_content: The variable `isLoading` is used in disabled attributes on the coupon input (line 912), remove button (line 887), and validate button (line 919) but is not defined in the component. This will cause a ReferenceError at runtime.
It likely should be replaced with the correct loading state variable from the hooks used in the component.
  security_concerns: No"#;
        let data = load_yaml(
            yaml,
            &[
                "estimated_effort_to_review_[1-5]:",
                "security_concerns:",
                "key_issues_to_review:",
                "relevant_file:",
                "issue_header:",
                "issue_content:",
            ],
            "review",
            "security_concerns",
        );
        assert!(
            data.is_some(),
            "should parse review YAML with long issue_content"
        );
        let data = data.unwrap();
        let review = &data["review"];
        assert!(review["key_issues_to_review"].is_sequence());
    }

    #[test]
    fn test_load_yaml_bracket_key_quoting() {
        // Key with brackets that might confuse some YAML parsers
        let yaml = "data:\n  estimated_effort_to_review_[1-5]: 3\n  score: 90";
        let data = load_yaml_simple(yaml);
        assert!(data.is_some(), "bracket key should parse");
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
