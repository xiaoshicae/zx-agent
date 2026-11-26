//! Which calls ask before they run, decided once ([`Asked::new`]): the agent's policies ask
//! about the tools it lists and the prompt section that tells the model names them from the
//! same lists, so the two cannot disagree.

use std::sync::Arc;

use zx_agent::builtin::tools::BASH;
use zx_agent::tools::{Effects, Tool};

use super::Approvals;
use crate::assembly::Asking;
use crate::assembly::execution::ExecutorFactory;
use crate::config::Permissions;

/// The name of the prompt section that says which calls ask.
pub(super) const APPROVALS: &str = "approvals";

/// What a host tells its user when bash commands run on this machine without anyone asked
/// ([`Asked::bash_runs_unasked`]): the exposure of skipped approvals, which have a notice of
/// their own, for bash alone.
const UNASKED_COMMANDS: &str = "[bash] nobody answers approvals: bash commands run on this machine without asking, with this process's permissions; only the agent's deny rules apply, and execution = \"container\" in config.toml isolates them";

/// What the section says when nothing asks.
const NOTHING_ASKS: &str =
    "No tool call asks for approval: every call runs at once, except those the agent's rules deny.";

/// The tools the sessions of an agent can have, by name and by where they come from. A call
/// of any other tool finds none, so nothing asks about it.
pub(super) struct Offered<'a> {
    /// Those the agent's `tools` enables: built-in ones and the host's own.
    pub(super) enabled: &'a [String],
    /// Those the host gives each session, whatever the agent enables.
    pub(super) session: &'a [String],
    /// Those of the MCP servers, which every agent offers.
    pub(super) mcp: &'a [Arc<dyn Tool>],
}

/// The calls that ask before they run.
pub(super) enum Asked {
    /// None: every call is approved, by the agent (`permissions.approveAll`), since its
    /// container is the sandbox, or by the host ([`Asking::Never`]).
    Nothing,
    /// Calls of the tools listed. Where a person answers, every bash command asks too, but
    /// the auto-approved ones.
    Tools {
        /// The tools of the agent's `permissions.ask` that its sessions can have.
        agent: Vec<String>,
        /// The tools of MCP servers that `permissions.allow` does not list, but the ones a
        /// trusted server says only read.
        mcp: Vec<String>,
        /// Whether the agent enables bash, whose commands a person is asked about.
        bash: bool,
    },
}

impl Asked {
    /// What asks among the calls of the agent with `permissions`, whose sessions can have
    /// the tools `offered`, with what the host says of `asking` over it.
    pub(super) fn new(asking: Asking, permissions: &Permissions, offered: &Offered<'_>) -> Self {
        if permissions.approve_all || asking == Asking::Never {
            return Self::Nothing;
        }
        let Offered { enabled, session, mcp } = offered;
        let holds = |names: &[String], name: &str| names.iter().any(|held| held == name);
        let agent = permissions
            .ask
            .iter()
            .filter(|name| {
                holds(enabled, name) || holds(session, name) || mcp.iter().any(|tool| tool.name() == name.as_str())
            })
            .cloned()
            .collect();
        let allowed = permissions.allowed_tools();
        // A server trusted to say which of its tools only read lets those run like built-in
        // tools.
        let mcp = mcp
            .iter()
            .filter(|tool| tool.effects() != Effects::ReadOnly)
            .map(|tool| tool.name().to_string())
            .filter(|name| !allowed.contains(name))
            .collect();
        Self::Tools {
            agent,
            mcp,
            bash: holds(enabled, BASH),
        }
    }

    /// What a host tells its user about these calls, once: [`UNASKED_COMMANDS`] where bash
    /// commands run without anyone asked ([`bash_runs_unasked`](Self::bash_runs_unasked)),
    /// given the agent's `permissions`, and the `executor` does not isolate them; nothing
    /// otherwise.
    pub(super) fn notices(
        &self,
        approvals: Approvals<'_>,
        permissions: &Permissions,
        executor: &dyn ExecutorFactory,
    ) -> Vec<String> {
        if executor.isolates() || !self.bash_runs_unasked(approvals, &permissions.denied_tools()) {
            return Vec::new();
        }
        vec![UNASKED_COMMANDS.to_string()]
    }

