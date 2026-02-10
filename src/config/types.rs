use std::collections::HashMap;
use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Redact a secret string for Debug output. Shows "[REDACTED]" if non-empty, "[]" if empty.
fn redact(s: &str) -> &str {
    if s.is_empty() { "[]" } else { "[REDACTED]" }
}

// ── Flexible value types ────────────────────────────────────────────

/// A config value that can be a boolean or a string in TOML.
/// Many pr-agent config fields accept `true`, `false`, or a string like `'table'`/`'adaptive'`.
#[derive(Debug, Clone, PartialEq)]
pub enum BoolOrString {
    Bool(bool),
    Str(String),
}

impl BoolOrString {
    pub fn as_str(&self) -> &str {
        match self {
            BoolOrString::Bool(true) => "true",
            BoolOrString::Bool(false) => "false",
            BoolOrString::Str(s) => s,
        }
    }

    #[allow(dead_code)]
    pub fn is_truthy(&self) -> bool {
        match self {
            BoolOrString::Bool(b) => *b,
            BoolOrString::Str(s) => {
                !matches!(s.to_lowercase().as_str(), "" | "false" | "0" | "no" | "off")
            }
        }
    }
}

impl Default for BoolOrString {
    fn default() -> Self {
        BoolOrString::Bool(false)
    }
}

impl From<bool> for BoolOrString {
    fn from(b: bool) -> Self {
        BoolOrString::Bool(b)
    }
}

impl From<&str> for BoolOrString {
    fn from(s: &str) -> Self {
        BoolOrString::Str(s.to_string())
    }
}

impl fmt::Display for BoolOrString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for BoolOrString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            BoolOrString::Bool(b) => serializer.serialize_bool(*b),
            BoolOrString::Str(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for BoolOrString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BoolOrStringVisitor;

        impl<'de> Visitor<'de> for BoolOrStringVisitor {
            type Value = BoolOrString;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a boolean or a string")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(BoolOrString::Bool(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(BoolOrString::Str(v.to_string()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(BoolOrString::Str(v))
            }
        }

        deserializer.deserialize_any(BoolOrStringVisitor)
    }
}

// ── Top-level Settings ──────────────────────────────────────────────

/// Top-level configuration. Each field maps to a TOML `[section]`.
/// Uses `#[serde(default)]` so missing sections gracefully fall back.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Settings {
    pub config: GlobalConfig,
    pub pr_reviewer: PrReviewerConfig,
    pub pr_description: PrDescriptionConfig,
    pub pr_questions: PrQuestionsConfig,
    pub pr_code_suggestions: PrCodeSuggestionsConfig,
    pub pr_custom_prompt: PrCustomPromptConfig,
    pub pr_add_docs: PrAddDocsConfig,
    pub pr_update_changelog: PrUpdateChangelogConfig,
    pub pr_analyze: PrAnalyzeConfig,
    pub pr_test: PrTestConfig,
    pub pr_improve_component: PrImproveComponentConfig,
    pub checks: ChecksConfig,
    pub pr_help: PrHelpConfig,
    pub pr_config: PrConfigSection,
    pub pr_help_docs: PrHelpDocsConfig,
    pub github: GithubConfig,
    pub github_action_config: GithubActionConfig,
    pub github_app: GithubAppConfig,
    pub gitlab: GitlabConfig,
    pub gitea: GiteaConfig,
    pub bitbucket_app: BitbucketAppConfig,
    pub bitbucket_server: BitbucketServerConfig,
    pub local: LocalConfig,
    pub gerrit: GerritConfig,
    pub litellm: LitellmConfig,
    pub pr_similar_issue: PrSimilarIssueConfig,
    pub pr_find_similar_component: PrFindSimilarComponentConfig,
    pub pinecone: PineconeConfig,
    pub lancedb: LancedbConfig,
    pub qdrant: QdrantConfig,
    pub best_practices: BestPracticesConfig,
    pub auto_best_practices: AutoBestPracticesConfig,
    pub azure_devops: AzureDevopsConfig,
    pub azure_devops_server: AzureDevopsServerConfig,
    pub ignore: IgnoreConfig,
    pub custom_labels: HashMap<String, CustomLabelEntry>,
    // Prompt templates (loaded from *_prompts.toml files)
    pub pr_review_prompt: PromptTemplate,
    pub pr_description_prompt: PromptTemplate,
    pub pr_code_suggestions_prompt: PromptTemplate,
    pub pr_code_suggestions_prompt_not_decoupled: PromptTemplate,
    pub pr_code_suggestions_reflect_prompt: PromptTemplate,
    pub pr_questions_prompt: PromptTemplate,
    pub pr_line_questions_prompt: PromptTemplate,
    pub pr_update_changelog_prompt: PromptTemplate,
    pub pr_information_from_user_prompt: PromptTemplate,
    pub pr_help_prompts: PromptTemplate,
    pub pr_help_docs_prompts: PromptTemplate,
    pub pr_help_docs_headings_prompts: PromptTemplate,
    pub pr_evaluate_prompt_response: PromptTemplate,
    // Secrets (loaded from .secrets.toml or env vars)
    pub openai: OpenAiSecrets,
    pub anthropic: AnthropicSecrets,
}

