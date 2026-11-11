//! What a summary reads: the records before its boundary; and the two messages of its
//! request, built the same way when it is sent and when it is replayed from the session.

use anyhow::{Result, anyhow, ensure};

use crate::ai::{Message, PREAMBLE, SystemMessage, SystemPrompt, UserMessageKind};
use crate::session::{ProjectedMessage, SessionStore, without_details};

/// The two messages of a summary request: `instructions` as the system prompt, `input` as
/// the user message. Built the same way when the request is sent and when it is replayed
/// from the session.
pub(crate) fn summary_messages(
    instructions: &str,
    input: String,
    system_timestamp: u64,
    user_timestamp: u64,
) -> Vec<Message> {
    vec![
        Message::System(SystemMessage {
            sections: SystemPrompt::default().diff(&SystemPrompt::default().with_section(PREAMBLE, instructions)),
            timestamp: system_timestamp,
        }),
        Message::User {
            content: input,
            timestamp: user_timestamp,
            kind: UserMessageKind::Prompt,
        },
    ]
}

/// The content of a summary request: the records it reads, without tool result details.
/// The same when the request is sent and when it is replayed.
pub(crate) fn summary_input(records: &[ProjectedMessage]) -> Result<String> {
    Ok(serde_json::to_string(&without_details(records))?)
}

impl SessionStore {
    /// The records a compaction reads when it starts after `parent` and keeps from
    /// `first_kept`: what the branch projects before `first_kept`, earlier summaries ignored.
    /// An error when `first_kept` is not a message of the branch, is a system message, or
    /// would split a tool exchange.
    pub(in crate::session) fn compaction_source(
        &self,
        parent: Option<&str>,
        first_kept: &str,
    ) -> Result<Vec<ProjectedMessage>> {
        let mut raw = Self::raw_projection_from(&self.branch_path_from(parent));
        let cut = raw
            .iter()
            .position(|item| item.entry_id == first_kept)
            .ok_or_else(|| anyhow!("compaction retained entry not on branch"))?;
        ensure!(
            !matches!(raw[cut].message, Message::ToolResult(_) | Message::System(_)),
            "compaction cannot split a tool exchange or retain from a system message"
        );
        raw.truncate(cut);
        Ok(raw)
    }
}
