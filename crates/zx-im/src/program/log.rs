//! The operator log: the server's events, what the channels go on after, and the program's
//! reloads of im.toml, as JSON lines on stdout, for the operator's log collection.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use zx_agent::ai::now_ms;
use zx_agent::events::RunOutcome;
use zx_util::stdio::print_line;

use crate::{ConversationId, ErrorReport, ImError, RunEnd, RunRecord, ServerEvent, ServerSink, SessionKey};

/// Reports what the channel of `platform` goes on after as `failed` lines, like the server's
/// own.
pub(super) fn failure_log(platform: &str) -> ErrorReport {
    let platform = platform.to_string();
    Arc::new(move |error: &ImError| {
        let line = json!({"type": "failed", "platform": platform, "error": error.to_string(), "timeMs": now_ms()});
        print_line(&line.to_string());
    })
}

/// Log that a reload of im.toml replaced the lists: with how many chats and approvers.
pub(super) fn reloaded(conversations: usize, approvers: usize) {
    let line = json!({"type": "reloaded", "conversations": conversations, "approvers": approvers, "timeMs": now_ms()});
    print_line(&line.to_string());
}

/// What a `deferred` line says of a chat that a reload left off the lists.
const SERVICE_WAITS: &str =
    "serving the chat waits for a restart: its settings in [[chats]] are not the ones in effect";
/// What a `deferred` line says of a served chat whose own settings the file changes.
const SETTINGS_WAIT: &str =
    "its settings in [[chats]] change at a restart: the chat is served with the ones in effect until then";

/// Log that a reload of im.toml left the chat `chat` on `platform` off the lists: the file
/// newly serves it, with other settings of its own than those in effect, so it is served
/// after a restart.
pub(super) fn held_back(platform: &str, chat: &ConversationId) {
    print_line(&deferred_line(platform, chat, SERVICE_WAITS).to_string());
}

/// Log that a reload of im.toml goes on serving the chat `chat` on `platform` with its own
/// settings in effect: the file changes them, which takes effect at a restart.
pub(super) fn settings_wait(platform: &str, chat: &ConversationId) {
    print_line(&deferred_line(platform, chat, SETTINGS_WAIT).to_string());
}

/// The `deferred` line of the chat `chat` on `platform`: `change` is what a reload of im.toml
/// left for a restart there.
fn deferred_line(platform: &str, chat: &ConversationId, change: &str) -> Value {
    json!({"type": "deferred", "platform": platform, "conversation": chat.0, "change": change, "timeMs": now_ms()})
}

/// Log a failure of the program's own, of no platform and no conversation.
pub(super) fn failed(error: &str) {
    let line = json!({"type": "failed", "error": error, "timeMs": now_ms()});
    print_line(&line.to_string());
}

/// The thread of `key` in a log line: `null` for a direct chat.
fn thread(key: &SessionKey) -> Value {
    json!(key.thread.as_ref().map(|thread| thread.0.as_str()))
}

/// The `run` line of `record`: where it ran, how it ended, and what it used.
fn run_line(record: &RunRecord) -> Value {
    let usage = &record.usage;
    let mut line = Map::new();
    line.insert("type".into(), json!("run"));
    line.insert("platform".into(), json!(record.key.platform));
    line.insert("conversation".into(), json!(record.key.conversation.0));
    line.insert("thread".into(), thread(&record.key));
    insert_end(&mut line, record.end);
    line.insert("modelRequests".into(), json!(record.model_requests));
    line.insert("inputTokens".into(), json!(usage.input));
    line.insert("outputTokens".into(), json!(usage.output));
    line.insert("cacheReadTokens".into(), json!(usage.cache_read));
    line.insert("cacheWriteTokens".into(), json!(usage.cache_write));
    line.insert("cost".into(), json!(usage.total_cost));
    line.insert("durationMs".into(), json!(record.duration_ms));
    line.insert("timeMs".into(), json!(now_ms()));
    Value::Object(line)
}

/// Put into `line` what a run was and how it `end`ed: `work` (`prompt` or `compaction`),
/// `outcome`, and for a budget that ran out, `budget` and `truncated`.
fn insert_end(line: &mut Map<String, Value>, end: RunEnd) {
    match end {
        RunEnd::Prompt(outcome) => {
            line.insert("work".into(), json!("prompt"));
            line.insert("outcome".into(), json!(outcome.name()));
            if let RunOutcome::BudgetExhausted { budget, truncated } = outcome {
                line.insert("budget".into(), json!(budget.name()));
                line.insert("truncated".into(), json!(truncated));
            }
        }
        RunEnd::Compaction(outcome) => {
            line.insert("work".into(), json!("compaction"));
            line.insert("outcome".into(), json!(outcome));
        }
    }
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
            ServerEvent::Triggered { key, source } => json!({
                "type": "triggered",
                "platform": key.platform,
                "conversation": key.conversation.0,
                "thread": thread(key),
                "source": source,
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
            ServerEvent::Ran(record) => run_line(record),
        };
        print_line(&line.to_string());
    }
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Usage;
    use zx_agent::compaction;
    use zx_agent::events::Budget;

    use super::*;
    use crate::ThreadId;

    /// A run in a thread of slack:C1 that ended as `end`.
    fn record(end: RunEnd) -> RunRecord {
        RunRecord {
            key: SessionKey {
                platform: "slack".into(),
                conversation: ConversationId("C1".into()),
                thread: Some(ThreadId("t1".into())),
            },
            end,
            model_requests: 3,
            usage: Usage {
                input: 100,
                output: 20,
                cache_read: 50,
                total_cost: Some(0.25),
                ..Usage::default()
            },
            duration_ms: 1500,
        }
    }

    #[test]
    fn a_run_line_says_where_the_run_was_how_it_ended_and_what_it_used() {
        let exhausted = RunOutcome::BudgetExhausted {
            budget: Budget::Cost,
            truncated: false,
        };
        let mut line = run_line(&record(RunEnd::Prompt(exhausted)));
        let time = line.as_object_mut().unwrap().remove("timeMs").unwrap();
        assert!(time.as_u64().unwrap() > 0);
        assert_eq!(
            line,
            json!({
                "type": "run", "platform": "slack", "conversation": "C1", "thread": "t1",
                "work": "prompt", "outcome": "budget_exhausted", "budget": "cost_budget", "truncated": false,
                "modelRequests": 3, "inputTokens": 100, "outputTokens": 20, "cacheReadTokens": 50,
                "cacheWriteTokens": 0, "cost": 0.25, "durationMs": 1500,
            })
        );
        let summary = run_line(&record(RunEnd::Compaction(compaction::Outcome::Noop)));
        assert_eq!(
            (&summary["work"], &summary["outcome"], summary.get("budget")),
            (&json!("compaction"), &json!("noop"), None)
        );
    }

    #[test]
    fn a_deferred_line_names_the_chat_and_says_what_waits_for_a_restart() {
        let mut line = deferred_line("slack", &ConversationId("C2".into()), SERVICE_WAITS);
        let time = line.as_object_mut().unwrap().remove("timeMs").unwrap();
        assert!(time.as_u64().unwrap() > 0);
        assert_eq!(
            line,
            json!({"type": "deferred", "platform": "slack", "conversation": "C2", "change": SERVICE_WAITS})
        );
    }
}
