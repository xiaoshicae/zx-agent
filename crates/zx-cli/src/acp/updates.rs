//! Run events as `session/update` notifications, and the replay of a loaded session's
//! conversation.
//!
//! - `shown`: what a finished tool call shows, its output as code and its edits as diffs

mod shown;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CompactionId, CompactionStatus, CompactionUpdate, Content, ContentBlock, ContentChunk, ImageContent, Notice,
    NoticeSeverity, SessionId, SessionNotification, SessionUpdate, TextContent, ToolCall, ToolCallContent, ToolCallId,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::Value;
use zx_agent::ai::{
    AssistantMessage, ContentBlock as MessageBlock, Delta, Image, Message, Role, ToolCall as MessageCall,
    ToolResultMessage, UserMessageKind,
};
use zx_agent::builtin::tools::{EDIT_FILE, READ_FILE, WRITE_FILE};
use zx_agent::events::{ApprovalResult, Event, EventKind};
use zx_agent::policy::PolicyDecision;
use zx_agent::transcript::{Entry, ProjectedMessage, Transcript};
use zx_util::stdio::eprint_line;

use self::shown::{result_content, text_content};
use super::tool_kind;
use crate::render::call_label;

/// Run events as `session/update` notifications. A turn owns the updates of its run.
pub(super) struct Updates {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    cwd: PathBuf,
    /// The answer in progress streamed text, so its end need not repeat it.
    wrote_text: bool,
    /// Arguments of the tool calls in progress, by call ID, for their results.
    calls: HashMap<String, Value>,
}

impl Updates {
    pub(super) fn new(cx: ConnectionTo<Client>, session_id: SessionId, cwd: PathBuf) -> Self {
        Self {
            cx,
            session_id,
            cwd,
            wrote_text: false,
            calls: HashMap::new(),
        }
    }

    fn send(&self, update: SessionUpdate) {
        send(&self.cx, &self.session_id, update);
    }

