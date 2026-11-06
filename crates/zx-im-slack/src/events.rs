//! What arrives over Socket Mode, messages addressed to the bot and button clicks, and the
//! escaped text and blocks the channel posts.

use serde_json::{Value, json};
use zx_core::agent::ApprovalResult;
use zx_im::{
    ApprovalId, CardLabels, Choice, ConversationId, Destination, Inbound, MessageId, ThreadId, UserId, split_text,
};

use super::{ALLOW_ACTION, DENY_ACTION, MAX_SECTION_CHARS};

/// A message addressed to the bot, with the key that identifies it among redeliveries.
pub(crate) fn message(payload: &Value, bot: Option<&str>) -> Option<((String, String), Inbound)> {
    // Socket Mode payloads are "identical to the typical Events API payloads", which wrap the
    // event; the page's own example shows the bare event, so both are accepted.
    let event = if payload["event"].is_object() {
        &payload["event"]
    } else {
        payload
    };
    let addressed = match event["type"].as_str() {
        Some("app_mention") => true,
        Some("message") => event["channel_type"] == "im",
        _ => false,
    };
    // Edits, deletions and joins carry a subtype; the bot's own posts carry a bot_id.
    if !addressed || event.get("subtype").is_some() || event.get("bot_id").is_some() {
        return None;
    }
    let user = event["user"].as_str()?;
    if Some(user) == bot {
        return None;
    }
    let channel = event["channel"].as_str()?;
    let ts = event["ts"].as_str()?;
    // A message without text has nothing for the model: not passed on, like one without a
    // sender.
    let text = unescape(&without_mention(event["text"].as_str()?, bot));
    let inbound = Inbound::Message {
        id: MessageId(ts.into()),
        at: Destination {
            conversation: ConversationId(channel.into()),
            thread: event["thread_ts"].as_str().map(|ts| ThreadId(ts.into())),
        },
        sender: UserId(user.into()),
        text,
    };
    Some(((channel.into(), ts.into()), inbound))
}

/// Clicks on the card buttons of a `block_actions` payload.
pub(crate) fn decisions(payload: &Value) -> Vec<Inbound> {
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
pub(crate) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        push_escaped(&mut escaped, c);
    }
    escaped
}

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
pub(crate) fn split(text: &str, max: usize) -> (String, Vec<String>) {
    split_text(text, max, push_escaped)
}

/// The escaped request shown on a card, within the length of a section block.
pub(crate) fn card_request(labels: &CardLabels, tool: &str, summary: &str, reason: &str) -> String {
    let head = format!("*{}*: `{}`\n```", escape(&labels.title), escape(tool));
    let tail = format!("```\n{}: {}", escape(&labels.reason), escape(reason));
    let room = MAX_SECTION_CHARS.saturating_sub(head.chars().count() + tail.chars().count() + 1);
    let (mut summary, rest) = split(summary, room);
    // A part ends before the room does when the next entity would not fit, so whether the
    // request was cut shows in the parts left, not in this part's length.
    if !rest.is_empty() {
        summary.push('…');
    }
    format!("{head}{summary}{tail}")
}

pub(crate) fn section(text: &str) -> Value {
    json!({"type": "section", "text": {"type": "mrkdwn", "text": text}})
}

pub(crate) fn button(label: &str, action: &str, value: &str, style: &str) -> Value {
    json!({
        "type": "button",
        "text": {"type": "plain_text", "text": label},
        "action_id": action,
        "value": value,
        "style": style,
    })
}

/// The escaped result line of a closed card: what happened, and a mention of who decided.
pub(crate) fn resolution(labels: &CardLabels, user: Option<&UserId>, result: ApprovalResult) -> String {
    let what = labels.outcome(result);
    // The mention is markup and stays as it is; only the label is text.
    match user {
        Some(user) => format!("{} (<@{user}>)", escape(what)),
        None => escape(what),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let request = card_request(&labels, "bash", &"<".repeat(5000), "why");
        assert!(request.chars().count() <= MAX_SECTION_CHARS);
        assert!(request.contains("&lt;…```"), "{request}");
        assert!(!card_request(&labels, "bash", "ls -l", "why").contains('…'));
    }

    #[test]
    fn a_long_request_fits_in_a_section() {
        let request = card_request(&CardLabels::default(), "bash", &"<".repeat(5000), "why");
        assert!(
            request.chars().count() <= MAX_SECTION_CHARS,
            "{}",
            request.chars().count()
        );
        assert!(request.contains("…"));
    }
}
