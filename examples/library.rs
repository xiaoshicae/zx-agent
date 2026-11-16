//! Embedding zx-agent as a library. The embedder chooses the instructions, tools, rules and
//! limits: the kernel has no scenario of its own, and its default limits are unlimited.
//!
//! Run: ZX_BASE_URL=http://127.0.0.1:11434/v1 ZX_MODEL=<model> cargo run --example library -- "task"

use std::sync::Arc;
use std::time::Duration;

use zx_agent::agent::RunLimits;
use zx_agent::ai::openai::{OpenAiCompatible, OpenAiConfig};
use zx_agent::builtin::continuation::ContinueOnLength;
use zx_agent::builtin::executor::{LocalExecutor, local_environment_section};
use zx_agent::builtin::policy::{AutoApprove, CommandPolicy, Rule, RuleMatcher, RulePolicy};
use zx_agent::builtin::redact::RedactSecrets;
use zx_agent::builtin::tools::{BashTool, ReadFileTool};
use zx_agent::events::EventKind;
use zx_agent::policy::PolicyDecision;
use zx_agent::prompt::build_system_prompt;
use zx_agent::{Agent, AgentConfig, SessionOptions};

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
    // Read-only diagnostics only: other commands ask, and nobody is there to approve them.
    // `ps` is left out: on Linux, `ps e` can print the environment of the user's processes,
    // which includes this process's API key. So is `sort`, whose `--compress-program` runs
    // another program.
    let rules = RulePolicy::new(vec![
        Rule {
            matcher: RuleMatcher::Tool("bash".to_string()),
            decision: PolicyDecision::Ask("only read-only diagnostics run unattended".to_string()),
        },
        Rule {
            matcher: RuleMatcher::Any,
            decision: PolicyDecision::Allow,
        },
    ])
    .auto_approve(AutoApprove::new(["df", "free", "uptime", "head"]));
    let system_prompt = build_system_prompt(INSTRUCTIONS, &[("environment", &local_environment_section(&cwd))]);
    // The key must not reach commands, where `env` would reveal it.
    let bash = BashTool {
        executor: Arc::new(LocalExecutor {
            hidden_env: vec!["OPENAI_API_KEY".to_string()],
        }),
        ..BashTool::default()
    };

    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: std::env::var("ZX_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        api_key: api_key.clone().into(),
        ..OpenAiConfig::default()
    })?;
    let mut config = AgentConfig {
        system_prompt,
        tools: vec![Arc::new(bash), Arc::new(ReadFileTool::default())],
        // Nobody can stop an unattended run, so it gets bounds of its own.
        limits: RunLimits {
            max_model_requests: Some(30),
            timeout: Some(Duration::from_secs(15 * 60)),
            token_budget: None,
            cost_budget: None,
        },
        ..AgentConfig::new(Arc::new(provider), model)
    };
    // Continue once if an answer stops at the output token limit.
    config
        .hooks
        .push("continue_on_length", Arc::new(ContinueOnLength::new(1)));
    config.policies.push("command_policy", Arc::new(CommandPolicy::new()));
    config.policies.push("permissions", Arc::new(rules));
    config
        .hooks
        .push("redact_secrets", Arc::new(RedactSecrets::new(api_key)));
    // The agent keeps its sessions in memory unless it is given a storage; this one works
    // in the current directory.
    let agent = Agent::new(config)?;
    let session = agent.create_session(SessionOptions::new(cwd)).await?;

    // A run makes its requests as it is read: its events first, then its report.
    let mut run = session.send(task)?;
    while let Some(event) = run.next().await {
        if let EventKind::ToolExecutionStart { tool_name, args, .. } = &event.event {
            eprintln!("[tool] {tool_name} {args}");
        }
    }
    let report = run.wait().await;
    println!("{}", report.final_text);
    eprintln!(
        "outcome: {:?}, model requests: {}, tokens: {}",
        report.outcome, report.model_requests, report.usage.total_tokens
    );
    Ok(())
}
