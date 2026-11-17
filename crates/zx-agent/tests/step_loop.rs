//! Loop semantics, mirroring pi's agent-loop behavior.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::AgentConfig;
use zx_agent::agent::RunLimits;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{
    ContentBlock, EventReceiver, Message, PREAMBLE, Provider, Request, StopReason, SystemPrompt, ToolCall, ToolDef,
    Usage,
};
use zx_agent::events::{Budget, EventKind, RunOutcome};
use zx_agent::input::Input;
use zx_agent::policy::{PolicyContext, PolicyDecision, ToolPolicy};
use zx_agent::tools::{Effects, ExecutionMode, MAX_DETAILS_BYTES, Tool, ToolContext, ToolOutput};
use zx_testing::{
    Collect, Driven, EchoTool, NotifyingProvider, RunnerSetup, config, run_prompt, run_prompt_with, summarize,
    take_queued_inputs,
};

fn provider(steps: impl IntoIterator<Item = FauxStep>) -> Arc<FauxProvider> {
    Arc::new(FauxProvider::new(steps))
}

fn echo() -> Arc<EchoTool> {
    Arc::new(EchoTool::new("echo"))
}

fn with_policy(mut config: AgentConfig, policy: Arc<dyn ToolPolicy>) -> AgentConfig {
    config.policies.push("policy", policy);
    config
}

/// A session of `provider`, limited to `max_model_requests` requests, whose `echo` tool
/// feeds the session's own queues when a call asks it to.
async fn fed(provider: Arc<FauxProvider>, max_model_requests: u32) -> (Driven, Arc<Collect>) {
    let echo = echo();
    let limits = RunLimits {
        max_model_requests: Some(max_model_requests),
        ..RunLimits::default()
    };
    let (runner, events) = RunnerSetup {
        limits,
        ..RunnerSetup::new(config(provider, vec![echo.clone()]))
    }
    .build()
    .await;
    echo.feeds(runner.queues());
    (runner, events)
}

// ---------------------------------------------------------------------------
// Steps and tool execution
// ---------------------------------------------------------------------------

/// Counts how often it is asked how it runs and what it changes.
struct Described(std::sync::atomic::AtomicUsize);

#[async_trait]
impl Tool for Described {
    fn name(&self) -> &str {
        "inspect"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "inspect".into(),
            description: "Inspects".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.0.fetch_add(1, Ordering::SeqCst);
        ExecutionMode::Parallel
    }

    fn effects(&self) -> Effects {
        self.0.fetch_add(1, Ordering::SeqCst);
        Effects::SideEffects
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        Ok(ToolOutput::ok("inspected"))
    }
}

#[tokio::test]
async fn a_run_goes_by_what_its_tools_declared_when_the_session_was_made() {
    let provider = provider([FauxStep::tool_call("c1", "inspect", json!({})), FauxStep::text("done")]);
    let tool = Arc::new(Described(std::sync::atomic::AtomicUsize::new(0)));
    let (runner, _) = RunnerSetup::new(config(provider, vec![tool.clone()])).build().await;
    let declared = tool.0.load(Ordering::SeqCst);

    let report = runner.run("go").await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(
        tool.0.load(Ordering::SeqCst),
        declared,
        "the run asks the tool nothing more"
    );
    let started = runner
        .session()
        .entries()
        .iter()
        .filter(|entry| entry.kind() == "tool_call_started")
        .count();
    assert_eq!(started, 1, "declared with side effects, the call's start is recorded");
}

