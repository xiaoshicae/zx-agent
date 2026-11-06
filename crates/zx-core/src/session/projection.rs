//! What a branch projects for the model: the messages after context edits, with the latest
//! summary that still holds standing in for the records it replaced.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::ai::Message;

use super::{
    CompactionEntry, CompactionStartedEntry, CompactionSummary, Entry, ProjectedMessage, SessionStore, SummaryFact,
    SummaryMode,
};

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

/// Whether `message` carries tool result details, which are for observers only.
fn has_details(message: &Message) -> bool {
    matches!(message, Message::ToolResult(result) if result.details.is_some())
}

fn drop_details(message: &mut Message) {
    if let Message::ToolResult(result) = message {
        result.details = None;
    }
}

/// `items` as a model may see them: without tool result details. Summary requests and
/// context sizes use this, since providers never send the details. Copies only when a
/// message has details.
pub(crate) fn without_details<T: HasMessage + Clone>(items: &[T]) -> Cow<'_, [T]> {
    if !items.iter().any(|item| has_details(item.message())) {
        return Cow::Borrowed(items);
    }
    let mut copy = items.to_vec();
    copy.iter_mut().for_each(|item| drop_details(item.message_mut()));
    Cow::Owned(copy)
}

impl SessionStore {
    /// Original projected messages, applying context edits but ignoring every summary.
    pub fn raw_projection(&self) -> Vec<ProjectedMessage> {
        Self::raw_projection_from(&self.branch_path())
    }

    /// The messages the branch to `leaf` projects before any summary: context edits applied,
    /// summaries ignored.
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
                let (id, message) = entry.projected()?;
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

    pub(super) fn compacted_projection(&self) -> Vec<ProjectedMessage> {
        self.compacted_projection_at(self.leaf.as_deref())
    }

    pub(super) fn compacted_projection_at(&self, leaf: Option<&str>) -> Vec<ProjectedMessage> {
        let path = self.branch_path_from(leaf);
        let raw = Self::raw_projection_from(&path);
        let Some(latest) = self.latest_compaction_on(&path) else {
            return raw;
        };
        let Ok(cut) = Self::summarized_count(&path, &raw, &latest.compaction.first_kept_entry_id) else {
            // A summary's boundary is validated to be on its branch; without one, the
            // originals are the only safe projection.
            return raw;
        };
        let compaction = latest.compaction;
        let message = match latest.attempt.mode {
            SummaryMode::Full => compaction.summary.message(compaction.timestamp),
            SummaryMode::Incremental => CompactionSummary {
                facts: Self::effective_facts(&path, &latest),
                inferences: compaction.summary.inferences.clone(),
                plans: compaction.summary.plans.clone(),
            }
            .message(compaction.timestamp),
        };
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
        self.latest_compaction_at(self.leaf.as_deref())
    }

    fn latest_compaction_at(&self, leaf: Option<&str>) -> Option<&CompactionEntry> {
        let path = self.branch_path_from(leaf);
        self.latest_compaction_on(&path).map(|latest| latest.compaction)
    }

    /// The latest successful summary on `path` that still holds. Invalid or incomplete control
    /// records never hide original evidence, and a correction of a full summary's evidence
    /// invalidates it as a whole: the projection falls back to the originals. An incremental
    /// summary stays, without the facts that cite corrected records ([`Self::effective_facts`]).
    pub(super) fn latest_compaction_on<'s>(&'s self, path: &[&'s Entry]) -> Option<LatestCompaction<'s>> {
        let (position, compaction) = path.iter().enumerate().rev().find_map(|(index, entry)| {
            if let Entry::Compaction(entry) = entry {
                Some((index, entry))
            } else {
                None
            }
        })?;
        let Some(Entry::CompactionStarted(attempt)) = self.get(&compaction.attempt_id) else {
            return None;
        };
        if attempt.mode == SummaryMode::Full
            && Self::corrected_source(&path[position + 1..], &attempt.source_entry_ids).is_some()
        {
            return None;
        }
        Some(LatestCompaction {
            position,
            compaction,
            attempt,
        })
    }

    /// The facts of `latest` that still hold at the end of `path`, each with its ID: an
    /// incremental summary loses the facts that cite a record corrected after it. A full
    /// summary, which a correction invalidates as a whole, keeps all of its facts, with IDs
    /// numbered after its attempt, so that an incremental summary can continue it.
    pub(super) fn effective_facts(path: &[&Entry], latest: &LatestCompaction<'_>) -> Vec<SummaryFact> {
        let corrected = Self::corrected_after(path, latest.position);
        latest
            .compaction
            .summary
            .facts
            .iter()
            .enumerate()
            .filter(|(_, fact)| {
                latest.attempt.mode == SummaryMode::Full
                    || !fact.source_entry_ids.iter().any(|id| corrected.contains(id.as_str()))
            })
            .map(|(index, fact)| SummaryFact {
                id: Some(
                    fact.id
                        .clone()
                        .unwrap_or_else(|| format!("{}-{}", latest.attempt.id, index + 1)),
                ),
                ..fact.clone()
            })
            .collect()
    }

    /// The targets of the context edits on `path` after `position`.
    pub(super) fn corrected_after<'p>(path: &[&'p Entry], position: usize) -> HashSet<&'p str> {
        path[position + 1..]
            .iter()
            .filter_map(|entry| match entry {
                Entry::ContextEdit(edit) => Some(edit.target_id.as_str()),
                _ => None,
            })
            .collect()
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
    /// recorded before it in branch order. Cutting by branch order, not by finding the kept
    /// message in the projection, matters when a later context edit omits that message: the
    /// summary must still replace everything recorded before it.
    pub(super) fn summarized_count(path: &[&Entry], raw: &[ProjectedMessage], first_kept: &str) -> Result<usize> {
        let boundary = path
            .iter()
            .position(|entry| entry.id() == first_kept)
            .ok_or_else(|| anyhow::anyhow!("summary boundary {first_kept:?} is not on this branch"))?;
        let summarized: HashSet<&str> = path[..boundary].iter().map(|entry| entry.id()).collect();
        Ok(raw
            .iter()
            .take_while(|item| summarized.contains(item.entry_id.as_str()))
            .count())
    }
}

/// The latest summary on a branch that still holds: its position on the path, its entry and
/// its attempt.
pub(super) struct LatestCompaction<'s> {
    pub position: usize,
    pub compaction: &'s CompactionEntry,
    pub attempt: &'s CompactionStartedEntry,
}
