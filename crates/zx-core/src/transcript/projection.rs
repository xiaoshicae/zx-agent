//! What a branch projects for the model: the messages after context edits, with the latest
//! summary that still holds standing in for the records it replaced; and how much of that
//! context an answer's reported usage still measures.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::ai::{AssistantMessage, ContentBlock, Message, StopReason, Usage};

use super::{CompactionEntry, CompactionStartedEntry, Entry, Transcript};

/// A model-visible message and the entry it came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedMessage {
    pub entry_id: String,
    pub message: Message,
}

/// What carries a message: a message itself, or a projected one with its entry.
pub(crate) trait HasMessage {
    fn message(&self) -> &Message;
    fn message_mut(&mut self) -> &mut Message;
}

impl HasMessage for Message {
    fn message(&self) -> &Message {
        self
    }

    fn message_mut(&mut self) -> &mut Message {
        self
    }
}

impl HasMessage for ProjectedMessage {
    fn message(&self) -> &Message {
        &self.message
    }

    fn message_mut(&mut self) -> &mut Message {
        &mut self.message
    }
}

/// Whether `message` carries tool result details or usage, which are for observers only.
fn has_details(message: &Message) -> bool {
    matches!(message, Message::ToolResult(result) if result.details.is_some() || result.usage.is_some())
}

fn drop_details(message: &mut Message) {
    if let Message::ToolResult(result) = message {
        result.details = None;
        result.usage = None;
    }
}

/// `items` as a model may see them: without tool result details and usage. Summary requests
/// and context sizes use this, since providers never send either. Copies only when a
/// message has them.
pub(crate) fn without_details<T: HasMessage + Clone>(items: &[T]) -> Cow<'_, [T]> {
    if !items.iter().any(|item| has_details(item.message())) {
        return Cow::Borrowed(items);
    }
    let mut copy = items.to_vec();
    copy.iter_mut().for_each(|item| drop_details(item.message_mut()));
    Cow::Owned(copy)
}

/// Replace the text of `message` with `text`, as a context edit's replacement does: the
/// content of a user message or a tool result, or the blocks of an assistant message, which
/// become one text block. A system message keeps its sections: prompt changes are appended,
/// never edited. Callers change a copy for the model, never the record.
pub(crate) fn replace_text(message: &mut Message, text: &str) {
    match message {
        Message::System(_) => {}
        Message::User { content, .. } => *content = text.to_string(),
        Message::Assistant(answer) => answer.content = vec![ContentBlock::Text { text: text.to_string() }],
        Message::ToolResult(result) => result.content = text.to_string(),
    }
}

/// Later edits of a target win, including edits of messages kept beside a summary.
fn edits_on<'a>(path: &[&'a Entry]) -> HashMap<&'a str, &'a Option<String>> {
    path.iter()
        .filter_map(|entry| match entry {
            Entry::ContextEdit(edit) => Some((edit.target_id.as_str(), &edit.replacement)),
            _ => None,
        })
        .collect()
}

fn project(entry: &Entry, edits: &HashMap<&str, &Option<String>>) -> Option<ProjectedMessage> {
    let id = entry.id();
    let replacement = match edits.get(id) {
        Some(None) => return None,
        Some(Some(text)) => Some(text),
        None => None,
    };
    let mut message = entry.projected()?.into_owned();
    if let Some(text) = replacement {
        replace_text(&mut message, text);
    }
    Some(ProjectedMessage {
        entry_id: id.to_string(),
        message,
    })
}

impl Transcript {
    /// The model-visible messages of the current branch.
    pub fn messages(&self) -> Vec<Message> {
        self.projection()
            .into_iter()
            .map(|projected| projected.message)
            .collect()
    }

