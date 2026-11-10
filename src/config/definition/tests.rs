use std::time::Duration;

use super::*;
use crate::config::Config;

/// The agent of a file `name` in a directory of its own, with `front` as front matter and
/// `body` below it.
fn load_agent(name: &str, front: &str, body: &str, config: &Config) -> Result<AgentDefinition> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, format!("+++\n{front}\n+++\n{body}")).unwrap();
    from_path(&path, config)
}

/// The agent of the file at `path`, checked against `config`, as a user's agent is loaded.
fn from_path(path: &Path, config: &Config) -> Result<AgentDefinition> {
    let agent = AgentDefinition::from_file(path)?;
    agent.validate(config)?;
    Ok(agent)
}

/// The agent of a shipped file, relative to the repository.
fn shipped(path: &str) -> AgentDefinition {
    from_path(&Path::new(env!("CARGO_MANIFEST_DIR")).join(path), &Config::default()).unwrap()
}

/// An agent file with `front`, extending the unattended agent unless `front` says otherwise.
fn load_with(front: &str, config: &Config) -> Result<AgentDefinition> {
    let front = if front.contains("extends") {
        front.to_string()
    } else {
        format!("extends = 'unattended'\n{front}")
    };
    load_agent("agent.md", &front, "", config)
}

fn load_file(front: &str) -> Result<AgentDefinition> {
    load_with(front, &Config::default())
}

/// The built-in agent `kind`, without a zx-agent home.
fn load(kind: Builtin) -> AgentDefinition {
    AgentDefinition::load(kind, None, &Config::default()).unwrap()
}

/// The configuration of a `config.toml` holding `text`.
fn config(text: &str) -> Config {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    Config::load(&path).unwrap()
}

/// The body of a built-in agent file: what follows its front matter.
fn body_of(file: &str) -> &str {
    file.splitn(3, "+++\n").nth(2).unwrap()
}

#[test]
fn builtin_names_are_spelt_as_extends_takes_them() {
    for builtin in Builtin::ALL {
        let parsed: Builtin = serde_json::from_value(serde_json::Value::String(builtin.name().into())).unwrap();
        assert_eq!(parsed, builtin);
        assert_eq!(load(builtin).name, builtin.name());
    }
}

#[test]
fn built_in_agents_preserve_the_existing_scenario_defaults() {
    for (agent, name, file) in [
        (
            load(Builtin::Unattended),
            "unattended",
            include_str!("../../../agents/unattended.md"),
        ),
        // The deployment's report agent keeps the unattended limits and tools.
        (
            shipped("deploy/kubernetes/agents/unattended.md"),
            "unattended",
            include_str!("../../../deploy/kubernetes/agents/unattended.md"),
        ),
    ] {
        assert_eq!(agent.name, name);
        assert_eq!(agent.instructions, body_of(file));
        assert_eq!(agent.tools, ["bash", "read_file", "read_skill"]);
        assert_eq!(agent.limits.max_model_requests, Some(30));
        assert_eq!(agent.limits.timeout, Some(Duration::from_secs(900)));
        assert_eq!(agent.continue_on_length, 1);
        assert!(agent.permissions.allowed_commands().is_empty());
        assert_eq!(agent.compaction.trigger_bytes, None);
    }
    let assistant = load(Builtin::Assistant);
    assert_eq!(
        assistant.instructions,
        body_of(include_str!("../../../agents/assistant.md"))
    );
    assert!(
        assistant
            .instructions
            .starts_with("You are zx-agent, a helpful assistant")
    );
    assert_eq!(assistant.limits.max_model_requests, None);
    assert_eq!(assistant.limits.timeout, None);
    assert_eq!(assistant.continue_on_length, 0);
    assert_eq!(
        assistant.tools,
        [
            "bash",
            "read_file",
            "edit_file",
            "write_file",
            "grep",
            "find",
            "ls",
            "read_skill",
            "read_output"
        ]
    );
    assert_eq!(assistant.permissions.ask, ["bash", "edit_file", "write_file"]);
    assert_eq!(
        assistant.permissions.allowed_commands(),
        ["grep", "head", "tail", "wc", "jq"]
    );
    assert_eq!(assistant.permissions.allow_except, ["secret"]);
    assert_eq!(assistant.compaction.trigger_bytes, Some(256 * 1024));
    assert_eq!(assistant.compaction.keep_recent_bytes, 64 * 1024);
}

