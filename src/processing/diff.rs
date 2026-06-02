use regex::Regex;
use std::sync::LazyLock;

/// Regex for parsing unified diff hunk headers.
/// Matches: `@@ -start1[,size1] +start2[,size2] @@ [section_header]`
static HUNK_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@[ ]?(.*)").unwrap());

/// Parsed hunk header values.
#[derive(Debug, Clone)]
pub struct HunkHeader {
    pub start1: usize,
    pub size1: usize,
    pub start2: usize,
    pub size2: usize,
    pub section_header: String,
}

impl HunkHeader {
    pub fn parse(line: &str) -> Option<Self> {
        let caps = HUNK_HEADER_RE.captures(line)?;
        Some(Self {
            start1: caps[1].parse().unwrap_or(0),
            size1: caps.get(2).map_or(1, |m| m.as_str().parse().unwrap_or(1)),
            start2: caps[3].parse().unwrap_or(0),
            size2: caps.get(4).map_or(1, |m| m.as_str().parse().unwrap_or(1)),
            section_header: caps.get(5).map_or("", |m| m.as_str()).to_string(),
        })
    }
}

/// Convert a unified diff patch into the pr-agent format with
/// `## File:`, `__new hunk__`/`__old hunk__` markers and line numbers.
///
/// Accepts raw parts to avoid requiring a full `FilePatchInfo` (which would
/// force callers to clone the filename into a temporary struct).
pub fn convert_to_hunks_with_line_numbers(
    filename: &str,
    patch: &str,
    edit_type: crate::git::types::EditType,
) -> String {
    if patch.is_empty() {
        if edit_type == crate::git::types::EditType::Deleted {
            return format!("## File '{}' was deleted\n", filename.trim());
        }
        return format!("## File: '{}'\n\n(empty patch)\n", filename.trim());
    }

    let mut output = format!("## File: '{}'\n", filename.trim());
    let lines: Vec<&str> = patch.lines().collect();
    let mut new_content = Vec::new();
    let mut old_content = Vec::new();
    let mut has_plus = false;
    let mut has_minus = false;
    let mut line_number: usize = 0;
    // Raw `@@ ... @@` header of the hunk currently being accumulated. Emitted
    // before the hunk's markers so the LLM keeps the section/function context.
    let mut current_header: Option<String> = None;

    for (i, &line) in lines.iter().enumerate() {
        // Skip the git "no newline" marker (`\ No newline at end of file`) —
        // it's not real content and would inflate line numbers. Match the
        // backslash prefix, not the phrase anywhere (a real changed line may
        // contain that text as content).
        if line.starts_with('\\') && line.to_lowercase().contains("no newline at end of file") {
            continue;
        }

        if let Some(header) = HunkHeader::parse(line) {
            // Flush previous hunk (with its own header)
            flush_hunk(
                &mut output,
                current_header.as_deref(),
                &new_content,
                &old_content,
                has_plus,
                has_minus,
            );
            new_content.clear();
            old_content.clear();
            has_plus = false;
            has_minus = false;
            line_number = header.start2;
            current_header = Some(line.to_string());
            continue;
        }

        if line.starts_with('+') {
            has_plus = true;
            new_content.push(format!("{line_number} {line}\n"));
            line_number += 1;
        } else if line.starts_with('-') {
            has_minus = true;
            old_content.push(format!("{line}\n"));
        } else {
            // Context line. Skip a blank line that directly precedes a hunk
            // header or ends the patch — otherwise it inflates line numbers.
            if line.is_empty() && i > 0 {
                let next_is_header = lines.get(i + 1).is_some_and(|n| n.starts_with("@@"));
                let is_last = i + 1 == lines.len();
                if next_is_header || is_last {
                    continue;
                }
            }
            // Context line — goes to both
            new_content.push(format!("{line_number} {line}\n"));
            old_content.push(format!("{line}\n"));
            line_number += 1;
        }
    }

    // Flush final hunk
    flush_hunk(
        &mut output,
        current_header.as_deref(),
        &new_content,
        &old_content,
        has_plus,
        has_minus,
    );

    output
}

