use super::diff::HunkHeader;
use crate::config::loader::get_settings;

/// Extend a unified diff patch by adding extra context lines around each hunk.
///
/// With `allow_dynamic_context` (default on), the "before" context is grown up
/// to `max_extra_lines_before_dynamic_context` lines searching for the enclosing
/// function/class line (the hunk's section header), so the model sees the start
/// of the surrounding scope instead of a fixed window. Falls back to a fixed
/// `extra_before`/`extra_after` window otherwise. Mirrors the Python
/// `process_patch_lines`.
///
/// `original_file` is the base-branch content, `new_file` the head-branch
/// content (needed to validate that the extra context is identical on both
/// sides). Pass an empty `new_file` to disable dynamic context.
pub fn extend_patch(
    original_file: &str,
    patch: &str,
    new_file: &str,
    extra_before: usize,
    extra_after: usize,
) -> String {
    if patch.is_empty() || original_file.is_empty() {
        return patch.to_string();
    }
    if extra_before == 0 && extra_after == 0 {
        return patch.to_string();
    }

    let settings = get_settings();
    let allow_dynamic = settings.config.allow_dynamic_context;
    let dynamic_before = settings.config.max_extra_lines_before_dynamic_context as i64;

    process_patch_lines(
        patch,
        original_file,
        new_file,
        extra_before as i64,
        extra_after as i64,
        allow_dynamic,
        dynamic_before,
    )
}

