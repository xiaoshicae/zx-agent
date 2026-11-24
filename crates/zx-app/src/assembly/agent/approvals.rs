//! Which calls ask before they run, decided once for the agent's policies and for the prompt
//! section that tells the model, so that the two cannot disagree.

use zx_agent::builtin::tools::BASH;

use super::Approvals;
use crate::assembly::execution::ExecutorFactory;
use crate::assembly::{Application, Asking};
use crate::config::{HostTools, Permissions};

/// The name of the prompt section that says which calls ask.
pub(super) const APPROVALS: &str = "approvals";

/// What a host tells its user when bash commands run on this machine without anyone asked
/// ([`Asked::bash_runs_unasked`]): the exposure of skipped approvals, which have a notice of
/// their own, for bash alone.
const UNASKED_COMMANDS: &str = "[bash] nobody answers approvals: bash commands run on this machine without asking, with this process's permissions; only the agent's deny rules apply, and execution = \"container\" in config.toml isolates them";

/// What the section says when nothing asks.
const NOTHING_ASKS: &str =
    "No tool call asks for approval: every call runs at once, except those the agent's rules deny.";

/// The calls that ask before they run.
pub(super) enum Asked<'a> {
    /// None: every call is approved, by the agent (`permissions.approveAll`), since its
    /// container is the sandbox, or by the host ([`Asking::Never`]).
    Nothing,
    /// Calls of the tools in the agent's `permissions.ask`, and of the `extensions`' tools:
    /// those of MCP servers and of the host's extensions that `permissions.allow` does not
    /// list, but the MCP tools a trusted server says only read. Where a person answers, every
    /// bash command asks too, but the auto-approved ones.
    Tools {
        agent: &'a [String],
        extensions: Vec<String>,
    },
}

impl<'a> Asked<'a> {
    /// What asks among the calls of the agent with `permissions`, given the tools of `app`'s
    /// MCP servers and the `host`'s extensions.
    pub(super) fn new(app: &Application, permissions: &'a Permissions, host: &HostTools) -> Self {
        if permissions.approve_all || app.overrides.asking == Asking::Never {
            return Self::Nothing;
        }
        let allowed = permissions.allowed_tools();
        // A server trusted to say which of its tools only read lets those run like built-in
        // tools.
        let extensions = app
            .mcp
            .names
            .iter()
            .filter(|name| !app.mcp.read_only.contains(name))
            .chain(&host.extensions)
            .filter(|name| !allowed.contains(name))
            .cloned()
            .collect();
        Self::Tools {
            agent: &permissions.ask,
            extensions,
        }
    }

    /// What a host tells its user about these calls, once: [`UNASKED_COMMANDS`] where bash
    /// commands run without anyone asked ([`bash_runs_unasked`](Self::bash_runs_unasked)),
    /// given the agent's `permissions`, and the `executor` does not isolate them; nothing
    /// otherwise.
    pub(super) fn notices(
        &self,
        approvals: Approvals<'_>,
        enabled: &[String],
        permissions: &Permissions,
        executor: &dyn ExecutorFactory,
    ) -> Vec<String> {
        if executor.isolates() || !self.bash_runs_unasked(approvals, enabled, &permissions.denied_tools()) {
            return Vec::new();
        }
        vec![UNASKED_COMMANDS.to_string()]
    }

    /// Whether bash commands run without anyone asked though approvals are not skipped:
    /// nobody answers `approvals`, the agent's tools (`enabled`) include bash, and neither its
    /// `permissions.ask` nor its `denied` tools hold it. Skipped approvals, under which nothing
    /// asks, are told of apart.
    fn bash_runs_unasked(&self, approvals: Approvals<'_>, enabled: &[String], denied: &[String]) -> bool {
        let Self::Tools { agent, .. } = self else {
            return false;
        };
        let holds_bash = |tools: &[String]| tools.iter().any(|tool| tool == BASH);
        matches!(approvals, Approvals::Nobody) && holds_bash(enabled) && !holds_bash(agent) && !holds_bash(denied)
    }

