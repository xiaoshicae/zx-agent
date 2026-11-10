//! What a summary reads: the records before its boundary and, for an incremental summary,
//! the facts it carries over; and the two messages of its request, built the same way
//! when it is sent and when it is replayed from the session.

use anyhow::{Result, ensure};
use serde::Serialize;

use super::{SummaryFact, SummaryRequest};
use crate::ai::{Message, PREAMBLE, SystemMessage, SystemPrompt, UserMessageKind};
use crate::session::{Entry, ProjectedMessage, SessionStore, without_details};

impl SummaryRequest {
    /// The messages of the request as it was sent: the instructions as the system prompt,
    /// `input` as the user message.
    pub(in crate::session) fn messages(
        &self,
        input: String,
        system_timestamp: u64,
        user_timestamp: u64,
    ) -> Vec<Message> {
        summary_messages(&self.instructions, input, system_timestamp, user_timestamp)
    }
}

/// The two messages of a summary request, built the same way when it is sent and when it is
/// replayed from the session.
pub(crate) fn summary_messages(
    instructions: &str,
    input: String,
    system_timestamp: u64,
    user_timestamp: u64,
) -> Vec<Message> {
    vec![
        Message::System(SystemMessage {
            sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section(PREAMBLE, instructions)),
            timestamp: system_timestamp,
        }),
        Message::User {
            content: input,
            timestamp: user_timestamp,
            kind: UserMessageKind::Prompt,
        },
    ]
}

/// The content of a full summary request: the records it reads, without tool result
/// details. The same when the request is sent and when it is replayed.
pub(crate) fn summary_input(records: &[ProjectedMessage]) -> Result<String> {
    Ok(serde_json::to_string(&without_details(records))?)
}

/// What an incremental summary that continues an earlier one reads: that summary's facts
/// that still hold, and the records to summarize.
pub(crate) struct IncrementalSource {
    pub previous_facts: Vec<SummaryFact>,
    pub records: Vec<ProjectedMessage>,
}

/// One earlier fact as the summary model sees it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviousFact<'a> {
    id: &'a str,
    text: &'a str,
    source_entry_ids: &'a [String],
}

/// The content of an incremental summary request.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IncrementalInput<'a> {
    previous_facts: Vec<PreviousFact<'a>>,
    records: &'a [ProjectedMessage],
}

impl IncrementalSource {
    /// The request content: `{"previousFacts": [...], "records": [...]}`. Every previous
    /// fact has an ID: the effective facts of a summary are always numbered.
    pub(crate) fn input(&self) -> Result<String> {
        let previous_facts = self
            .previous_facts
            .iter()
            .map(|fact| PreviousFact {
                id: fact
                    .id
                    .as_deref()
                    .expect("the effective facts of a summary are numbered"),
                text: &fact.text,
                source_entry_ids: &fact.source_entry_ids,
            })
            .collect();
        Ok(serde_json::to_string(&IncrementalInput {
            previous_facts,
            records: &without_details(&self.records),
        })?)
    }
}

impl SessionStore {
    /// Where the retained messages start in `raw`: at `first_kept`, which must not split a
    /// tool exchange or be a system message.
    pub(in crate::session) fn retained_from(raw: &[ProjectedMessage], first_kept: &str) -> Result<usize> {
        let cut = raw
            .iter()
            .position(|item| item.entry_id == first_kept)
            .ok_or_else(|| anyhow::anyhow!("compaction retained entry not on branch"))?;
        ensure!(
            !matches!(raw[cut].message, Message::ToolResult(_) | Message::System(_)),
            "compaction cannot split a tool exchange or retain from a system message"
        );
        Ok(cut)
    }

    /// The records before `first_kept`: what a full summary read.
    pub(in crate::session) fn summarized_prefix<'a>(
        raw: &'a [ProjectedMessage],
        first_kept: &str,
    ) -> Result<&'a [ProjectedMessage]> {
        let cut = Self::retained_from(raw, first_kept)?;
        Ok(&raw[..cut])
    }

    /// What an incremental summary at the end of `path` reads when it continues `previous`,
    /// the latest summary on the path, and keeps the records from `first_kept` on: the facts
    /// of `previous` that still hold, then the records it summarized that were corrected
    /// since and are still visible, then the records after its boundary. `None` when no
    /// message other than a system message follows the previous boundary: nothing to add.
    pub(crate) fn incremental_source_at(
        &self,
        path: &[&Entry],
        previous: &str,
        first_kept: &str,
    ) -> Result<Option<IncrementalSource>> {
        let latest = self
            .latest_compaction_on(path)
            .ok_or_else(|| anyhow::anyhow!("there is no summary to continue on this branch"))?;
        ensure!(
            latest.compaction.id == previous,
            "an incremental summary continues the latest summary on its branch"
        );
        let raw = Self::raw_projection_from(path);
        let old_cut = Self::summarized_count(path, &raw, &latest.compaction.first_kept_entry_id)?;
        let new_cut = Self::retained_from(&raw, first_kept)?;
        if new_cut <= old_cut
            || !raw[old_cut..new_cut]
                .iter()
                .any(|item| !matches!(item.message, Message::System(_)))
        {
            return Ok(None);
        }
        let corrected = Self::corrected_after(path, latest.position);
        let records = raw[..old_cut]
            .iter()
            .filter(|item| corrected.contains(item.entry_id.as_str()))
            .chain(&raw[old_cut..new_cut])
            .cloned()
            .collect();
        Ok(Some(IncrementalSource {
            previous_facts: Self::effective_facts(path, &latest),
            records,
        }))
    }

    /// [`Self::incremental_source_at`] for a recorded attempt, which must add something.
    pub(in crate::session) fn recorded_incremental_source(
        &self,
        path: &[&Entry],
        previous: &str,
        first_kept: &str,
    ) -> Result<IncrementalSource> {
        self.incremental_source_at(path, previous, first_kept)?
            .ok_or_else(|| anyhow::anyhow!("no new records to summarize after the previous summary"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::result_with_details;

    #[test]
    fn incremental_summary_input_leaves_details_out() {
        let source = IncrementalSource {
            previous_facts: Vec::new(),
            records: vec![ProjectedMessage {
                entry_id: "e1".into(),
                message: result_with_details(Some(serde_json::json!({"marker": "for-observers"}))),
            }],
        };
        let input = source.input().unwrap();
        assert!(input.contains("visible"), "{input}");
        assert!(!input.contains("for-observers"), "{input}");
    }
}
