//! The OpenTelemetry export of real runs: one trace per run, with a span for each attempt,
//! turn, model request and tool call; the metrics the same events feed; and a run that
//! continues the caller's trace.
mod common;

use std::future::Future;
use std::sync::{Arc, Mutex};

use opentelemetry::KeyValue;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use common::{EchoTool, RunnerSetup, config};
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::otel::{Telemetry, TraceParent};
use zx_agent::runner::RunOutcome;

/// Keeps every exported span, also after shutdown, which the SDK's in-memory exporter clears.
#[derive(Clone, Debug, Default)]
struct Kept(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Kept {
    fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        self.0.lock().unwrap().extend(batch);
        std::future::ready(Ok(()))
    }
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| attribute.value.as_str().into_owned())
}

#[tokio::test]
async fn a_run_exports_one_trace_with_a_span_per_request_and_tool_call() {
    let exporter = Kept::default();
    let telemetry = Telemetry::with_provider(
        SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build(),
    );
    // Each faux answer reports 10 input and 5 output tokens.
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "ok"})),
        FauxStep::text("done"),
    ]));
    let mut runner =
        RunnerSetup::new(config(provider, vec![Arc::new(EchoTool::new("echo"))])).build_with(telemetry.sink());

    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    telemetry.shutdown().await.unwrap();

    let spans = exporter.0.lock().unwrap().clone();
    let mut names: Vec<&str> = spans.iter().map(|span| span.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["attempt", "request", "request", "run", "tool echo", "turn", "turn"]
    );
    let run = spans.iter().find(|span| span.name == "run").unwrap();
    assert!(
        spans
            .iter()
            .all(|span| span.span_context.trace_id() == run.span_context.trace_id())
    );
    assert_eq!(attribute(run, "zx.outcome").as_deref(), Some("completed"));
    assert_eq!(attribute(run, "zx.usage.input_tokens").as_deref(), Some("20"));
    assert_eq!(attribute(run, "zx.usage.output_tokens").as_deref(), Some("10"));
    let tool = spans.iter().find(|span| span.name == "tool echo").unwrap();
    assert_eq!(attribute(tool, "zx.tool.call_id").as_deref(), Some("c1"));
    assert_eq!(attribute(tool, "zx.tool.is_error").as_deref(), Some("false"));
}

/// The sum of a `u64` counter's points that carry `expected`; 0 when nothing was recorded.
fn counted(all: &[ResourceMetrics], name: &str, expected: &[(&str, &str)]) -> u64 {
    points(all, name)
        .into_iter()
        .filter(|(attributes, _)| {
            expected
                .iter()
                .all(|pair| attributes.contains(&(pair.0.into(), pair.1.into())))
        })
        .map(|(_, value)| value)
        .sum()
}

/// Each point of a `u64` sum or an `f64` histogram as its attributes and its value or count.
fn points(all: &[ResourceMetrics], name: &str) -> Vec<(Vec<(String, String)>, u64)> {
    let attributes = |attributes: &mut dyn Iterator<Item = &KeyValue>| -> Vec<(String, String)> {
        attributes
            .map(|attribute| (attribute.key.to_string(), attribute.value.as_str().into_owned()))
            .collect()
    };
    all.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|metric| metric.name() == name)
        .flat_map(|metric| match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                .data_points()
                .map(|point| (attributes(&mut point.attributes()), point.value()))
                .collect::<Vec<_>>(),
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                .data_points()
                .map(|point| (attributes(&mut point.attributes()), point.count()))
                .collect(),
            other => panic!("{name} has unexpected data: {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_run_exports_its_metrics_through_the_meter_provider() {
    let spans = Kept::default();
    let metrics = InMemoryMetricExporter::default();
    let telemetry = Telemetry::with_providers(
        SdkTracerProvider::builder().with_simple_exporter(spans.clone()).build(),
        SdkMeterProvider::builder()
            .with_periodic_exporter(metrics.clone())
            .build(),
    );
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "echo", json!({"text": "ok"})),
        FauxStep::text("done"),
    ]));
    let mut runner =
        RunnerSetup::new(config(provider, vec![Arc::new(EchoTool::new("echo"))])).build_with(telemetry.sink());

    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    // Shutting down exports what the periodic reader has not sent yet.
    telemetry.shutdown().await.unwrap();

    let all = metrics.get_finished_metrics().unwrap();
    assert_eq!(counted(&all, "zx.runs", &[("zx.outcome", "completed")]), 1);
    assert_eq!(counted(&all, "zx.tokens", &[("zx.token.kind", "input")]), 20);
    assert_eq!(counted(&all, "zx.tokens", &[("zx.token.kind", "output")]), 10);
    assert_eq!(counted(&all, "zx.request.duration", &[("zx.request.kind", "task")]), 2);
    assert_eq!(
        counted(
            &all,
            "zx.tool.duration",
            &[("zx.tool.name", "echo"), ("zx.tool.is_error", "false")]
        ),
        1
    );
    assert_eq!(counted(&all, "zx.run.duration", &[]), 1);
    assert!(!format!("{all:?}").contains("done"), "no content leaves the process");
}

#[tokio::test]
async fn a_run_continues_the_callers_trace_through_sink_under() {
    let exporter = Kept::default();
    let telemetry = Telemetry::with_provider(
        SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build(),
    );
    let parent = TraceParent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", None).unwrap();
    let provider = Arc::new(FauxProvider::new([FauxStep::text("done")]));
    let mut runner = RunnerSetup::new(config(provider, Vec::new())).build_with(telemetry.sink_under(&parent));

    let report = runner.run("go", &CancellationToken::new()).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    telemetry.shutdown().await.unwrap();

    let spans = exporter.0.lock().unwrap().clone();
    let run = spans.iter().find(|span| span.name == "run").unwrap();
    assert_eq!(run.span_context.trace_id().to_string(), parent.trace_id());
    assert_eq!(run.parent_span_id.to_string(), parent.span_id());
    assert!(
        spans
            .iter()
            .all(|span| span.span_context.trace_id() == run.span_context.trace_id()),
        "{spans:?}"
    );
}
