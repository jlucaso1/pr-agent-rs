use url::Url;

use crate::error::PrAgentError;

/// Parsed git provider URL information.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPrUrl {
    /// The provider type (github, gitlab, bitbucket, azure, gitea).
    pub provider: ProviderType,
    /// Repository owner or workspace (e.g. "owner" or "org/project").
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// PR/MR number.
    pub pr_number: u64,
    /// Whether this is an issue URL (vs PR).
    pub is_issue: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ProviderType {
    GitHub,
    GitLab,
    Bitbucket,
    BitbucketServer,
    AzureDevOps,
    Gitea,
}

impl std::fmt::Display for ProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderType::GitHub => write!(f, "github"),
            ProviderType::GitLab => write!(f, "gitlab"),
            ProviderType::Bitbucket => write!(f, "bitbucket"),
            ProviderType::BitbucketServer => write!(f, "bitbucket_server"),
            ProviderType::AzureDevOps => write!(f, "azure"),
            ProviderType::Gitea => write!(f, "gitea"),
        }
    }
}

/// Validate that a PR number is non-zero.
fn validate_pr_number(num: u64, raw: &str) -> Result<u64, PrAgentError> {
    if num == 0 {
        return Err(PrAgentError::Other(format!(
            "invalid PR/MR number: '{raw}' (must be >= 1)"
        )));
    }
    Ok(num)
}

/// Parse a PR/MR/issue URL into its components.
/// Supports GitHub, GitLab, Bitbucket, Azure DevOps, and Gitea.
pub fn parse_pr_url(pr_url: &str) -> Result<ParsedPrUrl, PrAgentError> {
    let url = Url::parse(pr_url).map_err(|e| PrAgentError::Other(format!("invalid URL: {e}")))?;

    let host = url
        .host_str()
        .ok_or_else(|| PrAgentError::Other("URL has no host".into()))?;

    // Clean path and strip /api/v3 or /api/v1 prefix
    let raw_path = url.path().to_string();
    let cleaned_path = raw_path
        .strip_prefix("/api/v3")
        .or_else(|| raw_path.strip_prefix("/api/v1"))
        .unwrap_or(&raw_path);

    let parts: Vec<&str> = cleaned_path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // Detect provider by host
    if host.contains("github") || host == "api.github.com" {
        return parse_github(&parts, host, &raw_path);
    }
    if host.contains("gitlab") {
        return parse_gitlab(&parts);
    }
    if host.contains("bitbucket.org") {
        return parse_bitbucket(&parts);
    }
    if host.contains("dev.azure.com") || host.contains("visualstudio.com") {
        return parse_azure_devops(&parts);
    }

    // For unknown hosts, try Gitea format or generic format
    parse_gitea(&parts)
}

fn parse_github(parts: &[&str], host: &str, raw_path: &str) -> Result<ParsedPrUrl, PrAgentError> {
    // API URL: /repos/{owner}/{repo}/pulls/{pr_number}
    if host == "api.github.com" || raw_path.contains("/api/v3") {
        if parts.len() < 5 {
            return Err(PrAgentError::Other(
                "invalid GitHub API URL: too few path components".into(),
            ));
        }
        // parts[0] = "repos", parts[1] = owner, parts[2] = repo, parts[3] = pulls|issues, parts[4] = number
        let owner = parts[1].to_string();
        let repo = parts[2].to_string();
        let is_issue = parts[3] == "issues";
        let pr_number = parts[4]
            .parse::<u64>()
            .map_err(|_| PrAgentError::Other(format!("cannot parse PR number: '{}'", parts[4])))?;
        let pr_number = validate_pr_number(pr_number, parts[4])?;
        return Ok(ParsedPrUrl {
            provider: ProviderType::GitHub,
            owner,
            repo,
            pr_number,
            is_issue,
        });
    }

    // Web URL: /{owner}/{repo}/pull/{pr_number} or /{owner}/{repo}/issues/{number}
    if parts.len() < 4 {
        return Err(PrAgentError::Other(
            "invalid GitHub URL: too few path components".into(),
        ));
    }
    let owner = parts[0].to_string();
    let repo = parts[1].to_string();
    let is_issue = parts[2] == "issues";
    if parts[2] != "pull" && parts[2] != "issues" {
        return Err(PrAgentError::Other(format!(
            "expected 'pull' or 'issues' in GitHub URL, got '{}'",
            parts[2]
        )));
    }
    let pr_number = parts[3]
        .parse::<u64>()
        .map_err(|_| PrAgentError::Other(format!("cannot parse PR number: '{}'", parts[3])))?;
    let pr_number = validate_pr_number(pr_number, parts[3])?;

    Ok(ParsedPrUrl {
        provider: ProviderType::GitHub,
        owner,
        repo,
        pr_number,
        is_issue,
    })
}

