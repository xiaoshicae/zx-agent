//! The spans of each run, built from signals by one task.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::SystemTime;

use opentelemetry::trace::{SpanContext, Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::SdkTracer;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::signal::{Signal, What};
use crate::agent::{ApprovalResult, Interruption, RequestKind, RunOutcome, ToolVerdict};
use crate::ai::{StopReason, Usage};

/// Builds spans from signals, one task owning every open span.
pub(super) async fn export(mut receiver: mpsc::Receiver<Signal>, mut runs: Runs, stop: CancellationToken) {
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            signal = receiver.recv() => match signal {
                Some(signal) => runs.apply(signal),
                None => break,
            },
        }
    }
    // What was emitted before the stop still counts.
    while let Ok(signal) = receiver.try_recv() {
        runs.apply(signal);
    }
    runs.end_all(SystemTime::now());
}

/// The open spans of each run, by run ID.
pub(super) struct Runs {
    tracer: SdkTracer,
    open: HashMap<String, Open>,
}

#[derive(Default)]
struct Open {
    run: Option<Context>,
    attempt: Option<Context>,
    turn: Option<Context>,
    request: Option<Context>,
    compaction: Option<Context>,
    /// By call ID.
    tools: HashMap<String, Context>,
}

impl Open {
    /// The innermost span that a new span or event belongs to.
    fn current(&self) -> Option<&Context> {
        self.turn.as_ref().or(self.attempt.as_ref()).or(self.run.as_ref())
    }

    /// The span of a tool call, or the innermost open one when the call has none.
    fn for_call(&self, call: &str) -> Option<&Context> {
        self.tools.get(call).or(self.current())
    }

    /// End what an attempt holds open: tools, the request, a summary and the turn.
    fn end_attempt_parts(&mut self, at: SystemTime) {
        for (_, tool) in self.tools.drain() {
            end(Some(tool), at);
        }
        end(self.request.take(), at);
        end(self.compaction.take(), at);
        end(self.turn.take(), at);
    }

    fn is_empty(&self) -> bool {
        self.run.is_none()
            && self.attempt.is_none()
            && self.turn.is_none()
            && self.request.is_none()
            && self.compaction.is_none()
            && self.tools.is_empty()
    }
}

impl Runs {
    pub(super) fn new(tracer: SdkTracer) -> Self {
        Self {
            tracer,
            open: HashMap::new(),
        }
    }

    /// Apply `signal` to the spans of its run.
    pub(super) fn apply(&mut self, signal: Signal) {
        let Signal {
            run_id,
            session_id,
            at,
            what,
        } = signal;
        let Self { tracer, open } = self;
        let mut run = OpenRun {
            tracer,
            spans: open.entry(run_id.clone()).or_default(),
            run_id: &run_id,
            at,
        };
        match what {
            What::RunStart {
                model,
                provider,
                parent,
            } => run.run_start(session_id, model, provider, parent),
            What::RunEnd {
                outcome,
                model_requests,
                retries,
                usage,
            } => run.run_end(outcome, model_requests, retries, &usage),
            What::AttemptStart { attempt } => run.attempt_start(attempt),
            What::AttemptEnd { outcome } => run.attempt_end(outcome),
            What::TurnStart { turn } => run.turn_start(turn),
            What::TurnEnd => run.turn_end(),
            What::RequestStart { kind, model, attempt } => run.request_start(kind, model, attempt),
            What::Response { usage, stop } => run.response(&usage, stop),
            What::ToolStart { call, tool } => run.tool_start(call, tool),
            What::Decision { call, verdict } => run.decision(&call, verdict),
            What::ApprovalRequested { call } => run.approval_requested(&call),
            What::ApprovalResolved { call, result, wait_ms } => run.approval_resolved(&call, result, wait_ms),
            What::ToolEnd { call, is_error, usage } => run.tool_end(&call, is_error, usage.as_ref()),
            What::CompactionStart {
                automatic,
                input_bytes,
                parent,
            } => run.compaction_start(automatic, input_bytes, parent),
            What::CompactionEnd { usage, output_bytes } => run.compaction_end(&usage, output_bytes),
            What::CompactionFailed => run.compaction_failed(),
            What::Retry { attempt, delay_ms } => run.retry(attempt, delay_ms),
            What::Overflow => run.overflow(),
            What::HookFailed { hook, phase } => run.hook_failed(hook, phase),
            What::Recovered { call, tool, state } => run.recovered(call, tool, state),
        }
        // A manual compaction leaves nothing open once it ends; neither does a run's end.
        if open.get(&run_id).is_some_and(Open::is_empty) {
            open.remove(&run_id);
        }
    }

    /// End every span still open, as the export stops.
    pub(super) fn end_all(&mut self, at: SystemTime) {
        for (_, mut spans) in self.open.drain() {
            spans.end_attempt_parts(at);
            end(spans.attempt.take(), at);
            end(spans.run.take(), at);
        }
    }
}

