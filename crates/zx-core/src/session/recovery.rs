//! Start records for tool calls with side effects, and the calls that an interrupted run
//! left without results.

use std::collections::HashSet;

use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::ai::{Message, StopReason, ToolCall, now_ms};

use super::{Entry, SessionStore};

/// A tool call with side effects that is about to run.
///
/// Written and flushed after checks and approval, immediately before the call runs. A start
/// without a later result means that the call began and that its outcome is unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallStartedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub tool_call_id: String,
    pub tool_name: String,
}

/// What the session records about whether an interrupted call started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartRecord {
    /// A `tool_call_started` entry exists: the call began and may have taken effect.
    Present,
    /// No entry: a call with side effects never began. Read-only calls are never recorded,
    /// so for them this says nothing.
    Absent,
    /// The answer that issued the call ended with `Error` or `Aborted`: the loop runs no
    /// call of such an answer, so this one never began.
    NeverRun,
}

/// A tool call at the end of the current branch that has no recorded result: the run that
/// issued it stopped, through a crash, an exit or a dropped run future, before recording one,
/// or the answer that issued it failed or was cancelled, so it never ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptedToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub start: StartRecord,
}

impl SessionStore {
    /// Record that a call with side effects is about to run. Fails without writing anything
    /// if the call is not on the current branch, has another name, already has a result, or
    /// has already started.
    pub async fn append_tool_call_started(&mut self, tool_call_id: &str, tool_name: &str) -> Result<String> {
        self.append(Entry::ToolCallStarted(ToolCallStartedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
        }))
        .await
    }

    /// The calls of the branch's last assistant message that have no result, in call order.
    ///
    /// Empty unless that message is followed only by tool results and records that are not
    /// messages, and is still in the model projection. The calls of a message that ended
    /// with `Error` or `Aborted` are all [`StartRecord::NeverRun`]: the loop never runs
    /// tools from such a response, and the model must not take them for calls that ran. A
    /// result that a `context_edit` hides from the model still counts as recorded.
    pub fn interrupted_tool_calls(&self) -> Vec<InterruptedToolCall> {
        let path = self.branch_path();
        let issuer = path.iter().enumerate().rev().find_map(|(index, entry)| match entry {
            Entry::Message(entry) if !matches!(entry.message, Message::ToolResult(_)) => Some((index, Some(entry))),
            // A custom message is a message to the model too: calls before it are answered
            // or abandoned, not interrupted.
            Entry::CustomMessage(_) => Some((index, None)),
            _ => None,
        });
        let Some((position, Some(issuer))) = issuer else {
            return Vec::new();
        };
        let Message::Assistant(answer) = &issuer.message else {
            return Vec::new();
        };
        let never_ran = matches!(answer.stop_reason, StopReason::Error | StopReason::Aborted);
        if !self.projection().iter().any(|item| item.entry_id == issuer.id) {
            // Omitted or summarized: the model no longer sees these calls.
            return Vec::new();
        }
        let mut results = HashSet::new();
        let mut started = HashSet::new();
        for entry in &path[position + 1..] {
            if let Entry::Message(entry) = entry
                && let Message::ToolResult(result) = &entry.message
            {
                results.insert(result.tool_call_id.as_str());
            }
            if let Entry::ToolCallStarted(start) = entry {
                started.insert(start.tool_call_id.as_str());
            }
        }
        answer
            .tool_calls()
            .into_iter()
            .filter(|call| !results.contains(call.id.as_str()))
            .map(|call| {
                let start = if never_ran {
                    StartRecord::NeverRun
                } else if started.contains(call.id.as_str()) {
                    StartRecord::Present
                } else {
                    StartRecord::Absent
                };
                InterruptedToolCall {
                    tool_call_id: call.id,
                    tool_name: call.name,
                    start,
                }
            })
            .collect()
    }

    /// The call must have been issued on this branch and have neither a result nor an
    /// earlier start record since.
    pub(super) fn validate_tool_call_start(&self, start: &ToolCallStartedEntry) -> Result<()> {
        let path = self.branch_path_from(start.parent_id.as_deref());
        let (position, call) = path
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| issued_call(entry, &start.tool_call_id).map(|call| (index, call)))
            .ok_or_else(|| anyhow!("tool call {:?} is not on this branch", start.tool_call_id))?;
        ensure!(
            call.name == start.tool_name,
            "tool call {:?} is {:?}, not {:?}",
            start.tool_call_id,
            call.name,
            start.tool_name
        );
        for entry in &path[position + 1..] {
            if let Entry::Message(entry) = entry
                && let Message::ToolResult(result) = &entry.message
                && result.tool_call_id == start.tool_call_id
            {
                bail!("tool call {:?} already has a result", start.tool_call_id);
            }
            if let Entry::ToolCallStarted(earlier) = entry
                && earlier.tool_call_id == start.tool_call_id
            {
                bail!("tool call {:?} has already started", start.tool_call_id);
            }
        }
        Ok(())
    }
}

