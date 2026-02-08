# pr-agent-rs

AI-powered pull request agent — automated code review, PR descriptions, and improvement suggestions.
Same config format, same prompts, same GitHub App webhook interface — fast and lightweight.

## Quick start

```bash
cargo build
cargo test              # 106 tests (1 expected failure with .secrets.toml present)
cargo run -- --pr-url=https://github.com/owner/repo/pull/123 review
cargo run -- serve      # webhook server on port 3000
```

## Project structure

```
src/
├── main.rs / lib.rs        # Entry point, module exports
├── cli.rs                  # clap CLI: commands, config overrides, forbidden keys
├── error.rs                # PrAgentError enum (thiserror)
├── util.rs                 # Regex cache macro, string helpers
├── ai/                     # LLM integration
│   ├── mod.rs              # AiHandler trait (async_trait, object-safe)
│   ├── openai.rs           # OpenAI-compatible handler (covers LiteLLM, Ollama, etc.)
│   ├── token.rs            # tiktoken-rs o200k_base counting, model limits, budget
│   └── types.rs            # ChatResponse, FinishReason, ModelCapabilities
├── config/                 # figment-based config
│   ├── loader.rs           # Load/merge layers, global + task_local state
│   └── types.rs            # Settings structs (~200 keys)
├── git/                    # Git provider abstraction
│   ├── mod.rs              # GitProvider trait (publish_comment, get_diff_files, etc.)
│   ├── github.rs           # GitHub REST API (JWT App auth, installation tokens)
│   ├── types.rs            # FilePatchInfo, InlineComment, CodeSuggestion, CommentId
│   └── url_parser.rs       # Parse PR URLs (github.com, GHE)
├── processing/             # Diff pipeline
│   ├── diff.rs             # Hunk parsing, line numbering, pr-agent format
│   ├── patch.rs            # Context extension (extra lines before/after)
│   ├── filter.rs           # File filtering (extensions, globs, regex, binary)
│   └── compression.rs      # Token-aware diff compression
├── output/                 # AI response → markdown
│   ├── markdown.rs         # Table sanitization, code blocks, collapsibles
│   ├── yaml_parser.rs      # Parse structured YAML from AI responses
│   ├── review_formatter.rs # Review → markdown
│   ├── describe_formatter.rs # Description → markdown + PR body
│   └── improve_formatter.rs  # Suggestions → markdown + inline comments
├── template/               # minijinja (strict undefined)
│   └── render.rs           # Render PromptTemplate with vars
├── tools/                  # Core AI tools
│   ├── mod.rs              # parse_command() + handle_command() dispatcher
│   ├── review.rs           # PRReviewer: diff → AI → formatted review comment
│   ├── describe.rs         # PRDescription: diff → AI → PR body update
│   └── improve.rs          # PRCodeSuggestions: diff → AI → inline suggestions
└── server/                 # Axum webhook server
    ├── mod.rs              # start_server() on configurable port
    └── webhook.rs          # HMAC-SHA256 verification, event routing, background tasks
settings/                   # Embedded TOML (include_str! at compile time)
├── configuration.toml      # All defaults
├── ignore.toml             # File patterns to skip
├── custom_labels.toml      # Label definitions
├── language_extensions.toml
├── pr_*_prompts.toml       # Prompt templates for each tool
└── code_suggestions/       # Improve prompt variants
```

## Architecture decisions

- **Single crate**, no workspace — keeps things simple for now.
- **No octocrab for API calls** — raw `reqwest` for full control over GitHub REST API (JWT auth, retries, rate limits). `octocrab` is a dependency only for types.
- **Config via figment** — merges embedded TOML defaults → `.secrets.toml` → repo `.pr_agent.toml` → CLI args → env vars.
- **Global + per-request settings** — `RwLock<Option<Arc<Settings>>>` global singleton, `tokio::task_local!` for webhook request isolation.
- **Templates via minijinja** — `UndefinedBehavior::Strict` for strict variable resolution (missing vars are hard errors).
- **AI handler is a trait** — `AiHandler` is object-safe (`Arc<dyn AiHandler>`) for future provider swapping.
- **Error type** — single `PrAgentError` enum with `thiserror`, includes `is_retryable()`.

