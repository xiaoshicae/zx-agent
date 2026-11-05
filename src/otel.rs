//! OpenTelemetry traces of runs, exported with OTLP over HTTP (protobuf) when the standard
//! variables name an endpoint.
//!
//! One run is one trace: a `run` span, an `attempt` span for each try, `turn` spans, and
//! spans for model requests, tool calls and summaries, timed by the events' `atMs`. Only
//! metadata goes out: IDs, names, counts, token usage, durations and outcomes. User input,
//! model output, tool arguments and results never do, nor do reasons and error texts, which
//! may quote them.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::trace::{Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    AgentEvent, ApprovalResult, Event, EventSink, Interruption, LoopOutcome, RequestKind, RunOutcome, ToolVerdict,
};
use crate::ai::{Message, StopReason, Usage};

/// Either variable turns the export on. As the OTLP specification has it, the first is used
/// as it is, and the second gets `/v1/traces` appended.
pub const ENDPOINT_VARIABLES: [&str; 2] = ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"];
/// Variables that may carry credentials for the collector, such as an authorization header.
/// Commands must not inherit them.
pub const HEADER_VARIABLES: [&str; 2] = ["OTEL_EXPORTER_OTLP_HEADERS", "OTEL_EXPORTER_OTLP_TRACES_HEADERS"];
/// Events waiting for the exporter task. A run never waits for telemetry: once the queue is
/// full, new events are dropped and counted.
const QUEUE_CAPACITY: usize = 4096;
/// How long shutdown waits for the collector: the SDK's own default.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// `service.name`, unless `OTEL_SERVICE_NAME` sets one.
const SERVICE_NAME: &str = "zx-agent";

/// Exports the events of runs as OpenTelemetry traces. Give [`sink`](Self::sink) to each
/// session, and call [`shutdown`](Self::shutdown) before the process exits.
pub struct Telemetry {
    provider: SdkTracerProvider,
    sink: Arc<Spans>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Telemetry {
    /// Export to the endpoint that the standard variables name, or `None` when neither of
    /// [`ENDPOINT_VARIABLES`] is set: nothing is then sent anywhere. The SDK reads the other
    /// standard variables, such as `OTEL_EXPORTER_OTLP_HEADERS`. Needs a Tokio runtime. An
    /// error is a configuration that the exporter refuses, such as an invalid endpoint.
    pub fn from_env() -> Result<Option<Self>, String> {
        let configured = ENDPOINT_VARIABLES
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
        if !configured {
            return Ok(None);
        }
        let exporter = SpanExporter::builder()
            .with_http()
            .build()
            .map_err(|error| format!("cannot set up the OTLP exporter: {error}"))?;
        let mut resource = Resource::builder();
        if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
            resource = resource.with_service_name(SERVICE_NAME);
        }
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource.build())
            .build();
        Ok(Some(Self::with_provider(provider)))
    }

    /// Export through `provider`, such as one with an exporter that the host set up. Needs a
    /// Tokio runtime.
    pub fn with_provider(provider: SdkTracerProvider) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let stop = CancellationToken::new();
        let task = tokio::spawn(export(receiver, provider.tracer(SERVICE_NAME), stop.clone()));
        Self {
            provider,
            sink: Arc::new(Spans {
                sender,
                dropped: AtomicU64::new(0),
            }),
            stop,
            task,
        }
    }

    /// The sink to give a session: it passes each event on without waiting.
    pub fn sink(&self) -> Arc<dyn EventSink> {
        self.sink.clone()
    }

    /// Events dropped because the queue was full, or because the export had stopped.
    pub fn dropped(&self) -> u64 {
        self.sink.dropped.load(Ordering::Relaxed)
    }

    /// End the spans still open and export what is buffered, waiting at most 5 s for the
    /// collector. Events emitted afterwards are dropped.
    pub async fn shutdown(self) -> Result<(), String> {
        let Self {
            provider, stop, task, ..
        } = self;
        stop.cancel();
        task.await.map_err(|error| format!("the span task failed: {error}"))?;
        // Shutting down blocks until the last batch is sent or the time is up.
        tokio::task::spawn_blocking(move || provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT))
            .await
            .map_err(|error| format!("the span export failed: {error}"))?
            .map_err(|error| format!("cannot export the remaining spans: {error}"))
    }
}

/// Passes the events that spans need to the exporter task.
struct Spans {
    sender: mpsc::Sender<Signal>,
    dropped: AtomicU64,
}

