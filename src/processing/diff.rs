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
    let mut new_content = Vec::new();
    let mut old_content = Vec::new();
    let mut has_plus = false;
    let mut has_minus = false;
    let mut line_number: usize = 0;

    for line in patch.lines() {
        if let Some(header) = HunkHeader::parse(line) {
            // Flush previous hunk
            flush_hunk(&mut output, &new_content, &old_content, has_plus, has_minus);
            new_content.clear();
            old_content.clear();
            has_plus = false;
            has_minus = false;
            line_number = header.start2;
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
            // Context line — goes to both
            new_content.push(format!("{line_number} {line}\n"));
            old_content.push(format!("{line}\n"));
            line_number += 1;
        }
    }

    // Flush final hunk
    flush_hunk(&mut output, &new_content, &old_content, has_plus, has_minus);

    output
}

/// Write the hunk content to output with `__new hunk__` / `__old hunk__` markers.
fn flush_hunk(
    output: &mut String,
    new_content: &[String],
    old_content: &[String],
    has_plus: bool,
    has_minus: bool,
) {
    if new_content.is_empty() && old_content.is_empty() {
        return;
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
}
