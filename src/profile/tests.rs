use std::time::Duration;

use super::*;

fn load_file(text: &str) -> Result<Profile> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("profile.toml");
    std::fs::write(&path, text).unwrap();
    Profile::load(Some(path.to_str().unwrap()), Builtin::Troubleshoot)
}

#[test]
fn builtin_names_are_spelt_as_extends_takes_them() {
    for builtin in Builtin::ALL {
        let parsed: Builtin = serde_json::from_value(serde_json::Value::String(builtin.name().into())).unwrap();
        assert_eq!(parsed, builtin);
        assert_eq!(
            Profile::load(Some(builtin.name()), Builtin::Unattended).unwrap().name,
            { Profile::load(None, builtin).unwrap().name }
        );
    }
}

#[test]
fn embedded_profiles_preserve_the_existing_scenario_defaults() {
    for (selector, instructions) in [
        ("unattended", include_str!("../../profiles/unattended.md")),
        ("troubleshoot", include_str!("../../profiles/troubleshoot.md")),
    ] {
        let profile = Profile::load(Some(selector), Builtin::Assistant).unwrap();
        assert_eq!(profile.name, selector);
        assert_eq!(profile.instructions, instructions);
        assert_eq!(profile.tools, ["bash", "read_file", "read_skill"]);
        assert_eq!(profile.limits.max_model_requests, Some(30));
        assert_eq!(profile.limits.timeout, Some(Duration::from_secs(900)));
        assert_eq!(profile.continue_on_length, 1);
        assert!(profile.rules.auto_approve.is_empty());
        assert_eq!(profile.compaction.trigger_bytes, None);
    }
    let assistant = Profile::load(Some("assistant"), Builtin::Unattended).unwrap();
    assert_eq!(assistant.instructions, include_str!("../../profiles/assistant.md"));
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
    assert_eq!(assistant.rules.ask_tools, ["bash", "edit_file", "write_file"]);
    assert_eq!(assistant.rules.auto_approve, ["grep", "head", "tail", "wc", "jq"]);
    assert_eq!(assistant.rules.auto_approve_except, ["secret"]);
    assert_eq!(assistant.compaction.trigger_bytes, Some(256 * 1024));
    assert_eq!(assistant.compaction.keep_recent_bytes, 64 * 1024);
    assert_eq!(assistant.tool_output.threshold_bytes, Some(8 * 1024));
}

#[test]
fn no_embedded_profile_names_a_program_of_a_particular_environment() {
    for kind in Builtin::ALL {
        let profile = Profile::load(None, kind).unwrap();
        assert!(profile.rules.auto_approve_options.is_empty(), "{kind:?}");
        assert!(
            !profile
                .rules
                .auto_approve
                .iter()
                .any(|prefix| prefix.contains("kubectl")),
            "{kind:?}"
        );
    }
}

#[test]
fn only_the_interactive_profile_discovers_project_instructions_by_default() {
    assert!(!Profile::load(None, Builtin::Unattended).unwrap().context.discover);
    assert!(!Profile::load(None, Builtin::Troubleshoot).unwrap().context.discover);
    assert!(Profile::load(None, Builtin::Assistant).unwrap().context.discover);
}

#[test]
fn auto_approval_exceptions_accumulate_and_program_options_limit_their_program() {
    let profile = load_file(
        "extends = 'assistant'\n[rules]\nautoApprove = ['tool get', 'grep']\nautoApproveExcept = ['token']\n\
         [rules.autoApproveOptions.tool]\nlong = ['namespace']\nshortWithValue = ['n']\nshortFlags = ['A']",
    )
    .unwrap();
    assert_eq!(profile.rules.auto_approve_except, ["secret", "token"]);
    let list = profile.rules.auto_approval();
    assert!(list.covers("tool get pods -n prod -A --namespace=x | grep web"));
    assert!(!list.covers("tool get pods --raw /api"));
    assert!(!list.covers("tool get pods -Ap"));
    assert!(!list.covers("tool get tokens"));
    assert!(!list.covers("grep secret app.log"));
}