// ── [config] ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub model: String,
    pub fallback_models: Vec<String>,
    pub model_reasoning: String,
    pub model_weak: String,
    pub git_provider: String,
    pub publish_output: bool,
    pub publish_output_progress: bool,
    pub verbosity_level: u8,
    pub use_extra_bad_extensions: bool,
    pub log_level: String,
    pub use_wiki_settings_file: bool,
    pub use_repo_settings_file: bool,
    pub use_global_settings_file: bool,
    pub disable_auto_feedback: bool,
    pub ai_timeout: u64,
    pub skip_keys: Vec<String>,
    pub custom_reasoning_model: bool,
    pub response_language: String,
    pub max_description_tokens: u32,
    pub max_commits_tokens: u32,
    pub max_model_tokens: u32,
    pub custom_model_max_tokens: i32,
    pub model_token_count_estimate_factor: f32,
    pub patch_extension_skip_types: Vec<String>,
    pub allow_dynamic_context: bool,
    pub max_extra_lines_before_dynamic_context: u32,
    pub patch_extra_lines_before: usize,
    pub patch_extra_lines_after: usize,
    pub secret_provider: String,
    pub cli_mode: bool,
    pub ai_disclaimer_title: String,
    pub ai_disclaimer: String,
    pub output_relevant_configurations: bool,
    pub large_patch_policy: String,
    pub duplicate_prompt_examples: bool,
    pub seed: i32,
    pub temperature: f32,
    pub add_repo_metadata: bool,
    pub add_repo_metadata_file_list: Vec<String>,
    pub ignore_pr_title: Vec<String>,
    pub ignore_pr_target_branches: Vec<String>,
    pub ignore_pr_source_branches: Vec<String>,
    pub ignore_pr_labels: Vec<String>,
    pub ignore_pr_authors: Vec<String>,
    pub ignore_repositories: Vec<String>,
    pub ignore_language_framework: Vec<String>,
    pub is_auto_command: bool,
    pub enable_ai_metadata: bool,
    pub reasoning_effort: String,
    pub enable_auto_approval: bool,
    pub auto_approve_for_low_review_effort: i32,
    pub auto_approve_for_no_suggestions: bool,
    pub ensure_ticket_compliance: bool,
    pub enable_claude_extended_thinking: bool,
    pub extended_thinking_budget_tokens: u32,
    pub extended_thinking_max_output_tokens: u32,
    pub enable_vision: bool,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            model: "gpt-5.2-2025-12-11".into(),
            fallback_models: vec!["o4-mini".into()],
            model_reasoning: String::new(),
            model_weak: String::new(),
            git_provider: "github".into(),
            publish_output: true,
            publish_output_progress: true,
            verbosity_level: 0,
            use_extra_bad_extensions: false,
            log_level: "DEBUG".into(),
            use_wiki_settings_file: true,
            use_repo_settings_file: true,
            use_global_settings_file: true,
            disable_auto_feedback: false,
            ai_timeout: 120,
            skip_keys: vec![],
            custom_reasoning_model: false,
            response_language: "en-US".into(),
            max_description_tokens: 500,
            max_commits_tokens: 500,
            max_model_tokens: 32_000,
            custom_model_max_tokens: -1,
            model_token_count_estimate_factor: 0.3,
            patch_extension_skip_types: vec![".md".into(), ".txt".into()],
            allow_dynamic_context: true,
            max_extra_lines_before_dynamic_context: 10,
            patch_extra_lines_before: 5,
            patch_extra_lines_after: 1,
            secret_provider: String::new(),
            cli_mode: false,
            ai_disclaimer_title: String::new(),
            ai_disclaimer: String::new(),
            output_relevant_configurations: false,
            large_patch_policy: "clip".into(),
            duplicate_prompt_examples: false,
            seed: -1,
            temperature: 0.2,
            add_repo_metadata: false,
            add_repo_metadata_file_list: vec!["AGENTS.MD".into(), "CLAUDE.MD".into()],
            ignore_pr_title: vec!["^\\[Auto\\]".into(), "^Auto".into()],
            ignore_pr_target_branches: vec![],
            ignore_pr_source_branches: vec![],
            ignore_pr_labels: vec![],
            ignore_pr_authors: vec![],
            ignore_repositories: vec![],
            ignore_language_framework: vec![],
            is_auto_command: false,
            enable_ai_metadata: false,
            reasoning_effort: "medium".into(),
            enable_auto_approval: false,
            auto_approve_for_low_review_effort: -1,
            auto_approve_for_no_suggestions: false,
            ensure_ticket_compliance: false,
            enable_claude_extended_thinking: false,
            extended_thinking_budget_tokens: 2048,
            extended_thinking_max_output_tokens: 4096,
            enable_vision: true,
        }
    }
}

