//! The model's answer to a summary request: bounded as it streams, parsed into a summary,
//! and merged with the facts an earlier summary carries.

use anyhow::{Result, ensure};
use serde::Deserialize;

use crate::ai::{AssistantMessage, ContentBlock, Delta, StopReason};
use crate::session::{
    CompactionSummary, MAX_STATEMENTS, MAX_SUMMARY_BYTES, MergedSummary, SummaryFact, cite_only, superseded_by,
};

/// Reasoning a summary response may carry before its answer. Reasoning is not part of the
/// summary, so it does not count toward `MAX_SUMMARY_BYTES`; this separate, larger bound only
/// stops reasoning that does not end.
const MAX_SUMMARY_REASONING_BYTES: usize = 1024 * 1024;

/// What the summary model returns. Its facts have no IDs: the host numbers them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ModelSummary {
    facts: Vec<ModelFact>,
    inferences: Vec<String>,
    plans: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ModelFact {
    text: String,
    source_entry_ids: Vec<String>,
    /// IDs of earlier facts this one replaces; only an incremental summary has earlier facts.
    #[serde(default)]
    supersedes: Vec<String>,
}

/// A full summary from the model's answer: its facts have no IDs and supersede nothing, and
/// every fact cites one of `sources`.
pub(super) fn full_summary(answer: ModelSummary, sources: &[String]) -> Result<CompactionSummary> {
    let mut facts = Vec::with_capacity(answer.facts.len());
    for fact in answer.facts {
        ensure!(
            fact.supersedes.is_empty(),
            "a full summary has no earlier facts to supersede"
        );
        facts.push(SummaryFact {
            id: None,
            text: fact.text,
            source_entry_ids: fact.source_entry_ids,
            supersedes: Vec::new(),
        });
    }
    let summary = CompactionSummary {
        facts,
        inferences: answer.inferences,
        plans: answer.plans,
    };
    summary.validate(sources)?;
    Ok(summary)
}

/// An incremental summary from the model's answer: the `previous` facts that the new ones do
/// not supersede, without the oldest of them when the result would exceed the limits, then
/// the new facts, numbered `<attempt>-1`, `<attempt>-2`, ... and citing only `sources`.
pub(super) fn merge(
    attempt: &str,
    previous: Vec<SummaryFact>,
    answer: ModelSummary,
    sources: &[String],
) -> Result<MergedSummary> {
    let new: Vec<SummaryFact> = answer
        .facts
        .into_iter()
        .enumerate()
        .map(|(index, fact)| SummaryFact {
            id: Some(format!("{attempt}-{}", index + 1)),
            text: fact.text,
            source_entry_ids: fact.source_entry_ids,
            supersedes: fact.supersedes,
        })
        .collect();
    cite_only(&new, sources)?;
    let superseded = superseded_by(&previous, &new)?;
    let carried: Vec<SummaryFact> = previous
        .into_iter()
        .filter(|fact| !fact.id.as_ref().is_some_and(|id| superseded.contains(id)))
        .collect();
    let carried_count = carried.len();
    let mut summary = CompactionSummary {
        facts: carried,
        inferences: answer.inferences,
        plans: answer.plans,
    };
    summary.facts.extend(new);
    let mut evicted = Vec::new();
    // While earlier facts remain at the front, the oldest goes first; its record stays in the
    // earlier summary.
    while evicted.len() < carried_count
        && (summary.facts.len() > MAX_STATEMENTS || serde_json::to_vec(&summary)?.len() > MAX_SUMMARY_BYTES)
    {
        let oldest = summary.facts.remove(0);
        evicted.push(oldest.id.expect("the earlier facts of a summary are numbered"));
    }
    summary.validate_limits()?;
    Ok(MergedSummary {
        summary,
        superseded,
        evicted,
    })
}

/// The model's answer as a summary, or why it is not one.
pub(super) fn parse_answer(response: &Result<AssistantMessage, String>) -> Result<ModelSummary, String> {
    let message = response.as_ref().map_err(Clone::clone)?;
    match message.stop_reason {
        StopReason::Stop if message.tool_calls().is_empty() => serde_json::from_str::<ModelSummary>(&message.text())
            .map_err(|error| format!("invalid summary JSON: {error}")),
        StopReason::Stop => Err("summary contains a tool call; tools are not permitted".into()),
        StopReason::Length => Err("summary reached the output token limit".into()),
        StopReason::ToolUse => Err("summary requested a tool call; tools are not permitted".into()),
        StopReason::Error | StopReason::Aborted => Err(message.failure_reason().to_string()),
    }
}

/// What a summary response holds, counted the way its limits are stated: the answer's text,
/// with any tool call or error message, toward `MAX_SUMMARY_BYTES`, and the reasoning toward
/// `MAX_SUMMARY_REASONING_BYTES`. The encoding that carries them is not counted: a server
/// that streams about a token per event would otherwise reach the limit with a fraction of
/// the text.
#[derive(Default)]
pub(super) struct ResponseBytes {
    answer: usize,
    reasoning: usize,
}

impl ResponseBytes {
    /// Count a streamed delta; fails once a limit is exceeded.
    pub(super) fn add(&mut self, delta: &Delta) -> Result<(), String> {
        match delta {
            Delta::Text { text } | Delta::ToolCallArgs { text, .. } => {
                self.answer = self.answer.saturating_add(text.len());
            }
            Delta::ToolCallStart { id, name } => {
                self.answer = self.answer.saturating_add(id.len()).saturating_add(name.len());
            }
            Delta::Thinking { text } => self.reasoning = self.reasoning.saturating_add(text.len()),
        }
        self.validate()
    }

    /// Count the content of a final message.
    pub(super) fn of(message: &AssistantMessage) -> Self {
        let mut bytes = Self::default();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => bytes.answer = bytes.answer.saturating_add(text.len()),
                ContentBlock::ToolCall {
                    id, name, arguments, ..
                } => {
                    let call = id.len() + name.len() + arguments.to_string().len();
                    bytes.answer = bytes.answer.saturating_add(call);
                }
                ContentBlock::Thinking { thinking, .. } => {
                    bytes.reasoning = bytes.reasoning.saturating_add(thinking.len());
                }
            }
        }
        // A failed response is recorded with its error message.
        if let Some(error) = &message.error_message {
            bytes.answer = bytes.answer.saturating_add(error.len());
        }
        bytes
    }

    /// Whether the counts are within their limits.
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.answer > MAX_SUMMARY_BYTES {
            return Err(format!("summary text exceeds {MAX_SUMMARY_BYTES} bytes"));
        }
        if self.reasoning > MAX_SUMMARY_REASONING_BYTES {
            return Err(format!("summary reasoning exceeds {MAX_SUMMARY_REASONING_BYTES} bytes"));
        }
        Ok(())
    }
}
