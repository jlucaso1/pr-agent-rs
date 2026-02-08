use regex::Regex;

use crate::config::loader::get_settings;
use crate::git::types::FilePatchInfo;

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

/// Build the list of compiled ignore patterns from settings.
/// Combines regex patterns and glob patterns (converted to regex).
pub fn build_ignore_patterns() -> Vec<Regex> {
    let settings = get_settings();
    let mut patterns = Vec::new();

    // Regex patterns from settings
    for pattern in &settings.ignore.regex {
        if let Ok(re) = Regex::new(pattern) {
            patterns.push(re);
        } else {
            tracing::warn!(pattern, "invalid ignore regex pattern");
        }
    }

    // Glob patterns from settings (convert to regex)
    for glob in &settings.ignore.glob {
        let regex_str = glob_to_regex(glob);
        if let Ok(re) = Regex::new(&regex_str) {
            patterns.push(re);
        }
        // Also cover root-level files for `**/` prefixed globs
        if let Some(root_glob) = glob.strip_prefix("**/") {
            let root_regex = glob_to_regex(root_glob);
            if let Ok(re) = Regex::new(&root_regex) {
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
    fn test_is_binary() {
        assert!(is_binary("image.png"));
        assert!(is_binary("archive.tar.gz"));
        assert!(is_binary("doc.PDF")); // case-insensitive
        assert!(!is_binary("main.rs"));
        assert!(!is_binary("README.md"));
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
}