// ── [pr_reviewer] ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrReviewerConfig {
    pub require_score_review: bool,
    pub require_tests_review: bool,
    pub require_estimate_effort_to_review: bool,
    pub require_can_be_split_review: bool,
    pub require_security_review: bool,
    pub require_estimate_contribution_time_cost: bool,
    pub require_todo_scan: bool,
    pub require_ticket_analysis_review: bool,
    pub publish_output_no_suggestions: bool,
    pub persistent_comment: bool,
    pub extra_instructions: String,
    pub num_max_findings: u32,
    pub final_update_message: bool,
    pub enable_review_labels_security: bool,
    pub enable_review_labels_effort: bool,
    pub require_all_thresholds_for_incremental_review: bool,
    pub minimal_commits_for_incremental_review: u32,
    pub minimal_minutes_for_incremental_review: u32,
    pub enable_intro_text: bool,
    pub enable_help_text: bool,
}

impl Default for PrReviewerConfig {
    fn default() -> Self {
        Self {
            require_score_review: false,
            require_tests_review: true,
            require_estimate_effort_to_review: true,
            require_can_be_split_review: false,
            require_security_review: true,
            require_estimate_contribution_time_cost: false,
            require_todo_scan: false,
            require_ticket_analysis_review: true,
            publish_output_no_suggestions: true,
            persistent_comment: true,
            extra_instructions: String::new(),
            num_max_findings: 3,
            final_update_message: true,
            enable_review_labels_security: true,
            enable_review_labels_effort: true,
            require_all_thresholds_for_incremental_review: false,
            minimal_commits_for_incremental_review: 0,
            minimal_minutes_for_incremental_review: 0,
            enable_intro_text: true,
            enable_help_text: false,
        }
    }
}

// ── [pr_description] ────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrDescriptionConfig {
    pub publish_labels: bool,
    pub add_original_user_description: bool,
    pub generate_ai_title: bool,
    pub use_bullet_points: bool,
    pub extra_instructions: String,
    pub enable_pr_type: bool,
    pub final_update_message: bool,
    pub enable_help_text: bool,
    pub enable_help_comment: bool,
    pub enable_pr_diagram: bool,
    pub publish_description_as_comment: bool,
    pub publish_description_as_comment_persistent: bool,
    pub enable_semantic_files_types: bool,
    pub collapsible_file_list: BoolOrString,
    pub collapsible_file_list_threshold: u32,
    pub inline_file_summary: BoolOrString,
    pub use_description_markers: bool,
    pub include_generated_by_header: bool,
    pub enable_large_pr_handling: bool,
    pub max_ai_calls: u32,
    pub async_ai_calls: bool,
}

