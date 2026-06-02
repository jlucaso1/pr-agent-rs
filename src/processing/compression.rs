use std::fmt::Write;

use crate::ai::token::{
    OUTPUT_BUFFER_TOKENS_HARD_THRESHOLD, OUTPUT_BUFFER_TOKENS_SOFT_THRESHOLD, clip_tokens,
    count_tokens, get_max_tokens_with_fallback,
};
use crate::config::loader::get_settings;
use crate::git::types::{EditType, FilePatchInfo};
use crate::processing::diff::{convert_to_hunks_with_line_numbers, format_patch_simple};
use crate::processing::filter::filter_files;
use crate::processing::patch::extend_patch;

/// Processed file entry for compression.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct FileEntry {
    patch: String,
    tokens: u32,
    edit_type: EditType,
}

/// Result from generating a compressed diff batch.
pub struct CompressedDiffResult {
    /// Concatenated patch text for the batch.
    pub patches: String,
    /// Total token count for this batch.
    pub total_tokens: u32,
    /// Filenames that didn't fit in this batch.
    pub remaining_files: Vec<String>,
    /// Filenames included in this batch.
    pub files_in_patch: Vec<String>,
}

/// Result from `get_pr_diff`.
pub struct PrDiffResult {
    /// Final diff string to send to the AI model.
    pub diff: String,
    /// Token count of the diff.
    pub token_count: u32,
    /// Files included in the diff.
    pub files_in_diff: Vec<String>,
    /// Files that were skipped due to budget.
    pub remaining_files: Vec<String>,
}

/// Main entry: generate the PR diff with optional compression.
///
/// Algorithm:
/// 1. Filter files (binary, ignore patterns)
/// 2. Sort by language
/// 3. Generate extended diff with extra context lines
/// 4. If under token budget, return full diff
/// 5. If over budget, compress: sort by tokens, pack greedily
/// 6. Append unprocessed file lists if space remains
pub fn get_pr_diff(
    files: &mut Vec<FilePatchInfo>,
    model: &str,
    add_line_numbers: bool,
) -> PrDiffResult {
    let settings = get_settings();
    let extra_before = settings.config.patch_extra_lines_before;
    let extra_after = settings.config.patch_extra_lines_after;

    // 1. Filter out binary / ignored files
    filter_files(files);

    if files.is_empty() {
        return PrDiffResult {
            diff: String::new(),
            token_count: 0,
            files_in_diff: Vec::new(),
            remaining_files: Vec::new(),
        };
    }

    // 2. Build file dictionary (extends patches with context + counts tokens)
    let file_dict = build_file_dict(files, add_line_numbers, extra_before, extra_after);

    // Release large file contents — only needed during extend_patch above.
    // Filenames and edit_type are still available for append_remaining_file_lists.
    for file in files.iter_mut() {
        drop(std::mem::take(&mut file.base_file));
        drop(std::mem::take(&mut file.head_file));
    }

    let max_tokens = get_max_tokens_with_fallback(
        model,
        settings.config.max_model_tokens,
        settings.config.custom_model_max_tokens,
    );

    // 3. Check total tokens against budget
    let total_tokens: u32 = file_dict.iter().map(|(_, e)| e.tokens).sum();

    if total_tokens + OUTPUT_BUFFER_TOKENS_SOFT_THRESHOLD < max_tokens {
        // Under budget — consume file_dict, moving strings instead of cloning
        let mut full_diff = String::new();
        let mut filenames = Vec::with_capacity(file_dict.len());
        for (name, entry) in file_dict {
            full_diff.push_str(&entry.patch);
            filenames.push(name);
        }
        return PrDiffResult {
            diff: full_diff,
            token_count: total_tokens,
            files_in_diff: filenames,
            remaining_files: Vec::new(),
        };
    }

    // 4. Over budget — compress
    tracing::info!(
        total_tokens,
        max_tokens,
        "diff exceeds token budget, compressing"
    );

    let all_filenames: Vec<String> = file_dict.iter().map(|(f, _)| f.clone()).collect();
    let result = generate_full_patch(&file_dict, max_tokens, &all_filenames);

    // 5. Append unprocessed file lists if space remains
    let final_diff = append_remaining_file_lists(
        result.patches,
        result.total_tokens,
        max_tokens,
        files,
        &result.files_in_patch,
    );

    let final_tokens = count_tokens(&final_diff);

    PrDiffResult {
        diff: final_diff,
        token_count: final_tokens,
        files_in_diff: result.files_in_patch,
        remaining_files: result.remaining_files,
    }
}