    fn text_chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
    }

    /// `image` as the protocol carries it.
    fn image_block(image: &Image) -> ContentBlock {
        ContentBlock::Image(ImageContent::new(image.data.clone(), image.mime_type.clone()))
    }

    /// Replay the conversation `transcript` holds to a client that loads the session: what it
    /// would have seen as the turns ran, without the streaming. That is every turn of the
    /// branch, also those a summary stands in for in what the model reads.
    ///
    /// The person's prompts with their images, the answers with their thoughts, and the tool
    /// calls with their results; an extension's message when it asked to be shown. Summaries
    /// and the model's own continuations are the model's, not the person's.
    pub(super) fn replay(&self, transcript: &Transcript) {
        let projection = transcript.raw_projection();
        for (index, item) in projection.iter().enumerate() {
            match &item.message {
                Message::User {
                    content,
                    images,
                    kind: UserMessageKind::Prompt,
                    ..
                } => {
                    if !content.is_empty() {
                        self.send(SessionUpdate::UserMessageChunk(Self::text_chunk(content)));
                    }
                    for image in images {
                        self.send(SessionUpdate::UserMessageChunk(ContentChunk::new(Self::image_block(
                            image,
                        ))));
                    }
                }
                Message::User {
                    content,
                    kind: UserMessageKind::Custom,
                    ..
                } => {
                    if let Some(Entry::CustomMessage(entry)) = transcript.get(&item.entry_id)
                        && entry.message.display
                    {
                        self.send(SessionUpdate::UserMessageChunk(Self::text_chunk(content)));
                    }
                }
                Message::User {
                    kind: UserMessageKind::ContinueAnswer | UserMessageKind::Summary,
                    ..
                }
                | Message::ToolResult(_)
                | Message::System(_) => {}
                Message::Assistant(message) => {
                    for block in &message.content {
                        match block {
                            MessageBlock::Text { text } if !text.is_empty() => {
                                self.send(SessionUpdate::AgentMessageChunk(Self::text_chunk(text)));
                            }
                            MessageBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                                self.send(SessionUpdate::AgentThoughtChunk(Self::text_chunk(thinking)));
                            }
                            MessageBlock::Text { .. } | MessageBlock::Thinking { .. } => {}
                            MessageBlock::ToolCall(MessageCall {
                                id,
                                name,
                                arguments: args,
                                ..
                            }) => {
                                let call = self.recorded_call(id, name, args, &projection[index + 1..]);
                                self.send(SessionUpdate::ToolCall(call));
                            }
                        }
                    }
                }
            }
        }
    }

    /// A recorded call as one update: with its result when a later message holds one, else
    /// still pending, which the next run settles.
    fn recorded_call(&self, id: &str, name: &str, args: &Value, later: &[ProjectedMessage]) -> ToolCall {
        let call = self.call(id, name, args);
        let result = later.iter().find_map(|item| match &item.message {
            Message::ToolResult(result) if result.tool_call_id == id => Some(result),
            _ => None,
        });
        match result {
            Some(result) => {
                let (status, content, raw_output) = self.result_fields(Some(args), result);
                call.status(status).content(content).raw_output(raw_output)
            }
            None => call.status(ToolCallStatus::Pending),
        }
    }

    /// A call as the client lists it: its title, kind, raw input and locations.
    fn call(&self, id: &str, name: &str, args: &Value) -> ToolCall {
        ToolCall::new(ToolCallId::new(id), call_label(name, args))
            .kind(tool_kind(name))
            .raw_input(args.clone())
            .locations(self.locations(name, args))
    }

    /// What a result reports: its status, what the client shows, its images last, and the raw
    /// output.
    fn result_fields(
        &self,
        args: Option<&Value>,
        result: &ToolResultMessage,
    ) -> (ToolCallStatus, Vec<ToolCallContent>, Value) {
        let status = if result.is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        };
        let mut content = result_content(result, args, &self.cwd);
        content.extend(
            result
                .images
                .iter()
                .map(|image| ToolCallContent::Content(Content::new(Self::image_block(image)))),
        );
        let raw_output = result
            .details
            .clone()
            .unwrap_or_else(|| Value::String(result.content.clone()));
        (status, content, raw_output)
    }

    fn tool_update(&self, id: &str, fields: ToolCallUpdateFields) {
        self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(id),
            fields,
        )));
    }

    /// The file a call works on, for clients that show locations: the `path` argument of the
    /// built-in file tools, made absolute against the session's directory. Clients open a
    /// location as a file, and the `path` of `ls`, `find`, `grep` or another tool may name a
    /// directory, so those calls have none.
    fn locations(&self, name: &str, args: &Value) -> Vec<ToolCallLocation> {
        if ![READ_FILE, EDIT_FILE, WRITE_FILE].contains(&name) {
            return Vec::new();
        }
        args.get("path")
            .and_then(Value::as_str)
            .map(|path| vec![ToolCallLocation::new(self.absolute(path))])
            .unwrap_or_default()
    }

    fn absolute(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// Streamed answer text, which the answer's end then need not repeat.
    fn text_streamed(&mut self, text: &str) {
        self.wrote_text = true;
        self.send(SessionUpdate::AgentMessageChunk(Self::text_chunk(text)));
    }

    /// The answer ended: its text, when none was streamed. Its calls are kept as each starts,
    /// which every call of the run does before its result.
    fn answer_ended(&self, message: &AssistantMessage) {
        let text = message.text();
        if !self.wrote_text && !text.is_empty() {
            self.send(SessionUpdate::AgentMessageChunk(Self::text_chunk(&text)));
        }
    }

    /// A call starts: the client lists it as pending.
    fn tool_started(&mut self, tool_call_id: &str, tool_name: &str, args: &Value) {
        self.calls.insert(tool_call_id.to_string(), args.clone());
        self.send(SessionUpdate::ToolCall(
            self.call(tool_call_id, tool_name, args).status(ToolCallStatus::Pending),
        ));
    }

    /// A running call reported progress: the client shows it until the result takes its place.
    fn tool_progress(&self, tool_call_id: &str, text: &str) {
        self.tool_update(
            tool_call_id,
            ToolCallUpdateFields::new().content(vec![text_content(text)]),
        );
    }

    /// A call ended: its status, what it shows and the raw output.
    fn tool_ended(&mut self, result: &ToolResultMessage) {
        let args = self.calls.remove(&result.tool_call_id);
        let (status, content, raw_output) = self.result_fields(args.as_ref(), result);
        self.tool_update(
            &result.tool_call_id,
            ToolCallUpdateFields::new()
                .status(status)
                .content(content)
                .raw_output(raw_output),
        );
    }

    /// A notice to the client, described by the error or reason behind it.
    fn notice(&self, severity: NoticeSeverity, title: impl Into<String>, description: &str) {
        self.send(SessionUpdate::Notice(
            Notice::new(severity, title).description(description.to_string()),
        ));
    }

    /// Tell the client that a compaction it asked for found nothing to summarize.
    pub(super) fn nothing_to_compact(&self) {
        self.notice(
            NoticeSeverity::Info,
            "Nothing to summarize",
            "no part of the conversation is old enough yet",
        );
    }

    /// The state of the compaction that runs as `run_id`.
    fn compaction_update(&self, run_id: &str, status: CompactionStatus, error: Option<&str>) {
        let update = CompactionUpdate::new(CompactionId::new(run_id), status);
        let update = match error {
            Some(error) => update.error(error.to_string()),
            None => update,
        };
        self.send(SessionUpdate::CompactionUpdate(update));
    }
}

