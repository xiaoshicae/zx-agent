//! What a summary reads: the records before its boundary; and the two messages of its
//! request, built the same way when it is sent and when it is replayed from the session.

use anyhow::{Result, ensure};

use super::SummaryRequest;
use crate::ai::{Message, PREAMBLE, SystemMessage, SystemPrompt, UserMessageKind};
use crate::session::{ProjectedMessage, SessionStore, without_details};

impl SummaryRequest {
    /// The messages of the request as it was sent: the instructions as the system prompt,
    /// `input` as the user message.
    pub(in crate::session) fn messages(
        &self,
        input: String,
        system_timestamp: u64,
        user_timestamp: u64,
    ) -> Vec<Message> {
        summary_messages(&self.instructions, input, system_timestamp, user_timestamp)
    }
}

/// The two messages of a summary request, built the same way when it is sent and when it is
/// replayed from the session.
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

/// The content of a full summary request: the records it reads, without tool result
/// details. The same when the request is sent and when it is replayed.
pub(crate) fn summary_input(records: &[ProjectedMessage]) -> Result<String> {
    Ok(serde_json::to_string(&without_details(records))?)
}

impl SessionStore {
    /// Where the retained messages start in `raw`: at `first_kept`, which must not split a
    /// tool exchange or be a system message.
    pub(in crate::session) fn retained_from(raw: &[ProjectedMessage], first_kept: &str) -> Result<usize> {
        let cut = raw
            .iter()
            .position(|item| item.entry_id == first_kept)
            .ok_or_else(|| anyhow::anyhow!("compaction retained entry not on branch"))?;
        ensure!(
            !matches!(raw[cut].message, Message::ToolResult(_) | Message::System(_)),
            "compaction cannot split a tool exchange or retain from a system message"
        );
        Ok(cut)
    }

    /// The records before `first_kept`: what a full summary read.
    pub(in crate::session) fn summarized_prefix<'a>(
        raw: &'a [ProjectedMessage],
        first_kept: &str,
    ) -> Result<&'a [ProjectedMessage]> {
        let cut = Self::retained_from(raw, first_kept)?;
        Ok(&raw[..cut])
    }
}