impl Default for PrDescriptionConfig {
    fn default() -> Self {
        Self {
            publish_labels: false,
            add_original_user_description: true,
            generate_ai_title: false,
            use_bullet_points: true,
            extra_instructions: String::new(),
            enable_pr_type: true,
            final_update_message: true,
            enable_help_text: false,
            enable_help_comment: false,
            enable_pr_diagram: true,
            publish_description_as_comment: false,
            publish_description_as_comment_persistent: true,
            enable_semantic_files_types: true,
            collapsible_file_list: BoolOrString::Str("adaptive".into()),
            collapsible_file_list_threshold: 6,
            inline_file_summary: BoolOrString::Bool(false),
            use_description_markers: false,
            include_generated_by_header: true,
            enable_large_pr_handling: true,
            max_ai_calls: 4,
            async_ai_calls: true,
        }
    }
}

// ── [pr_questions] ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrQuestionsConfig {
    pub enable_help_text: bool,
    pub use_conversation_history: bool,
}

impl Default for PrQuestionsConfig {
    fn default() -> Self {
        Self {
            enable_help_text: false,
            use_conversation_history: true,
        }
    }
}

// ── [pr_code_suggestions] ───────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrCodeSuggestionsConfig {
    pub commitable_code_suggestions: bool,
    pub dual_publishing_score_threshold: i32,
    pub focus_only_on_problems: bool,
    pub extra_instructions: String,
    pub enable_help_text: bool,
    pub enable_chat_text: bool,
    pub persistent_comment: bool,
    pub max_history_len: u32,
    pub publish_output_no_suggestions: bool,
    pub apply_suggestions_checkbox: bool,
    pub suggestions_score_threshold: u32,
    pub new_score_mechanism: bool,
    pub new_score_mechanism_th_high: u32,
    pub new_score_mechanism_th_medium: u32,
    pub auto_extended_mode: bool,
    pub num_code_suggestions_per_chunk: u32,
    pub num_best_practice_suggestions: u32,
    pub max_number_of_calls: u32,
    pub parallel_calls: bool,
    pub final_clip_factor: f32,
    pub decouple_hunks: bool,
    pub demand_code_suggestions_self_review: bool,
    pub code_suggestions_self_review_text: String,
    pub approve_pr_on_self_review: bool,
    pub fold_suggestions_on_self_review: bool,
    pub publish_post_process_suggestion_impact: bool,
    pub wiki_page_accepted_suggestions: bool,
    pub allow_thumbs_up_down: bool,
}

impl Default for PrCodeSuggestionsConfig {
    fn default() -> Self {
        Self {
            commitable_code_suggestions: false,
            dual_publishing_score_threshold: -1,
            focus_only_on_problems: true,
            extra_instructions: String::new(),
            enable_help_text: false,
            enable_chat_text: false,
            persistent_comment: true,
            max_history_len: 4,
            publish_output_no_suggestions: true,
            apply_suggestions_checkbox: true,
            suggestions_score_threshold: 0,
            new_score_mechanism: true,
            new_score_mechanism_th_high: 9,
            new_score_mechanism_th_medium: 7,
            auto_extended_mode: true,
            num_code_suggestions_per_chunk: 3,
            num_best_practice_suggestions: 1,
            max_number_of_calls: 3,
            parallel_calls: true,
            final_clip_factor: 0.8,
            decouple_hunks: false,
            demand_code_suggestions_self_review: false,
            code_suggestions_self_review_text: "**Author self-review**: I have reviewed the PR code suggestions, and addressed the relevant ones.".into(),
            approve_pr_on_self_review: false,
            fold_suggestions_on_self_review: true,
            publish_post_process_suggestion_impact: true,
            wiki_page_accepted_suggestions: true,
            allow_thumbs_up_down: false,
        }
    }
}

// ── [pr_custom_prompt] ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrCustomPromptConfig {
    pub prompt: String,
    pub suggestions_score_threshold: u32,
    pub num_code_suggestions_per_chunk: u32,
    pub self_reflect_on_custom_suggestions: bool,
    pub enable_help_text: bool,
}

impl Default for PrCustomPromptConfig {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            suggestions_score_threshold: 0,
            num_code_suggestions_per_chunk: 3,
            self_reflect_on_custom_suggestions: true,
            enable_help_text: false,
        }
    }
}