/// Build a dictionary of filename → FileEntry with token counts.
///
/// Files are sorted by token count descending (largest first).
fn build_file_dict(
    files: &[FilePatchInfo],
    add_line_numbers: bool,
    extra_before: usize,
    extra_after: usize,
) -> Vec<(String, FileEntry)> {
    let mut entries: Vec<(String, FileEntry)> = Vec::with_capacity(files.len());

    for file in files {
        let extended = extend_patch(&file.base_file, &file.patch, extra_before, extra_after);

        // Pass raw parts directly — avoids constructing a temporary FilePatchInfo
        // and eliminates one filename clone per file.
        let patch_text = if add_line_numbers {
            convert_to_hunks_with_line_numbers(&file.filename, &extended, file.edit_type)
        } else {
            format_patch_simple(&file.filename, &extended, file.edit_type)
        };

        let tokens = count_tokens(&patch_text);

        entries.push((
            file.filename.clone(),
            FileEntry {
                patch: patch_text,
                tokens,
                edit_type: file.edit_type,
            },
        ));
    }

    // Sort by tokens descending (largest first get priority)
    entries.sort_by(|a, b| b.1.tokens.cmp(&a.1.tokens));
    entries
}

/// Pack files into a single patch batch, respecting token budget.
///
/// Uses two thresholds:
/// - **Soft**: skip file but keep in remaining (can go in next batch)
/// - **Hard**: skip file entirely (no more tokens available at all)
fn generate_full_patch(
    file_dict: &[(String, FileEntry)],
    max_tokens: u32,
    remaining_files_prev: &[String],
) -> CompressedDiffResult {
    let remaining_set: std::collections::HashSet<&str> =
        remaining_files_prev.iter().map(|s| s.as_str()).collect();

    let mut patches = String::new();
    let mut total_tokens: u32 = 0;
    let mut remaining_files: Vec<String> = Vec::new();
    let mut files_in_patch: Vec<String> = Vec::new();

    for (filename, entry) in file_dict {
        if !remaining_set.contains(filename.as_str()) {
            continue;
        }

        // Hard stop: no more tokens available. NOTE (C35): the soft threshold
        // below (a larger buffer) always defers a file before `total_tokens`
        // can exceed this hard limit, so this branch is effectively unreachable
        // in practice. It is kept intentionally as a defensive backstop.
        if total_tokens > max_tokens.saturating_sub(OUTPUT_BUFFER_TOKENS_HARD_THRESHOLD) {
            tracing::warn!(file = %filename, "skipped: hard token limit reached");
            continue;
        }

        // Soft threshold: file would push us over the preferred buffer
        if total_tokens + entry.tokens
            > max_tokens.saturating_sub(OUTPUT_BUFFER_TOKENS_SOFT_THRESHOLD)
        {
            tracing::debug!(
                file = %filename,
                file_tokens = entry.tokens,
                total_tokens,
                "deferred: would exceed soft threshold"
            );
            remaining_files.push(filename.clone());
            continue;
        }

        // Add to patch
        if !entry.patch.is_empty() {
            patches.push_str(&entry.patch);
            total_tokens += entry.tokens;
            files_in_patch.push(filename.clone());
        }
    }

    CompressedDiffResult {
        patches,
        total_tokens,
        remaining_files,
        files_in_patch,
    }
}