    /// The projected messages whose entries were appended at position `from` or later, so
    /// that a run reports only its own answer and not an earlier run's.
    pub(crate) fn messages_since(&self, from: usize) -> Vec<Message> {
        let added: HashSet<&str> = self.entries[from..]
            .iter()
            .filter(|entry| entry.projected().is_some())
            .map(Entry::id)
            .collect();
        self.projection()
            .into_iter()
            .filter(|projected| added.contains(projected.entry_id.as_str()))
            .map(|projected| projected.message)
            .collect()
    }

    /// The projected messages of the current branch before any summary: context edits
    /// applied, summaries ignored.
    pub fn raw_projection(&self) -> Vec<ProjectedMessage> {
        Self::raw_projection_from(&self.branch_path())
    }

    /// The messages `path` projects before any summary: context edits applied, summaries
    /// ignored.
    pub(super) fn raw_projection_from(path: &[&Entry]) -> Vec<ProjectedMessage> {
        let edits = edits_on(path);
        path.iter().filter_map(|entry| project(entry, &edits)).collect()
    }

    /// The model-visible messages of the current branch, with their source entries.
    pub fn projection(&self) -> Vec<ProjectedMessage> {
        self.projection_at(self.leaf.as_deref())
    }

    /// The model-visible messages of the branch to `leaf`, with their source entries.
    pub(super) fn projection_at(&self, leaf: Option<&str>) -> Vec<ProjectedMessage> {
        let path = self.branch_path_from(leaf);
        let Some((compaction, started)) = Self::compaction_in_effect_on(&path) else {
            return Self::raw_projection_from(&path);
        };
        // The boundary is in record order even when a later edit omits that message.
        let Some(cut) = path
            .iter()
            .position(|entry| entry.id() == compaction.first_kept_entry_id)
        else {
            return Self::raw_projection_from(&path);
        };
        let edits = edits_on(&path);
        let kept_input = started.kept_input_entry_id.as_deref();
        // Inspect entry types before projecting: summarized tool bodies are never cloned.
        let mut projected: Vec<_> = path[..cut]
            .iter()
            .filter(|entry| {
                Some(entry.id()) == kept_input
                    || matches!(entry, Entry::Message(message) if matches!(message.message, Message::System(_)))
            })
            .filter_map(|entry| project(entry, &edits))
            .collect();
        projected.push(ProjectedMessage {
            entry_id: compaction.id.clone(),
            message: compaction.summary.message(compaction.timestamp),
        });
        projected.extend(path[cut..].iter().filter_map(|entry| project(entry, &edits)));
        projected
    }

    /// The latest compaction on the current branch that succeeded and still holds.
    pub fn latest_compaction(&self) -> Option<&CompactionEntry> {
        Self::compaction_in_effect_on(&self.branch_path()).map(|(compaction, _)| compaction)
    }