// ── [pr_add_docs] ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrAddDocsConfig {
    pub extra_instructions: String,
    pub docs_style: String,
    pub file: String,
    pub class_name: String,
}

impl Default for PrAddDocsConfig {
    fn default() -> Self {
        Self {
            extra_instructions: String::new(),
            docs_style: "Sphinx".into(),
            file: String::new(),
            class_name: String::new(),
        }
    }
}

// ── [pr_update_changelog] ───────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrUpdateChangelogConfig {
    pub push_changelog_changes: bool,
    pub extra_instructions: String,
    pub add_pr_link: bool,
    pub skip_ci_on_push: bool,
}

impl Default for PrUpdateChangelogConfig {
    fn default() -> Self {
        Self {
            push_changelog_changes: false,
            extra_instructions: String::new(),
            add_pr_link: true,
            skip_ci_on_push: true,
        }
    }
}

// ── [pr_analyze] ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrAnalyzeConfig {
    pub enable_help_text: bool,
}

impl Default for PrAnalyzeConfig {
    fn default() -> Self {
        Self {
            enable_help_text: true,
        }
    }
}

// ── [pr_test] ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrTestConfig {
    pub extra_instructions: String,
    pub testing_framework: String,
    pub num_tests: u32,
    pub avoid_mocks: bool,
    pub file: String,
    pub class_name: String,
    pub enable_help_text: bool,
}

impl Default for PrTestConfig {
    fn default() -> Self {
        Self {
            extra_instructions: String::new(),
            testing_framework: String::new(),
            num_tests: 3,
            avoid_mocks: true,
            file: String::new(),
            class_name: String::new(),
            enable_help_text: false,
        }
    }
}

// ── [pr_improve_component] ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrImproveComponentConfig {
    pub num_code_suggestions: u32,
    pub extra_instructions: String,
    pub file: String,
    pub class_name: String,
}

impl Default for PrImproveComponentConfig {
    fn default() -> Self {
        Self {
            num_code_suggestions: 4,
            extra_instructions: String::new(),
            file: String::new(),
            class_name: String::new(),
        }
    }
}

// ── [checks] ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ChecksConfig {
    pub enable_auto_checks_feedback: bool,
    pub excluded_checks_list: Vec<String>,
    pub persistent_comment: bool,
    pub enable_help_text: bool,
    pub final_update_message: bool,
}

impl Default for ChecksConfig {
    fn default() -> Self {
        Self {
            enable_auto_checks_feedback: true,
            excluded_checks_list: vec!["lint".into()],
            persistent_comment: true,
            enable_help_text: true,
            final_update_message: false,
        }
    }
}

// ── [pr_help] ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrHelpConfig {
    pub force_local_db: bool,
    pub num_retrieved_snippets: u32,
}

impl Default for PrHelpConfig {
    fn default() -> Self {
        Self {
            force_local_db: false,
            num_retrieved_snippets: 5,
        }
    }
}

// ── [pr_config] ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct PrConfigSection {}

// ── [pr_help_docs] ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrHelpDocsConfig {
    pub repo_url: String,
    pub repo_default_branch: String,
    pub docs_path: String,
    pub exclude_root_readme: bool,
    pub supported_doc_exts: Vec<String>,
    pub enable_help_text: bool,
}

impl Default for PrHelpDocsConfig {
    fn default() -> Self {
        Self {
            repo_url: String::new(),
            repo_default_branch: "main".into(),
            docs_path: "docs".into(),
            exclude_root_readme: false,
            supported_doc_exts: vec![".md".into(), ".mdx".into(), ".rst".into()],
            enable_help_text: false,
        }
    }
}

// ── Git provider configs ────────────────────────────────────────────

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GithubConfig {
    pub deployment_type: String,
    pub ratelimit_retries: u32,
    pub base_url: String,
    pub publish_inline_comments_fallback_with_verification: bool,
    pub try_fix_invalid_inline_comments: bool,
    pub app_name: String,
    pub ignore_bot_pr: bool,
    /// User token for authentication (set via GITHUB_TOKEN env var).
    pub user_token: String,
    /// GitHub App ID (for app deployment type).
    pub app_id: u64,
    /// GitHub App RSA private key PEM (for app deployment type).
    pub private_key: String,
    /// GitHub App webhook secret.
    pub webhook_secret: String,
}

