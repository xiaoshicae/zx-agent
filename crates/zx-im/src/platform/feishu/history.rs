//! What was said before the bot's first answer in a conversation, read through the message
//! APIs: a topic, or a group since the bot was last mentioned there. Reading a group's
//! messages as the app needs the permission to read every group message
//! (`im:message.group_msg`), as the documentation of these APIs says.

use reqwest::Method;
use serde_json::Value;

use super::FeishuChannel;
use super::events::text_and_images;
use crate::platform::report;
use crate::{Addressee, Destination, ImError, MessageId, MessageRef, Said, UserId};

/// Messages asked for in one page of the message list: the most it gives (`page_size`,
/// "取值范围：1 ～ 50").
const PAGE_MESSAGES: &str = "50";

impl FeishuChannel {
    /// The topic of `at`, oldest first, at most the `limit` latest messages: its root, read by
    /// itself for the topic it names, and one page of the topic's messages, the latest first.
    /// A root that names no topic, such as a message quoted by a reply outside topics, is all
    /// there is.
    pub(super) async fn topic_history(&self, at: &Destination, limit: usize) -> Result<Vec<Said>, ImError> {
        let Some(root) = &at.thread else {
            return Ok(Vec::new());
        };
        let me = self.me().await;
        let root = self.message(&MessageId(root.0.clone())).await?;
        let mut latest = Vec::new();
        if let Some(topic) = root["thread_id"].as_str().filter(|topic| !topic.is_empty()) {
            let page = self
                .list(&[("container_id_type", "thread"), ("container_id", topic)])
                .await?;
            // The documentation does not say whether the root is among them.
            let replies = items(&page).filter(|item| item["message_id"] != root["message_id"]);
            latest.extend(replies.filter_map(|item| self.said(item, me)));
        }
        latest.extend(self.said(&root, me));
        latest.truncate(limit);
        latest.reverse();
        Ok(latest)
    }

    /// What was said in the group of `mention` itself before it, oldest first, at most the
    /// `limit` latest messages: the mention is read for its time, then one page of the
    /// group's messages up to that time, the latest first. A topic is in the page by its root
    /// only ("通过 chat 容器类型仅能获取到话题的根消息").
    pub(super) async fn group_history(&self, mention: &MessageRef, limit: usize) -> Result<Vec<Said>, ImError> {
        let me = self.me().await;
        let found = self.message(&mention.id).await?;
        let sent = sent_ms(&found)
            .ok_or_else(|| ImError::Transport(format!("feishu: message {} has no create_time", mention.id)))?;
        // `end_time` counts seconds: the one after the mention's keeps its second in, whether
        // the bound is inclusive or not; what came after it within that second is left out
        // below.
        let end = (sent / 1000 + 1).to_string();
        let chat = mention.at.conversation.0.as_str();
        let page = self
            .list(&[
                ("container_id_type", "chat"),
                ("container_id", chat),
                ("end_time", &end),
            ])
            .await?;
        let before =
            |item: &&Value| item["message_id"] != mention.id.0.as_str() && sent_ms(item).is_some_and(|at| at <= sent);
        let mut latest: Vec<Said> = items(&page)
            .filter(before)
            .filter_map(|item| self.said(item, me))
            .collect();
        latest.truncate(limit);
        latest.reverse();
        Ok(latest)
    }

    /// The message `id` as the message API gives it.
    async fn message(&self, id: &MessageId) -> Result<Value, ImError> {
        let path = format!("/open-apis/im/v1/messages/{}", id.0);
        let answer = self.api(Method::GET, &path, &[], None).await?;
        // A merged forward comes first, the messages it holds after it.
        answer["data"]["items"]
            .get(0)
            .cloned()
            .ok_or_else(|| ImError::Transport(format!("feishu {path}: no message in the answer")))
    }

    /// One page of the messages of the chat or topic `container` names, the latest first.
    async fn list(&self, container: &[(&str, &str)]) -> Result<Value, ImError> {
        let mut args = vec![("sort_type", "ByCreateTimeDesc"), ("page_size", PAGE_MESSAGES)];
        args.extend_from_slice(container);
        Ok(self.api(Method::GET, "/open-apis/im/v1/messages", &args, None).await?)
    }

    /// The bot's open_id, which a mention of it names, asked for once from the bot
    /// information API, which needs no permission, only the bot capability. `None` when it
    /// cannot be had, which is reported: no message is then taken as meant for the bot.
    async fn me(&self) -> Option<&str> {
        if let Some(me) = self.me.get() {
            return Some(me.as_str());
        }
        let path = "/open-apis/bot/v3/info";
        let answer = match self.api(Method::GET, path, &[], None).await {
            Ok(answer) => answer,
            Err(error) => {
                report(self.config.on_error.as_ref(), &ImError::from(error));
                return None;
            }
        };
        match answer["bot"]["open_id"].as_str() {
            Some(id) => Some(self.me.get_or_init(|| id.to_string()).as_str()),
            None => {
                let missing = ImError::Transport(format!("feishu {path}: no open_id in the answer"));
                report(self.config.on_error.as_ref(), &missing);
                None
            }
        }
    }

    /// What `item`, a message as the message APIs give it, says, who said it and for whom;
    /// `None` for one deleted, posted by this app, or without text, such as an image or a
    /// card.
    fn said(&self, item: &Value, me: Option<&str>) -> Option<Said> {
        let sender = &item["sender"];
        // An app posts with its App ID.
        let own = sender["sender_type"] == "app" && sender["id"] == self.config.app_id.as_str();
        if own || item["deleted"] == true {
            return None;
        }
        let mentions = item["mentions"].as_array().map(Vec::as_slice).unwrap_or_default();
        let (kind, content) = (item["msg_type"].as_str()?, item["body"]["content"].as_str()?);
        let (text, _) = text_and_images(kind, content, mentions)?;
        if text.is_empty() {
            return None;
        }
        // A mention names the open_id of whom it mentions, a bot's too.
        let addressee = if sender["sender_type"] != "user" {
            Addressee::Others
        } else if me.is_some_and(|me| mentions.iter().any(|mention| mention["id"] == me)) {
            Addressee::Bot
        } else if mentions.is_empty() {
            Addressee::Nobody
        } else {
            Addressee::Others
        };
        Some(Said {
            id: MessageId(item["message_id"].as_str()?.into()),
            sender: UserId(sender["id"].as_str()?.into()),
            text,
            addressee,
        })
    }
}

/// The messages of a list answer.
fn items(page: &Value) -> impl Iterator<Item = &Value> {
    page["data"]["items"].as_array().into_iter().flatten()
}

/// When `item` was sent, in milliseconds ("消息生成的时间戳。单位：毫秒").
fn sent_ms(item: &Value) -> Option<u64> {
    item["create_time"].as_str()?.parse().ok()
}
