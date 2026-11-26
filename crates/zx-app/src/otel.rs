//! OpenTelemetry traces of runs, exported with OTLP over HTTP (protobuf) when the standard
//! variables name an endpoint.
//!
//! One run is one trace: a `run` span, an `attempt` span for each try, `step` spans, and
//! spans for model requests, tool calls and summaries, timed by the events' `atMs`. A run
//! continues the caller's trace when its hook was made with a [`TraceParent`]. Token usage
//! and cost are attributes of the spans they belong to; a collector derives metrics from the
//! spans. Only metadata goes out: IDs, names, counts, token usage, durations and outcomes.
//! User input, model output, tool arguments and results never do, nor do reasons and error
//! texts, which may quote them.
//!
//! A hook observes the events of every run and turns them into signals (`signal`), which
//! one task turns into spans (`spans`). What their tests share is in `test_support`, compiled
//! for tests only.

mod signal;
mod spans;
#[cfg(test)]
mod test_support;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{SpanContext, TraceContextExt, TracerProvider};
use opentelemetry::{Key, Value};
use opentelemetry_otlp::{
    OTEL_EXPORTER_OTLP_TIMEOUT, OTEL_EXPORTER_OTLP_TIMEOUT_DEFAULT, OTEL_EXPORTER_OTLP_TRACES_TIMEOUT, SpanExporter,
    WithHttpConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::resource::{EnvResourceDetector, ResourceDetector};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zx_agent::ai::Proxy;
use zx_ai_http::client::routed;

use self::signal::Signal;
pub use self::signal::TraceHook;
use self::spans::{Runs, export};
use crate::config::env_value;

/// Either variable turns the trace export on. As the OTLP specification has it, the first is
/// used as it is, and the second gets `/v1/traces` appended.
const ENDPOINT_ENVS: [&str; 2] = ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"];
/// Variables that may carry credentials for a collector, such as an authorization header.
/// Commands must not inherit them, the metrics one included: this process exports no
/// metrics, but the variable may be set for others.
pub(crate) const HEADER_ENVS: [&str; 3] = [
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
];
/// The W3C trace context a caller passes in the environment, as `otel-cli` and the scripts
/// it instruments do: the value of a `traceparent` header, and the optional `tracestate`.
const TRACE_CONTEXT_ENVS: [&str; 2] = ["TRACEPARENT", "TRACESTATE"];
/// Events waiting for the exporter task. A run never waits for telemetry: once the queue is
/// full, new events are dropped and counted.
const QUEUE_CAPACITY: usize = 4096;
/// How long shutdown waits for the collector: the SDK's own default.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// The standard variable that names the service.
const SERVICE_NAME_ENV: &str = "OTEL_SERVICE_NAME";
/// The resource attribute that names the service, which `OTEL_RESOURCE_ATTRIBUTES` may hold.
const SERVICE_NAME_ATTRIBUTE: &str = "service.name";
/// The tracer's name, the instrumentation scope of every span: the agent's runs, whichever
/// host exports them.
const TRACER_NAME: &str = "zx-agent";

/// A W3C trace context received from the caller: the values of the `traceparent` and
/// `tracestate` headers, or of `TRACEPARENT` and `TRACESTATE` in the environment. Runs observed
/// through [`Telemetry::hook_under`] continue that trace, under the caller's span.
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

    /// From `TRACEPARENT` and `TRACESTATE`: `Ok(None)` when `TRACEPARENT` is unset or blank,
    /// an error naming the variable when it is set but not a valid header value.
    pub fn from_env() -> Result<Option<Self>> {
        let [parent, state] = TRACE_CONTEXT_ENVS.map(|name| env_value(name).filter(|value| !value.trim().is_empty()));
        let Some(parent) = parent else {
            return Ok(None);
        };
        Self::parse(&parent, state.as_deref())
            .map(Some)
            .ok_or_else(|| anyhow!("{} is not a W3C traceparent: {parent:?}", TRACE_CONTEXT_ENVS[0]))
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

/// Exports the events of runs as OpenTelemetry traces. Register [`hook`](Self::hook) with an
/// agent's hooks, which observe every session of it, and call [`shutdown`](Self::shutdown)
/// before the process exits.
pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    sender: mpsc::Sender<Signal>,
    dropped: Arc<AtomicU64>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Telemetry {
    /// Export to the endpoint that the standard variables name, as the service `service_name`:
    /// the host's name, which tells the traces of different programs apart, unless the
    /// environment names the service: `OTEL_SERVICE_NAME`, which the specification puts first,
    /// or else `service.name` in `OTEL_RESOURCE_ATTRIBUTES`.
    ///
    /// Traces go out when `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT`
    /// is set and not empty; `None` when neither is: nothing is then sent anywhere. The SDK reads the other
    /// standard variables, such as `OTEL_EXPORTER_OTLP_HEADERS`. Requests go through `proxy`,
    /// config.toml's ([`Config::proxy`](crate::config::Config::proxy)). Needs a Tokio
    /// runtime. An error is a configuration that the exporter refuses, such as an invalid
    /// endpoint, or a proxy that is not the `http://` URL of one.
    pub fn from_env(service_name: &str, proxy: &Proxy) -> Result<Option<Self>> {
        if ENDPOINT_ENVS.iter().all(|name| env_value(name).is_none()) {
            return Ok(None);
        }
        let exporter = exporter(proxy)?;
        let tracer_provider = SdkTracerProvider::builder()
            .with_resource(resource(service_name))
            .with_batch_exporter(exporter)
            .build();
        Ok(Some(Self::with_provider(tracer_provider)))
    }

    /// Export traces through `tracer_provider`, such as one with an exporter that the host set
    /// up. Needs a Tokio runtime.
    pub fn with_provider(tracer_provider: SdkTracerProvider) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let stop = CancellationToken::new();
        let runs = Runs::new(tracer_provider.tracer(TRACER_NAME));
        let task = tokio::spawn(export(receiver, runs, stop.clone()));
        Self {
            tracer_provider,
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            stop,
            task,
        }
    }

    /// The hook to register with an agent: it observes the runs of the agent's sessions and
    /// passes each event on without waiting. Every run starts a trace of its own.
    pub fn hook(&self) -> TraceHook {
        self.trace_hook(None)
    }

    /// A hook whose runs continue the caller's trace: each `run` span, and each manual
    /// compaction, is a child of the span that `parent` names.
    pub fn hook_under(&self, parent: &TraceParent) -> TraceHook {
        self.trace_hook(Some(parent.context.clone()))
    }

    fn trace_hook(&self, parent: Option<SpanContext>) -> TraceHook {
        TraceHook {
            sender: self.sender.clone(),
            dropped: Arc::clone(&self.dropped),
            parent,
        }
    }

    /// Events dropped because the queue was full, or because the export had stopped.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// End the spans still open and export what is buffered, waiting at most
    /// `SHUTDOWN_TIMEOUT` for the collector.
    ///
    /// Events emitted afterwards are dropped.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            tracer_provider,
            stop,
            task,
            ..
        } = self;
        stop.cancel();
        task.await.context("the span task failed")?;
        // Shutting down blocks until the last batch is sent or the time is up.
        tokio::task::spawn_blocking(move || {
            tracer_provider
                .shutdown_with_timeout(SHUTDOWN_TIMEOUT)
                .context("cannot export the remaining spans")
        })
        .await
        .context("the export failed")?
    }
}

