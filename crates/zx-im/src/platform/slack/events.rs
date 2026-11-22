//! What arrives over Socket Mode, messages and button clicks, a thread's history,
//! and the escaped text and blocks the channel posts.

use serde_json::{Value, json};
use zx_agent::events::ApprovalResult;

use super::{ALLOW_ACTION, DENY_ACTION, MAX_SECTION_CHARS};
use crate::platform::split_text;
use crate::{
    Addressee, ApprovalCard, ApprovalId, CardLabels, ChatKind, Choice, ConversationId, Destination, ImageRef, Inbound,
    MessageId, Said, ThreadId, UserId,
};

/// The types of the files passed on as images: those the model APIs take.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The bot as `auth.test` names it, which tells its own posts from everyone else's.
#[derive(Default)]
pub(super) struct Me {
    /// Its user ID, which a mention of it names.
    pub(super) user: Option<String>,
    /// Its bot ID, which its posts carry, some of them without its user ("bot_message": "it
    /// has no associated user"). Other bots are heard only once it is known.
    pub(super) bot: Option<String>,
}

impl Me {
    /// Whether `message` is said by someone else: not an edit or a join, not the bot's own
    /// post, and another bot's only when the bot can tell it from its own.
    fn hears(&self, message: &Value) -> bool {
        let mine = |own: &Option<String>, field: &str| own.is_some() && message[field].as_str() == own.as_deref();
        let own = mine(&self.user, "user") || mine(&self.bot, "bot_id");
        is_talk(message) && !own && (message.get("bot_id").is_none() || self.bot.is_some())
    }
}

/// A message to pass on: a direct message, a channel message that mentions the bot, or a
/// reply in a channel's thread. Another bot's reply in a thread is passed on as meant for
/// others, whatever it mentions: bots are heard, and start no conversation and no answer.
///
/// A mention can arrive twice, as `app_mention` and as a channel `message`; both are the
/// message with its channel and `ts`, which the backlog knows the second time.
pub(super) fn message(payload: &Value, me: &Me) -> Option<Inbound> {
    // Socket Mode payloads are "identical to the typical Events API payloads", which wrap the
    // event; the page's own example shows the bare event, so both are accepted.
    let event = if payload["event"].is_object() {
        &payload["event"]
    } else {
        payload
    };
    let chat = match (event["type"].as_str(), event["channel_type"].as_str()) {
        (Some("app_mention"), _) | (Some("message"), Some("channel" | "group")) => ChatKind::Group,
        (Some("message"), Some("im")) => ChatKind::Direct,
        _ => return None,
    };
    let by_bot = event.get("bot_id").is_some();
    // Every direct message is meant for the bot, so none there is taken from a bot.
    if !me.hears(event) || (by_bot && chat == ChatKind::Direct) {
        return None;
    }
    let sender = speaker(event)?;
    let channel = event["channel"].as_str()?;
    let ts = event["ts"].as_str()?;
    // A message without text has nothing for the model: not passed on, like one without a
    // sender.
    let raw = text_of(event)?;
    let bot = me.user.as_deref();
    let thread = event["thread_ts"].as_str().map(|ts| ThreadId(ts.into()));
    let addressee = match (chat, event["type"].as_str()) {
        _ if by_bot => Addressee::Others,
        (ChatKind::Direct, _) | (ChatKind::Group, Some("app_mention")) => Addressee::Bot,
        (ChatKind::Group, _) => addressee(&raw, bot),
    };
    // Talk in the channel itself is none of the bot's business unless it is mentioned.
    if chat == ChatKind::Group && thread.is_none() && addressee != Addressee::Bot {
        return None;
    }
    Some(Inbound::Message {
        id: MessageId(ts.into()),
        at: Destination {
            conversation: ConversationId(channel.into()),
            thread,
        },
        chat,
        addressee,
        sender: UserId(sender.into()),
        text: unescape(&without_mention(&raw, bot)),
        images: images(event),
    })
}

/// The images attached to `message`: its files of the types the model APIs take, by their
/// private URLs, which need the bot token. A file whose URL Slack has not given, as for one
/// just shared into a Slack Connect channel, is left out.
fn images(message: &Value) -> Vec<ImageRef> {
    let Some(files) = message["files"].as_array() else {
        return Vec::new();
    };
    files
        .iter()
        .filter(|file| IMAGE_TYPES.contains(&file["mimetype"].as_str().unwrap_or_default()))
        .filter_map(|file| {
            let url = file["url_private_download"]
                .as_str()
                .or_else(|| file["url_private"].as_str())?;
            let name = file["name"]
                .as_str()
                .or_else(|| file["title"].as_str())
                .unwrap_or("image");
            Some(ImageRef {
                key: url.into(),
                name: name.into(),
            })
        })
        .collect()
}