/// The call with `id` if `entry` is an assistant message that issued it.
fn issued_call(entry: &Entry, id: &str) -> Option<ToolCall> {
    if let Entry::Message(entry) = entry
        && let Message::Assistant(answer) = &entry.message
    {
        answer.tool_calls().into_iter().find(|call| call.id == id)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::ai::{AssistantMessage, ContentBlock, ToolCall, ToolResultMessage};
    use crate::session::SessionHeader;

    fn store() -> SessionStore {
        SessionStore::in_memory(SessionHeader::new(Path::new("."), "m"))
    }

    fn answer(stop_reason: StopReason, calls: &[(&str, &str)]) -> Message {
        let mut answer = AssistantMessage::failure("p", "m", stop_reason, "");
        answer.error_message = None;
        answer.content = calls
            .iter()
            .map(|(id, name)| {
                ContentBlock::ToolCall(ToolCall {
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: json!({}),
                    arguments_error: None,
                })
            })
            .collect();
        Message::Assistant(answer)
    }

    fn result(id: &str, name: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: "done".into(),
            is_error: false,
            timestamp: 0,
            duration_ms: None,
            details: None,
            usage: None,
        })
    }

    fn starts(store: &SessionStore) -> Vec<(String, StartRecord)> {
        store
            .interrupted_tool_calls()
            .into_iter()
            .map(|call| (call.tool_call_id, call.start))
            .collect()
    }

    #[tokio::test]
    async fn start_records_use_snake_case_tags_and_camel_case_fields() {
        let mut store = store();
        let issuer = store
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        let id = store.append_tool_call_started("c1", "bash").await.unwrap();
        let line = serde_json::to_value(store.get(&id).unwrap()).unwrap();
        assert_eq!(line["type"], "tool_call_started");
        assert_eq!(line["parentId"], issuer);
        assert_eq!(line["toolCallId"], "c1");
        assert_eq!(line["toolName"], "bash");
        assert_eq!(&serde_json::from_value::<Entry>(line).unwrap(), store.get(&id).unwrap());
    }

    #[tokio::test]
    async fn a_start_must_name_an_unfinished_call_issued_on_the_branch() {
        let mut store = store();
        store
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash"), ("c2", "bash")]))
            .await
            .unwrap();
        store.append_message(result("c1", "bash")).await.unwrap();
        store.append_tool_call_started("c2", "bash").await.unwrap();
        let before = store.entries().len();
        // Unknown call, another tool's name, a call with a result, a call that already started.
        for (id, name) in [("c9", "bash"), ("c2", "read_file"), ("c1", "bash"), ("c2", "bash")] {
            assert!(store.append_tool_call_started(id, name).await.is_err(), "{id} {name}");
        }
        assert_eq!(store.entries().len(), before);
    }

    #[tokio::test]
    async fn a_reused_call_id_refers_to_the_latest_answer() {
        let mut store = store();
        store
            .append_message(answer(StopReason::ToolUse, &[("call_0", "bash")]))
            .await
            .unwrap();
        store.append_tool_call_started("call_0", "bash").await.unwrap();
        store.append_message(result("call_0", "bash")).await.unwrap();
        store
            .append_message(answer(StopReason::ToolUse, &[("call_0", "read_file")]))
            .await
            .unwrap();
        assert!(store.append_tool_call_started("call_0", "bash").await.is_err());
        store.append_tool_call_started("call_0", "read_file").await.unwrap();
    }

    #[tokio::test]
    async fn interrupted_calls_are_the_unanswered_calls_of_the_last_answer_in_call_order() {
        let mut store = store();
        store.append_message(Message::user("go")).await.unwrap();
        store
            .append_message(answer(
                StopReason::ToolUse,
                &[("c1", "bash"), ("c2", "bash"), ("c3", "read_file")],
            ))
            .await
            .unwrap();
        store.append_tool_call_started("c1", "bash").await.unwrap();
        store.append_tool_call_started("c2", "bash").await.unwrap();
        store.append_message(result("c1", "bash")).await.unwrap();
        assert_eq!(
            starts(&store),
            [("c2".into(), StartRecord::Present), ("c3".into(), StartRecord::Absent)]
        );
    }

    #[tokio::test]
    async fn calls_followed_by_other_messages_or_off_the_branch_are_not_interrupted() {
        let mut store = store();
        let question = store.append_message(Message::user("go")).await.unwrap();
        store
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        store.append_message(Message::user("later")).await.unwrap();
        assert!(store.interrupted_tool_calls().is_empty());
        store.branch(&question).unwrap();
        assert!(store.interrupted_tool_calls().is_empty());
    }

    #[tokio::test]
    async fn failed_and_cancelled_answers_never_ran_their_calls() {
        for stop_reason in [StopReason::Error, StopReason::Aborted] {
            let mut store = store();
            store
                .append_message(answer(stop_reason, &[("c1", "bash")]))
                .await
                .unwrap();
            assert_eq!(
                store.interrupted_tool_calls(),
                [InterruptedToolCall {
                    tool_call_id: "c1".into(),
                    tool_name: "bash".into(),
                    start: StartRecord::NeverRun,
                }],
                "{stop_reason:?}"
            );
        }
    }

    #[tokio::test]
    async fn context_edits_hide_answers_but_not_recorded_results() {
        let mut omitted = store();
        let issuer = omitted
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        omitted.append_context_edit(&issuer, None).await.unwrap();
        assert!(omitted.interrupted_tool_calls().is_empty());

        let mut hidden = store();
        hidden
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        let recorded = hidden.append_message(result("c1", "bash")).await.unwrap();
        hidden.append_context_edit(&recorded, None).await.unwrap();
        assert!(hidden.interrupted_tool_calls().is_empty());
    }
}