/// Write the hunk content to output: the raw `@@` header (when present),
/// then `__new hunk__` / `__old hunk__` markers with their lines.
fn flush_hunk(
    output: &mut String,
    header: Option<&str>,
    new_content: &[String],
    old_content: &[String],
    has_plus: bool,
    has_minus: bool,
) {
    if new_content.is_empty() && old_content.is_empty() {
        return;
    }

    if let Some(h) = header {
        output.push('\n');
        output.push_str(h);
    }

    if has_plus || !has_minus {
        output.push_str("\n__new hunk__\n");
        for line in new_content {
            output.push_str(line);
        }
    }

    if has_minus {
        output.push_str("\n__old hunk__\n");
        for line in old_content {
            output.push_str(line);
        }
    }
}

/// Format a file patch as a simple diff block without line numbers.
/// Used when `add_line_numbers_to_hunks` is false.
pub fn format_patch_simple(
    filename: &str,
    patch: &str,
    edit_type: crate::git::types::EditType,
) -> String {
    if edit_type == crate::git::types::EditType::Deleted {
        return format!("## File '{}' was deleted\n", filename.trim());
    }
    format!("\n\n## File: '{}'\n\n{}\n", filename.trim(), patch.trim())
}

/// Extract hunk lines from a diff patch for the /ask_line tool.
///
/// Given a raw diff hunk (typically from `body["comment"]["diff_hunk"]`),
/// returns a tuple of (full_hunk_formatted, selected_lines).
///
/// - `full_hunk_formatted`: The entire hunk with `## File:` header and line numbers
/// - `selected_lines`: Only the lines within `[line_start, line_end]` range
///
/// `side` is `"LEFT"` for removed lines or `"RIGHT"` (default) for added/context lines.
pub fn extract_hunk_lines_from_patch(
    patch: &str,
    filename: &str,
    line_start: usize,
    line_end: usize,
    side: &str,
) -> (String, String) {
    if patch.is_empty() {
        return (String::new(), String::new());
    }

    let use_left = side.eq_ignore_ascii_case("LEFT");

    let mut full_hunk = format!("## File: '{}'\n\n", filename.trim());
    let mut selected = String::new();
    let mut skip_hunk = false;
    // Current line number on each side of the diff. The old side advances on
    // context + removed lines; the new side on context + added lines.
    let mut old_line: usize = 0;
    let mut new_line: usize = 0;

    for line in patch.lines() {
        // The git "no newline" marker is the backslash-prefixed
        // `\ No newline at end of file` — match the prefix, not the phrase
        // anywhere (a real changed line may contain that text as content).
        if line.starts_with('\\') && line.to_lowercase().contains("no newline at end of file") {
            continue;
        }

        if let Some(header) = HunkHeader::parse(line) {
            skip_hunk = false;
            old_line = header.start1;
            new_line = header.start2;

            // Only include the hunk that contains the requested start line.
            let in_range = if use_left {
                header.start1 <= line_start && line_start <= header.start1 + header.size1
            } else {
                header.start2 <= line_start && line_start <= header.start2 + header.size2
            };
            if !in_range {
                skip_hunk = true;
                continue;
            }

            full_hunk.push_str(line);
            full_hunk.push('\n');
        } else if !skip_hunk {
            // Lines are emitted RAW (with their -/+/space prefix), NOT prefixed
            // with line numbers — the pr_line_questions template tells the model
            // to read those change markers.
            //
            // Select by the side's CURRENT line number, with BOTH bounds. Only
            // lines that exist on the requested side are eligible: the LEFT
            // (old) side ignores added (`+`) lines and the RIGHT (new) side
            // ignores removed (`-`) lines — otherwise an opposite-side line,
            // which shares the current line number, would be wrongly selected.
            // (The Python original is buggy here: its LEFT lower bound is a
            // tautology and it tracks the new-side count for the old side.)
            let (line_no, on_side) = if use_left {
                (old_line, !line.starts_with('+'))
            } else {
                (new_line, !line.starts_with('-'))
            };
            if on_side && line_start <= line_no && line_no <= line_end {
                selected.push_str(line);
                selected.push('\n');
            }
            full_hunk.push_str(line);
            full_hunk.push('\n');

            // Advance the side(s) this line exists on.
            if line.starts_with('-') {
                old_line += 1;
            } else if line.starts_with('+') {
                new_line += 1;
            } else {
                old_line += 1;
                new_line += 1;
            }
        }
    }

    // rstrip (mirrors Python remove_trailing_chars).
    (
        full_hunk.trim_end().to_string(),
        selected.trim_end().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::EditType;

    #[test]
    fn test_hunk_header_parse() {
        let h = HunkHeader::parse("@@ -10,5 +20,7 @@ fn main()").unwrap();
        assert_eq!(h.start1, 10);
        assert_eq!(h.size1, 5);
        assert_eq!(h.start2, 20);
        assert_eq!(h.size2, 7);
        assert_eq!(h.section_header, "fn main()");
    }

    #[test]
    fn test_convert_simple_patch() {
        let patch = "@@ -1,3 +1,4 @@\n context\n-removed\n+added\n+new line\n context2";
        let result = convert_to_hunks_with_line_numbers("src/main.rs", patch, EditType::Modified);

        assert!(result.contains("## File: 'src/main.rs'"));
        assert!(result.contains("__new hunk__"));
        assert!(result.contains("__old hunk__"));
        assert!(result.contains("1 ")); // line numbers
    }

    #[test]
    fn test_deleted_file() {
        let result = convert_to_hunks_with_line_numbers("src/main.rs", "", EditType::Deleted);
        assert!(result.contains("was deleted"));
    }

    #[test]
    fn test_convert_emits_hunk_header() {
        // C8: the @@ header (with section context) must be emitted before the hunk.
        let patch = "@@ -10,3 +10,4 @@ fn main()\n context\n-removed\n+added\n+new line";
        let result = convert_to_hunks_with_line_numbers("src/main.rs", patch, EditType::Modified);
        assert!(
            result.contains("@@ -10,3 +10,4 @@ fn main()"),
            "should emit the hunk header with section context: {result}"
        );
    }

    #[test]
    fn test_convert_keeps_content_with_no_newline_phrase() {
        // A real changed line that merely contains the phrase must be kept;
        // only the backslash-prefixed git marker is skipped.
        let patch =
            "@@ -1,1 +1,2 @@\n a\n+// no newline at end of file\n\\ No newline at end of file";
        let result = convert_to_hunks_with_line_numbers("f.rs", patch, EditType::Modified);
        assert!(
            result.contains("+// no newline at end of file"),
            "content line must be kept: {result}"
        );
        assert!(
            !result.contains("\\ No newline"),
            "the git marker should still be skipped: {result}"
        );
    }

    #[test]
    fn test_extract_hunk_lines_left_skips_added_lines() {
        // LEFT side: an added line shares the current old line number but does
        // not exist on the old side, so it must NOT be selected for an old-line
        // query (it would inject unrelated new-side content).
        let patch = "@@ -10,2 +10,3 @@\n context10\n+added\n-removed11";
        let (_full, selected) = extract_hunk_lines_from_patch(patch, "f.rs", 11, 11, "LEFT");
        assert!(
            selected.contains("-removed11"),
            "the removed old line must be selected: {selected:?}"
        );
        assert!(
            !selected.contains("+added"),
            "an added (new-side) line must NOT be selected on LEFT: {selected:?}"
        );
    }

    #[test]
    fn test_convert_skips_no_newline_marker() {
        // C10: the "\ No newline at end of file" marker is not content.
        let patch = "@@ -1,2 +1,2 @@\n context\n-old\n+new\n\\ No newline at end of file";
        let result = convert_to_hunks_with_line_numbers("f.rs", patch, EditType::Modified);
        assert!(
            !result.to_lowercase().contains("no newline"),
            "no-newline marker should be skipped: {result}"
        );
    }

    #[test]
    fn test_convert_skips_trailing_blank_context_line() {
        // C10: a blank context line ending the patch must not add a numbered
        // empty line (which would misalign the line numbers the LLM references).
        let patch = "@@ -1,1 +1,2 @@\n a\n+b\n\n";
        let result = convert_to_hunks_with_line_numbers("f.rs", patch, EditType::Modified);
        assert!(
            !result.contains("3 \n"),
            "trailing blank line should be skipped: {result:?}"
        );
    }

    #[test]
    fn test_extract_hunk_lines_right_side() {
        let patch = "@@ -10,4 +10,5 @@ fn example()\n context1\n-old_line\n+new_line\n+added_line\n context2";
        let (full, selected) = extract_hunk_lines_from_patch(patch, "src/lib.rs", 11, 12, "RIGHT");

        assert!(full.contains("## File: 'src/lib.rs'"));
        assert!(full.contains("@@ -10,4 +10,5 @@"));
        assert!(!selected.is_empty());
        // Lines 11-12 on RIGHT side = new_line (11) and added_line (12)
        assert!(selected.contains("+new_line"));
        assert!(selected.contains("+added_line"));
    }

    #[test]
    fn test_extract_hunk_lines_left_side() {
        let patch = "@@ -10,3 +10,3 @@\n context\n-removed\n+added\n context2";
        let (full, selected) = extract_hunk_lines_from_patch(patch, "src/lib.rs", 11, 11, "LEFT");

        assert!(full.contains("## File: 'src/lib.rs'"));
        // Line 11 on LEFT side = the removed line
        assert!(selected.contains("-removed"));
    }

    #[test]
    fn test_extract_hunk_lines_empty_patch() {
        let (full, selected) = extract_hunk_lines_from_patch("", "f.rs", 1, 1, "RIGHT");
        assert!(full.is_empty());
        assert!(selected.is_empty());
    }

    #[test]
    fn test_extract_hunk_lines_out_of_range() {
        let patch = "@@ -1,2 +1,2 @@\n context\n-old\n+new";
        let (full, selected) = extract_hunk_lines_from_patch(patch, "f.rs", 100, 200, "RIGHT");

        // Full hunk still has the file header (the out-of-range hunk is skipped).
        assert!(!full.is_empty());
        // But selected lines should be empty (out of range)
        assert!(selected.is_empty());
    }

    #[test]
    fn test_extract_hunk_lines_no_number_prefix() {
        // C9: lines are emitted raw (with -/+/space), NOT prefixed with line
        // numbers — matching what the pr_line_questions template expects.
        let patch = "@@ -10,2 +10,2 @@\n context\n-old\n+new";
        let (full, _selected) = extract_hunk_lines_from_patch(patch, "f.rs", 10, 11, "RIGHT");
        assert!(
            full.contains("\n+new"),
            "added line should be raw: {full:?}"
        );
        assert!(
            full.contains("\n context"),
            "context should be raw: {full:?}"
        );
        assert!(
            !full.contains("11 +new"),
            "must not prefix line numbers: {full:?}"
        );
    }

    #[test]
    fn test_extract_hunk_lines_left_respects_lower_bound() {
        // LEFT side: asking about old line 12 (a removed line mid-hunk) must
        // select ONLY that line — not the context/lines from the hunk start.
        // The previous behavior (a faithful port of the buggy Python) lacked a
        // lower bound on the LEFT side and selected everything up to line_end.
        let patch = "@@ -10,4 +10,3 @@\n context10\n-removed11\n-removed12\n context13";
        let (_full, selected) = extract_hunk_lines_from_patch(patch, "f.rs", 12, 12, "LEFT");
        assert!(
            selected.contains("-removed12"),
            "the queried old line must be selected: {selected:?}"
        );
        assert!(
            !selected.contains("context10"),
            "lines before the range must NOT be selected: {selected:?}"
        );
        assert!(
            !selected.contains("removed11"),
            "old line 11 is out of range: {selected:?}"
        );
        assert!(
            !selected.contains("context13"),
            "old line 13 is out of range: {selected:?}"
        );
    }

    #[test]
    fn test_extract_hunk_lines_skips_out_of_range_hunk() {
        // C9: only the hunk containing the requested line is included.
        let patch = "@@ -1,1 +1,1 @@\n a\n@@ -50,1 +50,2 @@\n b\n+c";
        let (full, _) = extract_hunk_lines_from_patch(patch, "f.rs", 50, 51, "RIGHT");
        assert!(
            full.contains("@@ -50,1 +50,2 @@"),
            "in-range hunk included: {full:?}"
        );
        assert!(
            !full.contains("@@ -1,1 +1,1 @@"),
            "out-of-range hunk skipped: {full:?}"
        );
    }
}
