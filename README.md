# pr-agent-rs

AI-powered pull request agent for automated code review, PR descriptions, and improvement suggestions.

Built as a fast, lightweight alternative to [pr-agent](https://github.com/Qodo-ai/pr-agent) — same config format, same prompts, same GitHub App webhook interface.

## Features

- **Review** — AI-generated code review with inline comments, security analysis, and effort estimation
- **Describe** — Auto-generate PR titles, descriptions, file change tables, and mermaid diagrams
- **Improve** — Code improvement suggestions with committable inline diffs and self-review checkboxes
- **Webhook server** — GitHub App webhook handler with HMAC-SHA256 verification
- **Flexible AI backend** — OpenAI-compatible API (works with OpenAI, LiteLLM, Ollama, Groq, Azure, and more)
- **Layered configuration** — Embedded defaults, org-level, repo-level, CLI args, and environment variables

## Quick start

```bash
# Build
cargo build --release

# Run against a specific PR
cargo run -- --pr-url=https://github.com/owner/repo/pull/123 review
cargo run -- --pr-url=https://github.com/owner/repo/pull/123 describe
cargo run -- --pr-url=https://github.com/owner/repo/pull/123 improve

# Start the webhook server (port 3000, or set PORT env var)
cargo run -- serve
```

## Configuration

pr-agent-rs uses a layered configuration system (highest precedence last):

1. **Embedded defaults** — `settings/*.toml` compiled into the binary
2. **Secrets file** — `.secrets.toml` in the working directory (git-ignored)
3. **Org-level config** — `.pr_agent.toml` from `{owner}/pr-agent-settings` repo
4. **Repo-level config** — `.pr_agent.toml` from the PR's repository
5. **CLI overrides** — `--config.key=value` arguments
6. **Environment variables** — `OPENAI_API_KEY`, `GITHUB_TOKEN`, etc.

### Minimal `.secrets.toml`

```toml
[openai]
key = "sk-..."

[github]
deployment_type = "app"
app_id = 123456
private_key = """
-----BEGIN RSA PRIVATE KEY-----
...
-----END RSA PRIVATE KEY-----
"""
webhook_secret = "your-webhook-secret"
```

### Repo-level `.pr_agent.toml`

```toml
[pr_reviewer]
num_max_findings = 5
extra_instructions = "Focus on security and error handling"

[pr_description]
generate_ai_title = true
enable_pr_diagram = true

[pr_code_suggestions]
num_code_suggestions = 4
```

## GitHub App Setup

1. Create a GitHub App with the following permissions:
   - **Pull requests**: Read & Write
   - **Issues**: Read & Write (for comments)
   - **Contents**: Read (for file access)
2. Subscribe to the **Pull request** webhook event
3. Set the webhook URL to `https://your-server/api/v1/github_webhooks`
4. Generate a private key and add it to `.secrets.toml`

## Environment Variables

| Variable | Description |
|----------|-------------|
| `OPENAI_API_KEY` | API key for the AI model provider |
| `GITHUB_TOKEN` | GitHub personal access token (alternative to App auth) |
| `PORT` | Webhook server port (default: 3000) |
| `RUST_LOG` | Log level (e.g., `debug`, `info`, `warn`) |

## Development

```bash
cargo test              # Run all 170 tests
cargo clippy            # Lint
cargo build --release   # Optimized build (stripped, LTO)
RUST_LOG=debug cargo run -- --pr-url=URL review  # Debug logging
```

## License

This project is licensed under the [GNU Affero General Public License v3.0](LICENSE).