fn parse_gitlab(parts: &[&str]) -> Result<ParsedPrUrl, PrAgentError> {
    // Find "merge_requests" or "issues" in path
    let mr_idx = parts.iter().position(|&p| p == "merge_requests");
    let issue_idx = parts.iter().position(|&p| p == "issues");

    let (idx, is_issue) = match (mr_idx, issue_idx) {
        (Some(i), _) => (i, false),
        (None, Some(i)) => (i, true),
        _ => {
            return Err(PrAgentError::Other(
                "invalid GitLab URL: missing 'merge_requests' or 'issues'".into(),
            ));
        }
    };

    if idx + 1 >= parts.len() {
        return Err(PrAgentError::Other(
            "invalid GitLab URL: no MR/issue ID after keyword".into(),
        ));
    }

    let number = parts[idx + 1]
        .parse::<u64>()
        .map_err(|_| PrAgentError::Other(format!("cannot parse MR ID: '{}'", parts[idx + 1])))?;
    let number = validate_pr_number(number, parts[idx + 1])?;

    // Project path is everything before the keyword, minus trailing "-"
    let mut project_parts: Vec<&str> = parts[..idx].to_vec();
    if project_parts.last() == Some(&"-") {
        project_parts.pop();
    }

    if project_parts.is_empty() {
        return Err(PrAgentError::Other(
            "invalid GitLab URL: empty project path".into(),
        ));
    }

    // Split into owner (all but last) and repo (last)
    // project_parts is guaranteed non-empty by the check above
    let repo = match project_parts.pop() {
        Some(r) => r.to_string(),
        None => {
            return Err(PrAgentError::Other(
                "invalid GitLab URL: empty project path".into(),
            ));
        }
    };
    let owner = project_parts.join("/");

    Ok(ParsedPrUrl {
        provider: ProviderType::GitLab,
        owner,
        repo,
        pr_number: number,
        is_issue,
    })
}

fn parse_bitbucket(parts: &[&str]) -> Result<ParsedPrUrl, PrAgentError> {
    // /{workspace}/{repo}/pull-requests/{pr_number}
    if parts.len() < 4 || parts[2] != "pull-requests" {
        return Err(PrAgentError::Other(
            "invalid Bitbucket URL: expected /{workspace}/{repo}/pull-requests/{pr}".into(),
        ));
    }

    let owner = parts[0].to_string();
    let repo = parts[1].to_string();
    let pr_number = parts[3]
        .parse::<u64>()
        .map_err(|_| PrAgentError::Other(format!("cannot parse PR number: '{}'", parts[3])))?;
    let pr_number = validate_pr_number(pr_number, parts[3])?;

    Ok(ParsedPrUrl {
        provider: ProviderType::Bitbucket,
        owner,
        repo,
        pr_number,
        is_issue: false,
    })
}

fn parse_azure_devops(parts: &[&str]) -> Result<ParsedPrUrl, PrAgentError> {
    // /{organization}/{project}/_git/{repository}/pullrequest/{pr_number}
    let n = parts.len();
    if n < 5 {
        return Err(PrAgentError::Other(
            "invalid Azure DevOps URL: too few path components".into(),
        ));
    }

    if parts[n - 2] != "pullrequest" {
        return Err(PrAgentError::Other(
            "invalid Azure DevOps URL: expected 'pullrequest' keyword".into(),
        ));
    }

    let owner = parts[n - 5].to_string();
    let repo = parts[n - 3].to_string();
    let pr_number = parts[n - 1]
        .parse::<u64>()
        .map_err(|_| PrAgentError::Other(format!("cannot parse PR number: '{}'", parts[n - 1])))?;
    let pr_number = validate_pr_number(pr_number, parts[n - 1])?;

    Ok(ParsedPrUrl {
        provider: ProviderType::AzureDevOps,
        owner,
        repo,
        pr_number,
        is_issue: false,
    })
}

