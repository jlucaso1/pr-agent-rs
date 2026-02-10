use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

/// Markdown image: `![alt](url)`
static MD_IMAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[[^\]]*\]\(([^)\s]+)\)").unwrap());

/// HTML img tag: `<img src="url">` or `<img src='url'>`
static HTML_IMG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<img\s+[^>]*src\s*=\s*["']([^"']+)["']"#).unwrap());

/// Bare HTTPS URL token (greedy up to whitespace / markdown delimiters).
static BARE_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s<>"'\]\)]+"#).unwrap());

const IMAGE_EXTENSIONS: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".bmp"];

/// GitHub user-attachment host patterns (often lack file extensions).
const GITHUB_ASSET_PATTERNS: &[&str] = &[
    "github.com/user-attachments/assets/",
    "githubusercontent.com/",
];

/// Extract all image URLs from markdown/HTML text.
///
/// Supports:
/// - `![alt](url)` markdown images
/// - `<img src="url">` / `<img src='url'>` HTML tags
/// - Bare HTTPS links ending in image extensions
/// - GitHub user-attachment URLs (no extension required)
///
/// Returns deduplicated URLs in order of first appearance.
pub fn extract_image_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();

    // Pass 1: Markdown images  ![alt](url)
    for cap in MD_IMAGE_RE.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str().trim_end_matches(['.', ',', ';', '!']);
            if !url.is_empty() && seen.insert(url.to_string()) {
                urls.push(url.to_string());
            }
        }
    }

    // Pass 2: HTML <img src="url">
    for cap in HTML_IMG_RE.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str().trim_end_matches(['.', ',', ';', '!']);
            if !url.is_empty() && seen.insert(url.to_string()) {
                urls.push(url.to_string());
            }
        }
    }

    // Pass 3: Bare HTTPS image URLs (not already captured)
    for m in BARE_URL_RE.find_iter(text) {
        let url = m.as_str().trim_end_matches(['.', ',', ';', '!']);

        if seen.contains(url) {
            continue;
        }

        // GitHub asset URLs (no extension needed)
        let is_github_asset = GITHUB_ASSET_PATTERNS.iter().any(|pat| url.contains(pat));

        if is_github_asset {
            seen.insert(url.to_string());
            urls.push(url.to_string());
            continue;
        }

        // Check image extension on the path portion (before ? or #)
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let lower = path.to_lowercase();
        if IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
            seen.insert(url.to_string());
            urls.push(url.to_string());
        }
    }

    urls
}

/// Validate image URLs with HEAD requests, filtering out broken links.
///
/// - Concurrent requests via `futures_util::join_all`
/// - 5-second timeout per URL
/// - GitHub-hosted URLs are trusted (skip validation — they require auth for private repos)
/// - URLs returning 4xx/5xx or failing to connect are dropped with a warning log
pub async fn validate_image_urls(urls: Vec<String>) -> Vec<String> {
    if urls.is_empty() {
        return urls;
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_default();

    let futures: Vec<_> = urls
        .into_iter()
        .map(|url| {
            let client = client.clone();
            async move {
                // Trust GitHub-hosted assets (require auth for private repos)
                if GITHUB_ASSET_PATTERNS
                    .iter()
                    .any(|pat| url.contains(pat))
                {
                    return Some(url);
                }

                match client.head(&url).send().await {
                    Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
                        Some(url)
                    }
                    Ok(resp) => {
                        tracing::warn!(url, status = %resp.status(), "image URL validation failed, skipping");
                        None
                    }
                    Err(e) => {
                        tracing::warn!(url, error = %e, "image URL validation failed, skipping");
                        None
                    }
                }
            }
        })
        .collect();

    futures_util::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Extract image URLs from text and validate them with HEAD requests.
///
/// Convenience wrapper: [`extract_image_urls`] + [`validate_image_urls`].
pub async fn extract_and_validate_image_urls(text: &str) -> Vec<String> {
    let urls = extract_image_urls(text);
    if urls.is_empty() {
        return urls;
    }
    validate_image_urls(urls).await
}

/// Relative issue reference: `#123` (avoids matching inside URLs).
static ISSUE_HASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^/\w])#(\d+)").unwrap());

/// Maximum number of linked issues to fetch (avoids excessive API calls).
pub const MAX_LINKED_ISSUES: usize = 5;