/// Clamp a 0-based `[start, end)` line range to the slice bounds (Python-style
/// slicing never panics; Rust slicing does).
fn safe_slice<'a>(lines: &'a [&'a str], start: i64, end: i64) -> &'a [&'a str] {
    let len = lines.len() as i64;
    let s = start.clamp(0, len);
    let e = end.clamp(s, len);
    &lines[s as usize..e as usize]
}

/// Validate that the first context line of a hunk matches the original file at
/// the hunk's start. A mismatch means extending the hunk backwards could
/// produce an invalid patch, so extension is skipped for that hunk.
fn check_if_hunk_lines_matches_to_file(
    i: usize,
    original_lines: &[&str],
    patch_lines: &[&str],
    start1: i64,
) -> bool {
    if let Some(next) = patch_lines.get(i + 1)
        && next.starts_with(' ')
        && start1 >= 1
        && let Some(orig) = original_lines.get((start1 - 1) as usize)
        && next.trim() != orig.trim()
    {
        return false;
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn process_patch_lines(
    patch_str: &str,
    original_file_str: &str,
    new_file_str: &str,
    extra_before: i64,
    extra_after: i64,
    allow_dynamic: bool,
    dynamic_before: i64,
) -> String {
    let file_original_lines: Vec<&str> = original_file_str.lines().collect();
    let file_new_lines: Vec<&str> = if new_file_str.is_empty() {
        Vec::new()
    } else {
        new_file_str.lines().collect()
    };
    let len_original = file_original_lines.len() as i64;
    let patch_lines: Vec<&str> = patch_str.lines().collect();
    let mut out: Vec<String> = Vec::new();

    let mut is_valid_hunk = true;
    // start1/size1 are loop-carried (used for the next hunk's trailing context);
    // start2/size2 are only needed within each header branch.
    let (mut start1, mut size1): (i64, i64) = (-1, -1);

    // before, extra_after, len_original are captured; computes the extended range.
    let calc = |s1: i64, sz1: i64, s2: i64, sz2: i64, before: i64| -> (i64, i64, i64, i64) {
        let es1 = (s1 - before).max(1);
        let mut esz1 = sz1 + (s1 - es1) + extra_after;
        let es2 = (s2 - before).max(1);
        let mut esz2 = sz2 + (s2 - es2) + extra_after;
        if es1 - 1 + esz1 > len_original {
            let cap = es1 - 1 + esz1 - len_original;
            esz1 = (esz1 - cap).max(sz1);
            esz2 = (esz2 - cap).max(sz2);
        }
        (es1, esz1, es2, esz2)
    };

    for (i, &line) in patch_lines.iter().enumerate() {
        if line.starts_with("@@")
            && let Some(header) = HunkHeader::parse(line)
        {
            // Finish the previous hunk: append its trailing context.
            if is_valid_hunk && start1 != -1 && extra_after > 0 {
                let from = start1 + size1 - 1;
                for l in safe_slice(&file_original_lines, from, from + extra_after) {
                    out.push(format!(" {l}"));
                }
            }

            let mut section_header = header.section_header.clone();
            start1 = header.start1 as i64;
            size1 = header.size1 as i64;
            let start2 = header.start2 as i64;
            let size2 = header.size2 as i64;

            is_valid_hunk =
                check_if_hunk_lines_matches_to_file(i, &file_original_lines, &patch_lines, start1);

            let (ext_start1, ext_size1, ext_start2, ext_size2, delta_lines_original);

            if is_valid_hunk && (extra_before > 0 || extra_after > 0) {
                let (mut es1, mut esz1, mut es2, mut esz2);

                if allow_dynamic && !file_new_lines.is_empty() {
                    let r = calc(start1, size1, start2, size2, dynamic_before);
                    es1 = r.0;
                    esz1 = r.1;
                    es2 = r.2;
                    esz2 = r.3;

                    let lines_before_original =
                        safe_slice(&file_original_lines, es1 - 1, start1 - 1);
                    let lines_before_new = safe_slice(&file_new_lines, es2 - 1, start2 - 1);
                    let mut found_header = false;
                    for (idx, l) in lines_before_original.iter().enumerate() {
                        if !section_header.is_empty() && l.contains(&section_header) {
                            let idx = idx as i64;
                            es1 += idx;
                            es2 += idx;
                            esz1 -= idx;
                            esz2 -= idx;
                            // Dynamic context only valid if the extra lines are
                            // identical on both sides from the section header down.
                            // `lines_before_new` can be shorter than
                            // `lines_before_original` (when start1 > start2), so
                            // compare via `.get()` — a missing tail means "not
                            // equal", never a panic (Python relies on forgiving
                            // slicing here).
                            let tail = idx as usize;
                            if lines_before_new.get(tail..) == Some(&lines_before_original[tail..])
                            {
                                found_header = true;
                                section_header = String::new();
                            }
                            break;
                        }
                    }
                    if !found_header {
                        let r = calc(start1, size1, start2, size2, extra_before);
                        es1 = r.0;
                        esz1 = r.1;
                        es2 = r.2;
                        esz2 = r.3;
                    }
                } else {
                    let r = calc(start1, size1, start2, size2, extra_before);
                    es1 = r.0;
                    esz1 = r.1;
                    es2 = r.2;
                    esz2 = r.3;
                }

                // Build the "before" context, validating it matches on both sides.
                let mut delta: Vec<String> = safe_slice(&file_original_lines, es1 - 1, start1 - 1)
                    .iter()
                    .map(|l| format!(" {l}"))
                    .collect();
                if !file_new_lines.is_empty() {
                    let delta_new: Vec<String> = safe_slice(&file_new_lines, es2 - 1, start2 - 1)
                        .iter()
                        .map(|l| format!(" {l}"))
                        .collect();
                    if delta != delta_new {
                        // Find the longest common suffix; if none, drop before-context.
                        let mut found_mini = false;
                        for k in 0..delta.len() {
                            if k <= delta_new.len() && delta[k..] == delta_new[k..] {
                                let k = k as i64;
                                delta = delta[k as usize..].to_vec();
                                es1 += k;
                                esz1 -= k;
                                es2 += k;
                                esz2 -= k;
                                found_mini = true;
                                break;
                            }
                        }
                        if !found_mini {
                            es1 = start1;
                            esz1 = size1;
                            es2 = start2;
                            esz2 = size2;
                            delta = Vec::new();
                        }
                    }
                }

                // Drop the section header if it already appears in the context.
                if !section_header.is_empty() && !allow_dynamic {
                    for l in &delta {
                        if l.contains(&section_header) {
                            section_header = String::new();
                            break;
                        }
                    }
                }

                ext_start1 = es1;
                ext_size1 = esz1;
                ext_start2 = es2;
                ext_size2 = esz2;
                delta_lines_original = delta;
            } else {
                ext_start1 = start1;
                ext_size1 = size1;
                ext_start2 = start2;
                ext_size2 = size2;
                delta_lines_original = Vec::new();
            }

            // Separate consecutive hunks with a blank line, but never emit one
            // before the first hunk: a leading blank is consumed as line-0
            // context by convert_to_hunks_with_line_numbers and surfaces as a
            // bogus "line 0" hunk in the prompt. (Python emits it unconditionally
            // but its converter drops the empty-content hunk; ours keeps it.)
            if !out.is_empty() {
                out.push(String::new());
            }
            let sep = if section_header.is_empty() { "" } else { " " };
            out.push(format!(
                "@@ -{ext_start1},{ext_size1} +{ext_start2},{ext_size2} @@{sep}{section_header}"
            ));
            out.extend(delta_lines_original);
            continue;
        }
        out.push(line.to_string());
    }

    // Finish the last hunk: trailing context.
    if start1 != -1 && extra_after > 0 && is_valid_hunk {
        let from = start1 + size1 - 1;
        for l in safe_slice(&file_original_lines, from, from + extra_after) {
            out.push(format!(" {l}"));
        }
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::loader::with_settings;
    use std::sync::Arc;

    fn settings_with(dynamic: bool) -> Arc<crate::config::types::Settings> {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "config.allow_dynamic_context".to_string(),
            dynamic.to_string(),
        );
        Arc::new(crate::config::loader::load_settings(&overrides, None, None).unwrap())
    }

    #[tokio::test]
    async fn test_extend_patch_adds_fixed_context() {
        // The hunk's context line must match the original file (otherwise the
        // hunk is treated as invalid and not extended).
        let original = "line1\nline2\nline3\ncontext\nremoved\nline6\nline7\nline8\nline9\nline10";
        let patch = "@@ -4,3 +4,3 @@\n context\n-removed\n+added\n line6";
        let result = with_settings(settings_with(false), async {
            extend_patch(original, patch, "", 2, 2)
        })
        .await;
        // Header extended back by 2 (4 -> 2) and context pulled from the file.
        assert!(result.contains("@@ -2,"), "header extended: {result}");
        assert!(result.contains(" line2"));
        assert!(result.contains(" line3"));
        // The first hunk must not be preceded by a blank line, which would be
        // read as bogus line-0 context downstream.
        assert!(
            !result.starts_with('\n'),
            "no leading blank line before first hunk: {result:?}"
        );
        assert!(
            result.starts_with("@@ -2,"),
            "starts at the header: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_extend_patch_no_leading_blank_feeds_clean_hunks() {
        // Regression: a leading blank line in the extended patch surfaces as a
        // "line 0" hunk in convert_to_hunks_with_line_numbers.
        use crate::git::types::EditType;
        let original = "line1\nline2\nline3\ncontext\nremoved\nline6\nline7\nline8\nline9\nline10";
        let patch = "@@ -4,3 +4,3 @@\n context\n-removed\n+added\n line6";
        let extended = with_settings(settings_with(false), async {
            extend_patch(original, patch, "", 2, 2)
        })
        .await;
        let hunks = crate::processing::diff::convert_to_hunks_with_line_numbers(
            "f.rs",
            &extended,
            EditType::Modified,
        );
        // No "0 " line-numbered content (the bogus line-0 hunk).
        assert!(!hunks.contains("\n0 "), "no line-0 hunk content: {hunks}");
    }

    #[test]
    fn test_extend_patch_empty() {
        assert_eq!(extend_patch("file", "", "", 2, 2), "");
        assert_eq!(extend_patch("", "patch", "", 2, 2), "patch");
    }

    #[test]
    fn test_extend_patch_no_extra() {
        let patch = "@@ -1,3 +1,3 @@\n context\n";
        assert_eq!(extend_patch("file", patch, "", 0, 0), patch);
    }

    #[tokio::test]
    async fn test_dynamic_context_reaches_section_header() {
        // The hunk is deep inside `fn outer()`. With dynamic context, the before
        // window grows past the fixed 2 lines to include the `fn outer()` line.
        let file = "fn outer() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n}";
        // Hunk changes line 6 (`let e = 5;`); section header is `fn outer()`.
        let patch = "@@ -6,1 +6,1 @@ fn outer()\n-    let e = 5;\n+    let e = 6;";
        let result = with_settings(settings_with(true), async {
            // base and head identical except the changed line region.
            extend_patch(file, patch, file, 2, 0)
        })
        .await;
        // Dynamic context pulls back to the function start (line 1).
        assert!(
            result.contains(" fn outer() {"),
            "should reach the section header: {result}"
        );
        assert!(
            result.contains("@@ -1,"),
            "header starts at line 1: {result}"
        );
    }

    #[tokio::test]
    async fn test_dynamic_context_falls_back_when_sides_differ() {
        // When the before-context differs between old and new files, dynamic
        // context can't apply; it falls back without panicking.
        let base = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}";
        let new = "fn f() {\n    let a = 1;\n    let b = 99;\n    let c = 3;\n}";
        let patch = "@@ -4,1 +4,1 @@ fn f()\n-    let c = 3;\n+    let c = 4;";
        let result = with_settings(settings_with(true), async {
            extend_patch(base, patch, new, 2, 0)
        })
        .await;
        // Still produces a valid extended patch (no panic) containing the change.
        assert!(result.contains("@@ -"));
        assert!(result.contains("let c"));
    }

    #[tokio::test]
    async fn test_dynamic_context_no_panic_when_old_side_longer() {
        // Regression: the section header matches deep in the old-side "before"
        // window (idx 4) while the new-side window has only one line (start2=2).
        // The tail comparison must not index `lines_before_new[4..]` — Python
        // slicing tolerates it, Rust slicing would panic.
        let base = "a\nb\nc\nd\nMARKER\nf\ntarget\n}";
        let new = "header\ntarget\n}";
        let patch = "@@ -7,1 +2,1 @@ MARKER\n-target\n+changed";
        let result = with_settings(settings_with(true), async {
            extend_patch(base, patch, new, 2, 0)
        })
        .await;
        // The point is simply that it didn't panic and produced a hunk.
        assert!(result.contains("@@ -"), "produced a hunk: {result}");
    }
}