/// The span exporter of the endpoint the standard variables name, sending through `proxy`.
/// The client the exporter makes itself sends through the environment's proxies; another
/// proxy takes a client of the same kind made here.
fn exporter(proxy: &Proxy) -> Result<SpanExporter> {
    const SET_UP: &str = "cannot set up the OTLP span exporter";
    let builder = SpanExporter::builder().with_http();
    let route = match proxy {
        Proxy::FromEnvironment => return builder.build().context(SET_UP),
        Proxy::Direct => None,
        Proxy::Through(proxies) => Some(routed(proxies).map_err(|error| anyhow!("[proxy]: {error}"))?),
    };
    builder
        .with_http_client(blocking_client(route)?)
        .build()
        .context(SET_UP)
}

/// The blocking client the exporter would make itself, with the same timeout, sending through
/// `route`, or directly without one. Made on a thread of its own, as the exporter makes
/// its client: a blocking client cannot be made on a thread that runs async tasks.
fn blocking_client(route: Option<reqwest::Proxy>) -> Result<reqwest::blocking::Client> {
    let timeout = export_timeout();
    std::thread::spawn(move || {
        let builder = reqwest::blocking::Client::builder().timeout(timeout).no_proxy();
        match route {
            Some(route) => builder.proxy(route),
            None => builder,
        }
        .build()
    })
    .join()
    // The panic hook has already said why.
    .map_err(|_| anyhow!("the thread making the trace export's HTTP client panicked"))?
    .context("cannot build the trace export's HTTP client")
}

/// How long one export request may take: the standard variables, in milliseconds, as the
/// exporter reads them for its own client; otherwise its default.
fn export_timeout() -> Duration {
    [OTEL_EXPORTER_OTLP_TRACES_TIMEOUT, OTEL_EXPORTER_OTLP_TIMEOUT]
        .iter()
        // A value that is not a number is passed over, as the exporter does.
        .find_map(|name| env_value(name)?.parse().ok())
        .map_or(OTEL_EXPORTER_OTLP_TIMEOUT_DEFAULT, Duration::from_millis)
}

/// The resource that the traces come from: the SDK's, with its own attributes and those in
/// `OTEL_RESOURCE_ATTRIBUTES`, under the service name that [`Telemetry::from_env`] describes,
/// `default_name` when the environment gives none. Left to itself, the SDK puts `service.name`
/// in that variable over `OTEL_SERVICE_NAME`, and names an unnamed service
/// `unknown_service:<executable>`.
fn resource(default_name: &str) -> Resource {
    let named = env_value(SERVICE_NAME_ENV).map(Value::from).or_else(|| {
        EnvResourceDetector::new()
            .detect()
            .get(&Key::from_static_str(SERVICE_NAME_ATTRIBUTE))
    });
    Resource::builder()
        .with_service_name(named.unwrap_or_else(|| Value::from(default_name.to_string())))
        .build()
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
