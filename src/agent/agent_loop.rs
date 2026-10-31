//! The agent loop (pi: `runLoop` in packages/agent/src/agent-loop.ts).
//!
//! Each turn sends one model request and executes the tool calls in the response. The
//! model decides when to stop: a response without tool calls ends the inner loop.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use futures::FutureExt;
use futures::StreamExt;
use futures::future::join_all;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::events::{AgentEvent, EventSink, LoopOutcome};
use super::policy::{Decision, ToolPolicy};
use super::queue::MessageQueues;
use super::tool::{ExecutionMode, Tool, ToolContext, ToolOutput};
use crate::ai::{
    AssistantMessage, Message, Provider, Request, StopReason, StreamEvent, SystemMessage, SystemPrompt, ToolCall,
    ToolResultMessage, duration_ms, now_ms,
};

/// Everything the loop needs. Policies that pi injects as hooks are fields here.
pub struct LoopConfig {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    /// The system prompt this run should use. A run first records it, or the sections that
    /// differ from the prompt already in the conversation, as a system message.
    pub system_prompt: SystemPrompt,
    pub tools: Vec<Arc<dyn Tool>>,
    pub policy: Arc<dyn ToolPolicy>,
    /// Working directory handed to tools.
    pub cwd: PathBuf,
    /// Output token limit per response; `None` leaves it to the provider.
    pub max_tokens: Option<u32>,
    /// Run every batch of tool calls one at a time.
    pub sequential_tools: bool,
    pub queues: Arc<MessageQueues>,
}

/// How a `run_loop` call ended. The messages it produced are appended to `context`.
#[derive(Debug)]
pub struct LoopResult {
    pub outcome: LoopOutcome,
    /// Model requests made in this run.
    pub turns: u32,
}

/// Run the loop until the model stops, the turn budget is spent, a request fails, or
/// `cancel` fires. Every message of the run, prompts included, is appended to `context`.
///
/// A system message comes first when `config.system_prompt` differs from the prompt in
/// `context`. Then `prompts` are appended (pi: `runAgentLoop`); pass none to continue from the
/// current context (pi: `runAgentLoopContinue`), whose last message must then be a user
/// or tool-result message.
pub async fn run_loop(
    context: &mut Vec<Message>,
    prompts: Vec<Message>,
    config: &LoopConfig,
    max_turns: u32,
    cancel: &CancellationToken,
    sink: &dyn EventSink,
) -> LoopResult {
    let mut turns: u32 = 0;
    sink.emit(AgentEvent::AgentStart);
    sink.emit(AgentEvent::TurnStart { turn: 1 });
    // pi: AgentSession appends a system message when the prompt sections differ.
    let changes = SystemPrompt::from_transcript(context).diff(&config.system_prompt);
    if !changes.is_empty() {
        let update = SystemMessage {
            sections: changes,
            timestamp: now_ms(),
        };
        append(context, sink, Message::System(update));
    }
    for message in prompts {
        append(context, sink, message);
    }
    // The user may have queued input while the run was starting.
    let mut pending = config.queues.drain_steering();

    // Outer loop: continues when follow-up messages arrive after the agent would stop.
    let outcome = 'outer: loop {
        let mut has_more_tool_calls = true;
        // Inner loop: one iteration per turn.
        while has_more_tool_calls || !pending.is_empty() {
            if turns > 0 {
                if pending.is_empty() {
                    pending = config.queues.drain_steering();
                }
                sink.emit(AgentEvent::TurnStart { turn: turns + 1 });
            }
            for message in pending.drain(..) {
                append(context, sink, message);
            }
            if cancel.is_cancelled() {
                break 'outer LoopOutcome::Aborted;
            }
            if turns >= max_turns {
                break 'outer LoopOutcome::MaxTurns;
            }
            turns += 1;

            let message = stream_assistant_response(context, config, cancel, sink).await;
            let stop_reason = message.stop_reason;
            let tool_calls = message.tool_calls();
            context.push(Message::Assistant(message));

            // Provider failures end the run; retrying is the runner's decision (pi: agent-loop.ts:254).
            match stop_reason {
                StopReason::Error => {
                    sink.emit(AgentEvent::TurnEnd { turn: turns });
                    break 'outer LoopOutcome::Error;
                }
                StopReason::Aborted => {
                    sink.emit(AgentEvent::TurnEnd { turn: turns });
                    break 'outer LoopOutcome::Aborted;
                }
                StopReason::Stop | StopReason::Length | StopReason::ToolUse => {}
            }

            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                // A length stop may have cut off tool-call arguments: do not run them (pi: agent-loop.ts:277).
                let batch = if stop_reason == StopReason::Length {
                    fail_truncated_calls(&tool_calls, sink)
                } else {
                    execute_tool_calls(&tool_calls, config, cancel, sink).await
                };
                has_more_tool_calls = !batch.terminate;
                context.extend(batch.results.into_iter().map(Message::ToolResult));
            }
            sink.emit(AgentEvent::TurnEnd { turn: turns });
            if cancel.is_cancelled() {
                break 'outer LoopOutcome::Aborted;
            }
            pending = config.queues.drain_steering();
        }

        let follow_ups = config.queues.drain_follow_up();
        if !follow_ups.is_empty() {
            pending = follow_ups;
            continue;
        }
        break LoopOutcome::Completed;
    };

    sink.emit(AgentEvent::AgentEnd { outcome });
    LoopResult { outcome, turns }
}

