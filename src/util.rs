use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;

/// Thread-safe cache for compiled regexes.
///
/// Patterns from config (e.g. `ignore_pr_title`) are compiled once and reused.
/// Stored as `Arc<Regex>` so cache hits are a cheap refcount bump instead of
/// cloning the entire compiled state machine.
static REGEX_CACHE: LazyLock<Mutex<HashMap<String, Arc<Regex>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get a compiled regex from the cache, or compile and cache it.
/// Returns `None` if the pattern is invalid.
pub fn get_or_compile_regex(pattern: &str) -> Option<Arc<Regex>> {
    let mut cache = REGEX_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(re) = cache.get(pattern) {
        return Some(re.clone());
    }
    match Regex::new(pattern) {
        Ok(re) => {
            let arc = Arc::new(re);
            cache.insert(pattern.to_string(), arc.clone());
            Some(arc)
        }
        Err(_) => None,
    }
}

/// Parse a `Retry-After` header value (seconds) from an HTTP response,
/// returning `default` when the header is absent or unparseable. Shared by the
/// AI and GitHub rate-limit (429) handlers.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap, default: u64) -> u64 {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default)
}

/// Find the largest byte offset <= `max_bytes` that falls on a UTF-8 char boundary.
pub(crate) fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    // Walk backwards from max_bytes until we hit a char boundary
    let mut i = max_bytes;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Truncate a string to approximately `max_bytes` bytes on a line boundary.
/// Safe for multi-byte UTF-8 text — never splits a character.
#[allow(dead_code)]
pub fn truncate_on_line_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let safe_end = floor_char_boundary(text, max_bytes);
    match text[..safe_end].rfind('\n') {
        Some(pos) => &text[..pos],
        None => &text[..safe_end],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_ascii() {
        assert_eq!(truncate_on_line_boundary("hello\nworld", 7), "hello");
        assert_eq!(truncate_on_line_boundary("short", 100), "short");
    }

    #[test]
    fn test_truncate_no_newline() {
        assert_eq!(truncate_on_line_boundary("abcdefgh", 5), "abcde");
    }

    #[test]
    fn test_truncate_utf8_boundary() {
        // "Hello 👋 world" — emoji is 4 bytes at offset 6..10
        let text = "Hello 👋 world";
        // max_bytes=8 falls inside the emoji, should back up to offset 6
        let result = truncate_on_line_boundary(text, 8);
        assert_eq!(result, "Hello ");
    }

    #[test]
    fn test_truncate_multibyte_with_newline() {
        let text = "café\nlatte";
        // 'é' is 2 bytes, so "café" is 5 bytes. max=7 → truncate at newline pos=5
        let result = truncate_on_line_boundary(text, 7);
        assert_eq!(result, "café");
    }

    #[test]
    fn test_get_or_compile_regex_valid() {
        let re = get_or_compile_regex(r"^\[WIP\]");
        assert!(re.is_some());
        assert!(re.unwrap().is_match("[WIP] draft PR"));
    }

    #[test]
    fn test_get_or_compile_regex_invalid() {
        let re = get_or_compile_regex("[unclosed");
        assert!(re.is_none());
    }

    #[test]
    fn test_get_or_compile_regex_cache_hit() {
        let pattern = r"^test-cache-\d+$";
        let first = get_or_compile_regex(pattern).unwrap();
        let second = get_or_compile_regex(pattern).unwrap();
        // Both should be the same Arc (cache hit)
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn test_parse_retry_after() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        // Absent → default.
        assert_eq!(parse_retry_after(&headers, 42), 42);
        // Present and valid.
        headers.insert("retry-after", HeaderValue::from_static("17"));
        assert_eq!(parse_retry_after(&headers, 42), 17);
        // Present but unparseable → default.
        headers.insert("retry-after", HeaderValue::from_static("soon"));
        assert_eq!(parse_retry_after(&headers, 42), 42);
    }

    #[test]
    fn test_floor_char_boundary_within_text() {
        let text = "Hello 🌍"; // 🌍 is 4 bytes at offset 6..10
        // max_bytes=8 falls inside emoji, should back up to 6
        assert_eq!(floor_char_boundary(text, 8), 6);
        // max_bytes=10 is at end
        assert_eq!(floor_char_boundary(text, 10), 10);
        // max_bytes exceeds length
        assert_eq!(floor_char_boundary(text, 100), text.len());
    }
}
