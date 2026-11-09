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
//!
//! Events become signals (`signal`), which one task turns into spans (`spans`) and metrics
//! (`metrics`).

mod metrics;
mod signal;
mod spans;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use opentelemetry::metrics::MeterProvider;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{SpanContext, TraceContextExt, TracerProvider};
use opentelemetry_otlp::{MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::EventSink;
use metrics::Instruments;
use signal::{Signal, SignalSink};
use spans::{Runs, export};

/// Either variable turns the trace export on. As the OTLP specification has it, the first is
/// used as it is, and the second gets `/v1/traces` appended.
const ENDPOINT_VARIABLES: [&str; 2] = ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"];
/// Either variable turns the metric export on, the same way, with `/v1/metrics` appended to
/// the second.
const METRIC_ENDPOINT_VARIABLES: [&str; 2] = ["OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"];
/// Variables that may carry credentials for the collector, such as an authorization header.
/// Commands must not inherit them.
pub const HEADER_VARIABLES: [&str; 3] = [
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
];
/// The W3C trace context a caller passes in the environment, as `otel-cli` and the scripts
/// it instruments do: the value of a `traceparent` header, and the optional `tracestate`.
const TRACE_CONTEXT_VARIABLES: [&str; 2] = ["TRACEPARENT", "TRACESTATE"];
/// Milliseconds between metric exports, as the SDK specification names it; unset uses the
/// specification's default of a minute. What is buffered is also exported at shutdown.
const METRIC_INTERVAL_VARIABLE: &str = "OTEL_METRIC_EXPORT_INTERVAL";
const DEFAULT_METRIC_INTERVAL: Duration = Duration::from_secs(60);
/// Events waiting for the exporter task. A run never waits for telemetry: once the queue is
/// full, new events are dropped and counted.
const QUEUE_CAPACITY: usize = 4096;
/// How long shutdown waits for the collector: the SDK's own default.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// `service.name`, unless `OTEL_SERVICE_NAME` sets one.
const SERVICE_NAME: &str = "zx-agent";

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
}

/// What goes out, with the meter provider where metrics do. Spans are built either way,
/// since the metrics are recorded as they end, so the tracer provider is always there.
enum Exporters {
    Traces,
    Metrics(SdkMeterProvider),
    TracesAndMetrics(SdkMeterProvider),
}

impl Exporters {
    fn exports(&self) -> Exports {
        match self {
            Self::Traces => Exports::Traces,
            Self::Metrics(_) => Exports::Metrics,
            Self::TracesAndMetrics(_) => Exports::TracesAndMetrics,
        }
    }

    fn meter_provider(&self) -> Option<&SdkMeterProvider> {
        match self {
            Self::Traces => None,
            Self::Metrics(provider) | Self::TracesAndMetrics(provider) => Some(provider),
        }
    }
}

/// A W3C trace context received from the caller: the values of the `traceparent` and
/// `tracestate` headers, or of `TRACEPARENT` and `TRACESTATE` in the environment. Runs emitted
/// through [`Telemetry::sink_under`] continue that trace, under the caller's span.
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
    exporters: Exporters,
    sender: mpsc::Sender<Signal>,
    dropped: Arc<AtomicU64>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Telemetry {
    /// Export to the endpoints that the standard variables name.
    ///
    /// Traces go out when `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT`
    /// is set, metrics when `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT`
    /// is; `None` when neither is: nothing is then sent anywhere. The SDK reads the other
    /// standard variables, such as `OTEL_EXPORTER_OTLP_HEADERS`. Needs a Tokio runtime. An error
    /// is a configuration that the exporter refuses, such as an invalid endpoint or interval.
    pub fn from_env() -> Result<Option<Self>, String> {
        let mut resource = Resource::builder();
        if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
            resource = resource.with_service_name(SERVICE_NAME);
        }
        let resource = resource.build();
        let exporters = match (any_set(&ENDPOINT_VARIABLES), any_set(&METRIC_ENDPOINT_VARIABLES)) {
            (true, true) => Exporters::TracesAndMetrics(meter_provider(resource.clone())?),
            (true, false) => Exporters::Traces,
            (false, true) => Exporters::Metrics(meter_provider(resource.clone())?),
            (false, false) => return Ok(None),
        };
        let mut tracer_provider = SdkTracerProvider::builder().with_resource(resource);
        if exporters.exports().traces() {
            let exporter = SpanExporter::builder()
                .with_http()
                .build()
                .map_err(|error| format!("cannot set up the OTLP span exporter: {error}"))?;
            tracer_provider = tracer_provider.with_batch_exporter(exporter);
        }
        Ok(Some(Self::new(tracer_provider.build(), exporters)))
    }

    /// Export traces through `provider`, such as one with an exporter that the host set up,
    /// and no metrics. Needs a Tokio runtime.
    pub fn with_provider(provider: SdkTracerProvider) -> Self {
        Self::new(provider, Exporters::Traces)
    }

    /// Export traces and metrics through the host's providers. Needs a Tokio runtime.
    pub fn with_providers(tracer: SdkTracerProvider, meter: SdkMeterProvider) -> Self {
        Self::new(tracer, Exporters::TracesAndMetrics(meter))
    }

    fn new(tracer_provider: SdkTracerProvider, exporters: Exporters) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let stop = CancellationToken::new();
        let instruments = exporters
            .meter_provider()
            .map(|provider| Instruments::new(&provider.meter(SERVICE_NAME)));
        let runs = Runs::new(tracer_provider.tracer(SERVICE_NAME), instruments);
        let task = tokio::spawn(export(receiver, runs, stop.clone()));
        Self {
            tracer_provider,
            exporters,
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            stop,
            task,
        }
    }

    /// What goes out.
    pub fn exports(&self) -> Exports {
        self.exporters.exports()
    }

    /// The sink to give a session: it passes each event on without waiting. Every run starts
    /// a trace of its own.
    pub fn sink(&self) -> Arc<dyn EventSink> {
        Arc::new(self.signal_sink(None))
    }

    /// A sink whose runs continue the caller's trace: each `run` span, and each manual
    /// compaction, is a child of the span that `parent` names.
    pub fn sink_under(&self, parent: &TraceParent) -> Arc<dyn EventSink> {
        Arc::new(self.signal_sink(Some(parent.context.clone())))
    }

    fn signal_sink(&self, parent: Option<SpanContext>) -> SignalSink {
        SignalSink {
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
    /// at most `SHUTDOWN_TIMEOUT` for the collector each time.
    ///
    /// Events emitted afterwards are dropped.
    pub async fn shutdown(self) -> Result<(), String> {
        let Self {
            tracer_provider,
            exporters,
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
            if let Some(meter_provider) = exporters.meter_provider() {
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

/// A meter provider that exports over OTLP/HTTP at the configured interval.
fn meter_provider(resource: Resource) -> Result<SdkMeterProvider, String> {
    let interval = metric_interval_from_env()?;
    let exporter = MetricExporter::builder()
        .with_http()
        .build()
        .map_err(|error| format!("cannot set up the OTLP metric exporter: {error}"))?;
    let reader = PeriodicReader::builder(exporter).with_interval(interval).build();
    Ok(SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build())
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

#[cfg(test)]
mod tests {
    use super::*;

    const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

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
}
