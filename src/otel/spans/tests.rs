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
        entry_id: "e".into(),
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
                entry_id: "r".into(),
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
                outcome: RunOutcome::Completed,
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
    spent.total_cost = Some(0.5);
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
    assert_eq!(attribute(tool, "zx.usage.total_cost").as_deref(), Some("0.5"));
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
                entry_id: "r".into(),
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
                entry_id: "r".into(),
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