    /// The latest compaction on `path` that succeeded and still holds, with the record of its
    /// start. Invalid or incomplete control records never hide original evidence, and a
    /// correction of a summary's evidence invalidates it as a whole: the projection falls
    /// back to the originals.
    fn compaction_in_effect_on<'s>(path: &[&'s Entry]) -> Option<(&'s CompactionEntry, &'s CompactionStartedEntry)> {
        let (position, compaction) = path.iter().enumerate().rev().find_map(|(index, entry)| {
            if let Entry::Compaction(entry) = entry {
                Some((index, entry))
            } else {
                None
            }
        })?;
        // A result whose start is not before it on the branch is such an invalid record.
        let (_, started) = Self::compaction_start_on(&path[..position], &compaction.compaction_id).ok()?;
        if Self::corrected_source(&path[position + 1..], &started.source_entry_ids).is_some() {
            return None;
        }
        Some((compaction, started))
    }

    /// The first of `sources` that a context edit on `path` targets, in edit order: a summary
    /// that read it no longer describes the branch.
    pub(super) fn corrected_source<'p>(path: &[&'p Entry], sources: &[String]) -> Option<&'p str> {
        path.iter().find_map(|entry| match entry {
            Entry::ContextEdit(edit) if sources.contains(&edit.target_id) => Some(edit.target_id.as_str()),
            _ => None,
        })
    }

    /// The context split at its last measurement: the usage reported by the last answer that
    /// still measures the context up to it, and the messages after that answer. Without such
    /// an answer, no usage and every message.
    ///
    /// An answer measures the context when it finished and reported usage, and nothing
    /// recorded after it changed the messages up to it: no compaction, and no edit of a
    /// message at or before it. The usage of an answer that a compaction kept, or that
    /// precedes an edit of earlier messages, counts a context that is no longer sent (pi:
    /// `estimateProjectedContextTokens`).
    pub fn measured_context(&self) -> (Option<Usage>, Vec<Message>) {
        let projection = self.projection();
        let measured = self.measuring_answer().and_then(|(id, usage)| {
            let index = projection.iter().position(|item| item.entry_id == id)?;
            Some((index, usage.clone()))
        });
        let (usage, measured_messages) = match measured {
            Some((index, usage)) => (Some(usage), index + 1),
            None => (None, 0),
        };
        let rest = projection
            .into_iter()
            .skip(measured_messages)
            .map(|projected| projected.message)
            .collect();
        (usage, rest)
    }

    /// The last answer on the branch whose reported usage still measures the context up to
    /// it, as its entry ID and that usage.
    fn measuring_answer(&self) -> Option<(&str, &Usage)> {
        // Entries are stored in the order they were appended, so on one branch a smaller
        // position is an earlier entry.
        let position = |id: &str| self.index.get(id).copied();
        // Walking back from the leaf: the earliest entry that an edit met so far targets. An
        // answer at or after it was sent messages that an edit has changed since.
        let mut edited_from = usize::MAX;
        for entry in self.ancestors(self.leaf.as_deref()) {
            match entry {
                // Every earlier answer was sent what this compaction replaced.
                Entry::Compaction(_) => return None,
                // An edit's target is on its branch. A missing one counts as the first entry,
                // so that no answer is trusted.
                Entry::ContextEdit(edit) => edited_from = edited_from.min(position(&edit.target_id).unwrap_or(0)),
                Entry::Message(recorded) => {
                    if let Message::Assistant(answer) = &recorded.message
                        && reports_usage(answer)
                        && position(&recorded.id).is_some_and(|at| at < edited_from)
                    {
                        return Some((&recorded.id, &answer.usage));
                    }
                }
                Entry::CompactionStarted(_)
                | Entry::CompactionFailed(_)
                | Entry::RequestConfig(_)
                | Entry::RequestStarted(_)
                | Entry::ToolCallStarted(_)
                | Entry::Custom(_)
                | Entry::CustomMessage(_)
                | Entry::SessionInfo(_) => {}
            }
        }
        None
    }
}

