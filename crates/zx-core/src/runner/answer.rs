//! Assemble the final answer from recorded continuation fragments.

use crate::ai::{Message, UserMessageKind};

/// Join explicit continuation fragments, ignoring intervening system prompt updates.
pub(super) fn final_answer(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    let mut end = messages.len();
    while let Some((index, answer)) =
        messages[..end]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| match message {
                Message::Assistant(answer) => Some((index, answer)),
                Message::System(_) | Message::User { .. } | Message::ToolResult(_) => None,
            })
    {
        parts.push(answer.text());
        let mut preceding = messages[..index]
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| !matches!(message, Message::System(_)));
        match (preceding.next(), preceding.next()) {
            (
                Some((
                    _,
                    Message::User {
                        kind: UserMessageKind::ContinueAnswer,
                        ..
                    },
                )),
                Some((previous, Message::Assistant(_))),
            ) => end = previous + 1,
            _ => break,
        }
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
pub(super) fn overlap(previous: &str, next: &str) -> usize {
    let longest = previous.len().min(next.len()).min(1000);
    (8..=longest)
        .rev()
        .find(|&len| next.is_char_boundary(len) && previous.ends_with(&next[..len]))
        .unwrap_or(0)
}
