//! OpenTelemetry traces and metrics of runs, exported with OTLP over HTTP (protobuf) when the
//! standard variables name an endpoint.
//!
//! One run is one trace: a `run` span, an `attempt` span for each try, `turn` spans, and
//! spans for model requests, tool calls and summaries, timed by the events' `atMs`. A run
//! continues the caller's trace when its sink was made with a [`TraceParent`]. The same
//! events feed the metrics: runs, requests, tool calls and summaries counted and timed,
//! tokens and cost added up. Only metadata goes out: IDs, names, counts, token usage,
//! durations and outcomes. User input, model output, tool arguments and results never do,
//! nor do reasons and error texts, which may quote them.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider};
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{SpanContext, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::{MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    AgentEvent, ApprovalResult, Event, EventSink, Interruption, LoopOutcome, RequestKind, RunOutcome, ToolVerdict,
};
use crate::ai::{Message, StopReason, Usage};

/// Either variable turns the trace export on. As the OTLP specification has it, the first is
/// used as it is, and the second gets `/v1/traces` appended.
pub const ENDPOINT_VARIABLES: [&str; 2] = ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"];
/// Either variable turns the metric export on, the same way, with `/v1/metrics` appended to
/// the second.
pub const METRIC_ENDPOINT_VARIABLES: [&str; 2] = ["OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"];
/// Variables that may carry credentials for the collector, such as an authorization header.
/// Commands must not inherit them.
pub const HEADER_VARIABLES: [&str; 3] = [
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
];
/// The W3C trace context a caller passes in the environment, as `otel-cli` and the scripts
/// it instruments do: the value of a `traceparent` header, and the optional `tracestate`.
pub const TRACE_CONTEXT_VARIABLES: [&str; 2] = ["TRACEPARENT", "TRACESTATE"];
/// Milliseconds between metric exports, as the SDK specification names it; unset uses the
/// specification's default of a minute. What is buffered is also exported at shutdown.
pub const METRIC_INTERVAL_VARIABLE: &str = "OTEL_METRIC_EXPORT_INTERVAL";
const DEFAULT_METRIC_INTERVAL: Duration = Duration::from_secs(60);
/// Events waiting for the exporter task. A run never waits for telemetry: once the queue is
/// full, new events are dropped and counted.
const QUEUE_CAPACITY: usize = 4096;
/// How long shutdown waits for the collector: the SDK's own default.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// `service.name`, unless `OTEL_SERVICE_NAME` sets one.
const SERVICE_NAME: &str = "zx-agent";
/// Histogram buckets for durations in seconds: from a quick tool call to an hour-long run.
const DURATION_BOUNDARIES: [f64; 15] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

/// Which signals a [`Telemetry`] exports: traces need a trace endpoint, metrics a metric one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exports {
    Traces,
    Metrics,
    TracesAndMetrics,
}

impl Exports {
    /// The signals, for a message: `traces`, `metrics` or `traces and metrics`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Metrics => "metrics",
            Self::TracesAndMetrics => "traces and metrics",
        }
    }

    fn traces(self) -> bool {
        matches!(self, Self::Traces | Self::TracesAndMetrics)
    }

    fn metrics(self) -> bool {
        matches!(self, Self::Metrics | Self::TracesAndMetrics)
    }
}

/// A W3C trace context received from the caller: the values of the `traceparent` and
/// `tracestate` headers, or of the [`TRACE_CONTEXT_VARIABLES`]. Runs emitted through
/// [`Telemetry::sink_under`] continue that trace, under the caller's span.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceParent {
    context: SpanContext,
}

impl TraceParent {
    /// `None` when `traceparent` is not a valid header value: the wrong layout, uppercase
    /// hex, or an all-zero trace or span ID. A malformed `tracestate` is left out, as the
    /// specification allows.
    pub fn parse(traceparent: &str, tracestate: Option<&str>) -> Option<Self> {
        let mut carrier = HashMap::new();
        carrier.insert("traceparent".to_string(), traceparent.trim().to_string());
        if let Some(state) = tracestate {
            carrier.insert("tracestate".to_string(), state.to_string());
        }
        let extracted = TraceContextPropagator::new().extract(&carrier);
        let span = extracted.span();
        let context = span.span_context();
        context.is_valid().then(|| Self {
            context: context.clone(),
        })
    }