/// The open spans of one run while a signal is applied to them, with the tracer that starts
/// spans and the signal's time.
struct OpenRun<'a> {
    tracer: &'a SdkTracer,
    spans: &'a mut Open,
    run_id: &'a str,
    at: SystemTime,
}

impl OpenRun<'_> {
    fn run_start(&mut self, session_id: Option<String>, model: String, provider: String, parent: Option<SpanContext>) {
        let mut attributes = vec![
            KeyValue::new("zx.run.id", self.run_id.to_string()),
            KeyValue::new("zx.model", model),
            KeyValue::new("zx.provider", provider),
        ];
        if let Some(session) = session_id {
            attributes.push(KeyValue::new("zx.session.id", session));
        }
        self.spans.run = Some(start(self.tracer, "run", self.at, attributes, parent.map(remote)));
    }

    fn run_end(&mut self, outcome: RunOutcome, model_requests: u32, retries: u32, usage: &Usage) {
        self.spans.end_attempt_parts(self.at);
        end(self.spans.attempt.take(), self.at);
        if let Some(run) = self.spans.run.take() {
            let mut attributes = vec![
                KeyValue::new("zx.outcome", name(&outcome)),
                KeyValue::new("zx.model_requests", i64::from(model_requests)),
                KeyValue::new("zx.retries", i64::from(retries)),
            ];
            attributes.extend(usage_attributes(usage));
            finish(&run, self.at, attributes, outcome == RunOutcome::Error);
        }
    }

    fn attempt_start(&mut self, attempt: u32) {
        self.spans.end_attempt_parts(self.at);
        end(self.spans.attempt.take(), self.at);
        let attributes = vec![KeyValue::new("zx.attempt", i64::from(attempt))];
        let parent = self.spans.run.clone();
        self.spans.attempt = Some(start(self.tracer, "attempt", self.at, attributes, parent));
    }

    fn attempt_end(&mut self, outcome: RunOutcome) {
        self.spans.end_attempt_parts(self.at);
        if let Some(attempt) = self.spans.attempt.take() {
            let attributes = vec![KeyValue::new("zx.outcome", name(&outcome))];
            finish(&attempt, self.at, attributes, outcome == RunOutcome::Error);
        }
    }

    fn turn_start(&mut self, turn: u32) {
        end(self.spans.turn.take(), self.at);
        let parent = self.spans.attempt.as_ref().or(self.spans.run.as_ref()).cloned();
        let attributes = vec![KeyValue::new("zx.turn", i64::from(turn))];
        self.spans.turn = Some(start(self.tracer, "turn", self.at, attributes, parent));
    }

    fn turn_end(&mut self) {
        end(self.spans.turn.take(), self.at);
    }

    fn request_start(&mut self, kind: RequestKind, model: String, attempt: u32) {
        end(self.spans.request.take(), self.at);
        // A summary request belongs to its compaction.
        let parent = self.spans.compaction.as_ref().or(self.spans.current()).cloned();
        let attributes = vec![
            KeyValue::new("zx.request.kind", name(&kind)),
            KeyValue::new("zx.model", model),
            KeyValue::new("zx.attempt", i64::from(attempt)),
        ];
        self.spans.request = Some(start(self.tracer, "request", self.at, attributes, parent));
    }

    fn response(&mut self, usage: &Usage, stop: StopReason) {
        if let Some(request) = self.spans.request.take() {
            let mut attributes = usage_attributes(usage);
            attributes.push(KeyValue::new("zx.stop_reason", name(&stop)));
            finish(&request, self.at, attributes, stop == StopReason::Error);
        }
    }

    fn tool_start(&mut self, call: String, tool: String) {
        let name = format!("tool {tool}");
        let attributes = vec![
            KeyValue::new("zx.tool.name", tool),
            KeyValue::new("zx.tool.call_id", call.clone()),
        ];
        let cx = start(self.tracer, name, self.at, attributes, self.spans.current().cloned());
        self.spans.tools.insert(call, cx);
    }

    fn decision(&self, call: &str, verdict: ToolVerdict) {
        let attributes = vec![KeyValue::new("zx.verdict", name(&verdict))];
        event(self.spans.for_call(call), "decision", self.at, attributes);
    }

    fn approval_requested(&self, call: &str) {
        event(self.spans.for_call(call), "approval_requested", self.at, Vec::new());
    }

    fn approval_resolved(&self, call: &str, result: ApprovalResult, wait_ms: u64) {
        let attributes = vec![
            KeyValue::new("zx.approval.result", name(&result)),
            KeyValue::new("zx.approval.wait_ms", saturate(wait_ms)),
        ];
        event(self.spans.for_call(call), "approval_resolved", self.at, attributes);
    }

    /// End the call's span; `usage` is what the tool spent on work of its own, set on its
    /// span as a model's tokens are on the request.
    fn tool_end(&mut self, call: &str, is_error: bool, usage: Option<&Usage>) {
        if let Some(tool) = self.spans.tools.remove(call) {
            let mut attributes = vec![KeyValue::new("zx.tool.is_error", is_error)];
            if let Some(usage) = usage {
                attributes.extend(usage_attributes(usage));
            }
            finish(&tool, self.at, attributes, is_error);
        }
    }

    fn compaction_start(&mut self, automatic: bool, input_bytes: usize, parent: Option<SpanContext>) {
        end(self.spans.compaction.take(), self.at);
        let attributes = vec![
            KeyValue::new("zx.compaction.automatic", automatic),
            KeyValue::new("zx.compaction.input_bytes", saturate(input_bytes)),
        ];
        // A manual compaction belongs to no run, so it starts a trace of its own,
        // under the caller's span if there is one.
        let parent = self.spans.current().cloned().or_else(|| parent.map(remote));
        self.spans.compaction = Some(start(self.tracer, "compaction", self.at, attributes, parent));
    }

    /// End the summary and its request; the summary's answer is reported here, not as a
    /// message of the run.
    fn compaction_end(&mut self, usage: &Usage, output_bytes: usize) {
        end(self.spans.request.take(), self.at);
        if let Some(compaction) = self.spans.compaction.take() {
            let mut attributes = usage_attributes(usage);
            attributes.push(KeyValue::new("zx.compaction.output_bytes", saturate(output_bytes)));
            finish(&compaction, self.at, attributes, false);
        }
    }

    fn compaction_failed(&mut self) {
        end(self.spans.request.take(), self.at);
        if let Some(compaction) = self.spans.compaction.take() {
            finish(&compaction, self.at, Vec::new(), true);
        }
    }

    fn retry(&self, attempt: u32, delay_ms: u64) {
        let attributes = vec![
            KeyValue::new("zx.attempt", i64::from(attempt)),
            KeyValue::new("zx.delay_ms", saturate(delay_ms)),
        ];
        event(self.spans.run.as_ref(), "retry_scheduled", self.at, attributes);
    }

    fn overflow(&self) {
        event(self.spans.run.as_ref(), "context_overflow", self.at, Vec::new());
    }

    fn hook_failed(&self, hook: String, phase: &'static str) {
        let attributes = vec![KeyValue::new("zx.hook", hook), KeyValue::new("zx.hook.phase", phase)];
        event(self.spans.current(), "hook_failed", self.at, attributes);
    }

    fn recovered(&self, call: String, tool: String, state: Interruption) {
        let attributes = vec![
            KeyValue::new("zx.tool.name", tool),
            KeyValue::new("zx.tool.call_id", call),
            KeyValue::new("zx.recovery.state", name(&state)),
        ];
        event(self.spans.run.as_ref(), "tool_call_recovered", self.at, attributes);
    }
}