impl EventSink for Spans {
    fn emit(&self, event: &Event) {
        let Some(signal) = Signal::of(event) else {
            return;
        };
        if self.sender.try_send(signal).is_err() {
            // Full, or the export has stopped: a run never waits for telemetry.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What a span needs from one event: metadata only.
struct Signal {
    run_id: String,
    session_id: Option<String>,
    at: SystemTime,
    what: What,
}

enum What {
    RunStart {
        model: String,
        provider: String,
    },
    RunEnd {
        outcome: RunOutcome,
        model_requests: u32,
        retries: u32,
        usage: Usage,
    },
    AttemptStart {
        attempt: u32,
    },
    AttemptEnd {
        outcome: LoopOutcome,
    },
    TurnStart {
        turn: u32,
    },
    TurnEnd,
    RequestStart {
        kind: RequestKind,
        model: String,
        attempt: u32,
    },
    Response {
        usage: Usage,
        stop: StopReason,
    },
    ToolStart {
        call: String,
        tool: String,
    },
    Decision {
        call: String,
        verdict: ToolVerdict,
    },
    ApprovalRequested {
        call: String,
    },
    ApprovalResolved {
        call: String,
        result: ApprovalResult,
        wait_ms: u64,
    },
    ToolEnd {
        call: String,
        is_error: bool,
    },
    CompactionStart {
        automatic: bool,
        input_bytes: usize,
    },
    CompactionEnd {
        usage: Usage,
        output_bytes: usize,
    },
    CompactionFailed,
    Retry {
        attempt: u32,
        delay_ms: u64,
    },
    Overflow,
    HookFailed {
        hook: String,
        phase: &'static str,
    },
    Recovered {
        call: String,
        tool: String,
        state: Interruption,
    },
}

impl Signal {
    fn of(event: &Event) -> Option<Self> {
        let what = match &event.event {
            AgentEvent::RunStart { model, provider } => What::RunStart {
                model: model.clone(),
                provider: provider.clone(),
            },
            AgentEvent::RunEnd { report } => What::RunEnd {
                outcome: report.outcome,
                model_requests: report.model_requests,
                retries: report.retries,
                usage: report.usage.clone(),
            },
            AgentEvent::AgentStart { attempt } => What::AttemptStart { attempt: *attempt },
            AgentEvent::AgentEnd { outcome } => What::AttemptEnd { outcome: *outcome },
            AgentEvent::TurnStart { turn } => What::TurnStart { turn: *turn },
            AgentEvent::TurnEnd { .. } => What::TurnEnd,
            AgentEvent::RequestStart {
                kind, model, attempt, ..
            } => What::RequestStart {
                kind: *kind,
                model: model.clone(),
                attempt: *attempt,
            },
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
                ..
            } => What::Response {
                usage: message.usage.clone(),
                stop: message.stop_reason,
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                ..
            } => What::ToolStart {
                call: tool_call_id.clone(),
                tool: tool_name.clone(),
            },
            AgentEvent::ToolDecision {
                tool_call_id, verdict, ..
            } => What::Decision {
                call: tool_call_id.clone(),
                verdict: *verdict,
            },
            AgentEvent::ApprovalRequested { tool_call_id, .. } => What::ApprovalRequested {
                call: tool_call_id.clone(),
            },
            AgentEvent::ApprovalResolved {
                tool_call_id,
                approval,
                wait_ms,
                ..
            } => What::ApprovalResolved {
                call: tool_call_id.clone(),
                result: *approval,
                wait_ms: *wait_ms,
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id, is_error, ..
            } => What::ToolEnd {
                call: tool_call_id.clone(),
                is_error: *is_error,
            },
            AgentEvent::CompactionStart { input_bytes, automatic } => What::CompactionStart {
                automatic: *automatic,
                input_bytes: *input_bytes,
            },
            AgentEvent::CompactionEnd {
                usage, output_bytes, ..
            } => What::CompactionEnd {
                usage: usage.clone(),
                output_bytes: *output_bytes,
            },
            AgentEvent::CompactionFailed { .. } => What::CompactionFailed,
            AgentEvent::RetryScheduled { attempt, delay_ms, .. } => What::Retry {
                attempt: *attempt,
                delay_ms: *delay_ms,
            },
            // The error text may quote the request, so only the fact is exported.
            AgentEvent::ContextOverflow { .. } => What::Overflow,
            AgentEvent::HookFailed { hook, phase, .. } => What::HookFailed {
                hook: hook.clone(),
                phase,
            },
            AgentEvent::ToolCallRecovered {
                tool_call_id,
                tool_name,
                state,
            } => What::Recovered {
                call: tool_call_id.clone(),
                tool: tool_name.clone(),
                state: *state,
            },
            // Streaming, the other messages, context edits and extension records are
            // content, not spans.
            AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::ToolExecutionUpdate { .. }
            | AgentEvent::ContextEdited { .. }
            | AgentEvent::CustomEntry { .. } => return None,
        };
        Some(Self {
            run_id: event.run_id.clone(),
            session_id: event.session_id.clone(),
            at: UNIX_EPOCH + Duration::from_millis(event.at_ms),
            what,
        })
    }
}

/// Builds spans from signals, one task owning every open span.
async fn export(mut receiver: mpsc::Receiver<Signal>, tracer: SdkTracer, stop: CancellationToken) {
    let mut runs = Runs::new(tracer);
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
struct Runs {
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
    tools: HashMap<String, Context>,
}

impl Open {
    /// The innermost span that a new span or event belongs to.
    fn current(&self) -> Option<&Context> {
        self.turn.as_ref().or(self.attempt.as_ref()).or(self.run.as_ref())
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
    fn new(tracer: SdkTracer) -> Self {
        Self {
            tracer,
            open: HashMap::new(),
        }
    }

    fn apply(&mut self, signal: Signal) {
        let Signal {
            run_id,
            session_id,
            at,
            what,
        } = signal;
        let Self { tracer, open } = self;
        let spans = open.entry(run_id.clone()).or_default();
        match what {
            What::RunStart { model, provider } => {
                let mut attributes = vec![
                    KeyValue::new("zx.run.id", run_id.clone()),
                    KeyValue::new("zx.model", model),
                    KeyValue::new("zx.provider", provider),
                ];
                if let Some(session) = session_id {
                    attributes.push(KeyValue::new("zx.session.id", session));
                }
                spans.run = Some(start(tracer, "run", at, attributes, None));
            }
            What::RunEnd {
                outcome,
                model_requests,
                retries,
                usage,
            } => {
                spans.end_attempt_parts(at);
                end(spans.attempt.take(), at);
                if let Some(run) = spans.run.take() {
                    let span = run.span();
                    span.set_attribute(KeyValue::new("zx.outcome", name(&outcome)));
                    span.set_attribute(KeyValue::new("zx.model_requests", i64::from(model_requests)));
                    span.set_attribute(KeyValue::new("zx.retries", i64::from(retries)));
                    span.set_attributes(usage_attributes(&usage));
                    if outcome == RunOutcome::Error {
                        span.set_status(Status::error("error"));
                    }
                    span.end_with_timestamp(at);
                }
                open.remove(&run_id);
            }
            What::AttemptStart { attempt } => {
                spans.end_attempt_parts(at);
                end(spans.attempt.take(), at);
                let attributes = vec![KeyValue::new("zx.attempt", i64::from(attempt))];
                spans.attempt = Some(start(tracer, "attempt", at, attributes, spans.run.as_ref()));
            }
            What::AttemptEnd { outcome } => {
                spans.end_attempt_parts(at);
                if let Some(attempt) = spans.attempt.take() {
                    let span = attempt.span();
                    span.set_attribute(KeyValue::new("zx.outcome", name(&outcome)));
                    if outcome == LoopOutcome::Error {
                        span.set_status(Status::error("error"));
                    }
                    span.end_with_timestamp(at);
                }
            }
            What::TurnStart { turn } => {
                end(spans.turn.take(), at);
                let parent = spans.attempt.as_ref().or(spans.run.as_ref());
                let attributes = vec![KeyValue::new("zx.turn", i64::from(turn))];
                spans.turn = Some(start(tracer, "turn", at, attributes, parent));
            }
            What::TurnEnd => end(spans.turn.take(), at),
            What::RequestStart { kind, model, attempt } => {
                end(spans.request.take(), at);
                // A summary request belongs to its compaction.
                let parent = spans.compaction.as_ref().or(spans.current());
                let attributes = vec![
                    KeyValue::new("zx.request.kind", name(&kind)),
                    KeyValue::new("zx.model", model),
                    KeyValue::new("zx.attempt", i64::from(attempt)),
                ];
                spans.request = Some(start(tracer, "request", at, attributes, parent));
            }
            What::Response { usage, stop } => {
                if let Some(request) = spans.request.take() {
                    let span = request.span();
                    span.set_attributes(usage_attributes(&usage));
                    span.set_attribute(KeyValue::new("zx.stop_reason", name(&stop)));
                    if stop == StopReason::Error {
                        span.set_status(Status::error("error"));
                    }
                    span.end_with_timestamp(at);
                }
            }
            What::ToolStart { call, tool } => {
                let attributes = vec![
                    KeyValue::new("zx.tool.name", tool.clone()),
                    KeyValue::new("zx.tool.call_id", call.clone()),
                ];
                let span = start(tracer, format!("tool {tool}"), at, attributes, spans.current());
                spans.tools.insert(call, span);
            }
            What::Decision { call, verdict } => {
                let attributes = vec![KeyValue::new("zx.verdict", name(&verdict))];
                event(spans.tools.get(&call).or(spans.current()), "decision", at, attributes);
            }
            What::ApprovalRequested { call } => {
                event(
                    spans.tools.get(&call).or(spans.current()),
                    "approval_requested",
                    at,
                    Vec::new(),
                );
            }
            What::ApprovalResolved { call, result, wait_ms } => {
                let attributes = vec![
                    KeyValue::new("zx.approval.result", name(&result)),
                    KeyValue::new("zx.approval.wait_ms", saturate(wait_ms)),
                ];
                event(
                    spans.tools.get(&call).or(spans.current()),
                    "approval_resolved",
                    at,
                    attributes,
                );
            }
            What::ToolEnd { call, is_error } => {
                if let Some(tool) = spans.tools.remove(&call) {
                    let span = tool.span();
                    span.set_attribute(KeyValue::new("zx.tool.is_error", is_error));
                    if is_error {
                        span.set_status(Status::error("error"));
                    }
                    span.end_with_timestamp(at);
                }
            }
            What::CompactionStart { automatic, input_bytes } => {
                end(spans.compaction.take(), at);
                let attributes = vec![
                    KeyValue::new("zx.compaction.automatic", automatic),
                    KeyValue::new("zx.compaction.input_bytes", saturate_usize(input_bytes)),
                ];
                // A manual compaction belongs to no run, so it starts a trace of its own.
                spans.compaction = Some(start(tracer, "compaction", at, attributes, spans.current()));
            }
            What::CompactionEnd { usage, output_bytes } => {
                end(spans.request.take(), at);
                if let Some(compaction) = spans.compaction.take() {
                    let span = compaction.span();
                    span.set_attributes(usage_attributes(&usage));
                    span.set_attribute(KeyValue::new(
                        "zx.compaction.output_bytes",
                        saturate_usize(output_bytes),
                    ));
                    span.end_with_timestamp(at);
                }
            }
            What::CompactionFailed => {
                end(spans.request.take(), at);
                if let Some(compaction) = spans.compaction.take() {
                    let span = compaction.span();
                    span.set_status(Status::error("error"));
                    span.end_with_timestamp(at);
                }
            }
            What::Retry { attempt, delay_ms } => {
                let attributes = vec![
                    KeyValue::new("zx.attempt", i64::from(attempt)),
                    KeyValue::new("zx.delay_ms", saturate(delay_ms)),
                ];
                event(spans.run.as_ref(), "retry_scheduled", at, attributes);
            }
            What::Overflow => event(spans.run.as_ref(), "context_overflow", at, Vec::new()),
            What::HookFailed { hook, phase } => {
                let attributes = vec![KeyValue::new("zx.hook", hook), KeyValue::new("zx.hook.phase", phase)];
                event(spans.current(), "hook_failed", at, attributes);
            }
            What::Recovered { call, tool, state } => {
                let attributes = vec![
                    KeyValue::new("zx.tool.name", tool),
                    KeyValue::new("zx.tool.call_id", call),
                    KeyValue::new("zx.recovery.state", name(&state)),
                ];
                event(spans.run.as_ref(), "tool_call_recovered", at, attributes);
            }
        }
        // A manual compaction leaves nothing open once it ends; neither does a run's end.
        if open.get(&run_id).is_some_and(Open::is_empty) {
            open.remove(&run_id);
        }
    }

    /// End every span still open, as the export stops.
    fn end_all(&mut self, at: SystemTime) {
        for (_, mut spans) in self.open.drain() {
            spans.end_attempt_parts(at);
            end(spans.attempt.take(), at);
            end(spans.run.take(), at);
        }
    }
}

fn start(
    tracer: &SdkTracer,
    name: impl Into<Cow<'static, str>>,
    at: SystemTime,
    attributes: Vec<KeyValue>,
    parent: Option<&Context>,
) -> Context {
    let builder = tracer
        .span_builder(name)
        .with_start_time(at)
        .with_attributes(attributes);
    // An empty context makes a new trace; the current one is not looked at.
    let parent = parent.cloned().unwrap_or_default();
    let span = tracer.build_with_context(builder, &parent);
    parent.with_span(span)
}

fn end(span: Option<Context>, at: SystemTime) {
    if let Some(span) = span {
        span.span().end_with_timestamp(at);
    }
}

fn event(span: Option<&Context>, name: &'static str, at: SystemTime, attributes: Vec<KeyValue>) {
    if let Some(span) = span {
        span.span().add_event_with_timestamp(name, at, attributes);
    }
}

fn usage_attributes(usage: &Usage) -> Vec<KeyValue> {
    vec![
        KeyValue::new("zx.usage.input_tokens", saturate(usage.input)),
        KeyValue::new("zx.usage.output_tokens", saturate(usage.output)),
        KeyValue::new("zx.usage.cache_read_tokens", saturate(usage.cache_read)),
    ]
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
    use opentelemetry::trace::SpanId;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
    use serde_json::json;

    use super::*;
    use crate::agent::{EVENT_SCHEMA_VERSION, RunReport};
    use crate::ai::{AssistantMessage, ContentBlock};

    const RUN: &str = "6f1c2c4e-0000-4000-8000-000000000001";

    /// Spans built synchronously into an in-memory exporter; keep the provider alive.
    fn runs() -> (InMemorySpanExporter, SdkTracerProvider, Runs) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let runs = Runs::new(provider.tracer("test"));
        (exporter, provider, runs)
    }

    fn event(at_ms: u64, event: AgentEvent) -> Event {
        Event {
            schema_version: EVENT_SCHEMA_VERSION,
            seq: at_ms,
            at_ms,
            session_id: Some("s1".into()),
            run_id: RUN.into(),
            event,
        }
    }

    fn at(at_ms: u64, agent_event: AgentEvent) -> Signal {
        Signal::of(&event(at_ms, agent_event)).expect("a span needs this event")
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
                error: None,
                session_file: None,
            }),
        }
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

    #[test]
    fn a_run_is_one_trace_of_nested_spans_that_carry_metadata_only() {
        let (exporter, _provider, mut runs) = runs();
        let command = json!({"command": "cat secret.txt"});
        let call = ContentBlock::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: command.clone(),
            arguments_error: None,
        };
        for signal in [
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
        ] {
            runs.apply(signal);
        }

        let spans = exporter.get_finished_spans().unwrap();
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
    fn spans_left_open_end_when_their_run_does() {
        let (exporter, _provider, mut runs) = runs();
        for signal in [
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
        ] {
            runs.apply(signal);
        }

        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert!(spans.iter().all(|span| ms(span.end_time) == 1050), "{spans:?}");
        assert_eq!(attribute(span(&spans, "run"), "zx.outcome").as_deref(), Some("aborted"));
        assert!(runs.open.is_empty());
    }

    #[test]
    fn a_manual_compaction_is_a_trace_of_its_own_and_is_forgotten_when_it_ends() {
        let (exporter, _provider, mut runs) = runs();
        for signal in [
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
        ] {
            runs.apply(signal);
        }

        let spans = exporter.get_finished_spans().unwrap();
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
        assert!(runs.open.is_empty());
    }

    #[test]
    fn the_sink_drops_events_beyond_its_queue_and_counts_them() {
        let (sender, _receiver) = mpsc::channel(1);
        let sink = Spans {
            sender,
            dropped: AtomicU64::new(0),
        };
        sink.emit(&event(1, AgentEvent::TurnStart { turn: 1 }));
        sink.emit(&event(2, AgentEvent::TurnStart { turn: 2 }));
        // Not for spans: neither queued nor counted.
        sink.emit(&event(3, AgentEvent::MessageStart { role: "assistant" }));
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
    }
}
