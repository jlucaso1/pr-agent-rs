use std::fmt::Write;

use super::diff::HunkHeader;

/// Extend a unified diff patch by adding extra context lines from the original file.
///
/// Algorithm:
/// 1. Parse each hunk header to get line ranges
/// 2. Expand the range by `extra_before` lines before and `extra_after` lines after
/// 3. Pull context from the original file content
/// 4. Rebuild the hunk with updated headers
pub fn extend_patch(
    original_file: &str,
    patch: &str,
    extra_before: usize,
    extra_after: usize,
) -> String {
    if patch.is_empty() || original_file.is_empty() {
        return patch.to_string();
    }
    if extra_before == 0 && extra_after == 0 {
        return patch.to_string();
    }

    let original_lines: Vec<&str> = original_file.lines().collect();
    let total_lines = original_lines.len();

    let mut output = String::new();
    let mut current_hunk_lines: Vec<String> = Vec::new();
    let mut current_header: Option<HunkHeader> = None;

    for line in patch.lines() {
        if let Some(new_header) = HunkHeader::parse(line) {
            // Flush previous hunk with extension
            if let Some(ref header) = current_header {
                extend_and_write_hunk(
                    &mut output,
                    header,
                    &current_hunk_lines,
                    &original_lines,
                    total_lines,
                    extra_before,
                    extra_after,
                );
            }
            current_hunk_lines.clear();
            current_header = Some(new_header);
        } else {
            current_hunk_lines.push(line.to_string());
        }
    }

    // Flush final hunk
    if let Some(ref header) = current_header {
        extend_and_write_hunk(
            &mut output,
            header,
            &current_hunk_lines,
            &original_lines,
            total_lines,
            extra_before,
            extra_after,
        );
    }

    output
}

fn extend_and_write_hunk(
    output: &mut String,
    header: &HunkHeader,
    hunk_lines: &[String],
    original_lines: &[&str],
    total_lines: usize,
    extra_before: usize,
    extra_after: usize,
) {
    // Calculate extended range (clamp start to 1 since line numbers are 1-based)
    let ext_start1 = header.start1.saturating_sub(extra_before).max(1);
    let lines_added_before = header.start1.saturating_sub(ext_start1);

    let hunk_end1 = header.start1.saturating_add(header.size1);
    let ext_end1 = hunk_end1.saturating_add(extra_after).min(total_lines + 1);
    let lines_added_after = ext_end1.saturating_sub(hunk_end1);

    let ext_size1 = header.size1 + lines_added_before + lines_added_after;

    // Calculate new-file side extension (clamp start to 1)
    let ext_start2 = header.start2.saturating_sub(extra_before).max(1);
    let ext_size2 = header.size2 + lines_added_before + lines_added_after;

    // Write extended header
    let _ = writeln!(
        output,
        "@@ -{},{} +{},{} @@ {}",
        ext_start1, ext_size1, ext_start2, ext_size2, header.section_header
    );

    // Prepend context lines before
    for i in 0..lines_added_before {
        let idx = ext_start1 - 1 + i; // 0-based index
        if idx < original_lines.len() {
            let _ = writeln!(output, " {}", original_lines[idx]);
        }
    }

    // Write original hunk lines
    for line in hunk_lines {
        output.push_str(line);
        output.push('\n');
    }

    // Append context lines after
    for i in 0..lines_added_after {
        let idx = hunk_end1 - 1 + i; // 0-based index
        if idx < original_lines.len() {
            let _ = writeln!(output, " {}", original_lines[idx]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extend_patch_adds_context() {
        let original = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10";
        let patch = "@@ -4,3 +4,3 @@\n context\n-removed\n+added\n";

        let result = extend_patch(original, patch, 2, 2);
        // Should have extended header
        assert!(result.contains("@@ -2,"));
        // Should have context from original file before/after
        assert!(result.contains("line2") || result.contains("line3"));
    }

    #[test]
    fn test_extend_patch_empty() {
        assert_eq!(extend_patch("file", "", 2, 2), "");
        assert_eq!(extend_patch("", "patch", 2, 2), "patch");
    }

    #[test]
    fn test_extend_patch_no_extra() {
        let patch = "@@ -1,3 +1,3 @@\n context\n";
        assert_eq!(extend_patch("file", patch, 0, 0), patch);
    }
}