#[tokio::test]
async fn a_call_that_repeats_the_id_of_an_earlier_call_of_the_answer_is_answered_with_an_error() {
    let provider = provider([
        FauxStep::tool_calls(vec![
            ("c1", "echo", json!({"text": "first"})),
            ("c1", "echo", json!({"text": "second"})),
        ]),
        FauxStep::text("done"),
    ]);
    let tool = echo();
    let config = config(provider.clone(), vec![tool.clone()]);
    let mut context = Vec::new();

    let (result, _) = run_prompt(config, &mut context, "go").await;

    // The model decides what to do about it: the run goes on.
    assert_eq!(result.outcome, RunOutcome::Completed, "{result:?}");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1, "only the first call runs");
    let results: Vec<&Message> = context
        .iter()
        .filter(|message| matches!(message, Message::ToolResult(_)))
        .collect();
    let [Message::ToolResult(first), Message::ToolResult(second)] = results[..] else {
        panic!("each call gets a result: {:?}", summarize(&context));
    };
    assert_eq!((first.content.as_str(), first.is_error), ("first", false));
    assert!(second.is_error, "{second:?}");
    assert!(second.content.contains("same ID"), "{}", second.content);
}

#[tokio::test]
async fn runs_tools_until_the_model_answers() {
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]);
    let config = config(provider.clone(), vec![echo()]);
    let mut context = Vec::new();

    let (result, sink) = run_prompt(config, &mut context, "go").await;

    assert_eq!((result.outcome, sink.steps()), (RunOutcome::Completed, 2));
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
async fn events_bracket_the_run_its_steps_and_tool_calls() {
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]);
    let config = config(provider, vec![echo()]);

    let (_, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    let kinds = sink.kinds();
    assert_eq!(
        kinds[..5],
        [
            "run_start",
            "attempt_start",
            "step_start",
            "message_start",
            "message_end"
        ]
    );
    assert_eq!(kinds[kinds.len() - 2..], ["attempt_end", "run_end"]);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("tool_execution_start") < position("tool_execution_end"));
    assert_eq!(kinds.iter().filter(|kind| **kind == "step_start").count(), 2);
}

#[tokio::test]
async fn appends_to_the_existing_context_without_changing_it() {
    let provider = provider([FauxStep::text("second answer")]);
    let config = config(provider.clone(), vec![]);
    let earlier = vec![Message::user("first"), Message::user("still first")];
    let mut context = earlier.clone();

    run_prompt(config, &mut context, "second").await;

    assert_eq!(context[..2], earlier[..]);
    assert_eq!(summarize(&context[2..]), ["user:second", "assistant:second answer"]);
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:first", "user:still first", "user:second"]
    );
}

/// A tool whose call "waits" does not return before its call "arrives" has run: both
/// return only when they run at the same time.
#[derive(Default)]
struct Rendezvous {
    arrived: tokio::sync::Notify,
}

#[async_trait]
impl Tool for Rendezvous {
    fn name(&self) -> &str {
        "meet"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "meet".into(),
            description: "Waits for its other call".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        let role = args.get("role").and_then(Value::as_str).unwrap_or_default();
        if role == "waits" {
            self.arrived.notified().await;
        } else {
            self.arrived.notify_one();
        }
        Ok(ToolOutput::ok(role))
    }
}

#[tokio::test]
async fn parallel_calls_run_at_the_same_time_and_their_results_keep_call_order() {
    let provider = provider([
        FauxStep::tool_calls(vec![
            ("c1", "meet", json!({"role": "waits"})),
            ("c2", "meet", json!({"role": "arrives"})),
        ]),
        FauxStep::text("done"),
    ]);
    let config = config(provider, vec![Arc::new(Rendezvous::default())]);
    let mut context = Vec::new();

    // Run one after the other, the first call would wait for the second forever.
    let (result, _) = tokio::time::timeout(Duration::from_secs(10), run_prompt(config, &mut context, "go"))
        .await
        .expect("the calls run at the same time");

    assert_eq!(result.outcome, RunOutcome::Completed, "{result:?}");
    assert_eq!(summarize(&context)[2..4], ["tool:c1=waits", "tool:c2=arrives"]);
}

