//! Built-in tools (pi: coding-agent/src/core/tools).

pub mod bash;
pub mod read;
pub mod truncate;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};

pub use bash::BashTool;
pub use read::ReadFileTool;

use crate::agent::Tool;

/// Name of the built-in shell tool.
pub const BASH: &str = "bash";
/// Name of the built-in file reading tool.
pub const READ_FILE: &str = "read_file";

/// Names accepted by [`builtin_tools`].
pub const BUILTIN_TOOL_NAMES: &[&str] = &[BASH, READ_FILE];

/// Build built-in tools by name, in the given order, without duplicates. `hidden_env` names
/// environment variables that bash commands must not see, such as the API key variable.
pub fn builtin_tools(names: &[String], bash_timeout: Duration, hidden_env: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() || tools.iter().any(|tool| tool.name() == name) {
            continue;
        }
        let tool: Arc<dyn Tool> = match name {
            BASH => Arc::new(BashTool {
                default_timeout: bash_timeout,
                max_timeout: bash_timeout.max(BashTool::default().max_timeout),
                hidden_env: hidden_env.to_vec(),
                ..BashTool::default()
            }),
            READ_FILE => Arc::new(ReadFileTool::default()),
            other => bail!("unknown tool {other:?}; available: {}", BUILTIN_TOOL_NAMES.join(", ")),
        };
        tools.push(tool);
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tools: &[Arc<dyn Tool>]) -> Vec<&str> {
        tools.iter().map(|tool| tool.name()).collect()
    }

    #[test]
    fn builds_tools_in_order_without_duplicates_or_blanks() {
        let requested = ["read_file", " bash ", "read_file", ""].map(String::from);
        let tools = builtin_tools(&requested, Duration::from_secs(5), &[]).unwrap();
        assert_eq!(names(&tools), ["read_file", "bash"]);
    }

    #[test]
    fn rejects_unknown_tool_names() {
        let error = builtin_tools(&["curl".to_string()], Duration::from_secs(5), &[])
            .err()
            .unwrap()
            .to_string();
        assert!(error.starts_with("unknown tool \"curl\""), "{error}");
    }

    #[test]
    fn bash_gets_the_default_timeout_and_keeps_the_larger_maximum() {
        let tools = builtin_tools(&["bash".to_string()], Duration::from_secs(7), &[]).unwrap();
        let description = tools[0].definition().description;
        assert!(
            description.contains("after 7s unless timeout_secs is set (max 600s)"),
            "{description}"
        );
    }
}
