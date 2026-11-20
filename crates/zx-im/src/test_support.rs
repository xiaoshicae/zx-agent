//! What the unit tests of several modules share: the names of the files a store wrote, and a
//! message as the backlog names it.

use std::path::Path;

use crate::{ConversationId, MessageId, MessageKey};

/// The names of the files in `dir`, sorted.
pub(crate) fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The message `message` in the chat `C1` of Slack, as the backlog names it.
pub(crate) fn message_key(message: &str) -> MessageKey {
    MessageKey {
        platform: "slack".into(),
        conversation: ConversationId("C1".into()),
        message: MessageId(message.into()),
    }
}
