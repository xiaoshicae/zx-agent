//! What a branch projects for the model: the messages after context edits, with the latest
//! summary that still holds standing in for the records it replaced; and how much of that
//! context an answer's reported usage still measures.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::ai::{AssistantMessage, Message, StopReason, Usage};

use super::{CompactionEntry, Entry, SessionStore};

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

impl SessionStore {
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
        // Later edits of the same target win.
        let mut edits: HashMap<&str, &Option<String>> = HashMap::new();
        for entry in path {
            if let Entry::ContextEdit(edit) = entry {
                edits.insert(edit.target_id.as_str(), &edit.replacement);
            }
        }
        path.iter()
            .filter_map(|entry| {
                let id = entry.id();
                let message = entry.projected()?;
                let message = match edits.get(id) {
                    None => message.into_owned(),
                    Some(None) => return None,
                    Some(Some(text)) => message.with_replaced_text(text),
                };
                Some(ProjectedMessage {
                    entry_id: id.to_string(),
                    message,
                })
            })
            .collect()
    }

    /// The model-visible messages of the current branch, with their source entries.
    pub fn projection(&self) -> Vec<ProjectedMessage> {
        self.projection_at(self.leaf.as_deref())
    }

    /// The model-visible messages of the branch to `leaf`, with their source entries.
    pub(super) fn projection_at(&self, leaf: Option<&str>) -> Vec<ProjectedMessage> {
        let path = self.branch_path_from(leaf);
        let raw = Self::raw_projection_from(&path);
        let Some(compaction) = Self::latest_compaction_on(&path) else {
            return raw;
        };
        // A summary's boundary is validated to be on its branch; without one, the originals
        // are the only safe projection.
        let Some(cut) = Self::summarized_count(&path, &raw, &compaction.first_kept_entry_id) else {
            return raw;
        };
        let message = compaction.summary.message(compaction.timestamp);
        let mut projected: Vec<_> = raw[..cut]
            .iter()
            .filter(|item| matches!(item.message, Message::System(_)))
            .cloned()
            .collect();
        projected.push(ProjectedMessage {
            entry_id: compaction.id.clone(),
            message,
        });
        projected.extend(raw.into_iter().skip(cut));
        projected
    }

    /// Latest successful, still-valid summary on the active branch.
    pub fn latest_compaction(&self) -> Option<&CompactionEntry> {
        Self::latest_compaction_on(&self.branch_path())
    }

    /// The latest successful summary on `path` that still holds. Invalid or incomplete control
    /// records never hide original evidence, and a correction of a summary's evidence
    /// invalidates it as a whole: the projection falls back to the originals.
    fn latest_compaction_on<'s>(path: &[&'s Entry]) -> Option<&'s CompactionEntry> {
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
        Some(compaction)
    }

    /// The first of `sources` that a context edit on `path` targets, in edit order: a summary
    /// that read it no longer describes the branch.
    pub(super) fn corrected_source<'p>(path: &[&'p Entry], sources: &[String]) -> Option<&'p str> {
        path.iter().find_map(|entry| match entry {
            Entry::ContextEdit(edit) if sources.contains(&edit.target_id) => Some(edit.target_id.as_str()),
            _ => None,
        })
    }

    /// How many leading messages of `raw` a summary that keeps `first_kept` replaces: those
    /// recorded before it in branch order; `None` when `first_kept` is not on `path`. Cutting
    /// by branch order, not by finding the kept message in the projection, matters when a
    /// later context edit omits that message: the summary must still replace everything
    /// recorded before it.
    fn summarized_count(path: &[&Entry], raw: &[ProjectedMessage], first_kept: &str) -> Option<usize> {
        let boundary = path.iter().position(|entry| entry.id() == first_kept)?;
        let summarized: HashSet<&str> = path[..boundary].iter().map(|entry| entry.id()).collect();
        Some(
            raw.iter()
                .take_while(|item| summarized.contains(item.entry_id.as_str()))
                .count(),
        )
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
    use crate::session::test_support::result_with_details;

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