#[test]
fn no_built_in_agent_names_a_program_of_a_particular_environment() {
    for kind in Builtin::ALL {
        let agent = load(kind);
        assert!(agent.permissions.allow_options.is_empty(), "{kind:?}");
        assert!(
            !agent
                .permissions
                .allowed_commands()
                .iter()
                .any(|prefix| prefix.contains("kubectl")),
            "{kind:?}"
        );
    }
}

#[test]
fn only_the_interactive_agent_discovers_project_instructions_by_default() {
    assert!(!load(Builtin::Unattended).context.discover);
    assert!(!shipped("deploy/kubernetes/agents/unattended.md").context.discover);
    assert!(load(Builtin::Assistant).context.discover);
}

#[test]
fn auto_approval_exceptions_replace_the_inherited_ones_and_program_options_limit_their_program() {
    let agent = load_file(
        "extends = 'assistant'\n[permissions]\nallow = ['bash(tool get)', 'bash(grep)']\nallowExcept = ['token']\n\
         [permissions.allowOptions.tool]\nlong = ['namespace']\nshortWithValue = ['n']\nshortFlags = ['A']",
    )
    .unwrap();
    assert_eq!(agent.permissions.allow_except, ["token"]);
    let list = agent.permissions.auto_approval();
    assert!(list.covers("tool get pods -n prod -A --namespace=x | grep web"));
    assert!(!list.covers("tool get pods --raw /api"));
    assert!(!list.covers("tool get pods -Ap"));
    assert!(!list.covers("tool get tokens"));
    // The assistant's "secret" no longer applies: the file's list replaced it.
    assert!(list.covers("grep secret app.log"));
}

#[test]
fn program_options_that_cannot_match_a_command_are_configuration_errors() {
    for (table, message) in [
        ("[permissions.allowOptions.Tool]\nlong = ['x']", "one lowercase word"),
        (
            "[permissions.allowOptions.tool]\nlong = ['--namespace']",
            "without dashes",
        ),
        (
            "[permissions.allowOptions.tool]\nlong = ['output=json']",
            "without dashes",
        ),
        ("[permissions.allowOptions.tool]\nshortFlags = ['-']", "letter or digit"),
        (
            "[permissions.allowOptions.tool]\nshortFlags = ['n']\nshortWithValue = ['n']",
            "both a flag and an option with a value",
        ),
        (
            "[permissions.allowOptions.tool]\nlong = ['x']\nshort = ['y']",
            "unknown field",
        ),
        ("[permissions]\nallowExcept = [' ']", "an entry is empty"),
    ] {
        let error = format!("{:#}", load_file(table).unwrap_err());
        assert!(error.contains(message), "{table}: {error}");
    }
}

#[test]
fn the_kubernetes_deployment_agent_keeps_kubectl_reads_and_asks_for_the_rest() {
    let agent = shipped("deploy/kubernetes/agents/assistant.md");
    assert_eq!(agent.permissions.ask, ["bash"]);
    assert_eq!(agent.tools, ["bash", "read_file", "read_skill", "read_output"]);
    // Its body is empty: the instructions are the assistant's.
    assert_eq!(agent.instructions, load(Builtin::Assistant).instructions);
    let list = agent.permissions.auto_approval();
    for command in [
        "kubectl get pods -A -o wide | grep -v Running",
        "kubectl get pods -nkube-system -oyaml",
        "kubectl logs web-1 -c app --previous --tail=200 --since=1h",
        "kubectl top pods -A --sort-by=cpu",
        "kubectl version --client",
        "kubectl config current-context",
        "grep -i error app.log | head -n 5",
    ] {
        assert!(list.covers(command), "{command}");
    }
    for command in [
        "kubectl get secret db -o yaml",
        "kubectl get pods --server=https://example.com",
        "kubectl get pods -s example.com:443",
        "kubectl get pods --kubeconfig /tmp/other",
        "kubectl get pods --token abc",
        "kubectl get --raw /api/v1/namespaces/default/pods",
        "kubectl get pods --as=system:admin",
        "kubectl version --client --profile=heap --profile-output=/home/u/.bashrc",
        "kubectl get pods -Ap",
        "kubectl delete pod web-1",
        "grep secret app.log",
    ] {
        assert!(!list.covers(command), "{command}");
    }
}

