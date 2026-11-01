//! Extensions through the public SDK: what `extensions::register` adds to a loop, and what it
//! refuses without registering anything.
mod common;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use common::{EchoTool, config, run_prompt, summarize};
use zx_agent::agent::{AgentEvent, HookContext, HookResult, HookStage, Hooks, LoopConfig, Tool, ToolOutput};
use zx_agent::ai::ToolCall;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::extensions::{Extension, register};
use zx_agent::prompt::{ENVIRONMENT, build_system_prompt};

/// Appends a mark to every tool result.
struct Mark;

#[async_trait]
impl Hooks for Mark {
    async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        output.content.push_str(" [marked]");
        Ok(output)
    }
}

/// Declares the stage of the built-in policies, which extension hooks may not use.
struct Early;

#[async_trait]
impl Hooks for Early {
    fn stage(&self) -> HookStage {
        HookStage::Policy
    }
}

/// Fails on every tool result.
struct Broken;

#[async_trait]
impl Hooks for Broken {
    async fn after_tool(&self, _call: &ToolCall, _output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        Err("broken".to_string())
    }
}

struct Bundle {
    name: &'static str,
    tools: Vec<Arc<dyn Tool>>,
    sections: Vec<(&'static str, &'static str)>,
    hooks: fn() -> Vec<Box<dyn Hooks>>,
}

impl Bundle {
    fn named(name: &'static str) -> Self {
        Self {
            name,
            tools: Vec::new(),
            sections: Vec::new(),
            hooks: Vec::new,
        }
    }
}

impl Extension for Bundle {
    fn name(&self) -> &str {
        self.name
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    fn prompt_sections(&self) -> Vec<(String, String)> {
        self.sections
            .iter()
            .map(|(name, content)| (name.to_string(), content.to_string()))
            .collect()
    }

    fn hooks(&self) -> Vec<Box<dyn Hooks>> {
        (self.hooks)()
    }
}

/// A loop with the host's `echo` tool, and a prompt with a generated section and one of the
/// host's own.
fn base(provider: Arc<FauxProvider>) -> LoopConfig {
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config.spec.system_prompt =
        build_system_prompt("Do the task.", &[(ENVIRONMENT, "OS: test"), ("rules", "Be brief.")]);
    config
}

fn idle() -> Arc<FauxProvider> {
    Arc::new(FauxProvider::new(Vec::new()))
}

#[tokio::test]
async fn an_extension_adds_its_tools_hooks_and_prompt_sections() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "note", json!({"text": "saved"})),
        FauxStep::text("done"),
    ]));
    let mut config = base(provider.clone());
    let notes = Bundle {
        tools: vec![Arc::new(EchoTool::new("note"))],
        sections: vec![("notes", "Use note to keep notes.")],
        hooks: || vec![Box::new(Mark) as Box<dyn Hooks>],
        ..Bundle::named("notes")
    };
    register(&mut config, &[Arc::new(notes)]).unwrap();
    let mut context = Vec::new();
    run_prompt(&config, &mut context, "go").await;
    let requests = provider.requests();
    let tools: Vec<&str> = requests[0].tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(tools, ["echo", "note"]);
    assert_eq!(
        summarize(&requests[0].messages[..1]),
        ["system:preamble,environment,rules,notes"]
    );
    assert!(
        summarize(&requests[1].messages).contains(&"tool:c1=saved [marked]".to_string()),
        "{:?}",
        summarize(&requests[1].messages)
    );
}

#[test]
fn a_hook_outside_the_extension_stage_is_refused_and_nothing_is_registered() {
    let mut config = base(idle());
    let prompt = config.spec.system_prompt.clone();
    let early = Bundle {
        tools: vec![Arc::new(EchoTool::new("note"))],
        sections: vec![("notes", "Use note to keep notes.")],
        hooks: || vec![Box::new(Early) as Box<dyn Hooks>],
        ..Bundle::named("early")
    };
    let error = register(&mut config, &[Arc::new(early)]).unwrap_err();
    assert!(error.contains("declares the Policy stage"), "{error}");
    assert_eq!(config.spec.tools.len(), 1);
    assert_eq!(config.spec.system_prompt, prompt);
}

#[test]
fn an_extension_tool_may_not_take_a_name_in_use() {
    let echo = || -> Vec<Arc<dyn Tool>> { vec![Arc::new(EchoTool::new("echo"))] };
    let note = || -> Vec<Arc<dyn Tool>> { vec![Arc::new(EchoTool::new("note"))] };
    let cases: Vec<Vec<Bundle>> = vec![
        // The host's own tool.
        vec![Bundle {
            tools: echo(),
            ..Bundle::named("one")
        }],
        // Another extension's.
        vec![
            Bundle {
                tools: note(),
                ..Bundle::named("one")
            },
            Bundle {
                tools: note(),
                ..Bundle::named("two")
            },
        ],
    ];
    for bundles in cases {
        let mut config = base(idle());
        let extensions: Vec<Arc<dyn Extension>> = bundles
            .into_iter()
            .map(|bundle| Arc::new(bundle) as Arc<dyn Extension>)
            .collect();
        let error = register(&mut config, &extensions).unwrap_err();
        assert!(error.contains("more than one tool is named"), "{error}");
        assert_eq!(config.spec.tools.len(), 1, "only the host's tool is left");
    }
}

#[test]
fn extension_sections_may_not_take_reserved_existing_or_malformed_names() {
    for (sections, expected) in [
        (vec![("environment", "x")], "reserved"),
        (vec![("preamble", "x")], "reserved"),
        // Reserved even when the prompt has no such section.
        (vec![("available_skills", "x")], "reserved"),
        (vec![("rules", "x")], "already has a section"),
        (vec![("notes", "x"), ("notes", "y")], "already has a section"),
        (vec![("Notes", "x")], "must be ASCII lowercase"),
        (vec![("my notes", "x")], "must be ASCII lowercase"),
    ] {
        let mut config = base(idle());
        let prompt = config.spec.system_prompt.clone();
        let bundle = Bundle {
            sections,
            ..Bundle::named("notes")
        };
        let error = register(&mut config, &[Arc::new(bundle)]).unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert_eq!(config.spec.system_prompt, prompt);
    }
}

#[test]
fn extension_names_must_be_given_and_unique() {
    let mut config = base(idle());
    let error = register(&mut config, &[Arc::new(Bundle::named(" "))]).unwrap_err();
    assert!(error.contains("blank name"), "{error}");
    let error = register(
        &mut config,
        &[Arc::new(Bundle::named("notes")), Arc::new(Bundle::named("notes"))],
    )
    .unwrap_err();
    assert!(error.contains("more than one extension is named \"notes\""), "{error}");
}

#[tokio::test]
async fn a_failing_extension_hook_is_reported_under_the_extension_name() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]));
    let mut config = base(provider);
    let broken = Bundle {
        hooks: || vec![Box::new(Broken) as Box<dyn Hooks>],
        ..Bundle::named("broken")
    };
    register(&mut config, &[Arc::new(broken)]).unwrap();
    let mut context = Vec::new();
    let (_, events) = run_prompt(&config, &mut context, "go").await;
    assert!(
        events.events().iter().any(|event| matches!(
            event,
            AgentEvent::HookFailed { hook, phase, .. } if hook == "broken" && *phase == "after_tool"
        )),
        "{:?}",
        events.kinds()
    );
}
