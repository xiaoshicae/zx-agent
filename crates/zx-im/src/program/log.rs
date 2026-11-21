//! The operator log: the server's events, and what the channels go on after, as JSON lines on
//! stdout, for the operator's log collection.

use std::sync::Arc;

use serde_json::{Value, json};
use zx_agent::ai::now_ms;
use zx_util::stdio::print_line;

use crate::{ErrorReport, ImError, ServerEvent, ServerSink, SessionKey};

/// Reports what the channel of `platform` goes on after as `failed` lines, like the server's
/// own.
pub(super) fn failure_log(platform: &str) -> ErrorReport {
    let platform = platform.to_string();
    Arc::new(move |error: &ImError| {
        let line = json!({"type": "failed", "platform": platform, "error": error.to_string(), "timeMs": now_ms()});
        print_line(&line.to_string());
    })
}

/// The thread of `key` in a log line: `null` for a direct chat.
fn thread(key: &SessionKey) -> Value {
    json!(key.thread.as_ref().map(|thread| thread.0.as_str()))
}

/// Writes the server's events as JSON lines on stdout, for the operator's log collection.
pub(super) struct Log;

impl ServerSink for Log {
    fn emit(&self, event: &ServerEvent) {
        let line = match event {
            ServerEvent::Approval(record) => json!({
                "type": "approval",
                "platform": record.key.platform,
                "conversation": record.key.conversation.0,
                "thread": thread(&record.key),
                "toolCallId": record.tool_call_id,
                "tool": record.tool,
                "approval": record.approval.as_ref().map(|id| id.0.as_str()),
                "user": record.user.as_ref().map(|user| user.0.as_str()),
                "result": record.result,
                "endedMs": record.ended_ms,
            }),
            ServerEvent::Released { key } => json!({
                "type": "released",
                "platform": key.platform,
                "conversation": key.conversation.0,
                "thread": thread(key),
                "timeMs": now_ms(),
            }),
            ServerEvent::Unauthorized { key, user } => json!({
                "type": "unauthorized",
                "platform": key.platform,
                "conversation": key.conversation.0,
                "thread": thread(key),
                "user": user.0,
                "timeMs": now_ms(),
            }),
            ServerEvent::Failed { key, error } => {
                let mut line = json!({"type": "failed", "error": error, "timeMs": now_ms()});
                if let (Some(key), Value::Object(fields)) = (key, &mut line) {
                    fields.insert("platform".into(), json!(key.platform));
                    fields.insert("conversation".into(), json!(key.conversation.0));
                    fields.insert("thread".into(), thread(key));
                }
                line
            }
        };
        print_line(&line.to_string());
    }
}
