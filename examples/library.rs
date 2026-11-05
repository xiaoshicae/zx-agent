//! Embedding zx-agent as a library. The embedder chooses the instructions, tools, rules and
//! limits: the kernel has no scenario of its own, and its default limits are unlimited.
//!
//! Run: ZX_BASE_URL=http://127.0.0.1:11434/v1 ZX_MODEL=<model> cargo run --example library -- "task"

use std::sync::Arc;
use std::time::Duration;

use zx_agent::AgentSession;
use zx_agent::agent::{AgentEvent, Event, LoopConfig, LoopSpec};
use zx_agent::ai::openai::{OpenAiCompatible, OpenAiConfig};
use zx_agent::ai::retry::RetryPolicy;
use zx_agent::continuation::ContinueOnLength;
use zx_agent::policy::{CommandPolicy, PolicyHook};
use zx_agent::prompt::{build_system_prompt, environment_section};
use zx_agent::redact::RedactSecrets;
use zx_agent::runner::Limits;
use zx_agent::session::{SessionHeader, SessionStore};
use zx_agent::tools::{BashTool, ReadFileTool};

/// This embedder's scenario: a bounded, read-only report about the machine it runs on.
const INSTRUCTIONS: &str = "You are an agent running without a human operator. Use only the tools \
provided and only read-only commands. Finish with a short report: what you found, and the commands \
whose output shows it.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Report disk and memory usage.".to_string());
    let model = std::env::var("ZX_MODEL")?;
    let cwd = std::env::current_dir()?;
    // Read-only diagnostics only. `ps` is left out: on Linux, `ps e` can print the environment
    // of the user's processes, which includes this process's API key.
    let policy = CommandPolicy::new(["df", "free", "uptime", "head", "sort"]);
    let system_prompt = build_system_prompt(
        INSTRUCTIONS,
        &[
            ("environment", &environment_section(&cwd)),
            ("command_policy", &policy.prompt_section().unwrap_or_default()),
        ],
    );
    // The key must not reach commands, where `env` would reveal it.
    let bash = BashTool {
        hidden_env: vec!["OPENAI_API_KEY".to_string()],
        ..BashTool::default()
    };

    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: std::env::var("ZX_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        api_key: api_key.clone(),
        ..OpenAiConfig::default()
    })?;
    let mut config = LoopConfig::new(LoopSpec {
        provider: Arc::new(provider),
        model: model.clone(),
        system_prompt,
        tools: vec![Arc::new(bash), Arc::new(ReadFileTool::default())],
        cwd: cwd.clone(),
        max_tokens: None,
        reasoning: None,
        sequential_tools: false,
    });
    // Continue once if an answer stops at the output token limit.
    config.hooks.push(ContinueOnLength::new(1));
    config.hooks.push(PolicyHook::new(Arc::new(policy)));
    config.hooks.push(RedactSecrets::new(api_key));
    let session = SessionStore::in_memory(SessionHeader::new(&cwd, &model));
    // Any `Fn(&Event)` is an event sink.
    let sink = Arc::new(|event: &Event| {
        if let AgentEvent::ToolExecutionStart { tool_name, args, .. } = &event.event {
            eprintln!("[tool] {tool_name} {args}");
        }
    });
    // Nobody can stop an unattended run, so it gets bounds of its own.
    let limits = Limits {
        max_model_requests: Some(30),
        timeout: Some(Duration::from_secs(15 * 60)),
        token_budget: None,
    };
    let agent = AgentSession::new(config, RetryPolicy::default(), limits, session, sink);

    let report = agent.prompt(task).await?;
    println!("{}", report.final_text);
    eprintln!(
        "outcome: {:?}, model requests: {}, tokens: {}",
        report.outcome, report.model_requests, report.usage.total_tokens
    );
    Ok(())
}
