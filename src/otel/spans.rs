//! The spans of each run, built from signals by one task, and the metrics recorded as they
//! end.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use opentelemetry::trace::{SpanContext, Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::SdkTracer;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::metrics::Instruments;
use super::signal::{Signal, What};
use crate::agent::{ApprovalResult, Interruption, RequestKind, RunOutcome, ToolVerdict};
use crate::ai::{StopReason, Usage};

/// Builds spans and records metrics from signals, one task owning every open span.
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
    instruments: Option<Instruments>,
    open: HashMap<String, Open>,
}

/// A span still open, with what its metric needs when it ends.
struct OpenSpan<M> {
    cx: Context,
    since: SystemTime,
    meta: M,
}

struct Request {
    kind: RequestKind,
    model: String,
}

#[derive(Default)]
struct Open {
    /// With the model.
    run: Option<OpenSpan<String>>,
    attempt: Option<Context>,
    turn: Option<Context>,
    request: Option<OpenSpan<Request>>,
    /// With whether it was automatic.
    compaction: Option<OpenSpan<bool>>,
    /// By call ID, with the tool's name.
    tools: HashMap<String, OpenSpan<String>>,
}

impl Open {
    /// The innermost span that a new span or event belongs to.
    fn current(&self) -> Option<&Context> {
        self.turn
            .as_ref()
            .or(self.attempt.as_ref())
            .or(self.run.as_ref().map(|run| &run.cx))
    }

    /// The span of a tool call, or the innermost open one when the call has none.
    fn for_call(&self, call: &str) -> Option<&Context> {
        self.tools.get(call).map(|tool| &tool.cx).or(self.current())
    }

    /// End what an attempt holds open: tools, the request, a summary and the turn.
    fn end_attempt_parts(&mut self, at: SystemTime) {
        for (_, tool) in self.tools.drain() {
            end(Some(tool.cx), at);
        }
        end(self.request.take().map(|request| request.cx), at);
        end(self.compaction.take().map(|compaction| compaction.cx), at);
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
    pub(super) fn new(tracer: SdkTracer, instruments: Option<Instruments>) -> Self {
        Self {
            tracer,
            instruments,
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
        let Self {
            tracer,
            instruments,
            open,
        } = self;
        let mut run = OpenRun {
            tracer,
            instruments: instruments.as_ref(),
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
            What::Response { model, usage, stop } => run.response(&model, &usage, stop),
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
            end(spans.run.take().map(|run| run.cx), at);
        }
    }
}

/// The open spans of one run while a signal is applied to them, with the tracer that starts
/// spans, the instruments that record as they end, and the signal's time.
struct OpenRun<'a> {
    tracer: &'a SdkTracer,
    instruments: Option<&'a Instruments>,
    spans: &'a mut Open,
    run_id: &'a str,
    at: SystemTime,
}

impl OpenRun<'_> {
    fn run_start(&mut self, session_id: Option<String>, model: String, provider: String, parent: Option<SpanContext>) {
        let mut attributes = vec![
            KeyValue::new("zx.run.id", self.run_id.to_string()),
            KeyValue::new("zx.model", model.clone()),
            KeyValue::new("zx.provider", provider),
        ];
        if let Some(session) = session_id {
            attributes.push(KeyValue::new("zx.session.id", session));
        }
        let cx = start(self.tracer, "run", self.at, attributes, parent.map(remote));
        self.spans.run = Some(OpenSpan {
            cx,
            since: self.at,
            meta: model,
        });
    }

    fn run_end(&mut self, outcome: RunOutcome, model_requests: u32, retries: u32, usage: &Usage) {
        self.spans.end_attempt_parts(self.at);
        end(self.spans.attempt.take(), self.at);
        if let Some(run) = self.spans.run.take() {
            let span = run.cx.span();
            span.set_attribute(KeyValue::new("zx.outcome", name(&outcome)));
            span.set_attribute(KeyValue::new("zx.model_requests", i64::from(model_requests)));
            span.set_attribute(KeyValue::new("zx.retries", i64::from(retries)));
            span.set_attributes(usage_attributes(usage));
            if outcome == RunOutcome::Error {
                span.set_status(Status::error("error"));
            }
            span.end_with_timestamp(self.at);
            if let Some(instruments) = self.instruments {
                let attributes = [
                    KeyValue::new("zx.outcome", name(&outcome)),
                    KeyValue::new("zx.model", run.meta),
                ];
                instruments.runs.add(1, &attributes);
                instruments
                    .run_duration
                    .record(seconds(run.since, self.at), &attributes);
            }
        }
    }

