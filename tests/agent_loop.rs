//! Loop semantics, mirroring pi's agent-loop behavior.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use common::{Collect, EchoTool, config, run_prompt, summarize, with_policy};
use zx_agent::agent::{Decision, ExecutionMode, LoopControl, LoopOutcome, Tool, ToolPolicy, run_loop};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{ContentBlock, EventReceiver, Message, PREAMBLE, Provider, Request, StopReason, SystemPrompt};
use zx_agent::interaction::ApprovalCache;

fn provider(steps: impl IntoIterator<Item = FauxStep>) -> Arc<FauxProvider> {
    Arc::new(FauxProvider::new(steps))
}

fn echo() -> Arc<EchoTool> {
    Arc::new(EchoTool::new("echo"))
}

// ---------------------------------------------------------------------------
// Turns and tool execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runs_tools_until_the_model_answers() {
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]);
    let config = config(provider.clone(), vec![echo()]);
    let mut context = Vec::new();

    let (result, _) = run_prompt(&config, &mut context, "go").await;

    assert_eq!((result.outcome, result.turns), (LoopOutcome::Completed, 2));
    assert_eq!(
        summarize(&context),
        ["user:go", "assistant:calls[c1]", "tool:c1=hi", "assistant:done"]
    );
    // The second request carries the tool result back to the model.
    assert_eq!(
        summarize(&provider.requests()[1].messages),
        ["user:go", "assistant:calls[c1]", "tool:c1=hi"]
    );
}

#[tokio::test]
async fn events_bracket_the_run_its_turns_and_tool_calls() {
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]);
    let config = config(provider, vec![echo()]);

    let (_, sink) = run_prompt(&config, &mut Vec::new(), "go").await;

    let kinds = sink.kinds();
    assert_eq!(
        kinds[..4],
        ["agent_start", "turn_start", "message_start", "message_end"]
    );
    assert_eq!(kinds.last(), Some(&"agent_end"));
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("tool_execution_start") < position("tool_execution_end"));
    assert_eq!(kinds.iter().filter(|kind| **kind == "turn_start").count(), 2);
}

#[tokio::test]
async fn appends_to_the_existing_context_without_changing_it() {
    let provider = provider([FauxStep::text("second answer")]);
    let config = config(provider.clone(), vec![]);
    let earlier = vec![Message::user("first"), Message::user("still first")];
    let mut context = earlier.clone();

    run_prompt(&config, &mut context, "second").await;

    assert_eq!(context[..2], earlier[..]);
    assert_eq!(summarize(&context[2..]), ["user:second", "assistant:second answer"]);
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:first", "user:still first", "user:second"]
    );
}

#[tokio::test]
async fn parallel_calls_finish_in_any_order_but_results_keep_call_order() {
    let provider = provider([
        FauxStep::tool_calls(vec![
            ("slow", "echo", json!({"text": "slow", "ms": 300})),
            ("fast", "echo", json!({"text": "fast", "ms": 10})),
        ]),
        FauxStep::text("done"),
    ]);
    let config = config(provider, vec![echo()]);
    let mut context = Vec::new();

    let (_, sink) = run_prompt(&config, &mut context, "go").await;

    // "fast" ends first although "slow" started first, so they ran concurrently.
    assert_eq!(sink.tool_end_order(), ["fast", "slow"]);
    assert_eq!(summarize(&context)[2..4], ["tool:slow=slow", "tool:fast=fast"]);
}

#[tokio::test]
async fn one_sequential_tool_makes_the_batch_sequential() {
    let provider = provider([
        FauxStep::tool_calls(vec![
            ("slow", "echo", json!({"text": "slow", "ms": 200})),
            ("fast", "seq", json!({"text": "fast", "ms": 10})),
        ]),
        FauxStep::text("done"),
    ]);
    let mut seq = EchoTool::new("seq");
    seq.mode = ExecutionMode::Sequential;
    let config = config(provider, vec![echo(), Arc::new(seq)]);

    let (_, sink) = run_prompt(&config, &mut Vec::new(), "go").await;

    assert_eq!(sink.tool_end_order(), ["slow", "fast"]);
}