/// If there is remaining token budget after compression, append lists of
/// unprocessed files grouped by edit type (added, modified, deleted).
fn append_remaining_file_lists(
    patches: String,
    current_tokens: u32,
    max_tokens: u32,
    all_files: &[FilePatchInfo],
    files_in_patch: &[String],
) -> String {
    let budget = max_tokens.saturating_sub(OUTPUT_BUFFER_TOKENS_HARD_THRESHOLD);
    let delta_tokens: u32 = 10;

    if budget <= current_tokens + delta_tokens {
        return patches;
    }

    let mut remaining_budget = budget - current_tokens;
    let files_set: std::collections::HashSet<&str> =
        files_in_patch.iter().map(|s| s.as_str()).collect();

    // Collect unprocessed files by edit type
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for file in all_files {
        if files_set.contains(file.filename.as_str()) {
            continue;
        }
        match file.edit_type {
            EditType::Added => added.push(file.filename.as_str()),
            EditType::Modified | EditType::Renamed => modified.push(file.filename.as_str()),
            EditType::Deleted => deleted.push(file.filename.as_str()),
            EditType::Unknown => modified.push(file.filename.as_str()),
        }
    }

    let mut result = patches;

    // Helper closure: format and append a file list
    let mut append_list = |label: &str, files: &[&str], budget: &mut u32| {
        if files.is_empty() || *budget < delta_tokens {
            return;
        }
        // Build the list directly into a String (no intermediate Vec<String> +
        // join). Each entry is separated by a newline with NO trailing newline,
        // so the token count matches the previous join-based string exactly.
        let mut list_str = format!("\n\n### Additional {label} files (not included in diff):\n");
        for (i, f) in files.iter().enumerate() {
            if i > 0 {
                list_str.push('\n');
            }
            let _ = write!(list_str, "- {f}");
        }
        let clipped = clip_tokens(&list_str, *budget, true);
        if !clipped.is_empty() {
            let tokens = count_tokens(&clipped);
            result.push_str(&clipped);
            // Subtract the clipped tokens plus a small allowance for the "\n\n"
            // section separator (carried over from the Python original).
            *budget = budget.saturating_sub(tokens + 2);
        }
    };

    append_list("added", &added, &mut remaining_budget);
    append_list("modified", &modified, &mut remaining_budget);
    append_list("deleted", &deleted, &mut remaining_budget);

    result
}

/// A diff batch formatted in BOTH variants for the same set of files:
/// `patches` (no line numbers, for the suggestion prompt) and
/// `patches_with_lines` (for the reflect prompt).
pub struct DualDiffBatch {
    pub patches: String,
    pub patches_with_lines: String,
    pub files_in_patch: Vec<String>,
}

/// Per-file entry holding both formatted variants and the (line-numbered)
/// token count used as the packing budget.
struct DualFileEntry {
    no_lines: String,
    with_lines: String,
    tokens: u32,
}

/// Generate multiple compressed diff batches for large PRs, formatted in BOTH
/// the no-line-numbers and with-line-numbers variants.
///
/// The packing runs ONCE (over the line-numbered token budget, mirroring the
/// Python original), so batch *i* always contains the exact same files in both
/// variants. Previously two independent packings could place a file in
/// different batch indices, and the caller's `zip` would silently pair
/// mismatched file sets between the suggestion and reflect prompts. `extend_patch`
/// also runs only once per file.
pub fn get_pr_diff_multiple_patches(
    files: &mut Vec<FilePatchInfo>,
    model: &str,
    max_calls: usize,
) -> Vec<DualDiffBatch> {
    let settings = get_settings();
    let extra_before = settings.config.patch_extra_lines_before;
    let extra_after = settings.config.patch_extra_lines_after;

    filter_files(files);

    if files.is_empty() {
        return Vec::new();
    }

    let max_tokens = get_max_tokens_with_fallback(
        model,
        settings.config.max_model_tokens,
        settings.config.custom_model_max_tokens,
    );

    // Extend each patch once, format both variants, and count tokens on the
    // line-numbered variant (the packing budget).
    let mut entries: Vec<(String, DualFileEntry)> = files
        .iter()
        .map(|file| {
            let extended = extend_patch(&file.base_file, &file.patch, extra_before, extra_after);
            let no_lines = format_patch_simple(&file.filename, &extended, file.edit_type);
            let with_lines =
                convert_to_hunks_with_line_numbers(&file.filename, &extended, file.edit_type);
            let tokens = count_tokens(&with_lines);
            (
                file.filename.clone(),
                DualFileEntry {
                    no_lines,
                    with_lines,
                    tokens,
                },
            )
        })
        .collect();

    // Largest files first (same priority order as build_file_dict).
    entries.sort_by(|a, b| b.1.tokens.cmp(&a.1.tokens));

    let mut remaining: Vec<String> = entries.iter().map(|(f, _)| f.clone()).collect();
    let mut batches = Vec::new();

    for _ in 0..max_calls {
        if remaining.is_empty() {
            break;
        }
        let (batch, new_remaining) = pack_dual_batch(&entries, max_tokens, &remaining);
        // Stop if no progress was made (avoids an infinite loop on a single
        // file larger than the whole budget).
        if batch.files_in_patch.is_empty() {
            break;
        }
        remaining = new_remaining;
        batches.push(batch);
    }

    batches
}

