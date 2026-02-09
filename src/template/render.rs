use std::collections::HashMap;
use std::sync::LazyLock;

use minijinja::{Environment, UndefinedBehavior, Value};

use crate::config::types::PromptTemplate;
use crate::error::PrAgentError;

/// Shared minijinja environment with strict undefined behavior.
static JINJA_ENV: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env
});

/// Rendered prompt pair ready for the AI model.
#[derive(Debug, Clone)]
pub struct RenderedPrompt {
    pub system: String,
    pub user: String,
}

/// Render a prompt template pair with the given variables.
///
/// Takes ownership of `vars` to avoid cloning large Values (e.g. the diff
/// string, which can be 100 KB+). The context Value is built once from
/// the owned map and shared across both template renders via cheap Arc clone.
pub fn render_prompt(
    template: &PromptTemplate,
    vars: HashMap<String, Value>,
) -> Result<RenderedPrompt, PrAgentError> {
    let env = &*JINJA_ENV;

    // Build context once — moves Values instead of cloning them.
    // Value::clone() is cheap (Arc-based internally).
    let ctx = Value::from_iter(vars);

    let system = render_template(env, "system", &template.system, &ctx)?;
    let user = render_template(env, "user", &template.user, &ctx)?;

    Ok(RenderedPrompt { system, user })
}

/// Render a single template string with a pre-built context.
fn render_template(
    env: &Environment,
    name: &str,
    template_str: &str,
    ctx: &Value,
) -> Result<String, PrAgentError> {
    let tmpl = env
        .template_from_str(template_str)
        .map_err(|e| PrAgentError::Other(format!("failed to parse {name} template: {e}")))?;

    tmpl.render(ctx.clone())
        .map_err(|e| PrAgentError::Other(format!("failed to render {name} template: {e}")))
}

