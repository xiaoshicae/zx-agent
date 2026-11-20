//! What the server says besides the model's answers: the reply texts, and the prompt section
//! that tells the model how its answers reach people.

use serde::Deserialize;

/// The prompt section that tells the model what it cannot see of a conversation the server
/// runs: whose words a message carries, and that of all it writes in a run only the last
/// answer is posted, in place of [`Replies::working`]. The agent's instructions name no host,
/// so the host that builds the agent adds it.
pub(crate) const CONVERSATION_SECTION: &str = "conversation";
pub(crate) const CONVERSATION: &str = "You talk with people through a chat platform. In a group several people write, \
     and each message starts with its sender's name. Only your final answer to a message is posted there, not the \
     text you write before a tool call nor a tool's output, so the final answer must be complete on its own.";

/// Texts the server posts besides the model's answers. The defaults are English; a
/// deployment sets its users' language, for example in im.toml, the configuration of the
/// program `zx-im` (`[replies]`, fields in camelCase, each optional).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Replies {
    /// Posted when a run starts; replaced by the answer when it ends.
    pub working: String,
    /// Posted when a conversation waits for a free run slot.
    pub queued: String,
    /// The server or conversation has no capacity for this message; it was not accepted.
    pub overloaded: String,
    /// The only answer in a chat the server does not serve.
    pub unauthorized: String,
    /// The answer to a click by someone who is not an approver.
    pub not_approver: String,
    /// A run cancelled with `/cancel`, and the answer to `/cancel` while waiting for a slot.
    pub cancelled: String,
    /// The answer to `/cancel` when nothing runs.
    pub nothing_to_cancel: String,
    /// The answer to `/new`: the conversation goes on in a new session.
    pub new_session: String,
    /// `/compact` summarized the earlier part of the conversation.
    pub compacted: String,
    /// The answer to `/compact` when nothing is old enough to summarize.
    pub nothing_to_compact: String,
    /// The answer to `/compact` while a task runs or waits, or no run slot is free.
    pub busy: String,
    /// A run that completed without text.
    pub no_answer: String,
    /// Appended to an answer cut off at the output limit.
    pub truncated: String,
    /// Appended when the model request budget ran out.
    pub model_request_limit: String,
    /// Appended when the token budget ran out.
    pub token_budget: String,
    /// Appended when the cost budget ran out.
    pub cost_budget: String,
    /// A run that reached its time limit.
    pub timed_out: String,
    /// Followed by the error of a failed run.
    pub failed: String,
    /// Followed by the error of a session that could not be opened.
    pub cannot_open: String,
    /// A run that the stop cancelled.
    pub stopped: String,
    /// Replaces, as the server starts, a status that an earlier process left behind when it
    /// ended without replacing it; see [`ServerConfig::statuses`](super::ServerConfig::statuses).
    pub interrupted: String,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            working: "Working…".into(),
            queued: "Queued: as many tasks as allowed are running; this one starts when one ends.".into(),
            overloaded:
                "Too many pending messages or conversations; this message was not accepted. Please try again later."
                    .into(),
            unauthorized: "Not authorized: this chat is not on the service's list.".into(),
            not_approver: "Only listed approvers can allow or deny; this click has no effect.".into(),
            cancelled: "Cancelled.".into(),
            nothing_to_cancel: "Nothing is running.".into(),
            new_session: "Started a new conversation; earlier messages are no longer in its context.".into(),
            compacted: "Summarized the earlier part of the conversation; the summary now stands in for it.".into(),
            nothing_to_compact: "Nothing to summarize yet.".into(),
            busy: "A task is running; send this again once it has ended.".into(),
            no_answer: "(no answer)".into(),
            truncated: "(the answer reached the output limit and was cut off)".into(),
            model_request_limit: "(the model request limit was reached; the task is not finished)".into(),
            token_budget: "(the token budget was reached; the task is not finished)".into(),
            cost_budget: "(the cost budget was reached; the task is not finished)".into(),
            timed_out: "The run timed out; the task is not finished.".into(),
            failed: "The run failed: ".into(),
            cannot_open: "Cannot open the session: ".into(),
            stopped: "The service is stopping: this task was cancelled; please send it again later.".into(),
            interrupted: "The service restarted: this task did not finish; please send it again.".into(),
        }
    }
}
