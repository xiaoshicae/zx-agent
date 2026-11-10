//! The OpenTelemetry export of real runs: one trace per run, with a span for each attempt,
//! turn, model request and tool call, and a run that continues the caller's trace.
mod common;

use std::future::Future;
use std::sync::{Arc, Mutex};

use opentelemetry_sdk::error::OTelSdkResult;
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
