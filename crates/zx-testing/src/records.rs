//! Messages and records a test writes or reads back: answers of the shapes a test needs, the
//! short text of messages for compact assertions, the requests a session rebuilds from its
//! checkpoints, and JSON lines, of a session file or of a program's output.

use std::path::Path;

use serde_json::{Value, json};
use zx_agent::ai::{AssistantMessage, ContentBlock, Message, Request, StopReason, ToolCall, Usage};
use zx_agent::transcript::{Entry, Transcript};

/// An answer by `provider` and `model` that asked for `tool` with `arguments` and was cut
/// off before the call ran: the shape a crash leaves in a session file.
pub fn tool_call_answer(provider: &str, model: &str, tool: &str, arguments: Value) -> Message {
    let mut answer = AssistantMessage::failure(provider, model, StopReason::ToolUse, "");
    answer.error_message = None;
    answer.content = vec![ContentBlock::ToolCall(ToolCall {
        id: "c1".into(),
        name: tool.into(),
        arguments,
        arguments_error: None,
    })];
    Message::Assistant(answer)
}

/// A finished answer that reported `input` prompt tokens and 10 output tokens, as the last
/// answer of a long conversation does.
pub fn answer_reporting(input: u64) -> Message {
    let mut answer = AssistantMessage::failure("p", "m", StopReason::Stop, "");
    answer.error_message = None;
    answer.content = vec![ContentBlock::Text { text: "noted".into() }];
    answer.usage = Usage {
        input,
        output: 10,
        total_tokens: input + 10,
        ..Usage::default()
    };
    Message::Assistant(answer)
}

/// A summary citing `id`, as the JSON the model answers a summary request with.
pub fn summary_json(id: &str) -> String {
    json!({"facts":[{"text":"Old observation","sourceEntryIds":[id]}],"inferences":["A guess"],"plans":["Continue"]})
        .to_string()
}

/// Every request of the session, rebuilt from its checkpoints, in order.
pub fn replayed_requests(store: &Transcript) -> Vec<Request> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(store.replay_request(&started.id).unwrap().1),
            _ => None,
        })
        .collect()
}

/// Short text of each message, for compact assertions.
pub fn summarize(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            Message::System(system) => {
                let names: Vec<&str> = system.sections.iter().map(|change| change.name.as_str()).collect();
                format!("system:{}", names.join(","))
            }
            Message::User { content, .. } => format!("user:{content}"),
            Message::Assistant(assistant) => {
                let calls: Vec<String> = assistant.tool_calls().into_iter().map(|c| c.id).collect();
                if calls.is_empty() {
                    format!("assistant:{}", assistant.text())
                } else {
                    format!("assistant:calls[{}]", calls.join(","))
                }
            }
            Message::ToolResult(result) => {
                format!(
                    "tool:{}={}{}",
                    result.tool_call_id,
                    result.content,
                    if result.is_error { "!" } else { "" }
                )
            }
        })
        .collect()
}

/// Every line of `text` parsed as JSON, as the JSON-lines outputs are read.
pub fn json_lines(text: &str) -> Vec<Value> {
    text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

/// Every line of a session file, parsed.
pub fn read_lines(path: &Path) -> Vec<Value> {
    json_lines(&std::fs::read_to_string(path).unwrap())
}
