//! What the command line sets up for each session: where its commands run, the tools that
//! work there, the store of long tool results in its working directory, and the prompt
//! sections that describe the machine, the project and the skills.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::config::skills::TOOL_NAME;
use crate::config::{ContainerSettings, Execution, ProjectContext};
use zx_agent::agent::{SessionParts, SessionSetup};
use zx_agent::builtin::output_store::{self, OutputStore, ReadOutputTool, SpillOutputs};
use zx_agent::builtin::tools;
use zx_agent::hooks::HookChain;
use zx_agent::prompt;
use zx_agent::tools::Tool;
use zx_agent::transcript::SessionHeader;

use super::SessionPlace;
use super::execution::{check_session_outside, executor};

/// What each session of a mode gets for itself, made when the session is created or opened
/// and kept until it is closed: a container, when commands run in one, is the session's.
pub(super) struct PerSession {
    /// The tools the agent enables, by name: those of the executor are made for each
    /// session's working directory.
    pub(super) tools: Vec<String>,
    /// Which project instructions the agent reads from a session's working directory.
    pub(super) context: ProjectContext,
    /// read_skill and the catalog of its skills for the prompt, when the agent enables it.
    /// The sessions share the tool; it is listed with each session's own, which keeps its
    /// place among them.
    pub(super) skills: Option<(Arc<dyn Tool>, String)>,
    pub(super) execution: Execution,
    pub(super) container: ContainerSettings,
    /// Variables that commands on this machine must not inherit.
    pub(super) hidden_env: Vec<String>,
    pub(super) tool_timeout: Duration,
    /// Where long tool results go when the agent enables read_output.
    pub(super) tool_output: output_store::Config,
    /// Where the sessions are kept, which a container must not be able to read.
    pub(super) place: SessionPlace,
}

/// Whether `name` is a tool of this machine rather than of the executor: read_skill and
/// read_output read files here, whatever executor runs commands.
fn host_tool(name: &str) -> bool {
    name == TOOL_NAME || name == output_store::TOOL_NAME
}

#[async_trait]
impl SessionSetup for PerSession {
    /// Startup reads, and in container mode starting the container. Configuration errors
    /// are reported before the container starts.
    async fn setup(&self, header: &SessionHeader) -> Result<SessionParts> {
        let cwd = Path::new(&header.cwd);
        // A new session's directory was just resolved; one that is opened again may have
        // lost its own since.
        anyhow::ensure!(
            cwd.is_dir(),
            "the session's working directory no longer exists: {}; a session goes on in the directory it started in",
            cwd.display()
        );
        let context = self.context.section(cwd).context(
            "cannot load project instructions; an agent with discover = false under [context] skips AGENTS.md discovery",
        )?;
        let mut hooks = HookChain::new();
        let mut host: Vec<Arc<dyn Tool>> = Vec::new();
        let mut catalog = "";
        if let Some((tool, skills)) = &self.skills {
            host.push(tool.clone());
            catalog = skills;
        }
        if self.tools.iter().any(|name| name == output_store::TOOL_NAME) {
            let store = OutputStore::new(cwd, self.tool_output.clone())?;
            // The reading tool and the writer hook each get a copy of the store. The tools that
            // page their own output are exempt: a page of theirs is not stored again.
            let paging = [output_store::TOOL_NAME, TOOL_NAME, tools::READ_FILE];
            let spill = SpillOutputs::new(store.clone(), paging.map(String::from).to_vec());
            // Named like the hooks every session shares (`cli::agent::hooks`).
            hooks.push("spill_outputs", Arc::new(spill));
            host.push(Arc::new(ReadOutputTool::new(store)));
        }
        if self.execution == Execution::Container {
            check_session_outside(self.place.location(cwd).as_deref(), cwd)?;
        }
        let (executor, environment) = executor(
            self.execution,
            &self.container,
            cwd,
            &header.id,
            self.hidden_env.clone(),
        )
        .await?;
        let built_in: Vec<String> = self.tools.iter().filter(|name| !host_tool(name)).cloned().collect();
        let mut tools = tools::builtin_tools(&built_in, self.tool_timeout, &executor)?;
        tools.extend(host);
        Ok(SessionParts {
            tools,
            hooks,
            prompt_sections: vec![
                (prompt::ENVIRONMENT.to_string(), environment),
                (prompt::PROJECT_CONTEXT.to_string(), context),
                (prompt::AVAILABLE_SKILLS.to_string(), catalog.to_string()),
            ],
        })
    }
}