    /// Whether bash commands run without anyone asked though approvals are not skipped:
    /// nobody answers `approvals`, the agent enables bash, and neither its `permissions.ask`
    /// nor its `denied` tools hold it. Skipped approvals, under which nothing asks, are told
    /// of apart.
    fn bash_runs_unasked(&self, approvals: Approvals<'_>, denied: &[String]) -> bool {
        let Self::Tools { agent, bash, .. } = self else {
            return false;
        };
        let holds_bash = |tools: &[String]| tools.iter().any(|tool| tool == BASH);
        matches!(approvals, Approvals::Nobody) && *bash && !holds_bash(agent) && !holds_bash(denied)
    }

    /// The prompt section that tells the model which calls ask and what becomes of them, when
    /// `approvals` says who answers: every tool the policies ask about, and bash where a
    /// person answers and the agent enables it; empty when no tool asks. The auto-approved
    /// `commands` are named by their prefixes only, and what still makes them ask is described
    /// broadly: the model needs to know what to avoid, not the exact checks.
    pub(super) fn section(&self, approvals: Approvals<'_>, commands: &[String]) -> String {
        let Self::Tools { agent, mcp, bash } = self else {
            return NOTHING_ASKS.to_string();
        };
        let bash = (*bash && matches!(approvals, Approvals::Person(_))).then_some(BASH);
        let listed = bash
            .into_iter()
            .chain(agent.iter().map(String::as_str))
            .chain(mcp.iter().map(String::as_str));
        let mut tools: Vec<&str> = Vec::new();
        for name in listed {
            if !tools.contains(&name) {
                tools.push(name);
            }
        }
        if tools.is_empty() {
            return String::new();
        }
        let tools_list = tools.join(", ");
        let mut sentences = vec![match approvals {
            Approvals::Person(_) => format!("A person approves each call of these tools before it runs: {tools_list}."),
            Approvals::Nobody => {
                format!("No one answers approvals in this run, so calls of these tools are denied: {tools_list}.")
            }
        }];
        if tools.contains(&BASH) && !commands.is_empty() {
            sentences.push(format!(
                "Plain bash commands that start with {} can skip approval, unless they contain quotes, $, wildcards, redirection, chaining or a URL.",
                commands.join(", ")
            ));
        }
        sentences.push("Other tools run without asking.".to_string());
        sentences.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use zx_agent::ai::ToolDef;
    use zx_agent::tools::{ToolContext, ToolOutput};

    use super::*;
    use crate::assembly::execution::built_in;
    use crate::config::{CONTAINER, ContainerSettings, LOCAL};

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    /// A tool of an MCP server, as far as asking goes: its name and what its calls can change.
    struct Remote(&'static str, Effects);

    #[async_trait]
    impl Tool for Remote {
        fn name(&self) -> &str {
            self.0
        }

        fn definition(&self) -> ToolDef {
            ToolDef {
                name: self.0.into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }

        fn effects(&self) -> Effects {
            self.1
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
            Err("never called".into())
        }
    }

    /// The tools of an MCP server: `prom__query`, which may change state, and `prom__status`,
    /// which the server is trusted to say only reads.
    fn prom() -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(Remote("prom__query", Effects::SideEffects)),
            Arc::new(Remote("prom__status", Effects::ReadOnly)),
        ]
    }

    /// What asks of an agent whose `permissions.ask` is `ask` and whose `tools` are `enabled`,
    /// where the host gives each session `send_file` and the MCP servers have the tools `mcp`.
    fn asked(ask: &[&str], enabled: &[&str], mcp: &[Arc<dyn Tool>]) -> Asked {
        let mut permissions = Permissions::default();
        permissions.ask = names(ask);
        let offered = Offered {
            enabled: &names(enabled),
            session: &names(&["send_file"]),
            mcp,
        };
        Asked::new(Asking::AsConfigured, &permissions, &offered)
    }

    #[test]
    fn a_person_is_told_which_tools_ask_and_which_commands_may_skip_it() {
        let asked = asked(
            &["bash", "edit_file", "write_file"],
            &["bash", "read_file", "edit_file"],
            &prom(),
        );

        let section = asked.section(Approvals::Person("ask"), &names(&["grep", "head"]));

        assert_eq!(
            section,
            "A person approves each call of these tools before it runs: bash, edit_file, prom__query. \
             Plain bash commands that start with grep, head can skip approval, unless they contain quotes, $, \
             wildcards, redirection, chaining or a URL. Other tools run without asking."
        );
    }

    #[test]
    fn without_a_person_the_tools_that_ask_are_denied_and_bash_asks_only_when_listed() {
        let asked = asked(&[], &["bash"], &prom());

        let section = asked.section(Approvals::Nobody, &names(&["grep"]));

        assert_eq!(
            section,
            "No one answers approvals in this run, so calls of these tools are denied: prom__query. Other tools run without asking."
        );
    }

    #[test]
    fn a_tool_of_the_agents_list_asks_wherever_a_session_gets_it_and_not_where_none_does() {
        // Enabled, given to each session by the host, of a server trusted to say it only
        // reads, and one that no session has.
        let asked = asked(
            &["edit_file", "send_file", "prom__status", "write_file"],
            &["edit_file"],
            &prom(),
        );

        let Asked::Tools { agent, mcp, bash } = &asked else {
            panic!("nothing asks");
        };
        assert_eq!(*agent, ["edit_file", "send_file", "prom__status"]);
        assert_eq!(*mcp, ["prom__query"]);
        assert!(!bash);
        assert_eq!(
            asked.section(Approvals::Nobody, &[]),
            "No one answers approvals in this run, so calls of these tools are denied: edit_file, send_file, \
             prom__status, prom__query. Other tools run without asking."
        );
    }

    #[test]
    fn nothing_is_said_when_no_tool_asks() {
        assert_eq!(
            asked(&[], &["bash"], &[]).section(Approvals::Nobody, &names(&["grep"])),
            ""
        );
        // bash asks a person only when the agent has it.
        assert_eq!(
            asked(&[], &["read_file"], &[]).section(Approvals::Person("ask"), &[]),
            ""
        );
    }

    #[test]
    fn nothing_asks_where_the_agent_approves_every_call_or_the_host_skips_approvals() {
        let offered = Offered {
            enabled: &names(&["bash"]),
            session: &[],
            mcp: &prom(),
        };
        let mut approving = Permissions::default();
        approving.approve_all = true;
        assert!(matches!(
            Asked::new(Asking::AsConfigured, &approving, &offered),
            Asked::Nothing
        ));
        let mut asking = Permissions::default();
        asking.ask = names(&["bash"]);
        assert!(matches!(Asked::new(Asking::Never, &asking, &offered), Asked::Nothing));
    }

    #[test]
    fn bash_runs_unasked_only_where_nobody_answers_and_nothing_holds_it_back() {
        let none: &[String] = &[];
        let unasked = asked(&[], &["bash"], &[]);
        let bash = names(&["bash"]);
        assert!(unasked.bash_runs_unasked(Approvals::Nobody, none));
        // A person asked, bash not enabled, asking for it or denying it: it does not run unasked.
        assert!(!unasked.bash_runs_unasked(Approvals::Person("ask"), none));
        assert!(!asked(&[], &["read_file"], &[]).bash_runs_unasked(Approvals::Nobody, none));
        assert!(!unasked.bash_runs_unasked(Approvals::Nobody, &bash));
        assert!(!asked(&["bash"], &["bash"], &[]).bash_runs_unasked(Approvals::Nobody, none));
        // Skipped approvals have a notice of their own.
        assert!(!Asked::Nothing.bash_runs_unasked(Approvals::Nobody, none));
    }

    #[test]
    fn bash_running_unasked_is_told_only_where_the_executor_does_not_isolate_it() {
        let executors = built_in(&ContainerSettings::default());
        let unasked = asked(&[], &["bash"], &[]);
        let permissions = Permissions::default();
        let notices = |name: &str| {
            let executor = executors.get(name).unwrap().as_ref();
            unasked.notices(Approvals::Nobody, &permissions, executor)
        };
        assert_eq!(notices(LOCAL), [UNASKED_COMMANDS]);
        assert!(notices(CONTAINER).is_empty());
    }

    #[test]
    fn skipped_approvals_are_said_whoever_would_answer() {
        for approvals in [Approvals::Person("ask"), Approvals::Nobody] {
            assert_eq!(Asked::Nothing.section(approvals, &[]), NOTHING_ASKS);
        }
    }
}
