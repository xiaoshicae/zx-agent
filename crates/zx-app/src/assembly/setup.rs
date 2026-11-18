//! What the assembly sets up for each session: where its commands run, the tools that work
//! there, the store of long tool results in its working directory, and the prompt sections
//! that describe the machine, the project and the skills.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::config::{ContainerSettings, Execution, ProjectContext, READ_SKILL};
use zx_agent::agent::{SessionParts, SessionSetup};
use zx_agent::builtin::output_store::{OutputStore, OutputStoreConfig, SpillOutputs};
use zx_agent::builtin::tools::{self, EXECUTOR_TOOL_NAMES, FileMutationQueue, READ_OUTPUT};
use zx_agent::hooks::HookChain;
use zx_agent::tools::Tool;
use zx_agent::transcript::SessionHeader;

use super::SessionPlace;
use super::execution::{check_session_outside, executor};

/// What each session of an agent gets for itself, made when the session is created or opened
/// and kept until it is closed: a container, when commands run in one, is the session's.
pub(super) struct PerSession {
    /// The tools the agent enables, by name: those of the executor are made for each
    /// session's working directory, and read_skill and read_output read files of this
    /// machine, wherever commands run.
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
    pub(super) tool_output: OutputStoreConfig,
    /// Where the sessions are kept, which a container must not be able to read.
    pub(super) place: SessionPlace,
    /// The queue of changes to each file, shared by every session of the process.
    pub(super) file_queue: Arc<FileMutationQueue>,
}

/// The prompt sections a session gets: where commands run, the project's instructions such
/// as AGENTS.md, and the catalog of skills the model can read.
const ENVIRONMENT: &str = "environment";
const PROJECT_CONTEXT: &str = "project_context";
const AVAILABLE_SKILLS: &str = "available_skills";

#[async_trait]
impl SessionSetup for PerSession {
    fn section_names(&self) -> Vec<String> {
        [ENVIRONMENT, PROJECT_CONTEXT, AVAILABLE_SKILLS]
            .map(String::from)
            .to_vec()
    }

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
        if self.tools.iter().any(|name| name == READ_OUTPUT) {
            let store = OutputStore::new(cwd, self.tool_output.clone())?;
            // The tools that page their own output are exempt: a page of theirs is not stored
            // again.
            let paging = [READ_OUTPUT, READ_SKILL, tools::READ_FILE];
            let (spill, reader) = SpillOutputs::with_reader(store, paging.map(String::from).to_vec());
            // Named like the hooks every session shares (`agent::hooks`).
            hooks.push("spill_outputs", Arc::new(spill));
            host.push(Arc::new(reader));
        }
        if self.execution == Execution::Container {
            check_session_outside(self.place.location(cwd).as_deref(), cwd)?;
        }
        let executor = executor(
            self.execution,
            &self.container,
            cwd,
            &header.id,
            self.hidden_env.clone(),
        )
        .await?;
        let environment = executor.environment_section(cwd);
        // The agent's tools were checked when it was loaded: the others are those of `host`.
        let on_executor: Vec<String> = self
            .tools
            .iter()
            .filter(|name| EXECUTOR_TOOL_NAMES.contains(&name.as_str()))
            .cloned()
            .collect();
        let mut tools = tools::executor_tools(&on_executor, self.tool_timeout, &executor, &self.file_queue)?;
        tools.extend(host);
        Ok(SessionParts {
            tools,
            hooks,
            prompt_sections: vec![
                (ENVIRONMENT.to_string(), environment),
                (PROJECT_CONTEXT.to_string(), context),
                (AVAILABLE_SKILLS.to_string(), catalog.to_string()),
            ],
        })
    }
}