    /// From `TRACEPARENT` and `TRACESTATE`: `Ok(None)` when `TRACEPARENT` is unset or empty,
    /// an error naming the variable when it is set but not a valid header value.
    pub fn from_env() -> Result<Option<Self>, String> {
        // A value that is not UTF-8 cannot be a traceparent either: it counts as unset.
        let [parent, state] =
            TRACE_CONTEXT_VARIABLES.map(|name| std::env::var(name).ok().filter(|value| !value.trim().is_empty()));
        let Some(parent) = parent else {
            return Ok(None);
        };
        Self::parse(&parent, state.as_deref())
            .map(Some)
            .ok_or_else(|| format!("{} is not a W3C traceparent: {parent:?}", TRACE_CONTEXT_VARIABLES[0]))
    }

    /// The trace's ID, as 32 hex digits.
    pub fn trace_id(&self) -> String {
        self.context.trace_id().to_string()
    }

    /// The caller's span, as 16 hex digits.
    pub fn span_id(&self) -> String {
        self.context.span_id().to_string()
    }
}

/// Exports the events of runs as OpenTelemetry traces and metrics. Give [`sink`](Self::sink)
/// to each session, and call [`shutdown`](Self::shutdown) before the process exits.
pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    meter_provider: Option<SdkMeterProvider>,
    exports: Exports,
    sender: mpsc::Sender<Signal>,
    dropped: Arc<AtomicU64>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Telemetry {
    /// Export to the endpoints that the standard variables name: traces when one of
    /// [`ENDPOINT_VARIABLES`] is set, metrics when one of [`METRIC_ENDPOINT_VARIABLES`] is.
    /// `None` when neither is: nothing is then sent anywhere. The SDK reads the other standard
    /// variables, such as `OTEL_EXPORTER_OTLP_HEADERS`. Needs a Tokio runtime. An error is a
    /// configuration that the exporter refuses, such as an invalid endpoint or interval.
    pub fn from_env() -> Result<Option<Self>, String> {
        let exports = match (any_set(&ENDPOINT_VARIABLES), any_set(&METRIC_ENDPOINT_VARIABLES)) {
            (true, true) => Exports::TracesAndMetrics,
            (true, false) => Exports::Traces,
            (false, true) => Exports::Metrics,
            (false, false) => return Ok(None),
        };
        let mut resource = Resource::builder();
        if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
            resource = resource.with_service_name(SERVICE_NAME);
        }
        let resource = resource.build();
        let mut tracer_provider = SdkTracerProvider::builder().with_resource(resource.clone());
        if exports.traces() {
            let exporter = SpanExporter::builder()
                .with_http()
                .build()
                .map_err(|error| format!("cannot set up the OTLP span exporter: {error}"))?;
            tracer_provider = tracer_provider.with_batch_exporter(exporter);
        }
        let meter_provider = if exports.metrics() {
            let interval = metric_interval_from_env()?;
            let exporter = MetricExporter::builder()
                .with_http()
                .build()
                .map_err(|error| format!("cannot set up the OTLP metric exporter: {error}"))?;
            let reader = PeriodicReader::builder(exporter).with_interval(interval).build();
            Some(
                SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource)
                    .build(),
            )
        } else {
            None
        };
        Ok(Some(Self::new(tracer_provider.build(), meter_provider, exports)))
    }

    /// Export traces through `provider`, such as one with an exporter that the host set up,
    /// and no metrics. Needs a Tokio runtime.
    pub fn with_provider(provider: SdkTracerProvider) -> Self {
        Self::new(provider, None, Exports::Traces)
    }

    /// Export traces and metrics through the host's providers. Needs a Tokio runtime.
    pub fn with_providers(tracer: SdkTracerProvider, meter: SdkMeterProvider) -> Self {
        Self::new(tracer, Some(meter), Exports::TracesAndMetrics)
    }

    fn new(tracer_provider: SdkTracerProvider, meter_provider: Option<SdkMeterProvider>, exports: Exports) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let stop = CancellationToken::new();
        let instruments = meter_provider
            .as_ref()
            .map(|provider| Instruments::new(&provider.meter(SERVICE_NAME)));
        let runs = Runs::new(tracer_provider.tracer(SERVICE_NAME), instruments);
        let task = tokio::spawn(export(receiver, runs, stop.clone()));
        Self {
            tracer_provider,
            meter_provider,
            exports,
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            stop,
            task,
        }
    }

    /// What goes out.
    pub fn exports(&self) -> Exports {
        self.exports
    }

    /// The sink to give a session: it passes each event on without waiting. Every run starts
    /// a trace of its own.
    pub fn sink(&self) -> Arc<dyn EventSink> {
        Arc::new(self.spans(None))
    }

    /// A sink whose runs continue the caller's trace: each `run` span, and each manual
    /// compaction, is a child of the span that `parent` names.
    pub fn sink_under(&self, parent: &TraceParent) -> Arc<dyn EventSink> {
        Arc::new(self.spans(Some(parent.context.clone())))
    }

    fn spans(&self, parent: Option<SpanContext>) -> Spans {
        Spans {
            sender: self.sender.clone(),
            dropped: Arc::clone(&self.dropped),
            parent,
        }
    }

    /// Events dropped because the queue was full, or because the export had stopped.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// End the spans still open and export what is buffered, spans and then metrics, waiting
    /// at most 5 s for the collector each time. Events emitted afterwards are dropped.
    pub async fn shutdown(self) -> Result<(), String> {
        let Self {
            tracer_provider,
            meter_provider,
            stop,
            task,
            ..
        } = self;
        stop.cancel();
        task.await.map_err(|error| format!("the span task failed: {error}"))?;
        // Shutting down blocks until the last batch is sent or the time is up.
        tokio::task::spawn_blocking(move || {
            tracer_provider
                .shutdown_with_timeout(SHUTDOWN_TIMEOUT)
                .map_err(|error| format!("cannot export the remaining spans: {error}"))?;
            if let Some(meter_provider) = meter_provider {
                meter_provider
                    .shutdown_with_timeout(SHUTDOWN_TIMEOUT)
                    .map_err(|error| format!("cannot export the remaining metrics: {error}"))?;
            }
            Ok(())
        })
        .await
        .map_err(|error| format!("the export failed: {error}"))?
    }
}