fn append(context: &mut Vec<Message>, sink: &dyn EventSink, message: Message) {
    sink.emit(AgentEvent::MessageStart { role: message.role() });
    // Events own their payload, so the event gets a copy and the context keeps the original.
    sink.emit(AgentEvent::MessageEnd {
        message: message.clone(),
    });
    context.push(message);
}

/// One model call (pi: `streamAssistantResponse`). Never fails: provider errors come back
/// as a message with `stop_reason` `Error` or `Aborted`.
async fn stream_assistant_response(
    context: &[Message],
    config: &LoopConfig,
    cancel: &CancellationToken,
    sink: &dyn EventSink,
) -> AssistantMessage {
    let request = Request {
        model: config.model.clone(),
        // The provider hands the request to a background task, which must own it.
        messages: context.to_vec(),
        tools: config.tools.iter().map(|tool| tool.definition()).collect(),
        max_tokens: config.max_tokens,
    };
    let mut events = config.provider.stream(request, cancel.clone());
    let mut started = false;
    // Listeners always see a start before the end, even when a request fails before
    // generation begins (pi: agent-loop.ts:461).
    let mut ensure_started = |sink: &dyn EventSink| {
        if !started {
            started = true;
            sink.emit(AgentEvent::MessageStart { role: "assistant" });
        }
    };
    while let Some(event) = events.next().await {
        match event {
            StreamEvent::Start => ensure_started(sink),
            StreamEvent::Delta(delta) => {
                ensure_started(sink);
                sink.emit(AgentEvent::MessageUpdate { delta });
            }
            StreamEvent::Done(message) | StreamEvent::Error(message) => {
                ensure_started(sink);
                sink.emit(AgentEvent::MessageEnd {
                    message: Message::Assistant(message.clone()),
                });
                return message;
            }
        }
    }
    // The provider broke its contract by ending without a final event.
    let stop = if cancel.is_cancelled() {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    let message = AssistantMessage::failure(
        config.provider.name(),
        &config.model,
        stop,
        "provider stream ended without a final event",
    );
    ensure_started(sink);
    sink.emit(AgentEvent::MessageEnd {
        message: Message::Assistant(message.clone()),
    });
    message
}

// ---------------------------------------------------------------------------
// Tool execution (pi: executeToolCalls and helpers)
// ---------------------------------------------------------------------------

struct Batch {
    results: Vec<ToolResultMessage>,
    /// True when every result asked to stop.
    terminate: bool,
}

struct Outcome {
    output: ToolOutput,
    duration_ms: Option<u64>,
}

enum Prepared {
    /// Resolved without running the tool: not found, invalid arguments, denied, aborted.
    Immediate(Outcome),
    Ready(Arc<dyn Tool>, Value),
}

fn find_tool(config: &LoopConfig, name: &str) -> Option<Arc<dyn Tool>> {
    config.tools.iter().find(|tool| tool.name() == name).cloned()
}

async fn execute_tool_calls(
    calls: &[ToolCall],
    config: &LoopConfig,
    cancel: &CancellationToken,
    sink: &dyn EventSink,
) -> Batch {
    // One sequential tool makes the whole batch sequential (pi: agent-loop.ts:526).
    let sequential = config.sequential_tools
        || calls.iter().any(|call| {
            find_tool(config, &call.name).is_some_and(|tool| tool.execution_mode() == ExecutionMode::Sequential)
        });
    if sequential {
        execute_sequential(calls, config, cancel, sink).await
    } else {
        execute_parallel(calls, config, cancel, sink).await
    }
}

async fn execute_sequential(
    calls: &[ToolCall],
    config: &LoopConfig,
    cancel: &CancellationToken,
    sink: &dyn EventSink,
) -> Batch {
    let mut results = Vec::new();
    let mut terminate = true;
    for call in calls {
        emit_start(call, sink);
        let outcome = match prepare(call, config, cancel).await {
            Prepared::Immediate(outcome) => outcome,
            Prepared::Ready(tool, args) => execute(tool, args, config, cancel).await,
        };
        emit_end(call, &outcome, sink);
        terminate &= outcome.output.terminate;
        let result = to_result(call, outcome);
        emit_result(&result, sink);
        results.push(result);
        if cancel.is_cancelled() {
            // Remaining calls get no result; the provider layer fills placeholders.
            break;
        }
    }
    Batch {
        terminate: terminate && !results.is_empty(),
        results,
    }
}

/// Three phases, as in pi:
/// 1. check every call in order (policy decisions never overlap);
/// 2. run every allowed call concurrently, emitting `tool_execution_end` as each finishes;
/// 3. emit tool results in the order the model issued the calls.
async fn execute_parallel(
    calls: &[ToolCall],
    config: &LoopConfig,
    cancel: &CancellationToken,
    sink: &dyn EventSink,
) -> Batch {
    enum Slot {
        Done(Outcome),
        Pending(Arc<dyn Tool>, Value),
    }

    let mut slots = Vec::new();
    for call in calls {
        emit_start(call, sink);
        match prepare(call, config, cancel).await {
            Prepared::Immediate(outcome) => {
                emit_end(call, &outcome, sink);
                slots.push((call, Slot::Done(outcome)));
            }
            Prepared::Ready(tool, args) => slots.push((call, Slot::Pending(tool, args))),
        }
        if cancel.is_cancelled() {
            break;
        }
    }

    // Running inside this task, not via spawn, lets the futures borrow `config`, `cancel`, and `sink`.
    let outcomes = join_all(slots.into_iter().map(|(call, slot)| async move {
        let outcome = match slot {
            Slot::Done(outcome) => outcome,
            Slot::Pending(tool, args) => {
                let outcome = execute(tool, args, config, cancel).await;
                emit_end(call, &outcome, sink);
                outcome
            }
        };
        (call, outcome)
    }))
    .await;

    let terminate = !outcomes.is_empty() && outcomes.iter().all(|(_, outcome)| outcome.output.terminate);
    let mut results = Vec::with_capacity(outcomes.len());
    for (call, outcome) in outcomes {
        let result = to_result(call, outcome);
        emit_result(&result, sink);
        results.push(result);
    }
    Batch { results, terminate }
}

async fn prepare(call: &ToolCall, config: &LoopConfig, cancel: &CancellationToken) -> Prepared {
    let immediate = |message: String| {
        Prepared::Immediate(Outcome {
            output: ToolOutput::error(message),
            duration_ms: None,
        })
    };
    let Some(tool) = find_tool(config, &call.name) else {
        return immediate(format!("Tool {} not found", call.name));
    };
    if let Some(error) = &call.arguments_error {
        return immediate(format!("Invalid arguments for tool \"{}\": {error}", call.name));
    }
    if !call.arguments.is_object() {
        return immediate(format!(
            "Invalid arguments for tool \"{}\": expected a JSON object",
            call.name
        ));
    }
    if let Decision::Deny(reason) = config.policy.check(&call.name, &call.arguments).await {
        return immediate(format!("Blocked by policy: {reason}"));
    }
    if cancel.is_cancelled() {
        return immediate("Operation aborted".to_string());
    }
    Prepared::Ready(tool, call.arguments.clone())
}

async fn execute(tool: Arc<dyn Tool>, args: Value, config: &LoopConfig, cancel: &CancellationToken) -> Outcome {
    if cancel.is_cancelled() {
        return Outcome {
            output: ToolOutput::error("Operation aborted"),
            duration_ms: None,
        };
    }
    let ctx = ToolContext {
        cwd: config.cwd.clone(),
        cancel: cancel.clone(),
    };
    let started = Instant::now();
    // A panicking tool must not take down an unattended run.
    let result = AssertUnwindSafe(tool.execute(args, &ctx)).catch_unwind().await;
    let duration_ms = Some(duration_ms(started.elapsed()));
    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(message)) => ToolOutput::error(message),
        Err(_) => ToolOutput::error(format!("Tool {} panicked", tool.name())),
    };
    Outcome { output, duration_ms }
}