/// Who posted `message`: its user, or for an integration, which has none, its bot ID.
fn speaker(message: &Value) -> Option<&str> {
    message["user"].as_str().or_else(|| message["bot_id"].as_str())
}

/// What `message` says: its text, or with none what its attachments say, where integrations
/// put theirs. `None` without a text field.
fn text_of(message: &Value) -> Option<String> {
    let text = message["text"].as_str()?;
    let attached = if text.trim().is_empty() {
        attached_text(message)
    } else {
        None
    };
    Some(attached.unwrap_or_else(|| text.to_string()))
}

/// What the attachments of `message` say, a line each: of an attachment its `pretext`,
/// `title` and `text`, or without them its `fallback`, the "plain text summary of the
/// attachment". `None` when they say nothing.
fn attached_text(message: &Value) -> Option<String> {
    fn said(value: &Value) -> Option<&str> {
        value.as_str().map(str::trim).filter(|text| !text.is_empty())
    }
    let mut lines = Vec::new();
    for attachment in message["attachments"].as_array()? {
        let before = lines.len();
        lines.extend(
            ["pretext", "title", "text"]
                .iter()
                .filter_map(|key| said(&attachment[*key])),
        );
        if lines.len() == before {
            lines.extend(said(&attachment["fallback"]));
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Whom a channel message is meant for, by its user mentions (`<@U…>`).
fn addressee(text: &str, bot: Option<&str>) -> Addressee {
    if bot.is_some_and(|bot| text.contains(&format!("<@{bot}>"))) {
        Addressee::Bot
    } else if text.contains("<@") {
        Addressee::Others
    } else {
        Addressee::Nobody
    }
}

/// What was said in a `conversations.replies` or `conversations.history` answer, in the
/// answer's order, other bots' messages among it: the bot's own posts, edits and joins, and
/// messages without a sender or text, left out.
pub(super) fn history(answer: &Value, me: &Me) -> Vec<Said> {
    let Some(messages) = answer["messages"].as_array() else {
        return Vec::new();
    };
    messages
        .iter()
        .filter(|message| me.hears(message))
        .filter_map(|message| {
            let raw = text_of(message)?;
            // As when it arrives: a bot's message starts nothing, whatever it mentions.
            let addressee = if message.get("bot_id").is_some() {
                Addressee::Others
            } else {
                addressee(&raw, me.user.as_deref())
            };
            Some(Said {
                id: MessageId(message["ts"].as_str()?.into()),
                sender: UserId(speaker(message)?.into()),
                text: unescape(raw.trim()),
                addressee,
            })
        })
        .collect()
}

/// Clicks on the card buttons of a `block_actions` payload.
pub(super) fn decisions(payload: &Value) -> Vec<Inbound> {
    if payload["type"] != "block_actions" {
        return Vec::new();
    }
    let (Some(user), Some(channel)) = (
        payload["user"]["id"].as_str(),
        payload["channel"]["id"]
            .as_str()
            .or_else(|| payload["container"]["channel_id"].as_str()),
    ) else {
        return Vec::new();
    };
    // Older cards, or other senders, may leave the thread out of the value.
    let fallback = payload["message"]["thread_ts"]
        .as_str()
        .or_else(|| payload["container"]["thread_ts"].as_str());
    // A payload without actions has nothing to decide.
    let actions = payload["actions"].as_array().map(Vec::as_slice).unwrap_or_default();
    actions
        .iter()
        .filter_map(|action| {
            let choice = match action["action_id"].as_str()? {
                ALLOW_ACTION => Choice::AllowOnce,
                DENY_ACTION => Choice::Deny,
                _ => return None,
            };
            let value = action["value"].as_str()?;
            let (thread, approval) = match value.split_once('/') {
                Some((thread, approval)) => (Some(thread), approval),
                None => (fallback, value),
            };
            Some(Inbound::Decision {
                at: Destination {
                    conversation: ConversationId(channel.into()),
                    thread: thread.map(|ts| ThreadId(ts.into())),
                },
                user: UserId(user.into()),
                approval: ApprovalId(approval.into()),
                choice,
            })
        })
        .collect()
}

/// The text with mentions of the bot removed.
fn without_mention(text: &str, bot: Option<&str>) -> String {
    match bot {
        Some(bot) => text.replace(&format!("<@{bot}>"), "").trim().to_string(),
        None => text.trim().to_string(),
    }
}

/// Slack writes `&`, `<` and `>` as entities in message text.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// `&`, `<` and `>` as entities, so that text is never read as a mention or a link.
pub(super) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        push_escaped(&mut escaped, c);
    }
    escaped
}

/// Write `c` as message text: `&`, `<` and `>` as entities.
fn push_escaped(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        c => out.push(c),
    }
}