    fn attempt_start(&mut self, attempt: u32) {
        self.spans.end_attempt_parts(self.at);
        end(self.spans.attempt.take(), self.at);
        let attributes = vec![KeyValue::new("zx.attempt", i64::from(attempt))];
        let parent = self.spans.run.as_ref().map(|run| run.cx.clone());
        self.spans.attempt = Some(start(self.tracer, "attempt", self.at, attributes, parent));
    }

    fn attempt_end(&mut self, outcome: RunOutcome) {
        self.spans.end_attempt_parts(self.at);
        let attributes = vec![KeyValue::new("zx.outcome", name(&outcome))];
        finish(
            self.spans.attempt.take(),
            self.at,
            attributes,
            outcome == RunOutcome::Error,
        );
    }

    fn turn_start(&mut self, turn: u32) {
        end(self.spans.turn.take(), self.at);
        let parent = self
            .spans
            .attempt
            .as_ref()
            .or(self.spans.run.as_ref().map(|run| &run.cx))
            .cloned();
        let attributes = vec![KeyValue::new("zx.turn", i64::from(turn))];
        self.spans.turn = Some(start(self.tracer, "turn", self.at, attributes, parent));
    }

    fn turn_end(&mut self) {
        end(self.spans.turn.take(), self.at);
    }

    fn request_start(&mut self, kind: RequestKind, model: String, attempt: u32) {
        end(self.spans.request.take().map(|request| request.cx), self.at);
        // A summary request belongs to its compaction.
        let parent = self
            .spans
            .compaction
            .as_ref()
            .map(|compaction| &compaction.cx)
            .or(self.spans.current())
            .cloned();
        let attributes = vec![
            KeyValue::new("zx.request.kind", name(&kind)),
            KeyValue::new("zx.model", model.clone()),
            KeyValue::new("zx.attempt", i64::from(attempt)),
        ];
        let cx = start(self.tracer, "request", self.at, attributes, parent);
        self.spans.request = Some(OpenSpan {
            cx,
            since: self.at,
            meta: Request { kind, model },
        });
    }

    fn response(&mut self, model: &str, usage: &Usage, stop: StopReason) {
        if let Some(request) = self.spans.request.take() {
            let span = request.cx.span();
            span.set_attributes(usage_attributes(usage));
            span.set_attribute(KeyValue::new("zx.stop_reason", name(&stop)));
            if stop == StopReason::Error {
                span.set_status(Status::error("error"));
            }
            span.end_with_timestamp(self.at);
            if let Some(instruments) = self.instruments {
                let attributes = [
                    KeyValue::new("zx.request.kind", name(&request.meta.kind)),
                    KeyValue::new("zx.model", request.meta.model),
                    KeyValue::new("zx.stop_reason", name(&stop)),
                ];
                instruments
                    .request_duration
                    .record(seconds(request.since, self.at), &attributes);
            }
        }
        if let Some(instruments) = self.instruments {
            instruments.count_usage(usage, model);
        }
    }

    fn tool_start(&mut self, call: String, tool: String) {
        let attributes = vec![
            KeyValue::new("zx.tool.name", tool.clone()),
            KeyValue::new("zx.tool.call_id", call.clone()),
        ];
        let cx = start(
            self.tracer,
            format!("tool {tool}"),
            self.at,
            attributes,
            self.spans.current().cloned(),
        );
        self.spans.tools.insert(
            call,
            OpenSpan {
                cx,
                since: self.at,
                meta: tool,
            },
        );
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
        if let Some(instruments) = self.instruments {
            let attributes = [KeyValue::new("zx.approval.result", name(&result))];
            instruments
                .approval_wait
                .record(Duration::from_millis(wait_ms).as_secs_f64(), &attributes);
        }
    }

    /// End the call's span; `usage` is what the tool spent on work of its own, counted
    /// under its name like a model's tokens under the model.
    fn tool_end(&mut self, call: &str, is_error: bool, usage: Option<&Usage>) {
        if let Some(tool) = self.spans.tools.remove(call) {
            let span = tool.cx.span();
            span.set_attribute(KeyValue::new("zx.tool.is_error", is_error));
            if let Some(usage) = usage {
                span.set_attributes(usage_attributes(usage));
            }
            if is_error {
                span.set_status(Status::error("error"));
            }
            span.end_with_timestamp(self.at);
            if let Some(instruments) = self.instruments {
                let attributes = [
                    KeyValue::new("zx.tool.name", tool.meta.clone()),
                    KeyValue::new("zx.tool.is_error", is_error),
                ];
                instruments
                    .tool_duration
                    .record(seconds(tool.since, self.at), &attributes);
                if let Some(usage) = usage {
                    instruments.count_tool_usage(usage, &tool.meta);
                }
            }
        }
    }

