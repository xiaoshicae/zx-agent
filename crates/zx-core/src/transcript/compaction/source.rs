//! What a summary reads: the records before its boundary; and the two messages of its
//! request, built the same way when it is sent and when it is replayed from the session.

use anyhow::{Result, anyhow, ensure};

use crate::ai::{Message, SystemMessage, SystemPrompt, UserMessageKind};
use crate::transcript::{ProjectedMessage, Transcript, without_details, without_image_data};

/// The two messages of a summary request: `instructions` as the system prompt, `input` as
/// the user message.
pub(crate) fn summary_messages(
    instructions: &str,
    input: String,
    system_timestamp: u64,
    user_timestamp: u64,
) -> Vec<Message> {
    vec![
        Message::System(SystemMessage {
            sections: SystemPrompt::default().diff(&SystemPrompt::new(instructions)),
            timestamp: system_timestamp,
        }),
        Message::User {
            content: input,
            images: Vec::new(),
            timestamp: user_timestamp,
            kind: UserMessageKind::Prompt,
        },
    ]
}

/// The content of a summary request: the records it reads, without tool result details and
/// without the data of their images, which a text cannot show.
pub(crate) fn summary_input(records: &[ProjectedMessage]) -> Result<String> {
    Ok(serde_json::to_string(&without_image_data(&without_details(records)))?)
}

impl Transcript {
    /// The records a compaction reads when it starts after `parent` and keeps from
    /// `first_kept`: what the branch projects before `first_kept`, earlier summaries ignored.
    /// An error when `first_kept` is not a message of the branch, is a system message, or
    /// would split a tool exchange.
    pub(in crate::transcript) fn compaction_source(
        &self,
        parent: Option<&str>,
        first_kept: &str,
    ) -> Result<Vec<ProjectedMessage>> {
        let mut raw = Self::raw_projection_from(&self.branch_path_from(parent));
        let cut = raw
            .iter()
            .position(|item| item.entry_id == first_kept)
            .ok_or_else(|| anyhow!("its first kept entry {first_kept} is not a message of this branch"))?;
        ensure!(
            !matches!(raw[cut].message, Message::ToolResult(_) | Message::System(_)),
            "its boundary at {first_kept} would split a tool exchange or keep from a system message"
        );
        raw.truncate(cut);
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Image;

    #[test]
    fn a_summary_reads_the_media_type_of_an_image_not_its_data() {
        let image = Image {
            mime_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        };
        let records = [ProjectedMessage {
            entry_id: "a".into(),
            message: Message::user_with_images("look", vec![image]),
        }];

        let input = summary_input(&records).unwrap();

        assert!(
            input.contains(r#""images":[{"mimeType":"image/png","data":""}]"#),
            "{input}"
        );
        assert!(!input.contains("iVBORw0KGgo"), "{input}");
    }
}