#[test]
fn a_file_extends_a_built_in_agent_and_its_body_replaces_the_instructions() {
    let agent = load_agent(
        "custom.md",
        "extends = 'assistant'\ntools = []\n[permissions]\ndeny = ['bash(secret)']",
        "自定义说明\n",
        &Config::default(),
    )
    .unwrap();
    assert_eq!(agent.name, "custom");
    assert_eq!(agent.instructions, "自定义说明\n");
    assert!(agent.tools.is_empty());
    assert_eq!(agent.permissions.denied_commands(), ["secret"]);
    assert_eq!(agent.permissions.ask, ["bash", "edit_file", "write_file"]);
    assert_eq!(agent.limits.max_model_requests, None);
}

#[test]
fn an_empty_body_inherits_the_instructions_of_the_extended_agent() {
    let assistant = load(Builtin::Assistant).instructions;
    for body in ["", "\n", "  \n\n"] {
        let agent = load_agent("a.md", "extends = 'assistant'", body, &Config::default()).unwrap();
        assert_eq!(agent.instructions, assistant, "{body:?}");
    }
}

#[test]
fn an_agent_without_extends_starts_from_nothing_and_needs_instructions() {
    let agent = load_agent("bare.md", "", "Only this.\n", &Config::default()).unwrap();
    assert_eq!(agent.instructions, "Only this.\n");
    assert!(agent.tools.is_empty());
    assert!(agent.permissions.ask.is_empty() && agent.permissions.allowed_commands().is_empty());
    assert_eq!(agent.limits.max_model_requests, Limits::default().max_model_requests);
    assert_eq!(agent.limits.timeout, Limits::default().timeout);
    assert_eq!(agent.continue_on_length, 0);
    assert!(!agent.context.discover);
    assert!(agent.skills.is_empty());
    assert_eq!(
        agent.compaction.trigger_bytes,
        compaction::Config::default().trigger_bytes
    );
    // In place of the unattended agent, too, the file starts from nothing.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("agents")).unwrap();
    std::fs::write(home.path().join("agents/unattended.md"), "+++\n+++\nOnly this.\n").unwrap();
    let in_run = AgentDefinition::load(Builtin::Unattended, Some(home.path()), &Config::default()).unwrap();
    assert!(in_run.tools.is_empty() && in_run.limits.max_model_requests.is_none());

    let error = format!(
        "{:#}",
        load_agent("bare.md", "tools = ['bash']", "\n \n", &Config::default()).unwrap_err()
    );
    assert!(
        error.contains("has no instructions: write them below the front matter or extend a built-in agent"),
        "{error}"
    );
}

#[test]
fn a_list_the_file_sets_replaces_the_inherited_one_whole() {
    let agent = load_file("extends = 'assistant'\ntools = ['read_file']\n[permissions]\nallow = ['bash(kubectl get)']")
        .unwrap();
    assert_eq!(agent.tools, ["read_file"]);
    assert_eq!(agent.permissions.allowed_commands(), ["kubectl get"]);
    // A table goes key by key: what the file leaves out is inherited.
    assert_eq!(agent.permissions.ask, ["bash", "edit_file", "write_file"]);
    assert_eq!(agent.permissions.allow_except, ["secret"]);
    let agent = load_file("extends = 'assistant'\n[permissions]\nallow = []").unwrap();
    assert!(agent.permissions.allowed_commands().is_empty());
}

#[test]
fn front_matter_errors_name_the_file() {
    let dir = tempfile::tempdir().unwrap();
    for (text, expected) in [
        ("Just instructions.\n", "the first line must be +++"),
        (
            "+++\nextends = 'assistant'\nInstructions.\n",
            "no line +++ closes the front matter",
        ),
        ("+++\nextends = 'other'\n+++\n", "unknown variant"),
        ("+++\nname = 'x'\n+++\nBody\n", "unknown field `name`"),
        (
            "+++\ninstructions = 'x.md'\n+++\nBody\n",
            "unknown field `instructions`",
        ),
        (
            "+++\n[extensions]\ncontinueOnLength = 1\n+++\nBody\n",
            "unknown field `extensions`",
        ),
    ] {
        let path = dir.path().join("broken.md");
        std::fs::write(&path, text).unwrap();
        let error = format!("{:#}", from_path(&path, &Config::default()).unwrap_err());
        assert!(error.contains(expected), "{text:?}: {error}");
        assert!(error.contains("broken.md"), "{text:?}: {error}");
    }
}