/// Escaped text in parts of at most `max` characters, never cut inside an entity: the first
/// part and the rest.
pub(super) fn split(text: &str, max: usize) -> (String, Vec<String>) {
    split_text(text, max, push_escaped)
}

/// What a card shows of the request `card`, open and once closed: escaped, within the length
/// of a section block.
pub(super) fn card_request(labels: &CardLabels, card: &ApprovalCard) -> String {
    let head = format!("*{}*: `{}`\n```", escape(&labels.title), escape(&card.tool));
    let tail = format!("```\n{}: {}", escape(&labels.reason), escape(&card.reason));
    let room = MAX_SECTION_CHARS.saturating_sub(head.chars().count() + tail.chars().count() + 1);
    let (mut summary, rest) = split(&card.summary, room);
    // A part ends before the room does when the next entity would not fit, so whether the
    // request was cut shows in the parts left, not in this part's length.
    if !rest.is_empty() {
        summary.push('…');
    }
    format!("{head}{summary}{tail}")
}

pub(super) fn section(text: &str) -> Value {
    json!({"type": "section", "text": {"type": "mrkdwn", "text": text}})
}

pub(super) fn button(label: &str, action: &str, value: &str, style: &str) -> Value {
    json!({
        "type": "button",
        "text": {"type": "plain_text", "text": label},
        "action_id": action,
        "value": value,
        "style": style,
    })
}

/// The escaped result line of a closed card: what happened, and a mention of who decided.
pub(super) fn resolution(labels: &CardLabels, user: Option<&UserId>, result: ApprovalResult) -> String {
    let what = labels.outcome(result);
    // The mention is markup and stays as it is; only the label is text.
    match user {
        Some(user) => format!("{} (<@{user}>)", escape(what)),
        None => escape(what),
    }
}

/// Whether `message` is what someone said: edits, deletions and joins carry a subtype, but a
/// thread reply also sent to the channel carries `thread_broadcast`, an integration's post
/// `bot_message` and a message with files `file_share`, and these are said like any other.
fn is_talk(message: &Value) -> bool {
    match message.get("subtype") {
        None => true,
        Some(subtype) => ["thread_broadcast", "bot_message", "file_share"]
            .iter()
            .any(|said| subtype == said),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A card for a bash call of `summary`, as the policy asks `why`.
    fn bash(summary: &str) -> ApprovalCard {
        ApprovalCard {
            tool: "bash".into(),
            summary: summary.into(),
            reason: "why".into(),
        }
    }

    /// The parts of a split in order.
    fn parts((first, rest): (String, Vec<String>)) -> Vec<String> {
        let mut parts = vec![first];
        parts.extend(rest);
        parts
    }

    #[test]
    fn text_is_escaped_and_split_without_breaking_an_entity() {
        assert_eq!(escape("a < b && c > d"), "a &lt; b &amp;&amp; c &gt; d");
        assert_eq!(parts(split("ab<cd", 4)), ["ab", "&lt;", "cd"]);
        assert_eq!(split("", 10), (String::new(), Vec::new()));
    }

    #[test]
    fn a_split_prefers_the_last_line_break_of_the_part() {
        assert_eq!(parts(split("aaaa\nbbbb", 7)), ["aaaa\n", "bbbb"]);
        // A line break in the first half would leave too short a part; the cut is hard.
        assert_eq!(parts(split("a\nbbbbbbb", 6)), ["a\nbbbb", "bbb"]);
    }

    #[test]
    fn a_split_counts_characters_not_bytes() {
        assert_eq!(parts(split("界界界", 2)), ["界界", "界"]);
    }

    #[test]
    fn incoming_entities_are_restored() {
        assert_eq!(unescape("a &lt; b &amp;&amp; c &gt; d"), "a < b && c > d");
    }

    #[test]
    fn the_bot_mention_is_removed() {
        assert_eq!(without_mention("<@UBOT> check <@U2>", Some("UBOT")), "check <@U2>");
    }

    #[test]
    fn a_cut_request_ends_with_an_ellipsis_when_an_entity_ends_its_part_early() {
        // The room for the request here (2974) is no multiple of the four characters of
        // `&lt;`, so the part that fits ends two characters before the room does.
        let labels = CardLabels {
            title: "T".into(),
            reason: "R".into(),
            ..CardLabels::default()
        };
        let request = card_request(&labels, &bash(&"<".repeat(5000)));
        assert!(request.chars().count() <= MAX_SECTION_CHARS);
        assert!(request.contains("&lt;…```"), "{request}");
        assert!(!card_request(&labels, &bash("ls -l")).contains('…'));
    }

    #[test]
    fn a_long_request_fits_in_a_section() {
        let request = card_request(&CardLabels::default(), &bash(&"<".repeat(5000)));
        assert!(
            request.chars().count() <= MAX_SECTION_CHARS,
            "{}",
            request.chars().count()
        );
        assert!(request.contains("…"));
    }
}
