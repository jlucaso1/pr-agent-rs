use crate::git::types::{EditType, FilePatchInfo};

/// Build a sample `FilePatchInfo` for testing with a realistic diff patch.
pub fn sample_diff_file(filename: &str, patch: &str) -> FilePatchInfo {
    let mut f = FilePatchInfo::new(
        String::new(),
        String::new(),
        patch.to_string(),
        filename.to_string(),
    );
    f.edit_type = EditType::Modified;
    f
}

/// A simple unified diff patch for use in tests.
pub const SAMPLE_PATCH: &str = r#"@@ -1,5 +1,7 @@
 fn main() {
-    println!("hello");
+    println!("hello world");
+    let x = 42;
+    dbg!(x);
 }
"#;

/// Review AI response in YAML format (matches pr_reviewer_prompts.toml schema).
pub const REVIEW_YAML: &str = r#"```yaml
review:
  estimated_effort_to_review_[1-5]: |
    3, because the changes are moderate in scope
  relevant_tests: |
    No
  possible_issues: |
    No
  security_concerns: |
    No
  key_issues_to_review:
    - issue_header: Potential null pointer
      issue_content: |
        The variable `x` could be null when accessed on line 5
      start_line: 5
      end_line: 5
      relevant_file: src/main.rs
```"#;

/// Describe AI response in YAML format (matches pr_description_prompts.toml schema).
pub const DESCRIBE_YAML: &str = r#"```yaml
type:
  - Enhancement
description: |
  This PR adds debug output and a variable assignment to the main function.
title: |
  Add debug output to main function
pr_files:
  - filename: |
      src/main.rs
    language: |
      Rust
    changes_summary: |
      Added variable assignment and debug output
    changes_title: |
      Add debug logging
    label: |
      enhancement
```"#;

/// Improve AI response — suggestion pass (matches pr_code_suggestions_prompts.toml schema).
pub const IMPROVE_YAML_PASS1: &str = r#"```yaml
code_suggestions:
  - relevant_file: |
      src/main.rs
    language: |
      Rust
    suggestion_content: |
      Consider using a named constant instead of a magic number
    existing_code: |
      let x = 42;
    improved_code: |
      const ANSWER: i32 = 42;
      let x = ANSWER;
    one_sentence_summary: |
      Replace magic number with named constant
    relevant_lines_start: 3
    relevant_lines_end: 3
    label: |
      best practice
  - relevant_file: |
      src/main.rs
    language: |
      Rust
    suggestion_content: |
      Use log::debug! instead of dbg! for production code
    existing_code: |
      dbg!(x);
    improved_code: |
      log::debug!("{x}");
    one_sentence_summary: |
      Replace dbg! with proper logging
    relevant_lines_start: 4
    relevant_lines_end: 4
    label: |
      enhancement
```"#;

/// Improve AI response — reflect pass with scores (matches pr_code_suggestions_reflect_prompts.toml).
pub const IMPROVE_YAML_PASS2_REFLECT: &str = r#"```yaml
code_suggestions:
  - relevant_file: |
      src/main.rs
    suggestion_content: |
      Consider using a named constant instead of a magic number
    existing_code: |
      let x = 42;
    improved_code: |
      const ANSWER: i32 = 42;
      let x = ANSWER;
    one_sentence_summary: |
      Replace magic number with named constant
    relevant_lines_start: 3
    relevant_lines_end: 3
    label: |
      best practice
    score: 6
  - relevant_file: |
      src/main.rs
    suggestion_content: |
      Use log::debug! instead of dbg! for production code
    existing_code: |
      dbg!(x);
    improved_code: |
      log::debug!("{x}");
    one_sentence_summary: |
      Replace dbg! with proper logging
    relevant_lines_start: 4
    relevant_lines_end: 4
    label: |
      enhancement
    score: 8
```"#;