    /// The prompt section that tells the model which calls ask and what becomes of them, when
    /// `approvals` says who answers: empty when no tool of the agent's `enabled` ones or of
    /// its extensions asks. The auto-approved `commands` are named by their prefixes only, and
    /// what still makes them ask is described broadly: the model needs to know what to avoid,
    /// not the exact checks.
    pub(super) fn section(&self, approvals: Approvals<'_>, enabled: &[String], commands: &[String]) -> String {
        let Self::Tools { agent, extensions } = self else {
            return NOTHING_ASKS.to_string();
        };
        let bash = matches!(approvals, Approvals::Person(_)).then_some(BASH);
        let own = bash.into_iter().chain(agent.iter().map(String::as_str));
        let listed = own
            .filter(|name| enabled.iter().any(|tool| tool == name))
            .chain(extensions.iter().map(String::as_str));
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
    use super::*;
    use crate::assembly::execution::built_in;
    use crate::config::{CONTAINER, ContainerSettings, LOCAL};

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_person_is_told_which_tools_ask_and_which_commands_may_skip_it() {
        let ask = names(&["bash", "edit_file", "write_file"]);
        let asked = Asked::Tools {
            agent: &ask,
            extensions: names(&["prom__query"]),
        };
        let enabled = names(&["bash", "read_file", "edit_file"]);

        let section = asked.section(Approvals::Person("ask"), &enabled, &names(&["grep", "head"]));

        assert_eq!(
            section,
            "A person approves each call of these tools before it runs: bash, edit_file, prom__query. \
             Plain bash commands that start with grep, head can skip approval, unless they contain quotes, $, \
             wildcards, redirection, chaining or a URL. Other tools run without asking."
        );
    }

    #[test]
    fn without_a_person_the_tools_that_ask_are_denied_and_bash_asks_only_when_listed() {
        let asked = Asked::Tools {
            agent: &[],
            extensions: names(&["prom__query"]),
        };

        let section = asked.section(Approvals::Nobody, &names(&["bash"]), &names(&["grep"]));

        assert_eq!(
            section,
            "No one answers approvals in this run, so calls of these tools are denied: prom__query. Other tools run without asking."
        );
    }

    #[test]
    fn nothing_is_said_when_no_tool_asks() {
        let asked = Asked::Tools {
            agent: &[],
            extensions: Vec::new(),
        };

        assert_eq!(
            asked.section(Approvals::Nobody, &names(&["bash"]), &names(&["grep"])),
            ""
        );
        // bash asks a person only when the agent has it.
        assert_eq!(asked.section(Approvals::Person("ask"), &names(&["read_file"]), &[]), "");
    }

    #[test]
    fn bash_runs_unasked_only_where_nobody_answers_and_nothing_holds_it_back() {
        let none: &[String] = &[];
        let unasked = Asked::Tools {
            agent: none,
            extensions: Vec::new(),
        };
        let bash = names(&["bash"]);
        assert!(unasked.bash_runs_unasked(Approvals::Nobody, &bash, none));
        // A person asked, bash not enabled, asking for it or denying it: it does not run unasked.
        assert!(!unasked.bash_runs_unasked(Approvals::Person("ask"), &bash, none));
        assert!(!unasked.bash_runs_unasked(Approvals::Nobody, &names(&["read_file"]), none));
        assert!(!unasked.bash_runs_unasked(Approvals::Nobody, &bash, &bash));
        let asking = Asked::Tools {
            agent: &bash,
            extensions: Vec::new(),
        };
        assert!(!asking.bash_runs_unasked(Approvals::Nobody, &bash, none));
        // Skipped approvals have a notice of their own.
        assert!(!Asked::Nothing.bash_runs_unasked(Approvals::Nobody, &bash, none));
    }

    #[test]
    fn bash_running_unasked_is_told_only_where_the_executor_does_not_isolate_it() {
        let executors = built_in(&ContainerSettings::default());
        let unasked = Asked::Tools {
            agent: &[],
            extensions: Vec::new(),
        };
        let bash = names(&["bash"]);
        let permissions = Permissions::default();
        let notices = |name: &str| {
            let executor = executors.get(name).unwrap().as_ref();
            unasked.notices(Approvals::Nobody, &bash, &permissions, executor)
        };
        assert_eq!(notices(LOCAL), [UNASKED_COMMANDS]);
        assert!(notices(CONTAINER).is_empty());
    }

    #[test]
    fn skipped_approvals_are_said_whoever_would_answer() {
        for approvals in [Approvals::Person("ask"), Approvals::Nobody] {
            assert_eq!(Asked::Nothing.section(approvals, &names(&["bash"]), &[]), NOTHING_ASKS);
        }
    }
}
