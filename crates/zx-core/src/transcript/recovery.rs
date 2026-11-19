//! Start records for tool calls with side effects, and the calls that an interrupted run
//! left without results.

use std::collections::HashSet;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::ai::{Message, StopReason, ToolCall, now_ms};

use super::{Entry, Transcript};

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

/// A tool call at the end of the current branch that has no recorded result there: the run
/// that issued it stopped, through a crash, an exit or a dropped run future, before recording
/// one; the answer that issued it failed or was cancelled, so it never ran; or the
/// conversation went back to that answer, which leaves the call's records on another branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptedToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub start: StartRecord,
}

impl Transcript {
    /// Record that a call with side effects is about to run. Fails without writing anything
    /// if the call is not on the current branch, has another name, already has a result, or
    /// has already started.
    pub(crate) async fn append_tool_call_started(&mut self, tool_call_id: &str, tool_name: &str) -> Result<String> {
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
    /// result that a `context_edit` hides from the model still counts as recorded. A call
    /// that started on a branch the conversation left since is [`StartRecord::Present`]:
    /// going back does not undo what it did.
    pub fn interrupted_tool_calls(&self) -> Vec<InterruptedToolCall> {
        self.interrupted_tool_calls_at(self.leaf.as_deref())
    }

    /// [`interrupted_tool_calls`](Self::interrupted_tool_calls) of the branch that ends at
    /// `leaf`.
    pub(super) fn interrupted_tool_calls_at(&self, leaf: Option<&str>) -> Vec<InterruptedToolCall> {
        let path = self.branch_path_from(leaf);
        // The last message that is not a tool result. A custom message is a message to the
        // model too: calls before it are answered or abandoned, not interrupted.
        let last_message = path.iter().rposition(|entry| {
            entry
                .projected()
                .is_some_and(|message| !matches!(*message, Message::ToolResult(_)))
        });
        let Some(position) = last_message else {
            return Vec::new();
        };
        let Entry::Message(issuer) = path[position] else {
            return Vec::new();
        };
        let Message::Assistant(answer) = &issuer.message else {
            return Vec::new();
        };
        let never_ran = matches!(answer.stop_reason, StopReason::Error | StopReason::Aborted);
        if !self.projection_at(leaf).iter().any(|item| item.entry_id == issuer.id) {
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
                } else if started.contains(call.id.as_str()) || self.started_off_branch(&issuer.id, &call.id) {
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

    /// Whether a start of the call `call_id` of the answer `issuer` is recorded anywhere in
    /// the session, also on a branch the conversation left. A call ID that a later answer
    /// used again belongs to that answer, as it does for a start record.
    fn started_off_branch(&self, issuer: &str, call_id: &str) -> bool {
        self.entries.iter().any(|entry| match entry {
            Entry::ToolCallStarted(start) if start.tool_call_id == call_id => self
                .ancestors(start.parent_id.as_deref())
                .find(|entry| issued_call(entry, call_id).is_some())
                .is_some_and(|entry| entry.id() == issuer),
            _ => false,
        })
    }

    /// The call must have been issued on this branch and have neither a result nor an
    /// earlier start record since. Walks the branch upward from the start to the answer that
    /// issued the call.
    pub(super) fn validate_tool_call_start(&self, start: &ToolCallStartedEntry) -> Result<()> {
        let id = &start.tool_call_id;
        for entry in self.ancestors(start.parent_id.as_deref()) {
            if let Some(call) = issued_call(entry, id) {
                ensure!(
                    call.name == start.tool_name,
                    "tool call {id:?} is {:?}, not {:?}",
                    call.name,
                    start.tool_name
                );
                return Ok(());
            }
            if let Entry::Message(entry) = entry
                && let Message::ToolResult(result) = &entry.message
                && result.tool_call_id == *id
            {
                bail!("tool call {id:?} already has a result");
            }
            if let Entry::ToolCallStarted(earlier) = entry
                && earlier.tool_call_id == *id
            {
                bail!("tool call {id:?} has already started");
            }
        }
        bail!("tool call {id:?} is not on this branch")
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
    use crate::transcript::{CustomMessage, SessionHeader};

    fn transcript() -> Transcript {
        Transcript::in_memory(SessionHeader::new(Path::new("."), "m"))
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

    fn starts(transcript: &Transcript) -> Vec<(String, StartRecord)> {
        transcript
            .interrupted_tool_calls()
            .into_iter()
            .map(|call| (call.tool_call_id, call.start))
            .collect()
    }

    #[tokio::test]
    async fn start_records_use_snake_case_tags_and_camel_case_fields() {
        let mut transcript = transcript();
        let issuer = transcript
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        let id = transcript.append_tool_call_started("c1", "bash").await.unwrap();
        let line = serde_json::to_value(transcript.get(&id).unwrap()).unwrap();
        assert_eq!(line["type"], "tool_call_started");
        assert_eq!(line["parentId"], issuer);
        assert_eq!(line["toolCallId"], "c1");
        assert_eq!(line["toolName"], "bash");
        assert_eq!(
            &serde_json::from_value::<Entry>(line).unwrap(),
            transcript.get(&id).unwrap()
        );
    }

    #[tokio::test]
    async fn a_start_must_name_an_unfinished_call_issued_on_the_branch() {
        let mut transcript = transcript();
        transcript
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash"), ("c2", "bash")]))
            .await
            .unwrap();
        transcript.append_message(result("c1", "bash")).await.unwrap();
        transcript.append_tool_call_started("c2", "bash").await.unwrap();
        let before = transcript.entries().len();
        // Unknown call, another tool's name, a call with a result, a call that already started.
        for (id, name) in [("c9", "bash"), ("c2", "read_file"), ("c1", "bash"), ("c2", "bash")] {
            assert!(
                transcript.append_tool_call_started(id, name).await.is_err(),
                "{id} {name}"
            );
        }
        assert_eq!(transcript.entries().len(), before);
    }

    #[tokio::test]
    async fn a_reused_call_id_refers_to_the_latest_answer() {
        let mut transcript = transcript();
        transcript
            .append_message(answer(StopReason::ToolUse, &[("call_0", "bash")]))
            .await
            .unwrap();
        transcript.append_tool_call_started("call_0", "bash").await.unwrap();
        transcript.append_message(result("call_0", "bash")).await.unwrap();
        transcript
            .append_message(answer(StopReason::ToolUse, &[("call_0", "read_file")]))
            .await
            .unwrap();
        assert!(transcript.append_tool_call_started("call_0", "bash").await.is_err());
        transcript
            .append_tool_call_started("call_0", "read_file")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn interrupted_calls_are_the_unanswered_calls_of_the_last_answer_in_call_order() {
        let mut transcript = transcript();
        transcript.append_message(Message::user("go")).await.unwrap();
        transcript
            .append_message(answer(
                StopReason::ToolUse,
                &[("c1", "bash"), ("c2", "bash"), ("c3", "read_file")],
            ))
            .await
            .unwrap();
        transcript.append_tool_call_started("c1", "bash").await.unwrap();
        transcript.append_tool_call_started("c2", "bash").await.unwrap();
        transcript.append_message(result("c1", "bash")).await.unwrap();
        assert_eq!(
            starts(&transcript),
            [("c2".into(), StartRecord::Present), ("c3".into(), StartRecord::Absent)]
        );
    }

    #[tokio::test]
    async fn calls_followed_by_other_messages_or_off_the_branch_are_not_interrupted() {
        let mut transcript = transcript();
        let question = transcript.append_message(Message::user("go")).await.unwrap();
        transcript
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        transcript.append_message(Message::user("later")).await.unwrap();
        assert!(transcript.interrupted_tool_calls().is_empty());
        transcript.branch(&question).unwrap();
        assert!(transcript.interrupted_tool_calls().is_empty());
    }

    #[tokio::test]
    async fn a_call_that_started_on_a_branch_left_behind_still_counts_as_started() {
        let mut transcript = transcript();
        transcript.append_message(Message::user("go")).await.unwrap();
        let issuer = transcript
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash"), ("c2", "read_file")]))
            .await
            .unwrap();
        transcript.append_tool_call_started("c1", "bash").await.unwrap();
        transcript.append_message(result("c1", "bash")).await.unwrap();
        transcript.append_message(result("c2", "read_file")).await.unwrap();

        // Back to the answer: its calls have no results on the branch that starts here.
        transcript.branch(&issuer).unwrap();

        assert_eq!(
            starts(&transcript),
            [("c1".into(), StartRecord::Present), ("c2".into(), StartRecord::Absent)]
        );
    }

    #[tokio::test]
    async fn a_start_of_a_reused_call_id_belongs_to_the_answer_that_issued_it_last() {
        let mut transcript = transcript();
        let first = transcript
            .append_message(answer(StopReason::ToolUse, &[("call_0", "bash")]))
            .await
            .unwrap();
        transcript.append_message(result("call_0", "bash")).await.unwrap();
        transcript
            .append_message(answer(StopReason::ToolUse, &[("call_0", "bash")]))
            .await
            .unwrap();
        transcript.append_tool_call_started("call_0", "bash").await.unwrap();

        // The start is the later answer's: the first answer's call has none.
        transcript.branch(&first).unwrap();

        assert_eq!(starts(&transcript), [("call_0".into(), StartRecord::Absent)]);
    }

    #[tokio::test]
    async fn failed_and_cancelled_answers_never_ran_their_calls() {
        for stop_reason in [StopReason::Error, StopReason::Aborted] {
            let mut transcript = transcript();
            transcript
                .append_message(answer(stop_reason, &[("c1", "bash")]))
                .await
                .unwrap();
            assert_eq!(
                transcript.interrupted_tool_calls(),
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
        let mut omitted = transcript();
        let issuer = omitted
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        omitted.append_context_edit(&issuer, None).await.unwrap();
        assert!(omitted.interrupted_tool_calls().is_empty());

        let mut hidden = transcript();
        hidden
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        let recorded = hidden.append_message(result("c1", "bash")).await.unwrap();
        hidden.append_context_edit(&recorded, None).await.unwrap();
        assert!(hidden.interrupted_tool_calls().is_empty());
    }

    #[tokio::test]
    async fn calls_before_a_message_from_an_extension_are_not_interrupted() {
        let mut transcript = transcript();
        transcript
            .append_message(answer(StopReason::ToolUse, &[("c1", "bash")]))
            .await
            .unwrap();
        assert_eq!(starts(&transcript), [("c1".to_string(), StartRecord::Absent)]);

        transcript
            .append_custom_message(CustomMessage {
                custom_type: "note".into(),
                content: "status".into(),
                display: false,
                details: None,
            })
            .await
            .unwrap();

        assert!(transcript.interrupted_tool_calls().is_empty());
    }
}