/// Extract issue numbers referenced in text.
///
/// Matches:
/// - `#123` (relative — `Fixes #123`, `Closes #456`, `Resolves #789`)
/// - `https://github.com/{owner}/{repo}/issues/123` (full URL, same repo only)
///
/// Returns deduplicated issue numbers, capped at [`MAX_LINKED_ISSUES`].
pub fn extract_linked_issue_numbers(text: &str, repo_owner: &str, repo_name: &str) -> Vec<u64> {
    let mut seen = HashSet::new();
    let mut numbers = Vec::new();

    // Build a regex for full GitHub issue URLs scoped to this repo.
    // We build it dynamically because owner/repo are runtime values.
    let url_pattern = format!(
        r"https?://github\.com/{}/{}/issues/(\d+)",
        regex::escape(repo_owner),
        regex::escape(repo_name),
    );
    let url_re = Regex::new(&url_pattern).unwrap();

    // Pass 1: Full URL references (same repo)
    for cap in url_re.captures_iter(text) {
        if let Some(m) = cap.get(1)
            && let Ok(n) = m.as_str().parse::<u64>()
            && n > 0
            && seen.insert(n)
        {
            numbers.push(n);
            if numbers.len() >= MAX_LINKED_ISSUES {
                return numbers;
            }
        }
    }

    // Pass 2: Relative #N references
    for cap in ISSUE_HASH_RE.captures_iter(text) {
        if let Some(m) = cap.get(1)
            && let Ok(n) = m.as_str().parse::<u64>()
            && n > 0
            && seen.insert(n)
        {
            numbers.push(n);
            if numbers.len() >= MAX_LINKED_ISSUES {
                return numbers;
            }
        }
    }

    numbers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_markdown_image() {
        let text = "Here is a screenshot: ![alt text](https://example.com/photo.png)";
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://example.com/photo.png"]);
    }

    #[test]
    fn test_extract_markdown_multiple() {
        let text = "![a](https://img.com/1.png) and ![b](https://img.com/2.jpg)";
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://img.com/1.png", "https://img.com/2.jpg"]);
    }

    #[test]
    fn test_extract_html_img_double_quotes() {
        let text = r#"See <img src="https://example.com/diagram.png"> for details"#;
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://example.com/diagram.png"]);
    }

    #[test]
    fn test_extract_html_img_single_quotes() {
        let text = "See <img src='https://example.com/diagram.jpg'> here";
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://example.com/diagram.jpg"]);
    }

    #[test]
    fn test_extract_html_img_with_attributes() {
        let text = r#"<img width="400" src="https://example.com/ui.png" alt="UI screenshot">"#;
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://example.com/ui.png"]);
    }

    #[test]
    fn test_extract_bare_https_link() {
        let text = "See https://example.com/screenshot.png for the new design";
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://example.com/screenshot.png"]);
    }

    #[test]
    fn test_extract_bare_link_with_query() {
        let text = "Image at https://cdn.example.com/img.png?token=abc123&size=large";
        let urls = extract_image_urls(text);
        assert_eq!(
            urls,
            vec!["https://cdn.example.com/img.png?token=abc123&size=large"]
        );
    }

    #[test]
    fn test_extract_non_image_skipped() {
        let text = "Visit https://example.com/docs and https://example.com/api";
        let urls = extract_image_urls(text);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_extract_mixed_formats() {
        let text = r#"
## Changes

![screenshot](https://img.com/shot.png)

Architecture:
<img src="https://img.com/arch.jpg">

Also see https://img.com/mobile.jpeg for mobile view.
"#;
        let urls = extract_image_urls(text);
        assert_eq!(
            urls,
            vec![
                "https://img.com/shot.png",
                "https://img.com/arch.jpg",
                "https://img.com/mobile.jpeg",
            ]
        );
    }

    #[test]
    fn test_extract_deduplication() {
        let text = "![a](https://img.com/1.png) and ![b](https://img.com/1.png)";
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://img.com/1.png"]);
    }

    #[test]
    fn test_extract_empty_text() {
        assert!(extract_image_urls("").is_empty());
    }

    #[test]
    fn test_extract_no_images() {
        let text = "This is a normal PR description with no images whatsoever.";
        assert!(extract_image_urls(text).is_empty());
    }

    #[test]
    fn test_extract_github_user_attachments() {
        let text = "![image](https://github.com/user-attachments/assets/abc123-def456-ghi789)";
        let urls = extract_image_urls(text);
        assert_eq!(
            urls,
            vec!["https://github.com/user-attachments/assets/abc123-def456-ghi789"]
        );
    }

    #[test]
    fn test_extract_github_user_attachments_bare() {
        // Bare URL without markdown, no file extension
        let text = "See https://github.com/user-attachments/assets/abc123-def456 for screenshot";
        let urls = extract_image_urls(text);
        assert_eq!(
            urls,
            vec!["https://github.com/user-attachments/assets/abc123-def456"]
        );
    }

    #[test]
    fn test_extract_webp_gif_svg() {
        let text = r#"
![a](https://img.com/anim.gif)
![b](https://img.com/icon.svg)
![c](https://img.com/photo.webp)
"#;
        let urls = extract_image_urls(text);
        assert_eq!(
            urls,
            vec![
                "https://img.com/anim.gif",
                "https://img.com/icon.svg",
                "https://img.com/photo.webp",
            ]
        );
    }

    #[test]
    fn test_extract_trailing_punctuation() {
        let text = "Check https://img.com/shot.png.";
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://img.com/shot.png"]);
    }

    #[test]
    fn test_extract_markdown_dedup_with_bare() {
        // Markdown image + same URL bare — should NOT duplicate
        let text = "![img](https://img.com/a.png) and also https://img.com/a.png in the text";
        let urls = extract_image_urls(text);
        assert_eq!(urls, vec!["https://img.com/a.png"]);
    }

    #[tokio::test]
    async fn test_validate_empty_input() {
        let result = validate_image_urls(vec![]).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_validate_github_assets_trusted() {
        // GitHub asset URLs should be trusted without HTTP call
        let urls = vec![
            "https://github.com/user-attachments/assets/abc123".to_string(),
            "https://raw.githubusercontent.com/user/repo/main/img.png".to_string(),
        ];
        let result = validate_image_urls(urls.clone()).await;
        assert_eq!(result, urls);
    }

    // ── extract_linked_issue_numbers tests ──────────────────────────

    #[test]
    fn test_extract_linked_issues_hash_format() {
        let text = "Fixes #123";
        let nums = extract_linked_issue_numbers(text, "owner", "repo");
        assert_eq!(nums, vec![123]);
    }

    #[test]
    fn test_extract_linked_issues_multiple() {
        let text = "Fixes #1, Closes #2, Resolves #3";
        let nums = extract_linked_issue_numbers(text, "owner", "repo");
        assert_eq!(nums, vec![1, 2, 3]);
    }

    #[test]
    fn test_extract_linked_issues_full_url() {
        let text = "See https://github.com/owner/repo/issues/42 for details";
        let nums = extract_linked_issue_numbers(text, "owner", "repo");
        assert_eq!(nums, vec![42]);
    }

    #[test]
    fn test_extract_linked_issues_dedup() {
        let text = "Fixes #5 and also #5 again";
        let nums = extract_linked_issue_numbers(text, "owner", "repo");
        assert_eq!(nums, vec![5]);
    }

    #[test]
    fn test_extract_linked_issues_wrong_repo_url_skipped() {
        let text = "See https://github.com/other/project/issues/99";
        let nums = extract_linked_issue_numbers(text, "owner", "repo");
        // Full URL for different repo should not match; no #N either
        assert!(nums.is_empty());
    }

    #[test]
    fn test_extract_linked_issues_capped_at_max() {
        let text = "#1 #2 #3 #4 #5 #6 #7 #8 #9 #10";
        let nums = extract_linked_issue_numbers(text, "owner", "repo");
        assert_eq!(nums.len(), MAX_LINKED_ISSUES);
        assert_eq!(nums, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_extract_linked_issues_url_inside_text_not_hash() {
        // A #N inside a URL path should NOT be extracted (the regex requires non-word/non-slash before #)
        let text = "https://github.com/owner/repo/pull/42#discussion_r123";
        let nums = extract_linked_issue_numbers(text, "owner", "repo");
        assert!(
            nums.is_empty(),
            "should not extract # from within URL fragments"
        );
    }

    #[test]
    fn test_extract_linked_issues_mixed_url_and_hash() {
        let text = "https://github.com/owner/repo/issues/10 and Fixes #20";
        let nums = extract_linked_issue_numbers(text, "owner", "repo");
        assert_eq!(nums, vec![10, 20]);
    }
}