#[test]
fn program_options_that_cannot_match_a_command_are_configuration_errors() {
    for (table, message) in [
        ("[rules.autoApproveOptions.Tool]\nlong = ['x']", "one lowercase word"),
        (
            "[rules.autoApproveOptions.tool]\nlong = ['--namespace']",
            "without dashes",
        ),
        (
            "[rules.autoApproveOptions.tool]\nlong = ['output=json']",
            "without dashes",
        ),
        ("[rules.autoApproveOptions.tool]\nshortFlags = ['-']", "letter or digit"),
        (
            "[rules.autoApproveOptions.tool]\nshortFlags = ['n']\nshortWithValue = ['n']",
            "both a flag and an option with a value",
        ),
        (
            "[rules.autoApproveOptions.tool]\nlong = ['x']\nshort = ['y']",
            "unknown field",
        ),
        ("[rules]\nautoApproveExcept = [' ']", "empty command rule"),
    ] {
        let error = format!("{:#}", load_file(table).unwrap_err());
        assert!(error.contains(message), "{table}: {error}");
    }
}

#[test]
fn the_kubernetes_deployment_profile_keeps_kubectl_reads_and_asks_for_the_rest() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/kubernetes/profiles/kubernetes.toml");
    let profile = Profile::load(Some(path.to_str().unwrap()), Builtin::Unattended).unwrap();
    assert_eq!(profile.rules.ask_tools, ["bash"]);
    assert_eq!(profile.tools, ["bash", "read_file", "read_skill", "read_output"]);
    let list = profile.rules.auto_approval();
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
fn a_file_extends_an_embedded_baseline_and_resolves_its_instructions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prompt.md"), "自定义说明").unwrap();
    let path = dir.path().join("scene.toml");
    std::fs::write(
        &path,
        "extends = 'assistant'\nname = 'custom'\ninstructions = 'prompt.md'\ntools = []\n[rules]\ndeny = ['secret']",
    )
    .unwrap();
    let profile = Profile::load(Some(path.to_str().unwrap()), Builtin::Troubleshoot).unwrap();
    assert_eq!(profile.name, "custom");
    assert_eq!(profile.instructions, "自定义说明");
    assert!(profile.tools.is_empty());
    assert_eq!(profile.rules.deny, ["secret"]);
    assert_eq!(profile.rules.ask_tools, ["bash", "edit_file", "write_file"]);
    assert_eq!(profile.limits.max_model_requests, None);
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
fn unsupported_execution_and_extensions_fail_instead_of_being_ignored() {
    for text in [
        "execution = 'kubernetes'",
        "[extensions]\nenabled = ['mcp']",
        "extends = 'other.toml'",
    ] {
        assert!(load_file(text).is_err(), "{text}");
    }
}

#[test]
fn container_execution_reads_its_settings_and_defaults_to_local() {
    assert_eq!(load_file("").unwrap().execution, Execution::Local);
    let profile = load_file(
        "execution = 'container'\n[container]\nimage = 'debian:bookworm-slim'\nuser = '1000:1000'\nmemory = '512m'\npidsLimit = 64\n\
         network = 'none'\nworkspace = 'readOnly'\nroot = 'readOnly'",
    )
    .unwrap();
    assert_eq!(profile.execution, Execution::Container);
    assert_eq!(
        profile.container,
        ContainerSettings {
            image: Some("debian:bookworm-slim".into()),
            user: Some("1000:1000".into()),
            memory: Some("512m".into()),
            pids_limit: Some(64),
            network: Some("none".into()),
            workspace: Some(Access::ReadOnly),
            root: Some(Access::ReadOnly),
        }
    );
    // Unset, the sandbox settings are left to the executor, whose defaults are open.
    let open = load_file("execution = 'container'\n[container]\nimage = 'x'").unwrap();
    assert_eq!(
        (open.container.network, open.container.workspace, open.container.root),
        (None, None, None)
    );
}

