use std::sync::Arc;

use regex::Regex;

use crate::config::loader::get_settings;
use crate::git::types::FilePatchInfo;
use crate::util::get_or_compile_regex;

/// Common binary file extensions that should be excluded from diff processing.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "webp", "tiff", "tif", "mp3", "mp4", "wav",
    "avi", "mov", "mkv", "flac", "ogg", "webm", "zip", "tar", "gz", "bz2", "xz", "7z", "rar",
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "exe", "dll", "so", "dylib", "bin", "obj",
    "o", "a", "lib", "woff", "woff2", "ttf", "eot", "otf", "pyc", "pyo", "class", "jar", "sqlite",
    "db", "dat",
];

/// Check if a filename has a binary extension.
pub fn is_binary(filename: &str) -> bool {
    if let Some(ext) = filename.rsplit('.').next() {
        BINARY_EXTENSIONS.contains(&ext.to_lowercase().as_str())
    } else {
        false
    }
}

/// Exact filenames that are auto-generated (lockfiles) and add no review value.
const AUTO_GENERATED_FILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "composer.lock",
    "Gemfile.lock",
    "poetry.lock",
    "go.sum",
    ".terraform.lock.hcl",
    "uv.lock",
    "Cargo.lock",
    "Pipfile.lock",
    "mix.lock",
    "pubspec.lock",
    "bun.lockb",
];

/// Filename suffixes that mark minified / source-map generated output.
const AUTO_GENERATED_SUFFIXES: &[&str] = &[".min.js", ".min.css", ".js.map", ".ts.map", ".css.map"];

/// Whether a file is auto-generated (a known lockfile or a minified/map file),
/// mirroring the upstream `is_valid_file` exact-name and suffix checks.
pub fn is_auto_generated(filename: &str) -> bool {
    let base = filename.replace('\\', "/");
    let base = base.rsplit('/').next().unwrap_or(&base);
    AUTO_GENERATED_FILES.contains(&base)
        || AUTO_GENERATED_SUFFIXES
            .iter()
            .any(|suffix| filename.ends_with(suffix))
}

/// Whether the file's extension is in the configured `bad_extensions` list
/// (noise: archives, generated data, lockfiles by extension). Mirrors the
/// upstream extension check, honoring `use_extra_bad_extensions`.
pub fn is_bad_extension(filename: &str) -> bool {
    let Some(ext) = filename.rsplit('.').next() else {
        return false;
    };
    if ext == filename {
        // No extension at all (rsplit returned the whole name).
        return false;
    }
    // The configured lists are lowercase; normalize so `REPORT.CSV` matches `csv`.
    let ext = ext.to_ascii_lowercase();
    let settings = get_settings();
    let bad = &settings.bad_extensions;
    bad.default.iter().any(|e| e == &ext)
        || (settings.config.use_extra_bad_extensions && bad.extra.iter().any(|e| e == &ext))
}

/// Build the list of compiled ignore patterns from settings.
/// Combines regex patterns and glob patterns (converted to regex).
///
/// Patterns go through the shared `Arc<Regex>` cache, so `filter_files`
/// (called repeatedly, e.g. twice per improve run) reuses already-compiled
/// regexes instead of recompiling them every time.
pub fn build_ignore_patterns() -> Vec<Arc<Regex>> {
    let settings = get_settings();
    let mut patterns = Vec::new();

    // Regex patterns from settings
    for pattern in &settings.ignore.regex {
        match get_or_compile_regex(pattern) {
            Some(re) => patterns.push(re),
            None => tracing::warn!(pattern, "invalid ignore regex pattern"),
        }
    }

    // Glob patterns from settings (convert to regex)
    for glob in &settings.ignore.glob {
        let regex_str = glob_to_regex(glob);
        if let Some(re) = get_or_compile_regex(&regex_str) {
            patterns.push(re);
        }
        // Also cover root-level files for `**/` prefixed globs
        if let Some(root_glob) = glob.strip_prefix("**/") {
            let root_regex = glob_to_regex(root_glob);
            if let Some(re) = get_or_compile_regex(&root_regex) {
                patterns.push(re);
            }
        }
    }

    patterns
}

/// Convert a glob pattern to a regex string.
/// Supports `*`, `**`, `?`, and character classes `[...]`.
fn glob_to_regex(glob: &str) -> String {
    let mut regex = String::from("^");
    let mut chars = glob.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next(); // consume second *
                    if chars.peek() == Some(&'/') {
                        chars.next(); // consume /
                        regex.push_str("(?:.*/)?");
                    } else {
                        regex.push_str(".*");
                    }
                } else {
                    regex.push_str("[^/]*");
                }
            }
            '?' => regex.push_str("[^/]"),
            '.' => regex.push_str("\\."),
            '[' => {
                regex.push('[');
                for c in chars.by_ref() {
                    if c == ']' {
                        regex.push(']');
                        break;
                    }
                    regex.push(c);
                }
            }
            c => regex.push(c),
        }
    }

    regex.push('$');
    regex
}

