//! What arrives over the long connection, people's messages and card clicks, and the text
//! and cards the channel posts.

use serde_json::{Value, json};
use zx_ai::sync::lock;
use zx_core::agent::ApprovalResult;
use zx_im::{
    Addressee, ApprovalId, CardLabels, ChatKind, Choice, ConversationId, Destination, ImError, Inbound, MessageId,
    Seen, ThreadId, UserId,
};

use super::FeishuChannel;
use super::connection::PREFIX;

impl FeishuChannel {
    /// The messages and clicks in one event.
    pub(crate) fn events(&self, payload: &[u8], seen: &mut Seen<MessageId>) -> Vec<Inbound> {
        let value: Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(error) => {
                self.report(&ImError::Transport(format!("{PREFIX}: an event is not JSON: {error}")));
                return Vec::new();
            }
        };
        match value["header"]["event_type"].as_str() {
            Some("im.message.receive_v1") => match message(&value["event"]) {
                Some(said) if seen.insert(&said.id) => {
                    if said.chat == ChatKind::Direct {
                        lock(&self.direct).insert(said.at.conversation.0.clone());
                    }
                    vec![said.into_inbound()]
                }
                // Not for the bot, or delivered before.
                _ => Vec::new(),
            },
            Some("card.action.trigger") => decision(&value["event"]).into_iter().collect(),
            // Events this app does not subscribe to.
            _ => Vec::new(),
        }
    }
}

/// A text message from a person, as [`Inbound::Message`] carries it.
struct Said {
    id: MessageId,
    at: Destination,
    chat: ChatKind,
    addressee: Addressee,
    sender: UserId,
    text: String,
}

impl Said {
    fn into_inbound(self) -> Inbound {
        Inbound::Message {
            id: self.id,
            at: self.at,
            chat: self.chat,
            addressee: self.addressee,
            sender: self.sender,
            text: self.text,
        }
    }
}

/// A text message from a person: in a direct chat, in a group with a bot mentioned, or a
/// reply in a group's thread. Replies that do not mention the bot arrive only with the
/// permission to read every group message (`im:message.group_msg`).
fn message(event: &Value) -> Option<Said> {
    if event["sender"]["sender_type"] != "user" {
        return None;
    }
    let message = &event["message"];
    if message["message_type"] != "text" {
        return None;
    }
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
    // The content of a text message is a JSON object with `text`; anything else is not text
    // this channel reads.
    let content: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
    Some(Said {
        id: MessageId(message["message_id"].as_str()?.into()),
        at: Destination {
            conversation: ConversationId(message["chat_id"].as_str()?.into()),
            thread,
        },
        chat,
        addressee,
        sender: UserId(event["sender"]["sender_id"]["open_id"].as_str()?.into()),
        text: readable(content["text"].as_str()?, mentions),
    })
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

/// The content of a text message: a JSON object as a string.
pub(crate) fn text_content(text: &str) -> String {
    json!({ "text": text }).to_string()
}

/// A card in JSON 2.0 that can be updated, as a string.
pub(crate) fn card(elements: &[Value]) -> String {
    json!({
        "schema": "2.0",
        "config": {"update_multi": true},
        "body": {"elements": elements},
    })
    .to_string()
}

pub(crate) fn markdown(content: &str) -> Value {
    json!({"tag": "markdown", "content": content})
}

pub(crate) fn button(label: &str, kind: &str, value: &Value) -> Value {
    json!({
        "tag": "button",
        "type": kind,
        "text": {"tag": "plain_text", "content": label},
        "behaviors": [{"type": "callback", "value": value}],
    })
}

/// The result of a closed card: what happened, and who decided.
pub(crate) fn resolution(labels: &CardLabels, user: Option<&UserId>, result: ApprovalResult) -> String {
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
    fn mentions_of_bots_go_and_people_are_named() {
        let mentions = serde_json::from_value::<Vec<Value>>(json!([
            {"key": "@_user_1", "mentioned_type": "bot", "name": "zx"},
            {"key": "@_user_10", "mentioned_type": "user", "name": "Tom"},
        ]))
        .unwrap();
        assert_eq!(readable("@_user_1 ask @_user_10 now", &mentions), "ask @Tom now");
    }
}