#[tokio::test]
async fn one_sequential_tool_makes_the_batch_sequential() {
    let provider = provider([
        FauxStep::tool_calls(vec![
            ("first", "echo", json!({"text": "first"})),
            ("second", "seq", json!({"text": "second"})),
        ]),
        FauxStep::text("done"),
    ]);
    let mut seq = EchoTool::new("seq");
    seq.mode = ExecutionMode::Sequential;
    let config = config(provider, vec![echo(), Arc::new(seq)]);

    let (_, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    // Each call ends before the next one starts.
    let order: Vec<&str> = sink
        .kinds()
        .into_iter()
        .filter(|kind| kind.starts_with("tool_execution_"))
        .collect();
    assert_eq!(
        order,
        [
            "tool_execution_start",
            "tool_execution_end",
            "tool_execution_start",
            "tool_execution_end"
        ]
    );
}

struct DenyEcho;

struct PanickingPolicy;

#[async_trait]
impl ToolPolicy for PanickingPolicy {
    async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
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

    let (result, _) = run_prompt(config, &mut context, "go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
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
    async fn check(&self, _call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
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
    let (runner, _) = RunnerSetup::new(config).build().await;
    let mut running = Box::pin(runner.run("go"));
    tokio::select! {
        () = entered.notified() => {}
        _ = &mut running => panic!("policy must still be waiting"),
    }
    runner.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .expect("cancellation must interrupt the policy");
    assert_eq!(result.outcome, RunOutcome::Cancelled);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

#[async_trait]
impl ToolPolicy for DenyEcho {
    async fn check(&self, call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        if call.name == "echo" {
            PolicyDecision::Deny("echo is disabled".into())
        } else {
            PolicyDecision::Allow
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

    let (result, _) = run_prompt(config, &mut context, "go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
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
        blocks: vec![ContentBlock::ToolCall(ToolCall {
            id: "t".into(),
            name: "echo".into(),
            arguments: json!({"text": "cut"}),
            arguments_error: None,
        })],
        stop: StopReason::Length,
    };
    let provider = provider([cut_off, FauxStep::text("retried")]);
    let echo = echo();
    let config = config(provider, vec![echo.clone()]);
    let mut context = Vec::new();

    run_prompt(config, &mut context, "go").await;

    assert_eq!(echo.calls.load(Ordering::SeqCst), 0);
    assert!(summarize(&context)[2].contains("output token limit"));
}

#[tokio::test]
async fn stops_at_the_step_budget() {
    let provider = provider((0..5).map(|i| FauxStep::tool_call(&format!("c{i}"), "echo", json!({"text": "again"}))));
    let config = config(provider.clone(), vec![echo()]);

    let (result, sink) = run_prompt_with(config, &mut Vec::new(), "go", 2).await;

    assert_eq!(
        (result.outcome, sink.steps()),
        (
            RunOutcome::BudgetExhausted {
                budget: Budget::ModelRequests,
                truncated: false
            },
            2
        )
    );
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

    let (result, _) = run_prompt(config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!((provider.requests().len(), provider.remaining()), (2, 1));
}

#[tokio::test]
async fn steering_enters_after_the_step_and_follow_ups_after_the_answer() {
    let provider = provider([
        FauxStep::tool_call(
            "c1",
            "echo",
            json!({"text": "ok", "steer": "also check disk", "follow_up": "then summarize"}),
        ),
        FauxStep::text("disk fine"),
        FauxStep::text("summary"),
    ]);
    let (runner, _) = fed(provider, 10).await;

    let result = runner.run("go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!(
        summarize(&runner.session().messages()),
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
async fn a_withdrawn_follow_up_is_never_taken() {
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "ok", "withdraw": "then summarize"})),
        FauxStep::text("done"),
        FauxStep::text("stopped"),
    ]);
    let (runner, _) = fed(provider, 10).await;
    runner.queues().follow_up("then summarize");
    runner.queues().follow_up("and then stop");

    let result = runner.run("go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!(
        summarize(&runner.session().messages()),
        [
            "user:go",
            "assistant:calls[c1]",
            "tool:c1=ok",
            "assistant:done",
            "user:and then stop",
            "assistant:stopped",
        ]
    );
    assert!(take_queued_inputs(&runner).unwrap().is_empty());
}

#[tokio::test]
async fn a_spent_budget_starts_no_new_step_and_leaves_queued_input_queued() {
    let provider = provider([FauxStep::tool_call(
        "c1",
        "echo",
        json!({"text": "ok", "steer": "also check disk"}),
    )]);
    let (runner, events) = fed(provider, 1).await;

    let result = runner.run("go").await;

    assert_eq!(
        (result.outcome, events.steps(), result.model_requests),
        (
            RunOutcome::BudgetExhausted {
                budget: Budget::ModelRequests,
                truncated: false
            },
            1,
            1
        )
    );
    assert_eq!(
        summarize(&runner.session().messages()),
        ["user:go", "assistant:calls[c1]", "tool:c1=ok"]
    );
    assert_eq!(take_queued_inputs(&runner).unwrap(), [Input::from("also check disk")]);
    let steps: Vec<_> = events
        .kinds()
        .into_iter()
        .filter(|kind| kind.starts_with("step_"))
        .collect();
    assert_eq!(steps, ["step_start", "step_end"]);
}

// ---------------------------------------------------------------------------
// Provider failures and cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_error_ends_the_loop_after_a_start_end_pair() {
    let config = config(provider([FauxStep::Fail("HTTP 500: boom".into())]), vec![]);

    let (result, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, RunOutcome::Failed);
    // A failure before streaming still produces a start/end pair for the assistant message.
    let kinds = sink.kinds();
    assert_eq!(
        kinds[kinds.len() - 5..],
        ["message_start", "message_end", "step_end", "attempt_end", "run_end"]
    );
}

#[tokio::test]
async fn cancellation_ends_the_loop() {
    let provider = NotifyingProvider::new(FauxProvider::new([FauxStep::Hang]));
    let (runner, _) = RunnerSetup::new(config(provider.clone(), vec![])).build().await;
    // Cancelled once the request has started.
    let (result, ()) = tokio::join!(runner.run("go"), async {
        provider.started.notified().await;
        runner.cancel();
    });

    assert_eq!(result.outcome, RunOutcome::Cancelled);
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

    let (result, _) = run_prompt(config, &mut context, "go").await;

    assert_eq!(result.outcome, RunOutcome::Failed);
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
    config.system_prompt = system_prompt;
    run_prompt(config, context, text).await;
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

// ---------------------------------------------------------------------------
// Argument preparation, progress and details
// ---------------------------------------------------------------------------

/// Takes `text` and accepts the older name `cmd` for it. `boom` makes the preparation panic.
#[derive(Default)]
struct Renaming {
    ran_with: Mutex<Vec<Value>>,
}

#[async_trait]
impl Tool for Renaming {
    fn name(&self) -> &str {
        "renaming"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "renaming".into(),
            description: "Echo text".into(),
            parameters: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
            }),
        }
    }

    fn prepare_arguments(&self, args: &Value) -> Option<Value> {
        assert!(args.get("boom").is_none(), "preparation bug");
        args.get("cmd").map(|cmd| json!({"text": cmd}))
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        self.ran_with.lock().unwrap().push(args.clone());
        Ok(ToolOutput::ok(args["text"].as_str().unwrap_or_default()))
    }
}

/// Allows every call and records the arguments it was asked about.
#[derive(Default)]
struct RecordingPolicy(Mutex<Vec<Value>>);

#[async_trait]
impl ToolPolicy for RecordingPolicy {
    async fn check(&self, call: &ToolCall, _ctx: &PolicyContext) -> PolicyDecision {
        self.0.lock().unwrap().push(call.arguments.clone());
        PolicyDecision::Allow
    }
}

fn tool_result(messages: &[Message], index: usize) -> &zx_agent::ai::ToolResultMessage {
    match &messages[index] {
        Message::ToolResult(result) => result,
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn policy_and_tool_see_prepared_arguments_while_the_context_keeps_the_original() {
    let provider = provider([
        FauxStep::tool_call("c1", "renaming", json!({"cmd": "hi"})),
        FauxStep::text("done"),
    ]);
    let tool = Arc::new(Renaming::default());
    let policy = Arc::new(RecordingPolicy::default());
    let config = with_policy(config(provider.clone(), vec![tool.clone()]), policy.clone());
    let mut context = Vec::new();

    let (result, _) = run_prompt(config, &mut context, "go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert_eq!(*policy.0.lock().unwrap(), [json!({"text": "hi"})]);
    assert_eq!(*tool.ran_with.lock().unwrap(), [json!({"text": "hi"})]);
    assert_eq!(summarize(&context)[2], "tool:c1=hi");
    let Message::Assistant(answer) = &context[1] else {
        panic!("expected the assistant message");
    };
    assert_eq!(answer.tool_calls()[0].arguments, json!({"cmd": "hi"}));
}

#[tokio::test]
async fn prepared_arguments_must_still_match_the_schema() {
    let provider = provider([
        FauxStep::tool_call("c1", "renaming", json!({"cmd": 5})),
        FauxStep::text("done"),
    ]);
    let tool = Arc::new(Renaming::default());
    let config = config(provider.clone(), vec![tool.clone()]);

    run_prompt(config, &mut Vec::new(), "go").await;

    assert!(tool.ran_with.lock().unwrap().is_empty());
    let requests = provider.requests();
    let result = tool_result(&requests[1].messages, 2);
    assert!(result.is_error);
    assert!(
        result.content.starts_with("Invalid arguments for tool \"renaming\""),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn a_panicking_argument_preparation_blocks_only_that_call() {
    let provider = provider([
        FauxStep::tool_call("c1", "renaming", json!({"boom": true})),
        FauxStep::text("handled"),
    ]);
    let tool = Arc::new(Renaming::default());
    let config = config(provider.clone(), vec![tool.clone()]);

    let (result, _) = run_prompt(config, &mut Vec::new(), "go").await;

    assert_eq!(result.outcome, RunOutcome::Completed);
    assert!(tool.ran_with.lock().unwrap().is_empty());
    let requests = provider.requests();
    let result = tool_result(&requests[1].messages, 2);
    assert!(result.is_error);
    assert_eq!(result.content, "Tool renaming argument preparation panicked");
}

#[tokio::test]
async fn progress_reaches_observers_in_order_before_the_call_ends_but_not_the_model() {
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi", "progress": ["one", "two"]})),
        FauxStep::text("done"),
    ]);
    let config = config(provider.clone(), vec![echo()]);

    let (_, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    let events = sink.events();
    let updates: Vec<(String, String)> = events
        .iter()
        .filter_map(|event| match event {
            EventKind::ToolExecutionUpdate {
                tool_call_id, content, ..
            } => Some((tool_call_id.clone(), content.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(updates, [("c1".into(), "one".into()), ("c1".into(), "two".into())]);
    let kinds = sink.kinds();
    let last_update = kinds.iter().rposition(|kind| *kind == "tool_execution_update").unwrap();
    let end = kinds.iter().position(|kind| *kind == "tool_execution_end").unwrap();
    assert!(last_update < end);
    assert_eq!(tool_result(&provider.requests()[1].messages, 2).content, "hi");
}

/// Keeps a copy of its context, to report progress after the call has finished.
#[derive(Default)]
struct KeepsContext(Mutex<Option<ToolContext>>);

#[async_trait]
impl Tool for KeepsContext {
    fn name(&self) -> &str {
        "keep"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "keep".into(),
            description: "Keep the context".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        *self.0.lock().unwrap() = Some(ctx.clone());
        Ok(ToolOutput::ok("kept"))
    }
}

#[tokio::test]
async fn the_tool_gets_the_call_id_and_progress_after_the_call_is_dropped() {
    let provider = provider([FauxStep::tool_call("c7", "keep", json!({})), FauxStep::text("done")]);
    let tool = Arc::new(KeepsContext::default());
    let config = config(provider, vec![tool.clone()]);

    let (_, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    let ctx = tool.0.lock().unwrap().take().unwrap();
    assert_eq!(ctx.tool_call_id.as_deref(), Some("c7"));
    assert!(!ctx.report_progress("late"));
    assert!(!sink.kinds().contains(&"tool_execution_update"));
}

fn end_details(sink: &Collect) -> Vec<Option<Value>> {
    sink.events()
        .into_iter()
        .filter_map(|event| match event {
            EventKind::ToolExecutionEnd { details, .. } => Some(details),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn details_reach_the_end_event_and_the_result_message() {
    let details = json!({"exitCode": 0, "files": ["a", "b"]});
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi", "details": details})),
        FauxStep::text("done"),
    ]);
    let config = config(provider, vec![echo()]);
    let mut context = Vec::new();

    let (_, sink) = run_prompt(config, &mut context, "go").await;

    assert_eq!(end_details(&sink), [Some(details.clone())]);
    let result = tool_result(&context, 2);
    assert_eq!(
        (result.content.as_str(), result.details.as_ref()),
        ("hi", Some(&details))
    );
}

fn end_usages(sink: &Collect) -> Vec<Option<Usage>> {
    sink.events()
        .into_iter()
        .filter_map(|event| match event {
            EventKind::ToolExecutionEnd { usage, .. } => Some(usage),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn tool_usage_reaches_the_end_event_the_result_message_and_the_loop_result() {
    let usage = json!({"input": 20, "output": 4, "cacheRead": 0, "totalTokens": 24, "totalCost": 0.001});
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi", "usage": usage})),
        FauxStep::tool_call("c2", "echo", json!({"text": "again", "usage": usage})),
        FauxStep::text("done"),
    ]);
    let config = config(provider, vec![echo()]);
    let mut context = Vec::new();

    let (result, sink) = run_prompt(config, &mut context, "go").await;

    let reported: Usage = serde_json::from_value(usage).unwrap();
    let mut both = reported.clone();
    both.add(&reported);
    assert_eq!(result.tool_usage, BTreeMap::from([("echo".to_string(), both)]));
    // The model's own usage: three faux answers of 15 tokens, without the tool's.
    assert_eq!(result.usage.input_and_output(), 45);
    assert_eq!(end_usages(&sink), [Some(reported.clone()), Some(reported.clone())]);
    assert_eq!(tool_result(&context, 2).usage.as_ref(), Some(&reported));
    assert_eq!(tool_result(&context, 4).usage.as_ref(), Some(&reported));
}

#[tokio::test]
async fn a_tool_without_usage_reports_none_and_the_loop_result_lists_no_tool() {
    let provider = provider([
        FauxStep::tool_call("c1", "echo", json!({"text": "hi"})),
        FauxStep::text("done"),
    ]);
    let config = config(provider, vec![echo()]);
    let mut context = Vec::new();

    let (result, sink) = run_prompt(config, &mut context, "go").await;

    assert!(result.tool_usage.is_empty());
    assert_eq!(end_usages(&sink), [None]);
    assert_eq!(tool_result(&context, 2).usage, None);
}

#[tokio::test]
async fn details_over_the_limit_are_dropped_from_the_event_and_the_result() {
    // As JSON a string also takes its two quotes: `fits` is exactly the limit, `over` one byte more.
    let fits = "x".repeat(MAX_DETAILS_BYTES - 2);
    let over = "x".repeat(MAX_DETAILS_BYTES - 1);
    let provider = provider([
        FauxStep::tool_calls(vec![
            ("c1", "echo", json!({"text": "a", "details": fits})),
            ("c2", "echo", json!({"text": "b", "details": over})),
        ]),
        FauxStep::text("done"),
    ]);
    let config = config(provider.clone(), vec![echo()]);

    let (_, sink) = run_prompt(config, &mut Vec::new(), "go").await;

    let details = end_details(&sink);
    assert_eq!(details.len(), 2);
    assert!(details.contains(&Some(Value::String(fits))));
    assert!(details.contains(&None));
    let requests = provider.requests();
    let over = tool_result(&requests[1].messages, 3);
    assert_eq!((over.content.as_str(), over.details.as_ref()), ("b", None));
    assert!(tool_result(&requests[1].messages, 2).details.is_some());
}
