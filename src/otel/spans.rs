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
use crate::agent::{ApprovalResult, Interruption, LoopOutcome, RequestKind, RunOutcome, ToolVerdict};
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

    fn attempt_end(&mut self, outcome: LoopOutcome) {
        self.spans.end_attempt_parts(self.at);
        let attributes = vec![KeyValue::new("zx.outcome", name(&outcome))];
        finish(
            self.spans.attempt.take(),
            self.at,
            attributes,
            outcome == LoopOutcome::Error,
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
            KeyValue::new("zx.compaction.input_bytes", saturate_usize(input_bytes)),
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
            span.set_attribute(KeyValue::new(
                "zx.compaction.output_bytes",
                saturate_usize(output_bytes),
            ));
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
    if let Some(cost) = usage.cost_usd {
        attributes.push(KeyValue::new("zx.usage.cost_usd", cost));
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
fn saturate(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn saturate_usize(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry::trace::{SpanId, TracerProvider};
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, Metric, MetricData, ResourceMetrics};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
    use serde_json::json;

    use super::*;
    use crate::agent::{AgentEvent, RunReport};
    use crate::ai::{AssistantMessage, ContentBlock, Message};
    use crate::otel::TraceParent;
    use crate::otel::signal::event;

    const RUN: &str = "6f1c2c4e-0000-4000-8000-000000000001";
    const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    /// Spans built synchronously into an in-memory exporter, metrics collected on demand;
    /// the providers must stay alive.
    struct Harness {
        spans: InMemorySpanExporter,
        _tracer_provider: SdkTracerProvider,
        metrics: InMemoryMetricExporter,
        meter_provider: SdkMeterProvider,
        runs: Runs,
    }

    impl Harness {
        fn new() -> Self {
            let spans = InMemorySpanExporter::default();
            let tracer_provider = SdkTracerProvider::builder().with_simple_exporter(spans.clone()).build();
            let metrics = InMemoryMetricExporter::default();
            let meter_provider = SdkMeterProvider::builder()
                .with_periodic_exporter(metrics.clone())
                .build();
            let instruments = Instruments::new(&meter_provider.meter("test"));
            let runs = Runs::new(tracer_provider.tracer("test"), Some(instruments));
            Self {
                spans,
                _tracer_provider: tracer_provider,
                metrics,
                meter_provider,
                runs,
            }
        }

        fn apply(&mut self, signals: impl IntoIterator<Item = Signal>) {
            for signal in signals {
                self.runs.apply(signal);
            }
        }

        fn spans(&self) -> Vec<SpanData> {
            self.spans.get_finished_spans().unwrap()
        }

        /// What a collection exports now.
        fn metrics(&self) -> Vec<ResourceMetrics> {
            self.meter_provider.force_flush().unwrap();
            self.metrics.get_finished_metrics().unwrap()
        }
    }

    fn at(at_ms: u64, agent_event: AgentEvent) -> Signal {
        Signal::of(&event(at_ms, agent_event), None).expect("a span needs this event")
    }

    fn under(parent: &TraceParent, at_ms: u64, agent_event: AgentEvent) -> Signal {
        Signal::of(&event(at_ms, agent_event), Some(&parent.context)).expect("a span needs this event")
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read: 0,
            total_tokens: input + output,
            ..Usage::default()
        }
    }

    fn answer(usage: Usage, stop: StopReason, content: Vec<ContentBlock>) -> AgentEvent {
        AgentEvent::MessageEnd {
            custom: None,
            message: Message::Assistant(AssistantMessage {
                content,
                provider: "p".into(),
                model: "m".into(),
                usage,
                stop_reason: stop,
                error_message: None,
                provider_error: None,
                timestamp: 0,
                duration_ms: None,
                response_id: None,
                response_model: None,
            }),
            entry_id: None,
        }
    }

    fn run_end(outcome: RunOutcome, usage: Usage) -> AgentEvent {
        AgentEvent::RunEnd {
            report: Box::new(RunReport {
                outcome,
                final_text: "the secret answer".into(),
                model_requests: 1,
                retries: 0,
                usage,
                tool_usage: std::collections::BTreeMap::new(),
                denied_calls: 0,
                error: None,
                session_file: None,
            }),
        }
    }

    /// A run with one request, one tool call and one answer, from 1000 ms to 1023 ms.
    fn one_run() -> Vec<Signal> {
        let command = json!({"command": "cat secret.txt"});
        let call = ContentBlock::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: command.clone(),
            arguments_error: None,
        };
        vec![
            at(
                1000,
                AgentEvent::RunStart {
                    model: "m".into(),
                    provider: "p".into(),
                },
            ),
            at(1001, AgentEvent::AgentStart { attempt: 1 }),
            at(1002, AgentEvent::TurnStart { turn: 1 }),
            at(
                1003,
                AgentEvent::RequestStart {
                    kind: RequestKind::Task,
                    entry_id: None,
                    model: "m".into(),
                    attempt: 1,
                },
            ),
            at(1010, answer(usage(10, 5), StopReason::ToolUse, vec![call])),
            at(
                1011,
                AgentEvent::ToolExecutionStart {
                    tool_call_id: "c1".into(),
                    tool_name: "bash".into(),
                    args: command,
                },
            ),
            at(
                1012,
                AgentEvent::ToolDecision {
                    tool_call_id: "c1".into(),
                    tool_name: "bash".into(),
                    verdict: ToolVerdict::Allow,
                    reason: Some("the secret rule".into()),
                },
            ),
            at(
                1020,
                AgentEvent::ToolExecutionEnd {
                    tool_call_id: "c1".into(),
                    tool_name: "bash".into(),
                    is_error: false,
                    duration_ms: Some(8),
                    details: None,
                    usage: None,
                },
            ),
            at(1021, AgentEvent::TurnEnd { turn: 1 }),
            at(
                1022,
                AgentEvent::AgentEnd {
                    outcome: LoopOutcome::Completed,
                },
            ),
            at(1023, run_end(RunOutcome::Completed, usage(10, 5))),
        ]
    }

    fn span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
        spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("no {name} span in {spans:?}"))
    }

    fn attribute(span: &SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| attribute.value.as_str().into_owned())
    }

    fn ms(time: SystemTime) -> u64 {
        u64::try_from(time.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
    }

    /// `None` when nothing was recorded: an instrument without measurements is not exported.
    fn metric<'a>(all: &'a [ResourceMetrics], name: &str) -> Option<&'a Metric> {
        all.iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .find(|metric| metric.name() == name)
    }

    fn has<'a>(attributes: impl Iterator<Item = &'a KeyValue>, expected: &[(&str, &str)]) -> bool {
        let attributes: Vec<(String, String)> = attributes
            .map(|attribute| (attribute.key.to_string(), attribute.value.as_str().into_owned()))
            .collect();
        expected
            .iter()
            .all(|(key, value)| attributes.iter().any(|(k, v)| k == key && v == value))
    }

    /// The sum of a `u64` counter's points that carry `expected`.
    fn counted(all: &[ResourceMetrics], name: &str, expected: &[(&str, &str)]) -> u64 {
        let Some(metric) = metric(all, name) else {
            return 0;
        };
        match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                .data_points()
                .filter(|point| has(point.attributes(), expected))
                .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                .sum(),
            other => panic!("{name} is not a u64 sum: {other:?}"),
        }
    }

    /// The count and sum of an `f64` histogram's points that carry `expected`.
    fn timed(all: &[ResourceMetrics], name: &str, expected: &[(&str, &str)]) -> (u64, f64) {
        let Some(metric) = metric(all, name) else {
            return (0, 0.0);
        };
        match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                .data_points()
                .filter(|point| has(point.attributes(), expected))
                .fold((0, 0.0), |(count, sum), point| {
                    (count + point.count(), sum + point.sum())
                }),
            other => panic!("{name} is not an f64 histogram: {other:?}"),
        }
    }

    fn close(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-9
    }

    #[test]
    fn a_run_is_one_trace_of_nested_spans_that_carry_metadata_only() {
        let mut harness = Harness::new();
        harness.apply(one_run());

        let spans = harness.spans();
        assert_eq!(spans.len(), 5, "{spans:?}");
        let run = span(&spans, "run");
        let attempt = span(&spans, "attempt");
        let turn = span(&spans, "turn");
        let request = span(&spans, "request");
        let tool = span(&spans, "tool bash");
        assert_eq!(run.parent_span_id, SpanId::INVALID);
        for (child, parent) in [(attempt, run), (turn, attempt), (request, turn), (tool, turn)] {
            assert_eq!(child.parent_span_id, parent.span_context.span_id(), "{}", child.name);
            assert_eq!(child.span_context.trace_id(), run.span_context.trace_id());
        }
        assert_eq!(attribute(run, "zx.run.id").as_deref(), Some(RUN));
        assert_eq!(attribute(run, "zx.session.id").as_deref(), Some("s1"));
        assert_eq!(attribute(run, "zx.outcome").as_deref(), Some("completed"));
        assert_eq!(attribute(run, "zx.model_requests").as_deref(), Some("1"));
        assert_eq!(attribute(request, "zx.usage.input_tokens").as_deref(), Some("10"));
        assert_eq!(attribute(request, "zx.usage.output_tokens").as_deref(), Some("5"));
        assert_eq!(attribute(request, "zx.stop_reason").as_deref(), Some("toolUse"));
        assert_eq!((ms(request.start_time), ms(request.end_time)), (1003, 1010));
        assert_eq!((ms(run.start_time), ms(run.end_time)), (1000, 1023));
        let decision = &tool.events.events[0];
        assert_eq!(decision.name, "decision");
        assert_eq!(decision.attributes[0].value.as_str(), "allow");
        // Nothing of the conversation leaves the process: not the command, the answer or the rule.
        let exported = format!("{spans:?}");
        assert!(!exported.contains("secret"), "{exported}");
    }

    #[test]
    fn metrics_count_runs_requests_tokens_and_tool_durations() {
        let mut harness = Harness::new();
        harness.apply(one_run());

        let all = harness.metrics();
        assert_eq!(
            counted(&all, "zx.runs", &[("zx.outcome", "completed"), ("zx.model", "m")]),
            1
        );
        assert_eq!(counted(&all, "zx.runs", &[("zx.outcome", "error")]), 0);
        let (count, sum) = timed(&all, "zx.run.duration", &[("zx.outcome", "completed")]);
        assert!(count == 1 && close(sum, 0.023), "{count} runs, {sum} s");
        let expected = [
            ("zx.request.kind", "task"),
            ("zx.model", "m"),
            ("zx.stop_reason", "toolUse"),
        ];
        let (count, sum) = timed(&all, "zx.request.duration", &expected);
        assert!(count == 1 && close(sum, 0.007), "{count} requests, {sum} s");
        assert_eq!(
            counted(&all, "zx.tokens", &[("zx.token.kind", "input"), ("zx.model", "m")]),
            10
        );
        assert_eq!(counted(&all, "zx.tokens", &[("zx.token.kind", "output")]), 5);
        // Zero counts make no series: the run reported no cached, written or reasoning tokens.
        assert_eq!(counted(&all, "zx.tokens", &[("zx.token.kind", "cache_read")]), 0);
        let (count, sum) = timed(
            &all,
            "zx.tool.duration",
            &[("zx.tool.name", "bash"), ("zx.tool.is_error", "false")],
        );
        assert!(count == 1 && close(sum, 0.009), "{count} calls, {sum} s");
        let exported = format!("{all:?}");
        assert!(!exported.contains("secret"), "{exported}");
    }

    #[test]
    fn tool_usage_is_set_on_the_tool_span_and_counted_under_the_tool() {
        let mut signals = one_run();
        assert!(matches!(signals[7].what, What::ToolEnd { .. }), "one_run changed shape");
        let mut spent = usage(3, 2);
        spent.cost_usd = Some(0.5);
        signals[7] = at(
            1020,
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "c1".into(),
                tool_name: "bash".into(),
                is_error: false,
                duration_ms: Some(8),
                details: None,
                usage: Some(spent),
            },
        );
        let mut harness = Harness::new();
        harness.apply(signals);

        let spans = harness.spans();
        let tool = span(&spans, "tool bash");
        assert_eq!(attribute(tool, "zx.usage.input_tokens").as_deref(), Some("3"));
        assert_eq!(attribute(tool, "zx.usage.output_tokens").as_deref(), Some("2"));
        assert_eq!(attribute(tool, "zx.usage.cost_usd").as_deref(), Some("0.5"));
        let all = harness.metrics();
        let by_tool = [("zx.token.kind", "input"), ("zx.tool.name", "bash")];
        assert_eq!(counted(&all, "zx.tokens", &by_tool), 3);
        assert_eq!(
            counted(
                &all,
                "zx.tokens",
                &[("zx.token.kind", "output"), ("zx.tool.name", "bash")]
            ),
            2
        );
        // The model's own tokens stay under the model, not the tool.
        assert_eq!(
            counted(&all, "zx.tokens", &[("zx.token.kind", "input"), ("zx.model", "m")]),
            10
        );
    }

    #[test]
    fn a_run_under_a_traceparent_continues_the_callers_trace() {
        let parent = TraceParent::parse(TRACEPARENT, Some("vendor=x")).unwrap();
        let mut harness = Harness::new();
        let mut signals = one_run();
        signals[0] = under(
            &parent,
            1000,
            AgentEvent::RunStart {
                model: "m".into(),
                provider: "p".into(),
            },
        );
        harness.apply(signals);

        let spans = harness.spans();
        let run = span(&spans, "run");
        assert_eq!(
            run.span_context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(run.parent_span_id.to_string(), "00f067aa0ba902b7");
        assert_eq!(run.span_context.trace_state().header(), "vendor=x");
        assert!(
            spans
                .iter()
                .all(|span| span.span_context.trace_id() == run.span_context.trace_id()),
            "{spans:?}"
        );
        // Spans within the run have their parents within the run.
        let turn = span(&spans, "turn");
        assert_eq!(turn.parent_span_id, span(&spans, "attempt").span_context.span_id());
    }

    #[test]
    fn spans_left_open_end_when_their_run_does() {
        let mut harness = Harness::new();
        harness.apply([
            at(
                1000,
                AgentEvent::RunStart {
                    model: "m".into(),
                    provider: "p".into(),
                },
            ),
            at(1001, AgentEvent::AgentStart { attempt: 1 }),
            at(1002, AgentEvent::TurnStart { turn: 1 }),
            at(
                1003,
                AgentEvent::RequestStart {
                    kind: RequestKind::Task,
                    entry_id: None,
                    model: "m".into(),
                    attempt: 1,
                },
            ),
            at(1050, run_end(RunOutcome::Aborted, usage(0, 0))),
        ]);

        let spans = harness.spans();
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert!(spans.iter().all(|span| ms(span.end_time) == 1050), "{spans:?}");
        assert_eq!(attribute(span(&spans, "run"), "zx.outcome").as_deref(), Some("aborted"));
        assert!(harness.runs.open.is_empty());
        // A request without an answer has no duration to report; the run has.
        let all = harness.metrics();
        assert_eq!(timed(&all, "zx.request.duration", &[]).0, 0);
        assert_eq!(counted(&all, "zx.runs", &[("zx.outcome", "aborted")]), 1);
    }

    #[test]
    fn a_manual_compaction_is_a_trace_of_its_own_and_is_forgotten_when_it_ends() {
        let mut harness = Harness::new();
        harness.apply([
            at(
                1000,
                AgentEvent::CompactionStart {
                    input_bytes: 100,
                    automatic: false,
                },
            ),
            at(
                1001,
                AgentEvent::RequestStart {
                    kind: RequestKind::Summary,
                    entry_id: None,
                    model: "m".into(),
                    attempt: 1,
                },
            ),
            at(
                1010,
                AgentEvent::CompactionEnd {
                    entry_id: "e1".into(),
                    input_bytes: 100,
                    output_bytes: 20,
                    usage: usage(7, 3),
                    automatic: false,
                },
            ),
        ]);

        let spans = harness.spans();
        let compaction = span(&spans, "compaction");
        let request = span(&spans, "request");
        assert_eq!(compaction.parent_span_id, SpanId::INVALID);
        assert_eq!(request.parent_span_id, compaction.span_context.span_id());
        assert_eq!(attribute(request, "zx.request.kind").as_deref(), Some("summary"));
        assert_eq!(
            attribute(compaction, "zx.compaction.output_bytes").as_deref(),
            Some("20")
        );
        assert_eq!(attribute(compaction, "zx.usage.input_tokens").as_deref(), Some("7"));
        assert!(harness.runs.open.is_empty());
        // The summary's tokens are counted once, for the model that was asked.
        let all = harness.metrics();
        assert_eq!(
            counted(&all, "zx.tokens", &[("zx.token.kind", "input"), ("zx.model", "m")]),
            7
        );
        assert_eq!(
            counted(&all, "zx.tokens", &[("zx.token.kind", "output"), ("zx.model", "m")]),
            3
        );
        let expected = [("zx.compaction.automatic", "false"), ("zx.outcome", "completed")];
        let (count, sum) = timed(&all, "zx.compaction.duration", &expected);
        assert!(count == 1 && close(sum, 0.010), "{count} summaries, {sum} s");
    }
}