#[test]
fn approving_every_call_needs_the_container_and_no_tool_that_asks() {
    let profile =
        load_file("execution = 'container'\n[container]\nimage = 'x'\n[rules]\napproveAll = true\naskTools = []")
            .unwrap();
    assert!(profile.rules.approve_all);
    assert!(!load_file("").unwrap().rules.approve_all);
    for (text, expected) in [
        ("[rules]\napproveAll = true", "needs execution = \"container\""),
        (
            "execution = 'container'\n[container]\nimage = 'x'\n[rules]\napproveAll = true\naskTools = ['bash']",
            "rules.askTools must be empty",
        ),
    ] {
        let error = format!("{:#}", load_file(text).unwrap_err());
        assert!(error.contains(expected), "{text}: {error}");
    }
}

#[test]
fn invalid_container_settings_are_configuration_errors() {
    for text in [
        "execution = 'container'",
        "execution = 'container'\n[container]\nimage = ' '",
        "[container]\nimage = 'x'\npidsLimit = 0",
        "[container]\nimage = 'x'\nuser = ''",
        "[container]\nimage = 'x'\nnetwork = 'host'",
        "[container]\nimage = 'x'\nnetwork = 'container:other'",
        "[container]\nimage = 'x'\nnetwork = ' '",
        "[container]\nimage = 'x'\nworkspace = 'ro'",
        "[container]\nimage = 'x'\nroot = 'writable'",
        "[rules]\napproveAll = 'yes'",
    ] {
        assert!(load_file(text).is_err(), "{text}");
    }
}

#[test]
fn redaction_cannot_be_removed_by_a_profile() {
    assert!(
        load_file("[extensions]\nredaction = false")
            .unwrap_err()
            .to_string()
            .contains("cannot be disabled")
    );
}

#[test]
fn summaries_are_full_unless_the_profile_asks_for_incremental_ones() {
    assert_eq!(
        Profile::load(None, Builtin::Troubleshoot).unwrap().compaction.mode,
        compaction::SummaryMode::Full
    );
    assert_eq!(
        Profile::load(Some("assistant"), Builtin::Troubleshoot)
            .unwrap()
            .compaction
            .mode,
        compaction::SummaryMode::Full
    );
    let incremental = load_file("extends = 'assistant'\n[compaction]\nmode = 'incremental'").unwrap();
    assert_eq!(incremental.compaction.mode, compaction::SummaryMode::Incremental);
    // Other settings still come from the base profile.
    assert_eq!(incremental.compaction.trigger_bytes, Some(256 * 1024));
    assert!(load_file("[compaction]\nmode = 'partial'").is_err());
}

#[test]
fn context_management_settings_overlay_individually_and_validate() {
    let profile = load_file("extends = 'assistant'\n[compaction]\ntriggerBytes = 0\nmaxSummaryTokens = 1024\n[toolOutput]\nthresholdBytes = 0").unwrap();
    assert_eq!(profile.compaction.trigger_bytes, None);
    assert_eq!(profile.compaction.keep_recent_bytes, 64 * 1024);
    assert_eq!(profile.compaction.max_summary_tokens, 1024);
    assert_eq!(profile.tool_output.threshold_bytes, None);
    for text in [
        "[compaction]\nkeepRecentBytes = 0",
        "[compaction]\ntimeoutSecs = 0",
        "[compaction]\nunknown = 1",
        "[toolOutput]\ndirectory = ''",
        "[toolOutput]\nthreshold_bytes = 256",
    ] {
        assert!(load_file(text).is_err(), "{text}");
    }
}

#[test]
fn invalid_tool_names_and_empty_rules_are_configuration_errors() {
    for text in [
        "tools = ['missing']",
        "[rules]\naskTools = ['missing']",
        "[rules]\ndeny = ['  ']",
        "[rules]\nautoApprove = ['']",
    ] {
        assert!(load_file(text).is_err(), "{text}");
    }
}