    fn compaction_start(&mut self, automatic: bool, input_bytes: usize, parent: Option<SpanContext>) {
        end(self.spans.compaction.take().map(|compaction| compaction.cx), self.at);
        let attributes = vec![
            KeyValue::new("zx.compaction.automatic", automatic),
            KeyValue::new("zx.compaction.input_bytes", saturate(input_bytes)),
        ];
        // A manual compaction belongs to no run, so it starts a trace of its own,
        // under the caller's span if there is one.
        let parent = self.spans.current().cloned().or_else(|| parent.map(remote));
        let cx = start(self.tracer, "compaction", self.at, attributes, parent);
        self.spans.compaction = Some(OpenSpan {
            cx,
            since: self.at,
            meta: automatic,
        });
    }

    fn compaction_end(&mut self, usage: &Usage, output_bytes: usize) {
        let model = self.spans.request.take().map(|request| {
            end(Some(request.cx), self.at);
            request.meta.model
        });
        if let Some(compaction) = self.spans.compaction.take() {
            let span = compaction.cx.span();
            span.set_attributes(usage_attributes(usage));
            span.set_attribute(KeyValue::new("zx.compaction.output_bytes", saturate(output_bytes)));
            span.end_with_timestamp(self.at);
            if let Some(instruments) = self.instruments {
                instruments.compaction_duration.record(
                    seconds(compaction.since, self.at),
                    &compaction_attributes(compaction.meta, "completed"),
                );
            }
        }
        // The summary's answer is reported here, not as a message of the run. A
        // summary without a request of its own (events out of order) has no model
        // name to count under: an empty one is still a count.
        if let Some(instruments) = self.instruments {
            instruments.count_usage(usage, model.as_deref().unwrap_or_default());
        }
    }

    fn compaction_failed(&mut self) {
        end(self.spans.request.take().map(|request| request.cx), self.at);
        if let Some(compaction) = self.spans.compaction.take() {
            let span = compaction.cx.span();
            span.set_status(Status::error("error"));
            span.end_with_timestamp(self.at);
            if let Some(instruments) = self.instruments {
                instruments.compaction_duration.record(
                    seconds(compaction.since, self.at),
                    &compaction_attributes(compaction.meta, "failed"),
                );
            }
        }
    }

    fn retry(&self, attempt: u32, delay_ms: u64) {
        let attributes = vec![
            KeyValue::new("zx.attempt", i64::from(attempt)),
            KeyValue::new("zx.delay_ms", saturate(delay_ms)),
        ];
        event(
            self.spans.run.as_ref().map(|run| &run.cx),
            "retry_scheduled",
            self.at,
            attributes,
        );
        if let Some(instruments) = self.instruments {
            instruments.retries.add(1, &[]);
        }
    }

    fn overflow(&self) {
        event(
            self.spans.run.as_ref().map(|run| &run.cx),
            "context_overflow",
            self.at,
            Vec::new(),
        );
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
        event(
            self.spans.run.as_ref().map(|run| &run.cx),
            "tool_call_recovered",
            self.at,
            attributes,
        );
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
    finish(span, at, Vec::new(), false);
}

/// End `span` at `at` with what it reports, and as an error when it `failed`.
fn finish(span: Option<Context>, at: SystemTime, attributes: Vec<KeyValue>, failed: bool) {
    if let Some(span) = span {
        let span = span.span();
        span.set_attributes(attributes);
        if failed {
            span.set_status(Status::error("error"));
        }
        span.end_with_timestamp(at);
    }
}

fn event(span: Option<&Context>, name: &'static str, at: SystemTime, attributes: Vec<KeyValue>) {
    if let Some(span) = span {
        span.span().add_event_with_timestamp(name, at, attributes);
    }
}

/// Events are timestamped in order; a clock set back between two of them makes a zero
/// duration rather than a failure.
fn seconds(since: SystemTime, at: SystemTime) -> f64 {
    at.duration_since(since).unwrap_or_default().as_secs_f64()
}

fn compaction_attributes(automatic: bool, outcome: &'static str) -> [KeyValue; 2] {
    [
        KeyValue::new("zx.compaction.automatic", automatic),
        KeyValue::new("zx.outcome", outcome),
    ]
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