fn parse_gitea(parts: &[&str]) -> Result<ParsedPrUrl, PrAgentError> {
    // /{owner}/{repo}/pulls/{pr_number} or /{owner}/{repo}/issues/{number}
    if parts.len() < 4 {
        return Err(PrAgentError::Other(
            "invalid URL: too few path components for any known provider".into(),
        ));
    }

    let is_issue = parts[2] == "issues";
    if parts[2] != "pulls" && parts[2] != "issues" {
        return Err(PrAgentError::Other(format!(
            "unrecognized URL format: expected 'pulls' or 'issues', got '{}'",
            parts[2]
        )));
    }

    let owner = parts[0].to_string();
    let repo = parts[1].to_string();
    let pr_number = parts[3]
        .parse::<u64>()
        .map_err(|_| PrAgentError::Other(format!("cannot parse PR number: '{}'", parts[3])))?;
    let pr_number = validate_pr_number(pr_number, parts[3])?;

    Ok(ParsedPrUrl {
        provider: ProviderType::Gitea,
        owner,
        repo,
        pr_number,
        is_issue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_web_url() {
        let parsed = parse_pr_url("https://github.com/owner/repo/pull/123").unwrap();
        assert_eq!(parsed.provider, ProviderType::GitHub);
        assert_eq!(parsed.owner, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.pr_number, 123);
        assert!(!parsed.is_issue);
    }

    #[test]
    fn test_github_issue_url() {
        let parsed = parse_pr_url("https://github.com/owner/repo/issues/42").unwrap();
        assert_eq!(parsed.provider, ProviderType::GitHub);
        assert_eq!(parsed.pr_number, 42);
        assert!(parsed.is_issue);
    }

    #[test]
    fn test_github_api_url() {
        let parsed = parse_pr_url("https://api.github.com/repos/owner/repo/pulls/456").unwrap();
        assert_eq!(parsed.provider, ProviderType::GitHub);
        assert_eq!(parsed.owner, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.pr_number, 456);
    }

    #[test]
    fn test_github_enterprise_url() {
        let parsed =
            parse_pr_url("https://github.example.com/api/v3/repos/org/repo/pulls/99").unwrap();
        assert_eq!(parsed.provider, ProviderType::GitHub);
        assert_eq!(parsed.owner, "org");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.pr_number, 99);
    }

    #[test]
    fn test_gitlab_url() {
        let parsed =
            parse_pr_url("https://gitlab.com/group/subgroup/project/-/merge_requests/10").unwrap();
        assert_eq!(parsed.provider, ProviderType::GitLab);
        assert_eq!(parsed.owner, "group/subgroup");
        assert_eq!(parsed.repo, "project");
        assert_eq!(parsed.pr_number, 10);
    }

    #[test]
    fn test_gitlab_simple_url() {
        let parsed = parse_pr_url("https://gitlab.com/owner/repo/-/merge_requests/5").unwrap();
        assert_eq!(parsed.provider, ProviderType::GitLab);
        assert_eq!(parsed.owner, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.pr_number, 5);
    }

    #[test]
    fn test_bitbucket_url() {
        let parsed =
            parse_pr_url("https://bitbucket.org/workspace/repo/pull-requests/789").unwrap();
        assert_eq!(parsed.provider, ProviderType::Bitbucket);
        assert_eq!(parsed.owner, "workspace");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.pr_number, 789);
    }

    #[test]
    fn test_azure_devops_url() {
        let parsed =
            parse_pr_url("https://dev.azure.com/myorg/myproject/_git/myrepo/pullrequest/101")
                .unwrap();
        assert_eq!(parsed.provider, ProviderType::AzureDevOps);
        assert_eq!(parsed.owner, "myproject");
        assert_eq!(parsed.repo, "myrepo");
        assert_eq!(parsed.pr_number, 101);
    }

    #[test]
    fn test_gitea_url() {
        let parsed = parse_pr_url("https://gitea.example.com/owner/repo/pulls/33").unwrap();
        assert_eq!(parsed.provider, ProviderType::Gitea);
        assert_eq!(parsed.owner, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.pr_number, 33);
    }

    #[test]
    fn test_invalid_url() {
        assert!(parse_pr_url("not-a-url").is_err());
        assert!(parse_pr_url("https://github.com/owner/repo").is_err());
    }

    #[test]
    fn test_pr_number_zero_rejected() {
        assert!(parse_pr_url("https://github.com/owner/repo/pull/0").is_err());
    }
}
