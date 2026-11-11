//! Assemble the final answer from recorded continuation fragments.

use crate::ai::{Message, UserMessageKind};

/// The last answer in `messages`, joined with the answers it continues: each is preceded by
/// a request to continue the one before it. System prompt updates in between are ignored.
pub(super) fn final_answer(messages: &[Message]) -> String {
    // Walked backwards: the last answer first, then what precedes it.
    let mut earlier = messages
        .iter()
        .rev()
        .filter(|message| !matches!(message, Message::System(_)));
    let mut answer = earlier.find_map(|message| match message {
        Message::Assistant(answer) => Some(answer),
        Message::System(_) | Message::User { .. } | Message::ToolResult(_) => None,
    });
    let mut parts = Vec::new();
    while let Some(part) = answer {
        parts.push(part.text());
        answer = match (earlier.next(), earlier.next()) {
            (
                Some(Message::User {
                    kind: UserMessageKind::ContinueAnswer,
                    ..
                }),
                Some(Message::Assistant(previous)),
            ) => Some(previous),
            _ => None,
        };
    }
    parts.reverse();
    let mut joined = String::new();
    for part in &parts {
        // Models often restart the cut-off line instead of continuing it.
        let repeated = overlap(&joined, part);
        joined.push_str(&part[repeated..]);
    }
    joined
}

/// Length in bytes of the longest end of `previous` that `next` starts with. Matches shorter
/// than 8 bytes are treated as coincidence, and only the last 1000 bytes are searched.
fn overlap(previous: &str, next: &str) -> usize {
    let longest = previous.len().min(next.len()).min(1000);
    (8..=longest)
        .rev()
        .find(|&len| next.is_char_boundary(len) && previous.ends_with(&next[..len]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AssistantMessage, ContentBlock, SectionChange, StopReason, SystemMessage};

    #[test]
    fn a_final_answer_joins_marked_continuations_across_system_messages() {
        let answer = |text: &str| {
            let mut message = AssistantMessage::failure("p", "m", StopReason::Stop, "");
            message.error_message = None;
            message.content = vec![ContentBlock::Text { text: text.into() }];
            Message::Assistant(message)
        };
        let update = Message::System(SystemMessage {
            sections: vec![SectionChange {
                name: "environment".into(),
                content: Some("changed".into()),
            }],
            timestamp: 0,
        });
        let messages = [
            Message::user("go"),
            answer("甲"),
            update,
            Message::continuation("continue"),
            answer("乙"),
        ];

        assert_eq!(final_answer(&messages), "甲乙");
    }

    #[test]
    fn short_overlaps_are_treated_as_coincidence() {
        assert_eq!(overlap("step 1", "1 more"), 0);
    }

    #[test]
    fn overlaps_are_measured_in_bytes_on_char_boundaries() {
        // "端口错误" is 12 bytes, so it counts even though it is only 4 characters.
        assert_eq!(overlap("数据库端口错误", "端口错误: 5433"), "端口错误".len());
    }
}
