//! The metrics, recorded as the spans they measure end: runs, requests, tool calls and
//! summaries counted and timed, tokens and cost added up.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::ai::Usage;

/// Histogram buckets for durations in seconds: from a quick tool call to an hour-long run.
const DURATION_BOUNDARIES: [f64; 15] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

/// The instruments; durations are in seconds, as the semantic conventions have them.
pub(super) struct Instruments {
    /// Runs that ended, by outcome and model.
    pub(super) runs: Counter<u64>,
    pub(super) run_duration: Histogram<f64>,
    /// By request kind, model and stop reason.
    pub(super) request_duration: Histogram<f64>,
    /// Token usage the model reported, by model and kind: `input`, `output`, `cache_read`,
    /// `cache_write` and `reasoning` (counted within `output`).
    tokens: Counter<u64>,
    /// Cost by the configured prices, in USD, by model.
    cost: Counter<f64>,
    /// By tool name and whether the result was an error.
    pub(super) tool_duration: Histogram<f64>,
    pub(super) retries: Counter<u64>,
    /// By whether the summary was automatic, and its outcome.
    pub(super) compaction_duration: Histogram<f64>,
    /// How long a tool call waited for a person, by the answer.
    pub(super) approval_wait: Histogram<f64>,
}

impl Instruments {
    pub(super) fn new(meter: &Meter) -> Self {
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

    /// Add `usage` to the token counters of `model`, and its cost when it has one.
    pub(super) fn count_usage(&self, usage: &Usage, model: &str) {
        self.count(usage, KeyValue::new("zx.model", model.to_string()));
    }

    /// Add what the tool `tool` spent on work of its own to the token and cost counters,
    /// under `zx.tool.name` instead of a model.
    pub(super) fn count_tool_usage(&self, usage: &Usage, tool: &str) {
        self.count(usage, KeyValue::new("zx.tool.name", tool.to_string()));
    }

    /// Add `usage` to the token counters, and to the cost counter when it has a cost, under
    /// `by`, the model or tool it belongs to.
    fn count(&self, usage: &Usage, by: KeyValue) {
        let counts = [
            ("input", usage.input),
            ("output", usage.output),
            ("cache_read", usage.cache_read),
            ("cache_write", usage.cache_write),
            ("reasoning", usage.reasoning),
        ];
        for (kind, count) in counts {
            if count > 0 {
                let attributes = [by.clone(), KeyValue::new("zx.token.kind", kind)];
                self.tokens.add(count, &attributes);
            }
        }
        if let Some(cost) = usage.cost_usd {
            self.cost.add(cost, &[by]);
        }
    }
}