## Code guidelines

### General

- Follow the pr-agent behavior exactly. When in doubt, check the original pr-agent source.
- Keep it DRY — reuse config types, markdown helpers, and the GitProvider trait.
- No over-engineering. If a tool isn't implemented yet, don't add stubs or abstractions for it.
- Tests go in `#[cfg(test)] mod tests` inside each source file, not in a separate `tests/` directory.

### Rust style

- Edition 2024. Use modern Rust idioms.
- `thiserror` for error enums, `?` for propagation. No `.unwrap()` in non-test code.
- `tracing` for logging (`tracing::debug!`, `tracing::warn!`, etc.), not `println!`.
- `Arc<dyn Trait>` for trait objects passed across async boundaries.
- Prefer `&str` parameters over `String` when the callee doesn't need ownership.
- Use `r#"..."#` for raw strings that contain quotes.

### Config

- All config fields must match the original pr-agent TOML keys for retrocompatibility.
- New settings go in `config/types.rs` inside the appropriate section struct.
- Embedded defaults in `settings/*.toml` — these are `include_str!`'d at compile time.
- Figment merge order matters: later sources override earlier ones.
- `patch_extra_lines_before/after` are `usize`, not `u32`.
- `enable_custom_labels` lives in `custom_labels.toml`, not `configuration.toml`.

### Templates

- Template variables must exactly match what the `.toml` prompt templates expect.
- Review diff needs `add_line_numbers=true`; improve diff needs `add_line_numbers=false`.
- Always test with `UndefinedBehavior::Strict` — missing vars cause hard errors, not silent empty strings.

### AI responses

- `serde_yaml` treats `null`/empty as valid — always check `!val.is_null()` after parse.
- AI may return mermaid diagrams already fenced in triple backticks — check before wrapping.
- Retry with exponential backoff (2s, 4s, 8s) on transient errors.

### GitHub provider

- Persistent comment search uses `.starts_with()` on the marker, not `.contains()`.
- Progress comments: capture `CommentId` from `publish_comment`, clean up with `remove_comment(id)` in all paths (including errors — use `run_inner()` pattern).
- Markdown table cells must sanitize newlines (`<br>`) and pipes (`\|`).
- Review fields: `issue_header`/`issue_content`/`start_line`/`end_line` — NOT `header`/`content`/`relevant_line`.
- "Possible Bug" is renamed to "Possible Issue" in review output.
- Inline comment line numbers with `lines_start/end == 0` should fall back to table mode.

### CLI

- clap uses kebab-case: `--pr-url`, not `--pr_url`.
- Security-sensitive keys (`openai.key`, `github.app_id`, `private_key`, etc.) are forbidden as CLI overrides.

### Testing

- Run `cargo test` before committing. The `test_load_default_settings` test is expected to fail if `.secrets.toml` exists in the working directory.
- Test names should describe the behavior, not the implementation.
- Use `#[test]` for sync tests, `#[tokio::test]` for async.

## Config merge order (highest precedence last)

```
1. Embedded TOML defaults (settings/*.toml)
2. .secrets.toml (filesystem, optional)
3. Repo .pr_agent.toml (fetched from git provider)
4. CLI args (--section.key=value)
5. Environment variables
```

## Common commands

```bash
cargo test                          # run all tests
cargo test test_name                # run specific test
cargo clippy                        # lint
cargo run -- --pr-url=URL review    # review a PR
cargo run -- --pr-url=URL describe  # generate PR description
cargo run -- --pr-url=URL improve   # suggest code improvements
cargo run -- serve                  # start webhook server
RUST_LOG=debug cargo run -- ...     # enable debug logging
```