#[test]
fn a_file_in_the_home_replaces_the_built_in_agent_of_its_name_only() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    std::fs::create_dir(&agents).unwrap();
    std::fs::write(
        agents.join("assistant.md"),
        "+++\nextends = 'assistant'\ntools = []\n+++\nMine.\n",
    )
    .unwrap();
    // Not the name of a built-in agent: never read, even though it is broken.
    std::fs::write(agents.join("ops.md"), "not an agent").unwrap();
    let load_in = |kind| AgentDefinition::load(kind, Some(home.path()), &Config::default());
    let assistant = load_in(Builtin::Assistant).unwrap();
    assert_eq!(assistant.name, "assistant");
    assert_eq!((assistant.instructions.as_str(), assistant.tools.len()), ("Mine.\n", 0));
    // Without a file of its name, the built-in agent.
    assert_eq!(
        load_in(Builtin::Unattended).unwrap().limits.max_model_requests,
        Some(30)
    );

    // A broken file of a built-in's name stops startup instead of being skipped.
    std::fs::write(agents.join("unattended.md"), "no front matter").unwrap();
    let error = format!("{:#}", load_in(Builtin::Unattended).unwrap_err());
    assert!(error.contains("unattended.md"), "{error}");
    assert!(error.contains("the first line must be +++"), "{error}");
}

#[test]
fn zero_turns_and_unlimited_are_distinct_configurations() {
    assert_eq!(
        load_file("[limits]\nmaxModelRequests = 0")
            .unwrap()
            .limits
            .max_model_requests,
        Some(0)
    );
    assert_eq!(
        load_file("[limits]\nmaxModelRequests = 'unlimited'")
            .unwrap()
            .limits
            .max_model_requests,
        None
    );
    assert_eq!(load_file("[limits]\ntimeoutSecs = 0").unwrap().limits.timeout, None);
}

#[test]
fn a_token_budget_of_zero_disables_it() {
    assert_eq!(
        load_file("[limits]\ntokenBudget = 50000").unwrap().limits.token_budget,
        Some(50000)
    );
    assert_eq!(
        load_file("[limits]\ntokenBudget = 0").unwrap().limits.token_budget,
        None
    );
    assert_eq!(load_file("").unwrap().limits.token_budget, None);
}

#[test]
fn a_cost_budget_of_zero_disables_it() {
    assert_eq!(
        load_file("[limits]\ncostBudget = 2.5").unwrap().limits.cost_budget,
        Some(2.5)
    );
    assert_eq!(load_file("[limits]\ncostBudget = 0").unwrap().limits.cost_budget, None);
    assert_eq!(load_file("").unwrap().limits.cost_budget, None);
}

#[test]
fn unknown_fields_and_invalid_budget_values_are_rejected() {
    for text in [
        "typo = true",
        "[limits]\nmax_turns = 2",
        "[limits]\nmaxTurns = 2",
        "[limits]\nmaxModelRequests = -1",
        "[limits]\nmaxModelRequests = 'infinite'",
        "[limits]\ntokenBudget = -1",
        "[limits]\ntokenBudget = 'unlimited'",
        "[limits]\ncostBudget = -0.5",
        "[limits]\ncostBudget = nan",
        "[limits]\ncostBudget = inf",
    ] {
        assert!(load_file(text).is_err(), "{text}");
    }
}

#[test]
fn environment_settings_and_unsupported_extensions_are_not_agent_fields() {
    for text in [
        "execution = 'kubernetes'",
        "execution = 'container'",
        "[extensions]\nenabled = ['mcp']",
        "extends = 'other.toml'",
        "model = 'm'",
        "[container]\nimage = 'x'",
        "[toolOutput]\nthresholdBytes = 0",
        "[[models]]\nid = 'm'",
        "[[mcp]]\nname = 'prom'",
    ] {
        assert!(load_file(text).is_err(), "{text}");
    }
}

#[test]
fn approving_every_call_needs_the_container_and_no_tool_that_asks() {
    let container = config("execution = 'container'\n[container]\nimage = 'x'");
    let agent = load_with("[permissions]\napproveAll = true\nask = []", &container).unwrap();
    assert!(agent.permissions.approve_all);
    assert!(!load_file("").unwrap().permissions.approve_all);
    for (text, config, expected) in [
        (
            "[permissions]\napproveAll = true",
            &Config::default(),
            "needs execution = \"container\"",
        ),
        (
            "[permissions]\napproveAll = true\nask = ['bash']",
            &container,
            "permissions.ask must be empty",
        ),
    ] {
        let error = format!("{:#}", load_with(text, config).unwrap_err());
        assert!(error.contains(expected), "{text}: {error}");
    }
}

