//! Messages and records a test writes or reads back: answers of the shapes a test needs, the
//! short text of messages for compact assertions, the requests a session rebuilds from its
//! checkpoints, and JSON lines, of a session file or of a program's output.

use std::path::Path;

use serde_json::{Value, json};
use zx_agent::ai::{AssistantMessage, ContentBlock, Message, Request, StopReason, ToolCall, Usage};
use zx_agent::transcript::{Entry, Transcript};

/// The call `id` of `tool` with `arguments`, as an answer holds it.
pub fn tool_call_block(id: &str, tool: &str, arguments: Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: tool.into(),
        arguments,
        arguments_error: None,
    })
}

/// An answer by `provider` and `model` that asked for `tool` with `arguments`, as the call
/// `c1`, and was cut off before the call ran: the shape a crash leaves in a session file.
pub fn tool_call_answer(provider: &str, model: &str, tool: &str, arguments: Value) -> Message {
    Message::Assistant(AssistantMessage::new(
        provider,
        model,
        StopReason::ToolUse,
        vec![tool_call_block("c1", tool, arguments)],
    ))
}

/// A finished answer that reported `input` prompt tokens and 10 output tokens, as the last
/// answer of a long conversation does.
pub fn answer_reporting(input: u64) -> Message {
    Message::Assistant(AssistantMessage {
        usage: Usage {
            input,
            output: 10,
            total_tokens: input + 10,
            ..Usage::default()
        },
        ..AssistantMessage::new(
            "p",
            "m",
            StopReason::Stop,
            vec![ContentBlock::Text { text: "noted".into() }],
        )
    })
}

/// A summary citing `id`, as the JSON the model answers a summary request with.
pub fn summary_json(id: &str) -> String {
    json!({"facts":[{"text":"Old observation","sourceEntryIds":[id]}],"inferences":["A guess"],"plans":["Continue"]})
        .to_string()
}

/// A summary of what the summary request `request` read, citing the first record it read, as
/// [`summary_json`].
pub fn summary_of_the_source(request: &Request) -> String {
    let Some(Message::User { content, .. }) = request.messages.last() else {
        panic!("a summary request ends with its input");
    };
    let records: Vec<Value> = serde_json::from_str(content).unwrap();
    summary_json(records[0]["entryId"].as_str().unwrap())
}

/// Every request of the session, rebuilt from its checkpoints, in order.
pub fn replayed_requests(transcript: &Transcript) -> Vec<Request> {
    transcript
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::RequestStarted(started) => Some(transcript.replay_request(&started.id).unwrap().1),
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
