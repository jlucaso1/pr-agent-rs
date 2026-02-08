use serde::{Deserialize, Serialize};

/// How a file was changed in the PR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditType {
    Added,
    Deleted,
    Modified,
    Renamed,
    #[default]
    Unknown,
}

/// Core diff information for a single file in a PR.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FilePatchInfo {
    /// Original file content (base branch).
    pub base_file: String,
    /// New file content (head branch).
    pub head_file: String,
    /// Unified diff patch string.
    pub patch: String,
    /// File path in the repository.
    pub filename: String,
    /// Computed token count for this file's patch. -1 means not yet computed.
    pub tokens: i32,
    /// Type of edit.
    pub edit_type: EditType,
    /// Original filename (set if renamed).
    pub old_filename: Option<String>,
    /// Count of added lines.
    pub num_plus_lines: i32,
    /// Count of removed lines.
    pub num_minus_lines: i32,
    /// Detected programming language.
    pub language: Option<String>,
    /// AI-generated summary of changes (populated by AI metadata pass).
    pub ai_file_summary: Option<String>,
}

impl FilePatchInfo {
    pub fn new(base_file: String, head_file: String, patch: String, filename: String) -> Self {
        Self {
            base_file,
            head_file,
            patch,
            filename,
            tokens: -1,
            edit_type: EditType::Unknown,
            old_filename: None,
            num_plus_lines: -1,
            num_minus_lines: -1,
            language: None,
            ai_file_summary: None,
        }
    }
}

/// Opaque comment identifier. Platform-specific (e.g. GitHub comment ID).
#[derive(Debug, Clone)]
pub struct CommentId(pub String);

/// An inline comment on a specific code line in the PR.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InlineComment {
    pub body: String,
    pub path: String,
    /// End line number.
    pub line: i32,
    /// Start line number (for multi-line comments).
    pub start_line: Option<i32>,
    /// Side: "RIGHT" for the modified file.
    pub side: String,
}

/// A code improvement suggestion.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CodeSuggestion {
    pub body: String,
    pub relevant_file: String,
    pub relevant_lines_start: i32,
    pub relevant_lines_end: i32,
    pub existing_code: String,
    pub improved_code: String,
}

/// A comment on the PR/issue.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct IssueComment {
    pub id: u64,
    pub body: String,
    pub user: String,
    pub created_at: String,
    /// HTML URL for the comment (for persistent comment link-back).
    pub url: Option<String>,
}