fn start(
    tracer: &SdkTracer,
    name: impl Into<Cow<'static, str>>,
    at: SystemTime,
    attributes: Vec<KeyValue>,
    parent: Option<Context>,
) -> Context {
    let builder = tracer
        .span_builder(name)
        .with_start_time(at)
        .with_attributes(attributes);
    // An empty context makes a new trace; the current one is not looked at.
    let parent = parent.unwrap_or_default();
    let span = tracer.build_with_context(builder, &parent);
    parent.with_span(span)
}

/// The context to start a trace under a span of another process.
fn remote(parent: SpanContext) -> Context {
    Context::new().with_remote_span_context(parent)
}

/// End `span` at `at`, if there is one.
fn end(span: Option<Context>, at: SystemTime) {
    if let Some(span) = span {
        finish(&span, at, Vec::new(), false);
    }
}

/// End `span` at `at` with what it reports, and as an error when it `failed`. Every span
/// ends here.
fn finish(span: &Context, at: SystemTime, attributes: Vec<KeyValue>, failed: bool) {
    let span = span.span();
    span.set_attributes(attributes);
    if failed {
        span.set_status(Status::error("error"));
    }
    span.end_with_timestamp(at);
}

fn event(span: Option<&Context>, name: &'static str, at: SystemTime, attributes: Vec<KeyValue>) {
    if let Some(span) = span {
        span.span().add_event_with_timestamp(name, at, attributes);
    }
}

fn usage_attributes(usage: &Usage) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new("zx.usage.input_tokens", saturate(usage.input)),
        KeyValue::new("zx.usage.output_tokens", saturate(usage.output)),
        KeyValue::new("zx.usage.cache_read_tokens", saturate(usage.cache_read)),
    ];
    if usage.cache_write > 0 {
        attributes.push(KeyValue::new(
            "zx.usage.cache_write_tokens",
            saturate(usage.cache_write),
        ));
    }
    if usage.reasoning > 0 {
        attributes.push(KeyValue::new("zx.usage.reasoning_tokens", saturate(usage.reasoning)));
    }
    if let Some(cost) = usage.total_cost {
        attributes.push(KeyValue::new("zx.usage.total_cost", cost));
    }
    attributes
}

/// The name a value has in events and reports, such as `model_request_limit`.
fn name(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        // Only unit variants are named here, and they serialize as strings.
        _ => String::new(),
    }
}

/// Attribute integers are signed; counts beyond them show as the largest one.
fn saturate(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
