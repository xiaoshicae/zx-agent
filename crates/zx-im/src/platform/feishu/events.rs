//! What arrives over the long connection, people's messages and card clicks, and the text
//! and cards the channel posts.

use serde_json::{Value, json};
use zx_agent::events::ApprovalResult;
use zx_util::sync::lock;

use super::FeishuChannel;
use super::connection::PREFIX;
use crate::platform::report;
use crate::{
    Addressee, ApprovalCard, ApprovalId, CardLabels, ChatKind, Choice, ConversationId, Destination, ImError, ImageRef,
    Inbound, MessageId, ThreadId, UserId,
};

impl FeishuChannel {
    /// The messages and clicks in one event. A message is named by its `message_id`, which
    /// the documentation says to deduplicate by (im.message.receive_v1).
    pub(super) fn events(&self, payload: &[u8]) -> Vec<Inbound> {
        let value: Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(error) => {
                report(
                    self.config.on_error.as_ref(),
                    &ImError::Transport(format!("{PREFIX}: an event is not JSON: {error}")),
                );
                return Vec::new();
            }
        };
        match value["header"]["event_type"].as_str() {
            Some("im.message.receive_v1") => match message(&value["event"]) {
                Some(said) => {
                    if let Inbound::Message {
                        chat: ChatKind::Direct,
                        at,
                        ..
                    } = &said
                    {
                        lock(&self.direct).insert(at.conversation.0.clone());
                    }
                    vec![said]
                }
                // Not for the bot.
                None => Vec::new(),
            },
            Some("card.action.trigger") => decision(&value["event"]).into_iter().collect(),
            // Events this app does not subscribe to.
            _ => Vec::new(),
        }
    }
}

/// A message from a person, of text, an image, or rich text (`post`) with text and images:
/// in a direct chat, in a group with a bot mentioned, or a reply in a group's thread. Replies
/// that do not mention the bot arrive only with the permission to read every group message
/// (`im:message.group_msg`).
fn message(event: &Value) -> Option<Inbound> {
    if event["sender"]["sender_type"] != "user" {
        return None;
    }
    let message = &event["message"];
    let kind = message["message_type"].as_str()?;
    let chat = match message["chat_type"].as_str()? {
        "p2p" => ChatKind::Direct,
        "group" => ChatKind::Group,
        _ => return None,
    };
    // No mentions at all is an empty list.
    let mentions = message["mentions"].as_array().map(Vec::as_slice).unwrap_or_default();
    // A reply names the root of its thread.
    let thread = message["root_id"]
        .as_str()
        .filter(|root| !root.is_empty())
        .map(|root| ThreadId(root.into()));
    // A mention of any bot counts as one of this bot: the event does not say which bot is
    // this app's.
    let addressee = match chat {
        ChatKind::Direct => Addressee::Bot,
        ChatKind::Group if mentions.iter().any(|mention| mention["mentioned_type"] == "bot") => Addressee::Bot,
        ChatKind::Group if mentions.is_empty() => Addressee::Nobody,
        ChatKind::Group => Addressee::Others,
    };
    // Talk in the group itself is none of the bot's business unless it is mentioned.
    if chat == ChatKind::Group && thread.is_none() && addressee != Addressee::Bot {
        return None;
    }
    let (text, images) = text_and_images(kind, message["content"].as_str()?, mentions)?;
    Some(Inbound::Message {
        id: MessageId(message["message_id"].as_str()?.into()),
        at: Destination {
            conversation: ConversationId(message["chat_id"].as_str()?.into()),
            thread,
        },
        chat,
        addressee,
        sender: UserId(event["sender"]["sender_id"]["open_id"].as_str()?.into()),
        text,
        images,
    })
}

/// The text and images of a message of type `kind`, text, image or rich text (`post`), whose
/// content is `content`, a JSON object as a string: `text` of a text message, `image_key` of
/// an image, `title` and `content` of rich text. `None` for another type, or content without
/// what its type has.
pub(super) fn text_and_images(kind: &str, content: &str, mentions: &[Value]) -> Option<(String, Vec<ImageRef>)> {
    let content: Value = serde_json::from_str(content).ok()?;
    match kind {
        "text" => Some((readable(content["text"].as_str()?, mentions), Vec::new())),
        "image" => Some((String::new(), vec![image(content["image_key"].as_str()?)])),
        "post" => rich_text(&content, mentions),
        _ => None,
    }
}

/// An image of a message, by its key; Feishu gives it no other name.
fn image(key: &str) -> ImageRef {
    ImageRef {
        key: key.into(),
        name: key.into(),
    }
}

/// The text and images of rich text: its title and paragraphs, a line each, with text,
/// links, code and mentions as a text message has them, and its images by their keys.
fn rich_text(content: &Value, mentions: &[Value]) -> Option<(String, Vec<ImageRef>)> {
    let mut lines = Vec::new();
    let title = content["title"].as_str().unwrap_or_default().trim();
    if !title.is_empty() {
        lines.push(title.to_string());
    }
    let mut images = Vec::new();
    for paragraph in content["content"].as_array()? {
        let mut line = String::new();
        for element in paragraph.as_array().map(Vec::as_slice).unwrap_or_default() {
            match element["tag"].as_str() {
                Some("text" | "a" | "code_block") => line.push_str(element["text"].as_str().unwrap_or_default()),
                // A mention names its key, as in the text of a text message.
                Some("at") => line.push_str(element["user_id"].as_str().unwrap_or_default()),
                Some("img") => images.extend(element["image_key"].as_str().map(image)),
                _ => {}
            }
        }
        if !line.trim().is_empty() {
            lines.push(line);
        }
    }
    Some((readable(&lines.join("\n"), mentions), images))
}