#[test]
fn approve_all_must_be_a_boolean() {
    assert!(load_file("[permissions]\napproveAll = 'yes'").is_err());
}

#[test]
fn redaction_cannot_be_removed_by_an_agent() {
    // Redaction has no switch: an agent that tries to set one is refused.
    let error = format!("{:#}", load_file("redaction = false").unwrap_err());
    assert!(error.contains("unknown field `redaction`"), "{error}");
}

#[test]
fn summaries_are_full_unless_the_agent_asks_for_incremental_ones() {
    assert_eq!(load(Builtin::Unattended).compaction.mode, compaction::SummaryMode::Full);
    assert_eq!(load(Builtin::Assistant).compaction.mode, compaction::SummaryMode::Full);
    let incremental = load_file("extends = 'assistant'\n[compaction]\nmode = 'incremental'").unwrap();
    assert_eq!(incremental.compaction.mode, compaction::SummaryMode::Incremental);
    // Other settings still come from the extended agent.
    assert_eq!(incremental.compaction.trigger_bytes, Some(256 * 1024));
    assert!(load_file("[compaction]\nmode = 'partial'").is_err());
}

#[test]
fn context_management_settings_overlay_individually_and_validate() {
    let agent = load_file("extends = 'assistant'\n[compaction]\ntriggerBytes = 0\nmaxSummaryTokens = 1024").unwrap();
    assert_eq!(agent.compaction.trigger_bytes, None);
    assert_eq!(agent.compaction.keep_recent_bytes, 64 * 1024);
    assert_eq!(agent.compaction.max_summary_tokens, 1024);
    for text in [
        "[compaction]\nkeepRecentBytes = 0",
        "[compaction]\ntimeoutSecs = 0",
        "[compaction]\nunknown = 1",
    ] {
        assert!(load_file(text).is_err(), "{text}");
    }
}

#[test]
fn invalid_tool_names_and_empty_entries_are_configuration_errors() {
    for text in [
        "tools = ['missing']",
        "[permissions]\nask = ['missing']",
        "[permissions]\ndeny = ['bash(  )']",
        "[permissions]\nallow = ['']",
    ] {
        assert!(load_file(text).is_err(), "{text}");
    }
}

#[test]
fn bounded_reads_accept_the_exact_limit_and_reject_oversize_and_invalid_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("text");
    let exact = "x".repeat(64 * 1024);
    std::fs::write(&path, &exact).unwrap();
    assert_eq!(read_text(&path).unwrap(), exact);
    std::fs::write(&path, format!("{exact}界")).unwrap();
    assert!(read_text(&path).unwrap_err().to_string().contains("exceeds"));
    std::fs::write(&path, [0xff]).unwrap();
    assert!(read_text(&path).unwrap_err().to_string().contains("UTF-8"));
    assert!(
        read_text(dir.path())
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
}

#[cfg(unix)]
#[test]
fn special_files_are_refused_before_reading() {
    assert!(
        read_text(Path::new("/dev/null"))
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
}

#[test]
fn the_shipped_review_example_resolves_its_context_and_skill_files() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/agents/review.md");
    let agent = from_path(&path, &Config::default()).unwrap();
    assert_eq!(agent.name, "review");
    assert_eq!(agent.tools, ["read_file", "read_skill"]);
    assert_eq!(agent.permissions.denied_tools(), ["bash"]);
    assert!(
        agent
            .instructions
            .starts_with("You review code without changing files.")
    );
    assert!(
        agent
            .context_section(path.parent().unwrap())
            .unwrap()
            .contains("This example agent")
    );
    assert_eq!(agent.skills[0].name, "rust-review");
    assert!(read_text(&agent.skills[0].path).unwrap().contains("Rust review"));
}

#[test]
fn permissions_may_name_the_configured_mcp_tools_only() {
    let prom = config("[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\ntools = ['query', 'range']");
    let agent = load_with("[permissions]\ndeny = ['prom__range']", &prom).unwrap();
    assert_eq!(agent.permissions.denied_tools(), ["prom__range"]);
    let error = load_with("[permissions]\nask = ['prom__admin']", &prom)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("permissions.ask: unknown tool \"prom__admin\""),
        "{error}"
    );
    assert!(load_file("[permissions]\nask = ['prom__query']").is_err());
}