fn any_set(names: &[&str]) -> bool {
    names
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

fn metric_interval_from_env() -> Result<Duration, String> {
    match std::env::var(METRIC_INTERVAL_VARIABLE) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|millis| *millis > 0)
            .map(Duration::from_millis)
            .ok_or_else(|| {
                format!("{METRIC_INTERVAL_VARIABLE} must be a number of milliseconds greater than zero, not {value:?}")
            }),
        // Unset or empty; a value that is not UTF-8 is not a number either and counts as unset.
        _ => Ok(DEFAULT_METRIC_INTERVAL),
    }
}

/// Passes the events that spans and metrics need to the exporter task.
struct Spans {
    sender: mpsc::Sender<Signal>,
    dropped: Arc<AtomicU64>,
    /// The caller's span, which the runs continue.
    parent: Option<SpanContext>,
}

impl EventSink for Spans {
    fn emit(&self, event: &Event) {
        let Some(signal) = Signal::of(event, self.parent.as_ref()) else {
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
    /// The span to start a trace under, for the events that start one.
    parent: Option<SpanContext>,
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
        model: String,
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
    fn of(event: &Event, parent: Option<&SpanContext>) -> Option<Self> {
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
                model: message.model.clone(),
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
        // Only a trace's first span has a parent outside the trace.
        let parent = match what {
            What::RunStart { .. } | What::CompactionStart { .. } => parent.cloned(),
            _ => None,
        };
        Some(Self {
            run_id: event.run_id.clone(),
            session_id: event.session_id.clone(),
            at: UNIX_EPOCH + Duration::from_millis(event.at_ms),
            parent,
            what,
        })
    }
}

/// Builds spans and records metrics from signals, one task owning every open span.
async fn export(mut receiver: mpsc::Receiver<Signal>, mut runs: Runs, stop: CancellationToken) {
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

/// The metrics, recorded as the spans they measure end. Durations are in seconds, as the
/// semantic conventions have them.
struct Instruments {
    /// Runs that ended, by outcome and model.
    runs: Counter<u64>,
    run_duration: Histogram<f64>,
    /// By request kind, model and stop reason.
    request_duration: Histogram<f64>,
    /// Token usage the model reported, by model and kind: `input`, `output`, `cache_read`,
    /// `cache_write` and `reasoning` (counted within `output`).
    tokens: Counter<u64>,
    /// Cost by the configured prices, in USD, by model.
    cost: Counter<f64>,
    /// By tool name and whether the result was an error.
    tool_duration: Histogram<f64>,
    retries: Counter<u64>,
    /// By whether the summary was automatic, and its outcome.
    compaction_duration: Histogram<f64>,
    /// How long a tool call waited for a person, by the answer.
    approval_wait: Histogram<f64>,
}

impl Instruments {
    fn new(meter: &Meter) -> Self {
        let duration = |name: &'static str, description: &'static str| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_description(description)
                .with_boundaries(DURATION_BOUNDARIES.to_vec())
                .build()
        };
        Self {
            runs: meter
                .u64_counter("zx.runs")
                .with_unit("{run}")
                .with_description("Runs that ended, by outcome.")
                .build(),
            run_duration: duration("zx.run.duration", "From a run's start to its end."),
            request_duration: duration("zx.request.duration", "From a model request to its complete answer."),
            tokens: meter
                .u64_counter("zx.tokens")
                .with_unit("{token}")
                .with_description("Token usage the model reported, by kind.")
                .build(),
            cost: meter
                .f64_counter("zx.cost")
                .with_unit("USD")
                .with_description("Cost of the answers by the configured prices.")
                .build(),
            tool_duration: duration("zx.tool.duration", "From a tool call's start to its result."),
            retries: meter
                .u64_counter("zx.retries")
                .with_unit("{retry}")
                .with_description("Retries of model requests.")
                .build(),
            compaction_duration: duration(
                "zx.compaction.duration",
                "From a summary's start to its end or failure.",
            ),
            approval_wait: duration("zx.approval.wait", "How long a tool call waited for a person's answer."),
        }
    }

    fn count_usage(&self, usage: &Usage, model: &str) {
        let counts = [
            ("input", usage.input),
            ("output", usage.output),
            ("cache_read", usage.cache_read),
            ("cache_write", usage.cache_write),
            ("reasoning", usage.reasoning),
        ];
        for (kind, count) in counts {
            if count > 0 {
                let attributes = [
                    KeyValue::new("zx.model", model.to_string()),
                    KeyValue::new("zx.token.kind", kind),
                ];
                self.tokens.add(count, &attributes);
            }
        }
        if let Some(cost) = usage.cost_usd {
            self.cost.add(cost, &[KeyValue::new("zx.model", model.to_string())]);
        }
    }
}

/// The open spans of each run, by run ID.
struct Runs {
    tracer: SdkTracer,
    instruments: Option<Instruments>,
    open: HashMap<String, Open>,
}

/// A span still open, with what its metric needs when it ends.
struct Live<M> {
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
    run: Option<Live<String>>,
    attempt: Option<Context>,
    turn: Option<Context>,
    request: Option<Live<Request>>,
    /// With whether it was automatic.
    compaction: Option<Live<bool>>,
    /// By call ID, with the tool's name.
    tools: HashMap<String, Live<String>>,
}

impl Open {
    /// The innermost span that a new span or event belongs to.
    fn current(&self) -> Option<&Context> {
        self.turn
            .as_ref()
            .or(self.attempt.as_ref())
            .or(self.run.as_ref().map(|run| &run.cx))
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
    fn new(tracer: SdkTracer, instruments: Option<Instruments>) -> Self {
        Self {
            tracer,
            instruments,
            open: HashMap::new(),
        }
    }

    fn apply(&mut self, signal: Signal) {
        let Signal {
            run_id,
            session_id,
            at,
            parent,
            what,
        } = signal;
        let Self {
            tracer,
            instruments,
            open,
        } = self;
        let spans = open.entry(run_id.clone()).or_default();
        match what {
            What::RunStart { model, provider } => {
                let mut attributes = vec![
                    KeyValue::new("zx.run.id", run_id.clone()),
                    KeyValue::new("zx.model", model.clone()),
                    KeyValue::new("zx.provider", provider),
                ];
                if let Some(session) = session_id {
                    attributes.push(KeyValue::new("zx.session.id", session));
                }
                let cx = start(tracer, "run", at, attributes, parent.map(remote));
                spans.run = Some(Live {
                    cx,
                    since: at,
                    meta: model,
                });
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
                    let span = run.cx.span();
                    span.set_attribute(KeyValue::new("zx.outcome", name(&outcome)));
                    span.set_attribute(KeyValue::new("zx.model_requests", i64::from(model_requests)));
                    span.set_attribute(KeyValue::new("zx.retries", i64::from(retries)));
                    span.set_attributes(usage_attributes(&usage));
                    if outcome == RunOutcome::Error {
                        span.set_status(Status::error("error"));
                    }
                    span.end_with_timestamp(at);
                    if let Some(instruments) = instruments {
                        let attributes = [
                            KeyValue::new("zx.outcome", name(&outcome)),
                            KeyValue::new("zx.model", run.meta),
                        ];
                        instruments.runs.add(1, &attributes);
                        instruments.run_duration.record(seconds(run.since, at), &attributes);
                    }
                }
                open.remove(&run_id);
            }
            What::AttemptStart { attempt } => {
                spans.end_attempt_parts(at);
                end(spans.attempt.take(), at);
                let attributes = vec![KeyValue::new("zx.attempt", i64::from(attempt))];
                let parent = spans.run.as_ref().map(|run| run.cx.clone());
                spans.attempt = Some(start(tracer, "attempt", at, attributes, parent));
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
                let parent = spans
                    .attempt
                    .as_ref()
                    .or(spans.run.as_ref().map(|run| &run.cx))
                    .cloned();
                let attributes = vec![KeyValue::new("zx.turn", i64::from(turn))];
                spans.turn = Some(start(tracer, "turn", at, attributes, parent));
            }
            What::TurnEnd => end(spans.turn.take(), at),
            What::RequestStart { kind, model, attempt } => {
                end(spans.request.take().map(|request| request.cx), at);
                // A summary request belongs to its compaction.
                let parent = spans
                    .compaction
                    .as_ref()
                    .map(|compaction| &compaction.cx)
                    .or(spans.current())
                    .cloned();
                let attributes = vec![
                    KeyValue::new("zx.request.kind", name(&kind)),
                    KeyValue::new("zx.model", model.clone()),
                    KeyValue::new("zx.attempt", i64::from(attempt)),
                ];
                let cx = start(tracer, "request", at, attributes, parent);
                spans.request = Some(Live {
                    cx,
                    since: at,
                    meta: Request { kind, model },
                });
            }
            What::Response { model, usage, stop } => {
                if let Some(request) = spans.request.take() {
                    let span = request.cx.span();
                    span.set_attributes(usage_attributes(&usage));
                    span.set_attribute(KeyValue::new("zx.stop_reason", name(&stop)));
                    if stop == StopReason::Error {
                        span.set_status(Status::error("error"));
                    }
                    span.end_with_timestamp(at);
                    if let Some(instruments) = instruments {
                        let attributes = [
                            KeyValue::new("zx.request.kind", name(&request.meta.kind)),
                            KeyValue::new("zx.model", request.meta.model),
                            KeyValue::new("zx.stop_reason", name(&stop)),
                        ];
                        instruments
                            .request_duration
                            .record(seconds(request.since, at), &attributes);
                    }
                }
                if let Some(instruments) = instruments {
                    instruments.count_usage(&usage, &model);
                }
            }
            What::ToolStart { call, tool } => {
                let attributes = vec![
                    KeyValue::new("zx.tool.name", tool.clone()),
                    KeyValue::new("zx.tool.call_id", call.clone()),
                ];
                let cx = start(tracer, format!("tool {tool}"), at, attributes, spans.current().cloned());
                spans.tools.insert(
                    call,
                    Live {
                        cx,
                        since: at,
                        meta: tool,
                    },
                );
            }
            What::Decision { call, verdict } => {
                let attributes = vec![KeyValue::new("zx.verdict", name(&verdict))];
                event(spans.tool_or_current(&call), "decision", at, attributes);
            }
            What::ApprovalRequested { call } => {
                event(spans.tool_or_current(&call), "approval_requested", at, Vec::new());
            }
            What::ApprovalResolved { call, result, wait_ms } => {
                let attributes = vec![
                    KeyValue::new("zx.approval.result", name(&result)),
                    KeyValue::new("zx.approval.wait_ms", saturate(wait_ms)),
                ];
                event(spans.tool_or_current(&call), "approval_resolved", at, attributes);
                if let Some(instruments) = instruments {
                    let attributes = [KeyValue::new("zx.approval.result", name(&result))];
                    instruments
                        .approval_wait
                        .record(Duration::from_millis(wait_ms).as_secs_f64(), &attributes);
                }
            }
            What::ToolEnd { call, is_error } => {
                if let Some(tool) = spans.tools.remove(&call) {
                    let span = tool.cx.span();
                    span.set_attribute(KeyValue::new("zx.tool.is_error", is_error));
                    if is_error {
                        span.set_status(Status::error("error"));
                    }
                    span.end_with_timestamp(at);
                    if let Some(instruments) = instruments {
                        let attributes = [
                            KeyValue::new("zx.tool.name", tool.meta),
                            KeyValue::new("zx.tool.is_error", is_error),
                        ];
                        instruments.tool_duration.record(seconds(tool.since, at), &attributes);
                    }
                }
            }
            What::CompactionStart { automatic, input_bytes } => {
                end(spans.compaction.take().map(|compaction| compaction.cx), at);
                let attributes = vec![
                    KeyValue::new("zx.compaction.automatic", automatic),
                    KeyValue::new("zx.compaction.input_bytes", saturate_usize(input_bytes)),
                ];
                // A manual compaction belongs to no run, so it starts a trace of its own,
                // under the caller's span if there is one.
                let parent = spans.current().cloned().or_else(|| parent.map(remote));
                let cx = start(tracer, "compaction", at, attributes, parent);
                spans.compaction = Some(Live {
                    cx,
                    since: at,
                    meta: automatic,
                });
            }
            What::CompactionEnd { usage, output_bytes } => {
                let model = spans.request.take().map(|request| {
                    end(Some(request.cx), at);
                    request.meta.model
                });
                if let Some(compaction) = spans.compaction.take() {
                    let span = compaction.cx.span();
                    span.set_attributes(usage_attributes(&usage));
                    span.set_attribute(KeyValue::new(
                        "zx.compaction.output_bytes",
                        saturate_usize(output_bytes),
                    ));
                    span.end_with_timestamp(at);
                    if let Some(instruments) = instruments {
                        instruments.compaction_duration.record(
                            seconds(compaction.since, at),
                            &compaction_attributes(compaction.meta, "completed"),
                        );
                    }
                }
                // The summary's answer is reported here, not as a message of the run. A
                // summary without a request of its own (events out of order) has no model
                // name to count under: an empty one is still a count.
                if let Some(instruments) = instruments {
                    instruments.count_usage(&usage, model.as_deref().unwrap_or_default());
                }
            }
            What::CompactionFailed => {
                end(spans.request.take().map(|request| request.cx), at);
                if let Some(compaction) = spans.compaction.take() {
                    let span = compaction.cx.span();
                    span.set_status(Status::error("error"));
                    span.end_with_timestamp(at);
                    if let Some(instruments) = instruments {
                        instruments.compaction_duration.record(
                            seconds(compaction.since, at),
                            &compaction_attributes(compaction.meta, "failed"),
                        );
                    }
                }
            }
            What::Retry { attempt, delay_ms } => {
                let attributes = vec![
                    KeyValue::new("zx.attempt", i64::from(attempt)),
                    KeyValue::new("zx.delay_ms", saturate(delay_ms)),
                ];
                event(spans.run.as_ref().map(|run| &run.cx), "retry_scheduled", at, attributes);
                if let Some(instruments) = instruments {
                    instruments.retries.add(1, &[]);
                }
            }
            What::Overflow => event(
                spans.run.as_ref().map(|run| &run.cx),
                "context_overflow",
                at,
                Vec::new(),
            ),
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
                event(
                    spans.run.as_ref().map(|run| &run.cx),
                    "tool_call_recovered",
                    at,
                    attributes,
                );
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
            end(spans.run.take().map(|run| run.cx), at);
        }
    }
}

impl Open {
    /// The tool call's span, or the innermost open one when the call is unknown.
    fn tool_or_current(&self, call: &str) -> Option<&Context> {
        self.tools.get(call).map(|tool| &tool.cx).or(self.current())
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
    use opentelemetry::trace::SpanId;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, Metric, MetricData, ResourceMetrics};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
    use serde_json::json;

    use super::*;
    use crate::agent::{EVENT_SCHEMA_VERSION, RunReport};
    use crate::ai::{AssistantMessage, ContentBlock};

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
            .flat_map(|scope| scope.metrics())
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
                .map(|point| point.value())
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
    fn traceparents_are_checked_before_use() {
        let parent = TraceParent::parse(TRACEPARENT, None).unwrap();
        assert_eq!(parent.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parent.span_id(), "00f067aa0ba902b7");
        assert_eq!(TraceParent::parse(&format!("  {TRACEPARENT}\n"), None), Some(parent));
        for invalid in [
            "",
            "garbage",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ] {
            assert_eq!(TraceParent::parse(invalid, None), None, "{invalid:?}");
        }
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

    #[test]
    fn the_sink_drops_events_beyond_its_queue_and_counts_them() {
        let (sender, _receiver) = mpsc::channel(1);
        let sink = Spans {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            parent: None,
        };
        sink.emit(&event(1, AgentEvent::TurnStart { turn: 1 }));
        sink.emit(&event(2, AgentEvent::TurnStart { turn: 2 }));
        // Not for spans: neither queued nor counted.
        sink.emit(&event(3, AgentEvent::MessageStart { role: "assistant" }));
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
    }
}