/// Convenience: render a prompt from raw system/user strings (not from Settings).
#[allow(dead_code)]
pub fn render_prompt_strings(
    system_template: &str,
    user_template: &str,
    vars: HashMap<String, Value>,
) -> Result<RenderedPrompt, PrAgentError> {
    let template = PromptTemplate {
        system: system_template.to_string(),
        user: user_template.to_string(),
    };
    render_prompt(&template, vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_simple_variables() {
        let template = PromptTemplate {
            system: "Review PR titled '{{ title }}' on branch '{{ branch }}'.".into(),
            user: "Diff:\n{{ diff }}".into(),
        };

        let mut vars = HashMap::new();
        vars.insert("title".into(), Value::from("Fix login bug"));
        vars.insert("branch".into(), Value::from("feature/login"));
        vars.insert("diff".into(), Value::from("+new line\n-old line"));

        let result = render_prompt(&template, vars).unwrap();
        assert!(result.system.contains("Fix login bug"));
        assert!(result.system.contains("feature/login"));
        assert!(result.user.contains("+new line"));
    }

    #[test]
    fn test_render_conditionals() {
        let template = PromptTemplate {
            system: "{%- if extra_instructions %}Extra: {{ extra_instructions }}{% endif %}".into(),
            user: "Hello".into(),
        };

        // With extra_instructions set
        let mut vars = HashMap::new();
        vars.insert(
            "extra_instructions".into(),
            Value::from("Focus on security"),
        );
        let result = render_prompt(&template, vars.clone()).unwrap();
        assert!(result.system.contains("Focus on security"));

        // With empty string (falsy)
        vars.insert("extra_instructions".into(), Value::from(""));
        let result = render_prompt(&template, vars).unwrap();
        assert!(!result.system.contains("Extra:"));
    }

    #[test]
    fn test_render_strict_undefined_fails() {
        let template = PromptTemplate {
            system: "{{ undefined_var }}".into(),
            user: "".into(),
        };

        let vars = HashMap::new();
        let result = render_prompt(&template, vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_render_list_iteration() {
        let template = PromptTemplate {
            system: "".into(),
            user: "{%- for item in items %}{{ item }}\n{% endfor %}".into(),
        };

        let mut vars = HashMap::new();
        vars.insert("items".into(), Value::from(vec!["alpha", "beta", "gamma"]));

        let result = render_prompt(&template, vars).unwrap();
        assert!(result.user.contains("alpha"));
        assert!(result.user.contains("beta"));
        assert!(result.user.contains("gamma"));
    }

    #[test]
    fn test_render_trim_filter() {
        let template = PromptTemplate {
            system: "".into(),
            user: "{{ diff|trim }}".into(),
        };

        let mut vars = HashMap::new();
        vars.insert("diff".into(), Value::from("  content  \n\n"));

        let result = render_prompt(&template, vars).unwrap();
        assert_eq!(result.user, "content");
    }

    #[test]
    fn test_template_injection_safe() {
        // Verify that Jinja syntax in variable values is NOT evaluated (no template injection)
        let template = PromptTemplate {
            system: "Title: {{ title }}".into(),
            user: "Branch: {{ branch }}".into(),
        };

        let mut vars = HashMap::new();
        // Malicious PR title containing Jinja syntax
        vars.insert(
            "title".into(),
            Value::from("{{ config.secret }} {% for i in range(999) %}x{% endfor %}"),
        );
        vars.insert(
            "branch".into(),
            Value::from("{{ __import__('os').system('rm -rf /') }}"),
        );

        let result = render_prompt(&template, vars).unwrap();
        // Must render as literal strings, not evaluate the Jinja syntax
        assert!(result.system.contains("{{ config.secret }}"));
        assert!(result.system.contains("{% for i in range(999) %}"));
        assert!(
            result
                .user
                .contains("{{ __import__('os').system('rm -rf /') }}")
        );
    }

    #[test]
    fn test_render_real_prompt_template() {
        // Load actual settings and render pr_review_prompt with test variables
        let settings =
            crate::config::loader::load_settings(&std::collections::HashMap::new(), None, None)
                .unwrap();

        let mut vars = HashMap::new();
        vars.insert("title".into(), Value::from("Add authentication"));
        vars.insert("branch".into(), Value::from("feature/auth"));
        vars.insert("description".into(), Value::from("Adds OAuth2 support"));
        vars.insert("language".into(), Value::from("Rust"));
        vars.insert("diff".into(), Value::from("+fn login() {}"));
        vars.insert("num_pr_files".into(), Value::from(3));
        vars.insert("num_max_findings".into(), Value::from(5));
        vars.insert("require_score".into(), Value::from(false));
        vars.insert("require_tests".into(), Value::from(true));
        vars.insert(
            "require_estimate_effort_to_review".into(),
            Value::from(true),
        );
        vars.insert(
            "require_estimate_contribution_time_cost".into(),
            Value::from(false),
        );
        vars.insert("require_can_be_split_review".into(), Value::from(false));
        vars.insert("require_security_review".into(), Value::from(true));
        vars.insert("require_todo_scan".into(), Value::from(false));
        vars.insert("question_str".into(), Value::from(""));
        vars.insert("answer_str".into(), Value::from(""));
        vars.insert("extra_instructions".into(), Value::from(""));
        vars.insert("commit_messages_str".into(), Value::from("feat: add auth"));
        vars.insert("custom_labels".into(), Value::from(""));
        vars.insert("enable_custom_labels".into(), Value::from(false));
        vars.insert("is_ai_metadata".into(), Value::from(false));
        vars.insert("related_tickets".into(), Value::from(Vec::<String>::new()));
        vars.insert("duplicate_prompt_examples".into(), Value::from(false));
        vars.insert("date".into(), Value::from("2025-01-15"));
        vars.insert("best_practices_content".into(), Value::from(""));
        vars.insert("repo_metadata".into(), Value::from(""));

        let result = render_prompt(&settings.pr_review_prompt, vars).unwrap();

        // System prompt should contain the PR-Reviewer description
        assert!(result.system.contains("PR-Reviewer"));
        // User prompt should contain our diff
        assert!(result.user.contains("+fn login() {}"));
    }
}