#[test]
fn a_file_replaces_the_auto_approve_list_of_its_baseline() {
    let profile = load_file("extends = 'assistant'\n[rules]\nautoApprove = ['kubectl get']").unwrap();
    assert_eq!(profile.rules.auto_approve, ["kubectl get"]);
    let profile = load_file("extends = 'assistant'\n[rules]\nautoApprove = []").unwrap();
    assert!(profile.rules.auto_approve.is_empty());
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
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/profiles/review.toml");
    let profile = Profile::load(Some(path.to_str().unwrap()), Builtin::Troubleshoot).unwrap();
    assert_eq!(profile.tools, ["read_file", "read_skill"]);
    assert_eq!(profile.rules.deny_tools, ["bash"]);
    assert!(
        profile
            .context_section(path.parent().unwrap())
            .unwrap()
            .contains("This example profile")
    );
    assert_eq!(profile.skills[0].name, "rust-review");
    assert!(read_text(&profile.skills[0].path).unwrap().contains("Rust review"));
}

const PROM: &str = "[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\ntokenEnv = 'PROM_TOKEN'\n\
                    tools = ['query', 'range']\nallowTools = ['query']\ntimeoutSecs = 30\n\
                    maxTimeoutSecs = 600\n";

#[test]
fn mcp_servers_are_read_from_the_profile() {
    let profile = load_file(PROM).unwrap();
    assert_eq!(
        profile.mcp,
        [McpServer {
            name: "prom".into(),
            url: "https://prom.example/mcp".into(),
            token_env: Some("PROM_TOKEN".into()),
            tools: vec!["query".into(), "range".into()],
            allow_tools: vec!["query".into()],
            timeout_secs: Some(30),
            max_timeout_secs: Some(600),
        }]
    );
    assert!(Profile::load(None, Builtin::Assistant).unwrap().mcp.is_empty());
}

#[test]
fn rules_may_name_the_configured_mcp_tools_only() {
    let profile = load_file(&format!("[rules]\ndenyTools = ['prom__range']\n{PROM}")).unwrap();
    assert_eq!(profile.rules.deny_tools, ["prom__range"]);
    let error = load_file(&format!("[rules]\naskTools = ['prom__admin']\n{PROM}"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown tool in rule \"prom__admin\""), "{error}");
}

#[test]
fn malformed_mcp_settings_are_refused() {
    let server = |fields: &str| format!("[[mcp]]\nname = 'prom'\nurl = 'https://prom.example/mcp'\n{fields}\n");
    for (text, expected) in [
        (server("tools = []"), "tools lists no tool"),
        (
            server("tools = ['query']\nallowTools = ['range']"),
            "which tools does not list",
        ),
        (
            server("tools = ['query']\ntimeoutSecs = 0"),
            "timeoutSecs must be greater than zero",
        ),
        (
            server("tools = ['query']\nmaxTimeoutSecs = 0"),
            "maxTimeoutSecs must be greater than zero",
        ),
        (server("tools = ['query']\ntokenEnv = ' '"), "tokenEnv is empty"),
        (server("tools = ['a.b', 'a_b']"), "like another of its tools"),
        (server(&format!("tools = ['{}']", "x".repeat(64))), "longer than 64"),
        (
            format!("{}{}", server("tools = ['query']"), server("tools = ['range']")),
            "more than one MCP server",
        ),
        (
            "[[mcp]]\nname = 'Prom'\nurl = 'https://prom.example/mcp'\ntools = ['query']\n".to_string(),
            "must be ASCII lowercase",
        ),
        (server("tools = ['query']\nheaders = {}"), "unknown field"),
    ] {
        let error = format!("{:#}", load_file(&text).unwrap_err());
        assert!(error.contains(expected), "{text}\n{error}");
    }
}