/// Pack one batch from `entries`, building both formatted variants for the
/// same selected files. Mirrors `generate_full_patch`'s soft/hard thresholds.
fn pack_dual_batch(
    entries: &[(String, DualFileEntry)],
    max_tokens: u32,
    remaining_prev: &[String],
) -> (DualDiffBatch, Vec<String>) {
    let remaining_set: std::collections::HashSet<&str> =
        remaining_prev.iter().map(|s| s.as_str()).collect();

    let mut patches = String::new();
    let mut patches_with_lines = String::new();
    let mut total_tokens: u32 = 0;
    let mut remaining_files: Vec<String> = Vec::new();
    let mut files_in_patch: Vec<String> = Vec::new();

    for (filename, entry) in entries {
        if !remaining_set.contains(filename.as_str()) {
            continue;
        }

        // Hard stop: no more tokens available at all.
        if total_tokens > max_tokens.saturating_sub(OUTPUT_BUFFER_TOKENS_HARD_THRESHOLD) {
            tracing::warn!(file = %filename, "skipped: hard token limit reached");
            continue;
        }

        // Soft threshold: defer to a later batch.
        if total_tokens + entry.tokens
            > max_tokens.saturating_sub(OUTPUT_BUFFER_TOKENS_SOFT_THRESHOLD)
        {
            remaining_files.push(filename.clone());
            continue;
        }

        if !entry.with_lines.is_empty() {
            patches.push_str(&entry.no_lines);
            patches_with_lines.push_str(&entry.with_lines);
            total_tokens += entry.tokens;
            files_in_patch.push(filename.clone());
        }
    }

    (
        DualDiffBatch {
            patches,
            patches_with_lines,
            files_in_patch,
        },
        remaining_files,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::{EditType, FilePatchInfo};

    fn make_file(filename: &str, patch: &str, edit_type: EditType) -> FilePatchInfo {
        let mut f = FilePatchInfo::new(String::new(), String::new(), patch.into(), filename.into());
        f.edit_type = edit_type;
        f
    }

    #[test]
    fn test_dual_batches_have_consistent_files() {
        // P4: both formatted variants of a batch must reference the same files,
        // since they come from a single packing pass.
        let mut files = vec![
            make_file("a.rs", "@@ -1,1 +1,1 @@\n-a\n+b", EditType::Modified),
            make_file("b.rs", "@@ -1,1 +1,1 @@\n-c\n+d", EditType::Modified),
        ];
        let batches = get_pr_diff_multiple_patches(&mut files, "gpt-4o", 5);
        assert!(!batches.is_empty(), "should produce at least one batch");

        for batch in &batches {
            assert!(!batch.files_in_patch.is_empty());
            for f in &batch.files_in_patch {
                assert!(
                    batch.patches.contains(f.as_str()),
                    "no-lines variant must mention {f}"
                );
                assert!(
                    batch.patches_with_lines.contains(f.as_str()),
                    "with-lines variant must mention {f}"
                );
            }
            // The with-lines variant carries numbered hunks (`__new hunk__`),
            // the no-lines variant does not.
            assert!(batch.patches_with_lines.contains("__new hunk__"));
            assert!(!batch.patches.contains("__new hunk__"));
        }
    }

    #[test]
    fn test_build_file_dict_sorts_by_tokens() {
        let files = vec![
            make_file("small.rs", "@@ -1,1 +1,1 @@\n-a\n+b", EditType::Modified),
            make_file(
                "large.rs",
                "@@ -1,5 +1,5 @@\n-line1\n-line2\n-line3\n-line4\n-line5\n+new1\n+new2\n+new3\n+new4\n+new5",
                EditType::Modified,
            ),
        ];

        let dict = build_file_dict(&files, true, 0, 0);
        // First entry should be the larger file
        assert_eq!(dict[0].0, "large.rs");
        assert!(dict[0].1.tokens > dict[1].1.tokens);
    }

    #[test]
    fn test_generate_full_patch_respects_thresholds() {
        let entries = vec![
            (
                "file1.rs".to_string(),
                FileEntry {
                    patch: "patch1".to_string(),
                    tokens: 500,
                    edit_type: EditType::Modified,
                },
            ),
            (
                "file2.rs".to_string(),
                FileEntry {
                    patch: "patch2".to_string(),
                    tokens: 500,
                    edit_type: EditType::Modified,
                },
            ),
            (
                "file3.rs".to_string(),
                FileEntry {
                    patch: "patch3".to_string(),
                    tokens: 500,
                    edit_type: EditType::Modified,
                },
            ),
        ];

        // max=3000, soft budget = 3000-1500=1500, hard budget = 3000-1000=2000
        // file1 (500): 0+500 <= 1500 → fits
        // file2 (500): 500+500=1000 <= 1500 → fits
        // file3 (500): 1000+500=1500 <= 1500 → fits (equal)
        let remaining = vec![
            "file1.rs".to_string(),
            "file2.rs".to_string(),
            "file3.rs".to_string(),
        ];
        let result = generate_full_patch(&entries, 3000, &remaining);
        assert_eq!(result.files_in_patch.len(), 3);
        assert!(result.remaining_files.is_empty());

        // max=2500, soft budget = 2500-1500=1000
        // file1 (500): 0+500 <= 1000 → fits
        // file2 (500): 500+500=1000 <= 1000 → fits (equal)
        // file3 (500): 1000+500=1500 > 1000 → deferred (soft)
        let result = generate_full_patch(&entries, 2500, &remaining);
        assert_eq!(result.files_in_patch.len(), 2);
        assert!(result.remaining_files.contains(&"file3.rs".to_string()));
    }

    #[test]
    fn test_generate_full_patch_fits_all() {
        let entries = vec![
            (
                "a.rs".to_string(),
                FileEntry {
                    patch: "p1".to_string(),
                    tokens: 100,
                    edit_type: EditType::Modified,
                },
            ),
            (
                "b.rs".to_string(),
                FileEntry {
                    patch: "p2".to_string(),
                    tokens: 100,
                    edit_type: EditType::Added,
                },
            ),
        ];

        let remaining = vec!["a.rs".to_string(), "b.rs".to_string()];
        let result = generate_full_patch(&entries, 100_000, &remaining);

        assert_eq!(result.files_in_patch.len(), 2);
        assert!(result.remaining_files.is_empty());
    }

    #[test]
    fn test_append_remaining_file_lists_adds_sections() {
        let files = vec![
            make_file("included.rs", "", EditType::Modified),
            make_file("skipped_add.rs", "", EditType::Added),
            make_file("skipped_del.rs", "", EditType::Deleted),
        ];

        let result = append_remaining_file_lists(
            "existing patch".to_string(),
            100,
            100_000,
            &files,
            &["included.rs".to_string()],
        );

        assert!(result.contains("existing patch"));
        assert!(result.contains("skipped_add.rs"));
        assert!(result.contains("skipped_del.rs"));
        assert!(result.contains("Additional added files"));
        assert!(result.contains("Additional deleted files"));
    }
}