/// Whether `answer` finished and reported the tokens of its request.
fn reports_usage(answer: &AssistantMessage) -> bool {
    answer.usage.total_tokens > 0 && !matches!(answer.stop_reason, StopReason::Error | StopReason::Aborted)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use crate::ai::UserMessageKind;
    use crate::transcript::test_support::{header, plan, result_with_details, seeded, summary};

    /// An answer that reported `input` prompt tokens and 10 output tokens.
    fn answer_reporting(input: u64) -> Message {
        let mut answer = AssistantMessage::new(
            "p",
            "m",
            StopReason::Stop,
            vec![ContentBlock::Text { text: "noted".into() }],
        );
        answer.usage = Usage {
            input,
            output: 10,
            total_tokens: input + 10,
            ..Usage::default()
        };
        Message::Assistant(answer)
    }

    #[test]
    fn a_text_replacement_keeps_a_continuation_a_continuation() {
        let mut message = Message::continuation("继续");
        replace_text(&mut message, "接着说");
        assert!(matches!(
            message,
            Message::User { kind: UserMessageKind::ContinueAnswer, content, .. } if content == "接着说"
        ));
    }

    #[tokio::test]
    async fn a_compaction_ends_the_measurement_by_the_answers_before_it() {
        let mut transcript = Transcript::in_memory(header());
        let old = transcript.append_message(Message::user("old evidence")).await.unwrap();
        let kept = transcript.append_message(answer_reporting(900)).await.unwrap();
        let latest = Message::user("latest input");
        transcript.append_message(latest.clone()).await.unwrap();
        let (usage, rest) = transcript.measured_context();
        assert_eq!(usage.map(|usage| usage.input), Some(900));
        assert_eq!(rest, [latest]);

        let started = transcript
            .append_compaction_started(plan(&transcript, &kept))
            .await
            .unwrap();
        transcript
            .append_compaction(&started, summary(&old), Usage::default())
            .await
            .unwrap();

        // The kept answer was sent the context from before the compaction.
        let (usage, rest) = transcript.measured_context();
        assert_eq!(usage, None);
        assert_eq!(rest, transcript.messages());
    }

    #[tokio::test]
    async fn an_answer_recorded_after_a_compaction_measures_the_compacted_context() {
        let mut transcript = Transcript::in_memory(header());
        let (old, kept) = seeded(&mut transcript).await;
        let started = transcript
            .append_compaction_started(plan(&transcript, &kept))
            .await
            .unwrap();
        transcript
            .append_compaction(&started, summary(&old), Usage::default())
            .await
            .unwrap();
        transcript.append_message(answer_reporting(300)).await.unwrap();
        let later = Message::user("next input");
        transcript.append_message(later.clone()).await.unwrap();

        let (usage, rest) = transcript.measured_context();

        assert_eq!(usage.map(|usage| usage.input), Some(300));
        assert_eq!(rest, [later]);
    }

    /// An old message, an answer that reported 900 prompt tokens, and a later input; returns
    /// the three entry IDs.
    async fn measured(transcript: &mut Transcript) -> [String; 3] {
        let earlier = transcript.append_message(Message::user("old evidence")).await.unwrap();
        let answer = transcript.append_message(answer_reporting(900)).await.unwrap();
        let later = transcript.append_message(Message::user("latest input")).await.unwrap();
        [earlier, answer, later]
    }

    #[tokio::test]
    async fn an_edit_of_a_later_message_keeps_the_measurement_by_an_answer() {
        let mut transcript = Transcript::in_memory(header());
        let [_, _, later] = measured(&mut transcript).await;

        // What the answer was sent is as it was.
        transcript
            .append_context_edit(&later, Some("rewritten".into()))
            .await
            .unwrap();

        assert_eq!(transcript.measured_context().0.map(|usage| usage.input), Some(900));
    }

    #[tokio::test]
    async fn an_edit_at_or_before_an_answer_ends_its_measurement() {
        for edited in [0, 1] {
            let mut transcript = Transcript::in_memory(header());
            let ids = measured(&mut transcript).await;

            transcript
                .append_context_edit(&ids[edited], Some("shorter".into()))
                .await
                .unwrap();

            assert_eq!(transcript.measured_context().0, None, "edited entry {edited}");
        }
    }

    #[test]
    fn messages_without_details_are_not_copied() {
        let messages = vec![Message::user("go"), result_with_details(None)];
        assert!(matches!(without_details(&messages), Cow::Borrowed(_)));
    }

    #[test]
    fn details_are_dropped_from_what_a_model_may_see_and_nothing_else() {
        // One user message for both sides: each `Message::user` takes the current time, and two
        // calls a millisecond apart would differ (seen once in CI).
        let prompt = Message::user("go");
        let messages = vec![
            prompt.clone(),
            result_with_details(Some(serde_json::json!({"marker": "for-observers"}))),
        ];
        let seen = without_details(&messages);
        assert_eq!(*seen, [prompt, result_with_details(None)]);
        assert!(matches!(&messages[1], Message::ToolResult(result) if result.details.is_some()));
    }
}