fn fail_truncated_calls(calls: &[ToolCall], sink: &dyn EventSink) -> Batch {
    let mut results = Vec::new();
    for call in calls {
        emit_start(call, sink);
        let outcome = Outcome {
            output: ToolOutput::error(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            )),
            duration_ms: None,
        };
        emit_end(call, &outcome, sink);
        let result = to_result(call, outcome);
        emit_result(&result, sink);
        results.push(result);
    }
    Batch {
        results,
        terminate: false,
    }
}

fn to_result(call: &ToolCall, outcome: Outcome) -> ToolResultMessage {
    let Outcome { output, duration_ms } = outcome;
    let content = match (output.content.is_empty(), output.is_error) {
        (false, _) => output.content,
        (true, true) => "(tool failed without output)".to_string(),
        (true, false) => "(no output)".to_string(),
    };
    ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content,
        is_error: output.is_error,
        timestamp: now_ms(),
        duration_ms,
    }
}

fn emit_start(call: &ToolCall, sink: &dyn EventSink) {
    sink.emit(AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: call.arguments.clone(),
    });
}

fn emit_end(call: &ToolCall, outcome: &Outcome, sink: &dyn EventSink) {
    sink.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        is_error: outcome.output.is_error,
        duration_ms: outcome.duration_ms,
    });
}

fn emit_result(result: &ToolResultMessage, sink: &dyn EventSink) {
    sink.emit(AgentEvent::MessageStart { role: "toolResult" });
    sink.emit(AgentEvent::MessageEnd {
        message: Message::ToolResult(result.clone()),
    });
}