impl std::fmt::Debug for GithubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubConfig")
            .field("deployment_type", &self.deployment_type)
            .field("ratelimit_retries", &self.ratelimit_retries)
            .field("base_url", &self.base_url)
            .field("app_name", &self.app_name)
            .field("app_id", &self.app_id)
            .field("user_token", &redact(&self.user_token))
            .field("private_key", &redact(&self.private_key))
            .field("webhook_secret", &redact(&self.webhook_secret))
            .finish()
    }
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            deployment_type: "user".into(),
            ratelimit_retries: 5,
            base_url: "https://api.github.com".into(),
            publish_inline_comments_fallback_with_verification: true,
            try_fix_invalid_inline_comments: true,
            app_name: "pr-agent".into(),
            ignore_bot_pr: true,
            user_token: String::new(),
            app_id: 0,
            private_key: String::new(),
            webhook_secret: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct GithubActionConfig {}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GithubAppConfig {
    pub bot_user: String,
    pub override_deployment_type: bool,
    pub handle_pr_actions: Vec<String>,
    pub pr_commands: Vec<String>,
    pub handle_push_trigger: bool,
    pub push_trigger_ignore_bot_commits: bool,
    pub push_trigger_ignore_merge_commits: bool,
    pub push_trigger_wait_for_initial_review: bool,
    pub push_trigger_pending_tasks_backlog: bool,
    pub push_trigger_pending_tasks_ttl: u64,
    pub push_commands: Vec<String>,
}