/// Filter a list of files, removing those that match ignore patterns or are binary.
pub fn filter_files(files: &mut Vec<FilePatchInfo>) {
    let patterns = build_ignore_patterns();

    files.retain(|file| {
        if is_binary(&file.filename) {
            tracing::debug!(file = file.filename, "filtered: binary extension");
            return false;
        }

        if is_auto_generated(&file.filename) {
            tracing::debug!(file = file.filename, "filtered: auto-generated/lockfile");
            return false;
        }

        if is_bad_extension(&file.filename) {
            tracing::debug!(file = file.filename, "filtered: bad extension");
            return false;
        }

        if let Some(pattern) = patterns.iter().find(|p| p.is_match(&file.filename)) {
            tracing::debug!(file = file.filename, pattern = %pattern, "filtered: ignore pattern");
            return false;
        }

        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_auto_generated() {
        assert!(is_auto_generated("Cargo.lock"));
        assert!(is_auto_generated("path/to/package-lock.json"));
        assert!(is_auto_generated("app.min.js"));
        assert!(is_auto_generated("dist/bundle.css.map"));
        assert!(!is_auto_generated("src/main.rs"));
        assert!(!is_auto_generated("locked.rs"));
    }

    #[tokio::test]
    async fn test_is_bad_extension() {
        use crate::config::loader::with_settings;
        let settings = Arc::new(
            crate::config::loader::load_settings(&std::collections::HashMap::new(), None, None)
                .unwrap(),
        );
        let (csv, log, rs, md, csv_upper) = with_settings(settings, async {
            (
                is_bad_extension("data.csv"),
                is_bad_extension("build.log"),
                is_bad_extension("src/main.rs"),
                is_bad_extension("README.md"),
                // Mixed/upper-case extensions must match the lowercase config.
                is_bad_extension("REPORT.CSV"),
            )
        })
        .await;
        assert!(csv, "csv is a bad (noisy) extension");
        assert!(log, "log is a bad extension");
        assert!(!rs, "rs is valid code");
        assert!(
            !md,
            "md is in 'extra' (off unless use_extra_bad_extensions)"
        );
        assert!(csv_upper, "uppercase .CSV is still a bad extension");
    }

    #[test]
    fn test_is_binary() {
        assert!(is_binary("image.png"));
        assert!(is_binary("archive.tar.gz"));
        assert!(is_binary("doc.PDF")); // case-insensitive
        assert!(!is_binary("main.rs"));
        assert!(!is_binary("README.md"));
    }

    #[tokio::test]
    async fn test_build_ignore_patterns_uses_cache() {
        // P5: repeated calls reuse the cached Arc<Regex> instead of recompiling.
        use crate::config::loader::with_settings;
        // A unique pattern string keeps the global cache deterministic here.
        let repo_toml = "[ignore]\nregex = [\"^vendor/p5_cache_probe/\"]\n";
        let settings = Arc::new(
            crate::config::loader::load_settings(
                &std::collections::HashMap::new(),
                None,
                Some(repo_toml),
            )
            .unwrap(),
        );

        let (a, b) = with_settings(settings, async {
            (build_ignore_patterns(), build_ignore_patterns())
        })
        .await;

        assert!(!a.is_empty(), "at least the configured regex is present");
        assert_eq!(a.len(), b.len());
        // Every pattern is the same cached Arc across the two calls.
        for (x, y) in a.iter().zip(b.iter()) {
            assert!(
                Arc::ptr_eq(x, y),
                "the same pattern must return the cached Arc"
            );
        }
    }

    #[test]
    fn test_glob_to_regex() {
        let re = Regex::new(&glob_to_regex("*.rs")).unwrap();
        assert!(re.is_match("main.rs"));
        assert!(!re.is_match("src/main.rs"));

        let re = Regex::new(&glob_to_regex("**/*.lock")).unwrap();
        assert!(re.is_match("Cargo.lock"));
        assert!(re.is_match("deep/path/package.lock"));
    }

    #[test]
    fn test_glob_double_star_slash() {
        let re = Regex::new(&glob_to_regex("**/node_modules/**")).unwrap();
        assert!(re.is_match("node_modules/foo/bar.js"));
        assert!(re.is_match("project/node_modules/foo.js"));
    }

    #[test]
    fn test_glob_question_mark() {
        let re = Regex::new(&glob_to_regex("file?.txt")).unwrap();
        assert!(re.is_match("file1.txt"));
        assert!(re.is_match("fileA.txt"));
        assert!(!re.is_match("file10.txt")); // ? = single char
        assert!(!re.is_match("file.txt")); // ? requires exactly one char
    }

    #[test]
    fn test_glob_character_class() {
        let re = Regex::new(&glob_to_regex("[abc].rs")).unwrap();
        assert!(re.is_match("a.rs"));
        assert!(re.is_match("b.rs"));
        assert!(!re.is_match("d.rs"));
    }

    #[test]
    fn test_filter_files_removes_binary_and_ignored() {
        use crate::git::types::{EditType, FilePatchInfo};

        let mut files = vec![
            {
                let mut f = FilePatchInfo::new(
                    String::new(),
                    String::new(),
                    "+code".into(),
                    "src/main.rs".into(),
                );
                f.edit_type = EditType::Modified;
                f
            },
            {
                let mut f = FilePatchInfo::new(
                    String::new(),
                    String::new(),
                    String::new(),
                    "image.png".into(),
                );
                f.edit_type = EditType::Added;
                f
            },
            {
                let mut f = FilePatchInfo::new(
                    String::new(),
                    String::new(),
                    "+data".into(),
                    "data.db".into(),
                );
                f.edit_type = EditType::Modified;
                f
            },
        ];

        filter_files(&mut files);

        // Only src/main.rs should remain — image.png and data.db are binary
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "src/main.rs");
    }

    #[test]
    fn test_is_binary_no_extension() {
        assert!(!is_binary("Makefile"));
        assert!(!is_binary("LICENSE"));
    }

    #[test]
    fn test_is_binary_nested_extension() {
        // tar.gz should match gz
        assert!(is_binary("archive.tar.gz"));
        assert!(is_binary("deep/path/file.woff2"));
    }
}
