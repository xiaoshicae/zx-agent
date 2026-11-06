//! What arrives over the long connection, messages addressed to the bot and card clicks,
//! and the text and cards the channel posts.

use serde_json::{Value, json};
use zx_ai::sync::lock;
use zx_core::agent::ApprovalResult;
use zx_im::{
    ApprovalId, CardLabels, Choice, ConversationId, Destination, ImError, Inbound, MessageId, Seen, ThreadId, UserId,
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
                Some(addressed) if seen.insert(&addressed.id) => {
                    if addressed.direct {
                        lock(&self.direct).insert(addressed.at.conversation.0.clone());
                    }
                    vec![addressed.into_inbound()]
                }
                // Not addressed to the bot, or delivered before.
                _ => Vec::new(),
            },
            Some("card.action.trigger") => decision(&value["event"]).into_iter().collect(),
            // Events this app does not subscribe to.
            _ => Vec::new(),
        }
    }
}

/// A text message addressed to the bot: what [`Inbound::Message`] carries, and whether it
/// came from a direct chat.
struct Addressed {
    id: MessageId,
    at: Destination,
    sender: UserId,
    text: String,
    direct: bool,
}

impl Addressed {
    fn into_inbound(self) -> Inbound {
        Inbound::Message {
            id: self.id,
            at: self.at,
            sender: self.sender,
            text: self.text,
        }
    }
}

/// A text message from a person: in a direct chat, or in a group with a bot mentioned.
fn message(event: &Value) -> Option<Addressed> {
    if event["sender"]["sender_type"] != "user" {
        return None;
    }
    let message = &event["message"];
    if message["message_type"] != "text" {
        return None;
    }
    let direct = match message["chat_type"].as_str()? {
        "p2p" => true,
        "group" => false,
        _ => return None,
    };
    // No mentions at all is an empty list.
    let mentions = message["mentions"].as_array().map(Vec::as_slice).unwrap_or_default();
    // With only the permission to read mentions of the bot, every group message delivered
    // mentions it; the check also keeps other group messages out.
    if !direct && !mentions.iter().any(|mention| mention["mentioned_type"] == "bot") {
        return None;
    }
    // The content of a text message is a JSON object with `text`; anything else is not text
    // this channel reads.
    let content: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
    Some(Addressed {
        id: MessageId(message["message_id"].as_str()?.into()),
        at: Destination {
            conversation: ConversationId(message["chat_id"].as_str()?.into()),
            // A reply names the root of its thread.
            thread: message["root_id"]
                .as_str()
                .filter(|root| !root.is_empty())
                .map(|root| ThreadId(root.into())),
        },
        sender: UserId(event["sender"]["sender_id"]["open_id"].as_str()?.into()),
        text: readable(content["text"].as_str()?, mentions),
        direct,
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