impl Default for GithubAppConfig {
    fn default() -> Self {
        Self {
            bot_user: "github-actions[bot]".into(),
            override_deployment_type: true,
            handle_pr_actions: vec![
                "opened".into(),
                "reopened".into(),
                "ready_for_review".into(),
            ],
            pr_commands: vec![
                "/describe --pr_description.final_update_message=false".into(),
                "/review".into(),
                "/improve".into(),
            ],
            handle_push_trigger: false,
            push_trigger_ignore_bot_commits: true,
            push_trigger_ignore_merge_commits: true,
            push_trigger_wait_for_initial_review: true,
            push_trigger_pending_tasks_backlog: true,
            push_trigger_pending_tasks_ttl: 300,
            push_commands: vec!["/describe".into(), "/review".into()],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GitlabConfig {
    pub url: String,
    pub expand_submodule_diffs: bool,
    pub pr_commands: Vec<String>,
    pub handle_push_trigger: bool,
    pub push_commands: Vec<String>,
}

impl Default for GitlabConfig {
    fn default() -> Self {
        Self {
            url: "https://gitlab.com".into(),
            expand_submodule_diffs: false,
            pr_commands: vec![
                "/describe --pr_description.final_update_message=false".into(),
                "/review".into(),
                "/improve".into(),
            ],
            handle_push_trigger: false,
            push_commands: vec!["/describe".into(), "/review".into()],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GiteaConfig {
    pub url: String,
    pub handle_push_trigger: bool,
    pub pr_commands: Vec<String>,
    pub push_commands: Vec<String>,
}

impl Default for GiteaConfig {
    fn default() -> Self {
        Self {
            url: "https://gitea.com".into(),
            handle_push_trigger: false,
            pr_commands: vec!["/describe".into(), "/review".into(), "/improve".into()],
            push_commands: vec!["/describe".into(), "/review".into()],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BitbucketAppConfig {
    pub pr_commands: Vec<String>,
    pub avoid_full_files: bool,
}

impl Default for BitbucketAppConfig {
    fn default() -> Self {
        Self {
            pr_commands: vec![
                "/describe --pr_description.final_update_message=false".into(),
                "/review".into(),
                "/improve --pr_code_suggestions.commitable_code_suggestions=true".into(),
            ],
            avoid_full_files: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BitbucketServerConfig {
    pub url: String,
    pub pr_commands: Vec<String>,
}

impl Default for BitbucketServerConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            pr_commands: vec![
                "/describe --pr_description.final_update_message=false".into(),
                "/review".into(),
                "/improve --pr_code_suggestions.commitable_code_suggestions=true".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct LocalConfig {}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct GerritConfig {}

// ── Service configs ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LitellmConfig {
    pub enable_callbacks: bool,
    pub success_callback: Vec<String>,
    pub failure_callback: Vec<String>,
    pub service_callback: Vec<String>,
    pub model_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrSimilarIssueConfig {
    pub skip_comments: bool,
    pub force_update_dataset: bool,
    pub max_issues_to_scan: u32,
    pub vectordb: String,
}

impl Default for PrSimilarIssueConfig {
    fn default() -> Self {
        Self {
            skip_comments: false,
            force_update_dataset: false,
            max_issues_to_scan: 500,
            vectordb: "pinecone".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PrFindSimilarComponentConfig {
    pub class_name: String,
    pub file: String,
    pub search_from_org: bool,
    pub allow_fallback_less_words: bool,
    pub number_of_keywords: u32,
    pub number_of_results: u32,
}

impl Default for PrFindSimilarComponentConfig {
    fn default() -> Self {
        Self {
            class_name: String::new(),
            file: String::new(),
            search_from_org: false,
            allow_fallback_less_words: true,
            number_of_keywords: 5,
            number_of_results: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct PineconeConfig {
    pub api_key: String,
    pub environment: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LancedbConfig {
    pub uri: String,
}

impl Default for LancedbConfig {
    fn default() -> Self {
        Self {
            uri: "./lancedb".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct QdrantConfig {
    pub url: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BestPracticesConfig {
    pub content: String,
    pub organization_name: String,
    pub max_lines_allowed: u32,
    pub enable_global_best_practices: bool,
}

impl Default for BestPracticesConfig {
    fn default() -> Self {
        Self {
            content: String::new(),
            organization_name: String::new(),
            max_lines_allowed: 800,
            enable_global_best_practices: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AutoBestPracticesConfig {
    pub enable_auto_best_practices: bool,
    pub utilize_auto_best_practices: bool,
    pub extra_instructions: String,
    pub content: String,
    pub max_patterns: u32,
}

impl Default for AutoBestPracticesConfig {
    fn default() -> Self {
        Self {
            enable_auto_best_practices: true,
            utilize_auto_best_practices: true,
            extra_instructions: String::new(),
            content: String::new(),
            max_patterns: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AzureDevopsConfig {
    pub default_comment_status: String,
}

impl Default for AzureDevopsConfig {
    fn default() -> Self {
        Self {
            default_comment_status: "closed".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AzureDevopsServerConfig {
    pub pr_commands: Vec<String>,
}

impl Default for AzureDevopsServerConfig {
    fn default() -> Self {
        Self {
            pr_commands: vec!["/describe".into(), "/review".into(), "/improve".into()],
        }
    }
}

// ── Prompt templates ────────────────────────────────────────────────

/// A Jinja2 prompt template pair (system + user) loaded from TOML.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct PromptTemplate {
    pub system: String,
    pub user: String,
}

// ── [custom_labels.*] ────────────────────────────────────────────────

/// Entry for a custom label defined in `[custom_labels.label_name]`.
///
/// Parsed from the `[custom_labels.label_name]` TOML section format:
/// ```toml
/// [custom_labels."Bug fix"]
/// description = "Changes to fix a bug"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct CustomLabelEntry {
    pub description: String,
}

// ── [ignore] ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct IgnoreConfig {
    pub glob: Vec<String>,
    pub regex: Vec<String>,
}

// ── Secrets ─────────────────────────────────────────────────────────

#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct OpenAiSecrets {
    pub key: String,
    pub org: String,
    pub api_type: String,
    pub api_version: String,
    pub api_base: String,
    pub deployment_id: String,
}

impl std::fmt::Debug for OpenAiSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiSecrets")
            .field("key", &redact(&self.key))
            .field("org", &self.org)
            .field("api_type", &self.api_type)
            .field("api_base", &self.api_base)
            .field("deployment_id", &self.deployment_id)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct AnthropicSecrets {
    pub key: String,
}

impl std::fmt::Debug for AnthropicSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicSecrets")
            .field("key", &redact(&self.key))
            .finish()
    }
}
