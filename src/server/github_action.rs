//! GitHub Action entry point.
//!
//! Lets pr-agent-rs run as a containerized GitHub Action — zero infrastructure,
//! no GitHub App or webhook endpoint required. The Action runtime provides the
//! triggering event as a JSON file (`GITHUB_EVENT_PATH`) plus a `GITHUB_TOKEN`;
//! we read those, then hand the event to the same [`dispatch_event`] used by the
//! webhook server so the routing logic (auto describe/review/improve on a new
//! PR, slash-command handling on comments) is shared, not duplicated.
//!
//! Mirrors the Python `github_action_runner.run_action`.

use std::collections::HashMap;

use crate::config::loader::init_settings;
use crate::error::PrAgentError;
use crate::server::webhook::dispatch_event;

/// Run pr-agent in GitHub Action mode.
///
/// Reads `GITHUB_EVENT_NAME` / `GITHUB_EVENT_PATH` (and relies on `GITHUB_TOKEN`
/// from the environment, which the config loader maps to `github.user_token`),
/// parses the event payload, and dispatches it. `cli_overrides` are the
/// `--section.key=value` flags passed on the command line, forwarded so a
/// workflow can still tune config.
pub async fn run_action(cli_overrides: &HashMap<String, String>) -> Result<(), PrAgentError> {
    let event_name = std::env::var("GITHUB_EVENT_NAME")
        .map_err(|_| PrAgentError::Other("GITHUB_EVENT_NAME not set".into()))?;
    let event_path = std::env::var("GITHUB_EVENT_PATH")
        .map_err(|_| PrAgentError::Other("GITHUB_EVENT_PATH not set".into()))?;

    if std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GITHUB_USER_TOKEN"))
        .is_err()
    {
        return Err(PrAgentError::Other("GITHUB_TOKEN not set".into()));
    }

    // The Action provides a plain user/installation token, so force the
    // user-token auth path — unconditionally, so a stray CLI override can't flip
    // us onto the App-auth path that has no credentials here. The token itself is
    // picked up from GITHUB_TOKEN by the env config layer.
    let mut overrides = cli_overrides.clone();
    overrides.insert("github.deployment_type".to_string(), "user".to_string());
    init_settings(&overrides, None, None)?;

    // Load and parse the event payload the Action runtime wrote to disk.
    let payload = load_event_payload(&event_path)?;
    let action = event_action(&payload);

    tracing::info!(event = %event_name, action, "github action: dispatching event");
    dispatch_event(&event_name, action, &payload).await
}

/// Read and parse the JSON event payload from `GITHUB_EVENT_PATH`.
fn load_event_payload(event_path: &str) -> Result<serde_json::Value, PrAgentError> {
    let raw = std::fs::read_to_string(event_path)
        .map_err(|e| PrAgentError::Other(format!("failed to read GITHUB_EVENT_PATH: {e}")))?;
    serde_json::from_str(&raw)
        .map_err(|e| PrAgentError::Other(format!("failed to parse event JSON: {e}")))
}

/// The `action` field of the event payload (e.g. "opened"), or "" if absent.
fn event_action(payload: &serde_json::Value) -> &str {
    payload.get("action").and_then(|a| a.as_str()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_event_file(contents: &str) -> std::path::PathBuf {
        // A unique path per call so parallel tests don't collide. We avoid env
        // mutation entirely — only the file path matters here.
        let mut path = std::env::temp_dir();
        let unique = format!(
            "pr_agent_event_{}_{}.json",
            std::process::id(),
            contents.len()
        );
        path.push(unique);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_load_event_payload_parses_valid_json() {
        let path = temp_event_file(r#"{"action":"opened","number":7}"#);
        let payload = load_event_payload(path.to_str().unwrap()).unwrap();
        assert_eq!(event_action(&payload), "opened");
        assert_eq!(payload["number"], 7);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_event_payload_rejects_malformed_json() {
        let path = temp_event_file("{not valid json");
        let err = load_event_payload(path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("failed to parse event JSON"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_event_payload_missing_file_errors() {
        let err = load_event_payload("/nonexistent/path/event.json").unwrap_err();
        assert!(err.to_string().contains("failed to read GITHUB_EVENT_PATH"));
    }

    #[test]
    fn test_event_action_defaults_to_empty() {
        let payload = serde_json::json!({"number": 1});
        assert_eq!(event_action(&payload), "");
    }
}
