//! Extensions through the public SDK: what an agent's extensions add to its sessions, and
//! which extensions refuse the agent.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use zx_agent::agent::{SessionParts, SessionSetup};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{SystemPrompt, ToolCall};
use zx_agent::events::{EventKind, HookPoint};
use zx_agent::extensions::Extension;
use zx_agent::hooks::{Hook, HookContext, HookResult, HookStage};
use zx_agent::tools::{Tool, ToolOutput};
use zx_agent::transcript::SessionHeader;
use zx_agent::{Agent, AgentConfig, SessionOptions};
use zx_testing::{EchoTool, config, refusal, run_prompt, summarize};

/// Appends a mark to every tool result.
struct Mark;

#[async_trait]
impl Hook for Mark {
    async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        output.content.push_str(" [marked]");
        Ok(output)
    }
}

/// Declares the stage of the built-in redaction, which extension hooks may not use.
struct Early;

#[async_trait]
impl Hook for Early {
    fn stage(&self) -> HookStage {
        HookStage::Redaction
    }
}

/// Fails on every tool result.
struct Broken;

#[async_trait]
impl Hook for Broken {
    async fn after_tool(&self, _call: &ToolCall, _output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        Err("broken".to_string())
    }
}

struct Bundle {
    name: &'static str,
    tools: Vec<Arc<dyn Tool>>,
    sections: Vec<(&'static str, &'static str)>,
    hooks: fn() -> Vec<Arc<dyn Hook>>,
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

    fn hooks(&self) -> Vec<Arc<dyn Hook>> {
        (self.hooks)()
    }
}

/// An agent with the host's `echo` tool, and a prompt with a generated section and one of
/// the host's own.
fn base(provider: Arc<FauxProvider>) -> AgentConfig {
    let mut config = config(provider, vec![Arc::new(EchoTool::new("echo"))]);
    config.system_prompt = SystemPrompt::new("Do the task.")
        .with_section("environment", "OS: test")
        .with_section("rules", "Be brief.");
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
        hooks: || vec![Arc::new(Mark) as Arc<dyn Hook>],
        ..Bundle::named("notes")
    };
    config.extensions = vec![Arc::new(notes)];
    let mut context = Vec::new();
    run_prompt(config, &mut context, "go").await;
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
fn an_extension_hook_outside_the_extension_stage_refuses_the_agent() {
    let mut config = base(idle());
    let early = Bundle {
        tools: vec![Arc::new(EchoTool::new("note"))],
        sections: vec![("notes", "Use note to keep notes.")],
        hooks: || vec![Arc::new(Early) as Arc<dyn Hook>],
        ..Bundle::named("early")
    };
    config.extensions = vec![Arc::new(early)];
    let error = refusal(config);
    assert!(error.contains("declares the Redaction stage"), "{error}");
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
        config.extensions = bundles
            .into_iter()
            .map(|bundle| Arc::new(bundle) as Arc<dyn Extension>)
            .collect();
        let error = refusal(config);
        assert!(error.contains("more than one tool is named"), "{error}");
    }
}

/// A session setup that gives the prompt sections `0`, each blank in every session.
struct Gives(&'static [&'static str]);

#[async_trait]
impl SessionSetup for Gives {
    async fn setup(&self, _header: &SessionHeader) -> anyhow::Result<SessionParts> {
        Ok(SessionParts::default())
    }

    fn section_names(&self) -> Vec<String> {
        self.0.iter().map(|name| (*name).to_string()).collect()
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
        config.session_setup = Some(Arc::new(Gives(&["environment", "available_skills"])));
        let bundle = Bundle {
            sections,
            ..Bundle::named("notes")
        };
        config.extensions = vec![Arc::new(bundle)];
        let error = refusal(config);
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn extension_names_must_be_given_and_unique() {
    let mut blank = base(idle());
    blank.extensions = vec![Arc::new(Bundle::named(" "))];
    let error = refusal(blank);
    assert!(error.contains("blank name"), "{error}");
    let mut twice = base(idle());
    twice.extensions = vec![Arc::new(Bundle::named("notes")), Arc::new(Bundle::named("notes"))];
    let error = refusal(twice);
    assert!(error.contains("more than one extension is named \"notes\""), "{error}");
}

/// Counts how often it is asked for its hooks, and gives one that remembers the sessions
/// whose input it saw.
#[derive(Default)]
struct Asked {
    asked: AtomicUsize,
    seen: Arc<Mutex<Vec<String>>>,
}

struct Remember(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Hook for Remember {
    async fn on_input(&self, text: String, ctx: &HookContext) -> HookResult<String> {
        self.0.lock().unwrap().push(ctx.session_id.clone());
        Ok(text)
    }
}

impl Extension for Asked {
    fn name(&self) -> &str {
        "asked"
    }

    fn hooks(&self) -> Vec<Arc<dyn Hook>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        vec![Arc::new(Remember(self.seen.clone()))]
    }
}

#[tokio::test]
async fn an_extension_is_asked_once_and_its_hooks_serve_every_session() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("one"), FauxStep::text("two")]));
    let extension = Arc::new(Asked::default());
    let mut config = base(provider);
    config.extensions = vec![extension.clone()];
    let agent = Agent::new(config).unwrap();

    let mut sessions = Vec::new();
    for _ in 0..2 {
        let session = agent.create_session(SessionOptions::new(".")).await.unwrap();
        session.send("go").unwrap().wait().await;
        sessions.push(session.id().to_string());
    }

    assert_eq!(extension.asked.load(Ordering::SeqCst), 1);
    assert_eq!(*extension.seen.lock().unwrap(), sessions);
}

#[tokio::test]
async fn a_failing_extension_hook_is_reported_under_the_extension_name() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]));
    let mut config = base(provider);
    let broken = Bundle {
        hooks: || vec![Arc::new(Broken) as Arc<dyn Hook>],
        ..Bundle::named("broken")
    };
    config.extensions = vec![Arc::new(broken)];
    let mut context = Vec::new();
    let (_, events) = run_prompt(config, &mut context, "go").await;
    assert!(
        events.events().iter().any(|event| matches!(
            event,
            EventKind::HookFailed { hook, phase, .. } if hook == "broken" && *phase == HookPoint::AfterTool
        )),
        "{:?}",
        events.kinds()
    );
}