/// The text with mentions of bots removed and mentions of people written as `@name`. A
/// mention appears in the text as a key such as `@_user_1`; longer keys are replaced first,
/// so that `@_user_1` does not cut into `@_user_10`.
fn readable(text: &str, mentions: &[Value]) -> String {
    let mut keyed: Vec<(&str, String)> = mentions
        .iter()
        .filter_map(|mention| {
            let key = mention["key"].as_str()?;
            let shown = if mention["mentioned_type"] == "bot" {
                String::new()
            } else {
                // A mention without a name shows as a bare `@`.
                format!("@{}", mention["name"].as_str().unwrap_or_default())
            };
            Some((key, shown))
        })
        .collect();
    keyed.sort_by_key(|(key, _)| std::cmp::Reverse(key.len()));
    let mut text = text.to_string();
    for (key, shown) in keyed {
        text = text.replace(key, &shown);
    }
    text.trim().to_string()
}

/// A click on a card button posted by this channel.
fn decision(event: &Value) -> Option<Inbound> {
    let value = &event["action"]["value"];
    let choice = match value["zx"].as_str()? {
        "allow_once" => Choice::AllowOnce,
        "deny" => Choice::Deny,
        _ => return None,
    };
    let chat = event["context"]["open_chat_id"]
        .as_str()
        .or_else(|| value["chat"].as_str())?;
    Some(Inbound::Decision {
        at: Destination {
            conversation: ConversationId(chat.into()),
            thread: value["thread"].as_str().map(|thread| ThreadId(thread.into())),
        },
        user: UserId(event["operator"]["open_id"].as_str()?.into()),
        approval: ApprovalId(value["approval"].as_str()?.into()),
        choice,
    })
}

/// The content of a text message: a JSON object as a string. Mentions in the text, which
/// can come from the model and from what a tool returned to it, notify nobody.
pub(super) fn text_content(text: &str) -> String {
    json!({ "text": without_mentions(text) }).to_string()
}

/// Put between characters that Feishu would read as markup, so that they are shown as text.
const ZERO_WIDTH_SPACE: char = '\u{200B}';

/// `text` with every mention tag (`<at …>`, which notifies people, everyone with
/// `user_id="all"`) broken by a zero-width space after its `<`.
fn without_mentions(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('<') {
        shown.push_str(&rest[..=index]);
        rest = &rest[index + 1..];
        if rest.get(..2).is_some_and(|tag| tag.eq_ignore_ascii_case("at")) {
            shown.push(ZERO_WIDTH_SPACE);
        }
    }
    shown.push_str(rest);
    shown
}

/// `text` with no run of three backticks, which would end the code block of a card that
/// shows it: a zero-width space goes between every two.
fn without_fences(text: &str) -> String {
    text.replace("``", &format!("`{ZERO_WIDTH_SPACE}`"))
}

/// What a card shows of the request `card`, open and once closed. What the request says
/// mentions nobody, and its summary stays in the code block.
pub(super) fn card_request(labels: &CardLabels, card: &ApprovalCard) -> String {
    format!(
        "**{}**: `{}`\n```\n{}\n```\n{}: {}",
        labels.title,
        without_mentions(&card.tool),
        without_fences(&without_mentions(&card.summary)),
        labels.reason,
        without_mentions(&card.reason)
    )
}

/// A card in JSON 2.0 that can be updated, as a string.
pub(super) fn card(elements: &[Value]) -> String {
    json!({
        "schema": "2.0",
        "config": {"update_multi": true},
        "body": {"elements": elements},
    })
    .to_string()
}

pub(super) fn markdown(content: &str) -> Value {
    json!({"tag": "markdown", "content": content})
}

pub(super) fn button(label: &str, kind: &str, value: &Value) -> Value {
    json!({
        "tag": "button",
        "type": kind,
        "text": {"tag": "plain_text", "content": label},
        "behaviors": [{"type": "callback", "value": value}],
    })
}

/// The result of a closed card: what happened, and who decided.
pub(super) fn resolution(labels: &CardLabels, user: Option<&UserId>, result: ApprovalResult) -> String {
    let what = labels.outcome(result);
    match user {
        Some(user) => format!("{what} ({user})"),
        None => what.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_text_reads_as_lines_with_its_images_apart() {
        let mentions = serde_json::from_value::<Vec<Value>>(json!([
            {"key": "@_user_1", "mentioned_type": "bot", "name": "zx"},
        ]))
        .unwrap();
        let content = json!({"title": "Crash", "content": [
            [{"tag": "at", "user_id": "@_user_1"}, {"tag": "text", "text": " what is this?"}],
            [{"tag": "img", "image_key": "img_1"}],
            [{"tag": "a", "text": "logs", "href": "https://example.com"}, {"tag": "emotion", "emoji_type": "SMILE"}],
        ]});
        let (text, images) = rich_text(&content, &mentions).unwrap();
        assert_eq!(text, "Crash\n what is this?\nlogs");
        assert_eq!(images, [image("img_1")]);
    }

    #[test]
    fn mentions_of_bots_go_and_people_are_named() {
        let mentions = serde_json::from_value::<Vec<Value>>(json!([
            {"key": "@_user_1", "mentioned_type": "bot", "name": "zx"},
            {"key": "@_user_10", "mentioned_type": "user", "name": "Tom"},
        ]))
        .unwrap();
        assert_eq!(readable("@_user_1 ask @_user_10 now", &mentions), "ask @Tom now");
    }
}