struct DenyEcho;

struct PanickingPolicy;

#[async_trait]
impl ToolPolicy for PanickingPolicy {
    async fn check(&self, _tool_name: &str, _args: &Value) -> Decision {
        panic!("policy bug");
    }
}

#[tokio::test]
async fn a_panicking_policy_blocks_the_tool_and_the_model_can_continue() {
    let provider = provider([
        FauxStep::tool_call("c", "echo", json!({"text": "must not run"})),
        FauxStep::text("handled"),
    ]);
    let tool = echo();
    let config = with_policy(config(provider.clone(), vec![tool.clone()]), Arc::new(PanickingPolicy));
    let mut context = Vec::new();

    let (result, _) = run_prompt(&config, &mut context, "go").await;

    assert_eq!(result.outcome, LoopOutcome::Completed);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    let tool_result = &provider.requests()[1].messages[2];
    let Message::ToolResult(tool_result) = tool_result else {
        panic!("expected a tool result");
    };
    assert!(tool_result.is_error);
    assert!(tool_result.content.contains("panicked"), "{}", tool_result.content);
}

struct WaitingPolicy(Arc<tokio::sync::Notify>);

#[async_trait]
impl ToolPolicy for WaitingPolicy {
    async fn check(&self, _tool_name: &str, _args: &Value) -> Decision {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_interrupts_a_waiting_policy() {
    let provider = provider([FauxStep::tool_call("c", "echo", json!({"text": "must not run"}))]);
    let entered = Arc::new(tokio::sync::Notify::new());
    let tool = echo();
    let config = with_policy(
        config(provider, vec![tool.clone()]),
        Arc::new(WaitingPolicy(entered.clone())),
    );
    let cancel = CancellationToken::new();
    let sink = Collect::default();
    let mut context = Vec::new();
    let mut approvals = ApprovalCache::default();
    let mut control = LoopControl::new(Some(10), &mut approvals, &cancel, &sink);
    let mut running = Box::pin(run_loop(&mut context, vec![Message::user("go")], &config, &mut control));
    tokio::select! {
        _ = entered.notified() => {}
        _ = &mut running => panic!("policy must still be waiting"),
    }
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .expect("cancellation must interrupt the policy");
    assert_eq!(result.outcome, LoopOutcome::Aborted);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

#[async_trait]
impl ToolPolicy for DenyEcho {
    async fn check(&self, tool_name: &str, _args: &Value) -> Decision {
        if tool_name == "echo" {
            Decision::Deny("echo is disabled".into())
        } else {
            Decision::Allow
        }
    }
}

#[tokio::test]
async fn failures_become_error_results_and_the_loop_continues() {
    let provider = provider([
        FauxStep::tool_calls(vec![
            ("a", "missing", json!({})),
            ("b", "other", json!("not an object")),
            ("c", "echo", json!({"text": "x"})),
            ("d", "other", json!({"fail": "disk not mounted"})),
            ("e", "other", json!({"panic": true})),
        ]),
        FauxStep::text("handled"),
    ]);
    let echo = echo();
    let tools: Vec<Arc<dyn Tool>> = vec![echo.clone(), Arc::new(EchoTool::new("other"))];
    let config = with_policy(config(provider, tools), Arc::new(DenyEcho));
    let mut context = Vec::new();

    let (result, _) = run_prompt(&config, &mut context, "go").await;

    assert_eq!(result.outcome, LoopOutcome::Completed);
    // One case per kind of failure: not found, invalid arguments, denied, error, panic.
    assert_eq!(
        summarize(&context)[2..7],
        [
            "tool:a=Tool missing not found!",
            "tool:b=Invalid arguments for tool \"other\": expected a JSON object!",
            "tool:c=Blocked by policy: echo is disabled!",
            "tool:d=disk not mounted!",
            "tool:e=Tool other panicked!",
        ]
    );
    assert_eq!(echo.calls.load(Ordering::SeqCst), 0, "denied calls never run");
    assert_eq!(summarize(&context).last().unwrap(), "assistant:handled");
}

#[tokio::test]
async fn length_stop_fails_tool_calls_without_running_them() {
    let cut_off = FauxStep::Respond {
        blocks: vec![ContentBlock::ToolCall {
            id: "t".into(),
            name: "echo".into(),
            arguments: json!({"text": "cut"}),
            arguments_error: None,
        }],
        stop: StopReason::Length,
    };
    let provider = provider([cut_off, FauxStep::text("retried")]);
    let echo = echo();
    let config = config(provider, vec![echo.clone()]);
    let mut context = Vec::new();

    run_prompt(&config, &mut context, "go").await;

    assert_eq!(echo.calls.load(Ordering::SeqCst), 0);
    assert!(summarize(&context)[2].contains("output token limit"));
}

#[tokio::test]
async fn stops_at_the_turn_budget() {
    let provider = provider((0..5).map(|i| FauxStep::tool_call(&format!("c{i}"), "echo", json!({"text": "again"}))));
    let config = config(provider.clone(), vec![echo()]);
    let prompts = vec![Message::user("go")];

    let result = run_loop(
        &mut Vec::new(),
        prompts,
        &config,
        &mut LoopControl::new(
            Some(2),
            &mut ApprovalCache::default(),
            &CancellationToken::new(),
            &Collect::default(),
        ),
    )
    .await;

    assert_eq!((result.outcome, result.turns), (LoopOutcome::MaxTurns, 2));
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn terminate_ends_only_when_every_result_asks() {
    let provider = provider([
        FauxStep::tool_calls(vec![
            ("a", "echo", json!({"terminate": true})),
            ("b", "echo", json!({})),
        ]),
        FauxStep::tool_calls(vec![
            ("c", "echo", json!({"terminate": true})),
            ("d", "echo", json!({"terminate": true})),
        ]),
        FauxStep::text("never requested"),
    ]);
    let config = config(provider.clone(), vec![echo()]);

    let (result, _) = run_prompt(&config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, LoopOutcome::Completed);
    assert_eq!((provider.requests().len(), provider.remaining()), (2, 1));
}

#[tokio::test]
async fn steering_enters_after_the_turn_and_follow_ups_after_the_answer() {
    let provider = provider([
        FauxStep::tool_call(
            "c1",
            "echo",
            json!({"text": "ok", "steer": "also check disk", "follow_up": "then summarize"}),
        ),
        FauxStep::text("disk fine"),
        FauxStep::text("summary"),
    ]);
    let config = config(provider, vec![]);
    let mut echo = EchoTool::new("echo");
    echo.queues = Some(config.queues.clone());
    let mut config = config;
    config.spec.tools = vec![Arc::new(echo)];
    let mut context = Vec::new();

    let (result, _) = run_prompt(&config, &mut context, "go").await;

    assert_eq!(result.outcome, LoopOutcome::Completed);
    assert_eq!(
        summarize(&context),
        [
            "user:go",
            "assistant:calls[c1]",
            "tool:c1=ok",
            "user:also check disk",
            "assistant:disk fine",
            "user:then summarize",
            "assistant:summary",
        ]
    );
}

#[tokio::test]
async fn a_spent_budget_starts_no_new_turn_and_leaves_queued_input_queued() {
    let provider = provider([FauxStep::tool_call(
        "c1",
        "echo",
        json!({"text": "ok", "steer": "also check disk"}),
    )]);
    let config = config(provider.clone(), vec![]);
    let mut echo = EchoTool::new("echo");
    echo.queues = Some(config.queues.clone());
    let mut config = config;
    config.spec.tools = vec![Arc::new(echo)];
    let mut context = Vec::new();
    let events = Collect::default();

    let result = run_loop(
        &mut context,
        vec![Message::user("go")],
        &config,
        &mut LoopControl::new(
            Some(1),
            &mut ApprovalCache::default(),
            &CancellationToken::new(),
            &events,
        ),
    )
    .await;

    assert_eq!(
        (result.outcome, result.turns, result.requests),
        (LoopOutcome::MaxTurns, 1, 1)
    );
    assert_eq!(summarize(&context), ["user:go", "assistant:calls[c1]", "tool:c1=ok"]);
    assert_eq!(summarize(&config.queues.drain_steering()), ["user:also check disk"]);
    let turns: Vec<_> = events
        .kinds()
        .into_iter()
        .filter(|kind| kind.starts_with("turn_"))
        .collect();
    assert_eq!(turns, ["turn_start", "turn_end"]);
}

// ---------------------------------------------------------------------------
// Provider failures and cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_error_ends_the_loop_after_a_start_end_pair() {
    let config = config(provider([FauxStep::Fail("HTTP 500: boom".into())]), vec![]);

    let (result, sink) = run_prompt(&config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, LoopOutcome::Error);
    // A failure before streaming still produces a start/end pair for the assistant message.
    let kinds = sink.kinds();
    assert_eq!(
        kinds[kinds.len() - 4..],
        ["message_start", "message_end", "turn_end", "agent_end"]
    );
}

#[tokio::test]
async fn cancellation_ends_the_loop() {
    let config = config(provider([FauxStep::Hang]), vec![]);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    // The delay only lets the request start; the assertion does not depend on timing.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let prompts = vec![Message::user("go")];

    let result = run_loop(
        &mut Vec::new(),
        prompts,
        &config,
        &mut LoopControl::new(Some(10), &mut ApprovalCache::default(), &cancel, &Collect::default()),
    )
    .await;

    assert_eq!(result.outcome, LoopOutcome::Aborted);
}

/// Breaks the provider contract: its stream ends without a `Done` or `Error` event.
struct SilentProvider;

impl Provider for SilentProvider {
    fn name(&self) -> &str {
        "silent"
    }

    fn stream(&self, _request: Request, _cancel: CancellationToken) -> EventReceiver {
        // Dropping the sender ends the stream at once.
        let (_sender, receiver) = zx_agent::ai::provider::event_channel();
        receiver
    }
}

#[tokio::test]
async fn a_stream_without_a_final_event_is_an_error() {
    let config = config(Arc::new(SilentProvider), vec![]);
    let mut context = Vec::new();

    let (result, _) = run_prompt(&config, &mut context, "go").await;

    assert_eq!(result.outcome, LoopOutcome::Error);
    let Some(Message::Assistant(answer)) = context.last() else {
        panic!("the failure is recorded as an assistant message: {context:?}");
    };
    assert_eq!(
        answer.error_message.as_deref(),
        Some("provider stream ended without a final event")
    );
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

fn prompt(started: &str) -> SystemPrompt {
    SystemPrompt::default()
        .with_section(PREAMBLE, "Do the task.")
        .with_section("environment", started)
}

/// Run `text` on `context` with `system_prompt`; the model answers `answer`.
async fn run_with_prompt(context: &mut Vec<Message>, system_prompt: SystemPrompt, text: &str, answer: &str) {
    let mut config = config(provider([FauxStep::text(answer)]), vec![]);
    config.spec.system_prompt = system_prompt;
    run_prompt(&config, context, text).await;
}

#[tokio::test]
async fn records_the_system_prompt_before_the_first_prompt() {
    let mut context = Vec::new();
    run_with_prompt(&mut context, prompt("t1"), "a", "one").await;
    assert_eq!(
        summarize(&context),
        ["system:preamble,environment", "user:a", "assistant:one"]
    );
}

#[tokio::test]
async fn later_runs_record_only_the_changed_sections() {
    let mut context = Vec::new();
    run_with_prompt(&mut context, prompt("t1"), "a", "one").await;
    run_with_prompt(&mut context, prompt("t2"), "b", "two").await;
    assert_eq!(
        summarize(&context)[3..],
        ["system:environment", "user:b", "assistant:two"]
    );
}

#[tokio::test]
async fn an_unchanged_prompt_adds_no_system_message() {
    let mut context = Vec::new();
    run_with_prompt(&mut context, prompt("t1"), "a", "one").await;
    run_with_prompt(&mut context, prompt("t1"), "b", "two").await;
    assert_eq!(summarize(&context)[3..], ["user:b", "assistant:two"]);
}
