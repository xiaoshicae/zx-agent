//! Run events as `session/update` notifications, and the replay of a loaded session's
//! conversation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CompactionId, CompactionStatus, CompactionUpdate, Content, ContentBlock, ContentChunk, Diff, Notice,
    NoticeSeverity, SessionId, SessionNotification, SessionUpdate, TextContent, ToolCall, ToolCallContent, ToolCallId,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::Value;

use zx_agent::agent::{AgentEvent, ApprovalResult, Event, EventSink, ToolVerdict};
use zx_agent::ai::{
    AssistantMessage, ContentBlock as MessageBlock, Delta, Message, ToolCall as MessageCall, ToolResultMessage,
    UserMessageKind,
};
use zx_agent::session::{Entry, ProjectedMessage, SessionStore};
use zx_agent::sync::lock;
use zx_agent::tools::{EDIT_FILE, WRITE_FILE};

use super::tool_kind;
use crate::render::call_label;

/// Run events as `session/update` notifications.
pub(super) struct Updates {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    cwd: PathBuf,
    state: Mutex<UpdateState>,
}

#[derive(Default)]
struct UpdateState {
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
            state: Mutex::new(UpdateState::default()),
        }
    }

    fn send(&self, update: SessionUpdate) {
        send(&self.cx, &self.session_id, update);
    }

    fn text_chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
    }

    /// Replay the conversation `store` holds to a client that loads the session: what it
    /// would have seen as the turns ran, without the streaming.
    ///
    /// The person's prompts, the answers with their thoughts, and the tool calls with their
    /// results; an extension's message when it asked to be shown. Summaries and the model's
    /// own continuations are the model's, not the person's.
    pub(super) fn replay(&self, store: &SessionStore) {
        let projection = store.projection();
        for (index, item) in projection.iter().enumerate() {
            match &item.message {
                Message::User {
                    content,
                    kind: UserMessageKind::Prompt,
                    ..
                } => self.send(SessionUpdate::UserMessageChunk(Self::text_chunk(content))),
                Message::User {
                    content,
                    kind: UserMessageKind::Custom,
                    ..
                } => {
                    if let Some(Entry::CustomMessage(entry)) = store.get(&item.entry_id)
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
                                id, name, arguments, ..
                            }) => {
                                let call = self.recorded_call(id, name, arguments, &projection[index + 1..]);
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
    fn recorded_call(&self, id: &str, name: &str, arguments: &Value, later: &[ProjectedMessage]) -> ToolCall {
        let call = self.call(id, name, arguments);
        let result = later.iter().find_map(|item| match &item.message {
            Message::ToolResult(result) if result.tool_call_id == id => Some(result),
            _ => None,
        });
        match result {
            Some(result) => {
                let (status, content, raw_output) = self.result_fields(name, Some(arguments), result);
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
            .locations(self.locations(args))
    }

    /// What a result reports: its status, what the client shows, and the raw output.
    fn result_fields(
        &self,
        name: &str,
        args: Option<&Value>,
        result: &ToolResultMessage,
    ) -> (ToolCallStatus, Vec<ToolCallContent>, Value) {
        let status = if result.is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        };
        let content = self.result_content(name, args, &result.content, result.details.as_ref());
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

    /// Where a call works, for clients that show locations: the `path` argument, made
    /// absolute against the session's directory.
    fn locations(&self, args: &Value) -> Vec<ToolCallLocation> {
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

    /// What a result shows: a write as a diff of the new file, an edit as its patch, anything
    /// else as the text the model received.
    fn result_content(
        &self,
        name: &str,
        args: Option<&Value>,
        content: &str,
        details: Option<&Value>,
    ) -> Vec<ToolCallContent> {
        let mut shown = Vec::new();
        if name == WRITE_FILE
            && let Some(args) = args
            && let (Some(path), Some(text)) = (
                args.get("path").and_then(Value::as_str),
                args.get("content").and_then(Value::as_str),
            )
        {
            shown.push(ToolCallContent::Diff(Diff::new(self.absolute(path), text)));
        }
        let patch = details.and_then(|details| details.get("patch")).and_then(Value::as_str);
        let text = match patch {
            Some(patch) if name == EDIT_FILE => patch,
            Some(_) | None => content,
        };
        if !text.is_empty() {
            shown.push(ToolCallContent::Content(Content::new(ContentBlock::Text(
                TextContent::new(text),
            ))));
        }
        shown
    }

    /// Streamed answer text, which the answer's end then need not repeat.
    fn text_streamed(&self, text: &str) {
        lock(&self.state).wrote_text = true;
        self.send(SessionUpdate::AgentMessageChunk(Self::text_chunk(text)));
    }

    /// The answer ended: its text when none was streamed, and the arguments of its calls
    /// kept for their results.
    fn answer_ended(&self, message: &AssistantMessage) {
        let text = message.text();
        let wrote_text = {
            let mut state = lock(&self.state);
            for block in &message.content {
                if let MessageBlock::ToolCall(MessageCall { id, arguments, .. }) = block {
                    state.calls.insert(id.clone(), arguments.clone());
                }
            }
            state.wrote_text
        };
        if !wrote_text && !text.is_empty() {
            self.send(SessionUpdate::AgentMessageChunk(Self::text_chunk(&text)));
        }
    }

    /// A call starts: the client lists it as pending.
    fn tool_started(&self, tool_call_id: &str, tool_name: &str, args: &Value) {
        lock(&self.state).calls.insert(tool_call_id.to_string(), args.clone());
        self.send(SessionUpdate::ToolCall(
            self.call(tool_call_id, tool_name, args).status(ToolCallStatus::Pending),
        ));
    }

    /// A call ended: its status, what it shows and the raw output.
    fn tool_ended(&self, result: &ToolResultMessage) {
        let args = lock(&self.state).calls.remove(&result.tool_call_id);
        let (status, content, raw_output) = self.result_fields(&result.tool_name, args.as_ref(), result);
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

    /// The state of the summary that runs as `run_id`.
    fn compaction_update(&self, run_id: &str, status: CompactionStatus) {
        self.send(SessionUpdate::CompactionUpdate(CompactionUpdate::new(
            CompactionId::new(run_id),
            status,
        )));
    }
}

/// Send `update` about `session_id` to the client.
pub(super) fn send(cx: &ConnectionTo<Client>, session_id: &SessionId, update: SessionUpdate) {
    if let Err(error) = cx.send_notification(SessionNotification::new(session_id.clone(), update)) {
        // The connection is going away; what was underway still ends, and its response says how.
        eprintln!("[acp] cannot send a session update: {error}");
    }
}

impl EventSink for Updates {
    fn emit(&self, event: &Event) {
        match &event.event {
            AgentEvent::MessageStart { role: "assistant" } => lock(&self.state).wrote_text = false,
            AgentEvent::MessageUpdate {
                delta: Delta::Text { text },
            } => self.text_streamed(text),
            AgentEvent::MessageUpdate {
                delta: Delta::Thinking { text },
            } => self.send(SessionUpdate::AgentThoughtChunk(Self::text_chunk(text))),
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
                ..
            } => self.answer_ended(message),
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => self.tool_started(tool_call_id, tool_name, args),
            // A call the policy lets through, or one the client approved, is running now.
            AgentEvent::ToolDecision {
                tool_call_id,
                verdict: ToolVerdict::Allow,
                ..
            }
            | AgentEvent::ApprovalResolved {
                tool_call_id,
                approval: ApprovalResult::AllowOnce | ApprovalResult::AllowSession | ApprovalResult::Remembered,
                ..
            } => self.tool_update(
                tool_call_id,
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            ),
            AgentEvent::MessageEnd {
                message: Message::ToolResult(result),
                ..
            } => self.tool_ended(result),
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => self.notice(
                NoticeSeverity::Warning,
                format!("Retrying the model request ({attempt}/{max_retries}) in {delay_ms}ms"),
                error,
            ),
            AgentEvent::ContextOverflow { error } => self.notice(
                NoticeSeverity::Warning,
                "The context is too long for the model; summarizing before another attempt",
                error,
            ),
            AgentEvent::HookFailed { hook, phase, error } => {
                self.notice(NoticeSeverity::Error, format!("Hook {hook} failed in {phase}"), error);
            }
            AgentEvent::CompactionStart { .. } => self.compaction_update(&event.run_id, CompactionStatus::InProgress),
            AgentEvent::CompactionEnd { .. } => self.compaction_update(&event.run_id, CompactionStatus::Completed),
            AgentEvent::CompactionFailed { error, .. } => self.send(SessionUpdate::CompactionUpdate(
                CompactionUpdate::new(CompactionId::new(event.run_id.as_str()), CompactionStatus::Failed)
                    .error(error.clone()),
            )),
            AgentEvent::RunStart { .. }
            | AgentEvent::RunEnd { .. }
            | AgentEvent::AgentStart { .. }
            | AgentEvent::TurnStart { .. }
            | AgentEvent::RequestStart { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::ToolExecutionUpdate { .. }
            | AgentEvent::ToolDecision { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::ToolExecutionEnd { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::AgentEnd { .. }
            | AgentEvent::ContextEdited { .. }
            | AgentEvent::CustomEntry { .. }
            | AgentEvent::ToolCallRecovered { .. } => {}
        }
    }
}