/// Send `update` about `session_id` to the client.
///
/// The SDK queues it without a bound and without waiting (`send_notification` puts it on
/// an unbounded channel): a client that stops reading stdout lets streamed updates pile up
/// in memory, though the run's own events are bounded. A bound needs a send that can wait,
/// which the SDK does not offer.
pub(super) fn send(cx: &ConnectionTo<Client>, session_id: &SessionId, update: SessionUpdate) {
    if let Err(error) = cx.send_notification(SessionNotification::new(session_id.clone(), update)) {
        // The connection is going away; what was underway still ends, and its response says how.
        eprint_line(&format!("[acp] cannot send a session update: {error}"));
    }
}

impl Updates {
    /// Send what `event` of a run means to the client.
    #[expect(clippy::too_many_lines, reason = "one arm per event variant")]
    pub(super) fn emit(&mut self, event: &Event) {
        match &event.event {
            EventKind::MessageStart { role: Role::Assistant } => self.wrote_text = false,
            EventKind::MessageUpdate {
                delta: Delta::Text { text },
            } => self.text_streamed(text),
            EventKind::MessageUpdate {
                delta: Delta::Thinking { text },
            } => self.send(SessionUpdate::AgentThoughtChunk(Self::text_chunk(text))),
            EventKind::MessageEnd {
                message: Message::Assistant(message),
                ..
            } => self.answer_ended(message),
            EventKind::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => self.tool_started(tool_call_id, tool_name, args),
            // A call the policy lets through, or one the client approved, is running now.
            EventKind::ToolDecision {
                tool_call_id,
                decision: PolicyDecision::Allow,
                ..
            }
            | EventKind::ApprovalResolved {
                tool_call_id,
                approval: ApprovalResult::AllowOnce | ApprovalResult::AllowSession | ApprovalResult::Remembered,
                ..
            } => self.tool_update(
                tool_call_id,
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            ),
            EventKind::ToolExecutionUpdate {
                tool_call_id, content, ..
            } => self.tool_progress(tool_call_id, content),
            EventKind::MessageEnd {
                message: Message::ToolResult(result),
                ..
            } => self.tool_ended(result),
            EventKind::RetryScheduled {
                retry,
                max_retries,
                delay_ms,
                error,
            } => self.notice(
                NoticeSeverity::Warning,
                format!("Retrying the model request ({retry}/{max_retries}) in {delay_ms}ms"),
                error,
            ),
            EventKind::ContextOverflow { error } => self.notice(
                NoticeSeverity::Warning,
                "The context is too long for the model; summarizing before another attempt",
                error,
            ),
            EventKind::HookFailed { hook, phase, error } => {
                self.notice(NoticeSeverity::Error, format!("Hook {hook} failed in {phase}"), error);
            }
            EventKind::CompactionStart { .. } => {
                self.compaction_update(&event.run_id, CompactionStatus::InProgress, None);
            }
            EventKind::CompactionEnd { .. } => {
                self.compaction_update(&event.run_id, CompactionStatus::Completed, None);
            }
            EventKind::CompactionFailed { error, .. } => {
                self.compaction_update(&event.run_id, CompactionStatus::Failed, Some(error));
            }
            EventKind::RunStart { .. }
            | EventKind::RunEnd { .. }
            | EventKind::AttemptStart { .. }
            | EventKind::StepStart { .. }
            | EventKind::RequestStart { .. }
            | EventKind::MessageStart { .. }
            | EventKind::MessageUpdate { .. }
            | EventKind::MessageEnd { .. }
            | EventKind::ToolDecision { .. }
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::ToolExecutionEnd { .. }
            | EventKind::StepEnd { .. }
            | EventKind::AttemptEnd { .. }
            | EventKind::ContextEdited { .. }
            | EventKind::CustomEntry { .. }
            | EventKind::ToolCallRecovered { .. } => {}
        }
    }
}
