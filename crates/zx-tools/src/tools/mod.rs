//! Built-in tools (pi: coding-agent/src/core/tools).

pub mod bash;
pub mod edit;
mod edit_text;
pub mod mutation;
pub mod read;
pub mod truncate;
pub mod write;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};

pub use bash::BashTool;
pub use edit::EditFileTool;
pub use mutation::FileMutationQueue;
pub use read::ReadFileTool;
pub use write::WriteFileTool;

use crate::agent::Tool;
use crate::executor::{Executor, LocalExecutor};

/// Name of the built-in shell tool.
pub const BASH: &str = "bash";
/// Name of the built-in file reading tool.
pub const READ_FILE: &str = "read_file";
/// Name of the built-in tool that replaces text in a file.
pub const EDIT_FILE: &str = "edit_file";
/// Name of the built-in tool that creates or overwrites a file.
pub const WRITE_FILE: &str = "write_file";

/// Names accepted by [`builtin_tools`].
pub const BUILTIN_TOOL_NAMES: &[&str] = &[BASH, READ_FILE, EDIT_FILE, WRITE_FILE];

/// Build built-in tools by name, in the given order, without duplicates. `hidden_env` names
/// environment variables that bash commands must not see, such as the API key variable.
/// The tools run on this machine; see [`builtin_tools_with`] for another executor.
pub fn builtin_tools(names: &[String], bash_timeout: Duration, hidden_env: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
    builtin_tools_with(names, bash_timeout, hidden_env, Arc::new(LocalExecutor))
}

/// Like [`builtin_tools`], with the tools running commands and reading and writing files through
/// `executor`. edit_file and write_file share one [`FileMutationQueue`].
pub fn builtin_tools_with(
    names: &[String],
    bash_timeout: Duration,
    hidden_env: &[String],
    executor: Arc<dyn Executor>,
) -> Result<Vec<Arc<dyn Tool>>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let queue = Arc::new(FileMutationQueue::default());
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
                executor: executor.clone(),
                ..BashTool::default()
            }),
            READ_FILE => Arc::new(ReadFileTool {
                executor: executor.clone(),
                ..ReadFileTool::default()
            }),
            EDIT_FILE => Arc::new(EditFileTool {
                executor: executor.clone(),
                queue: queue.clone(),
                ..EditFileTool::default()
            }),
            WRITE_FILE => Arc::new(WriteFileTool {
                executor: executor.clone(),
                queue: queue.clone(),
            }),
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
