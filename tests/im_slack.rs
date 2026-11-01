//! The Slack channel against a fake Slack: Socket Mode messages and clicks, acknowledgement,
//! deduplication, reconnection, credential errors, posting, cards, rate limits and the stop.
mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use common::fake_slack::{Answer, FakeSlack};
use zx_agent::agent::ApprovalResult;
use zx_agent::im::slack::{SlackChannel, SlackConfig};
use zx_agent::im::{
    ApprovalId, Choice, ConversationId, Destination, ImChannel, ImError, Inbound, MessageId, Outbound, ThreadId, UserId,
};

const APP_TOKEN: &str = "xapp-test";
const BOT_TOKEN: &str = "xoxb-test";
/// Only reached when an expected event never comes.
const WAIT: Duration = Duration::from_secs(10);

fn channel_with(slack: &FakeSlack, post_interval: Duration) -> Arc<SlackChannel> {
    let mut config = SlackConfig::new(APP_TOKEN.into(), BOT_TOKEN.into());
    config.api_base = slack.api_base.clone();
    config.post_interval = post_interval;
    Arc::new(SlackChannel::new(config).unwrap())
}

fn channel(slack: &FakeSlack) -> Arc<SlackChannel> {
    channel_with(slack, Duration::ZERO)
}

struct Running {
    inbound: mpsc::Receiver<Inbound>,
    cancel: CancellationToken,
    task: JoinHandle<Result<(), ImError>>,
}

fn run(channel: Arc<SlackChannel>) -> Running {
    let (sender, inbound) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let task = tokio::spawn(async move { channel.run(sender, &stop).await });
    Running { inbound, cancel, task }
}

impl Running {
    async fn next(&mut self) -> Inbound {
        tokio::time::timeout(WAIT, self.inbound.recv())
            .await
            .expect("nothing was passed on in time")
            .expect("the channel is running")
    }

    async fn stop(self) -> Result<(), ImError> {
        self.cancel.cancel();
        self.task.await.unwrap()
    }
}

fn envelope(id: &str, kind: &str, payload: Value) -> Value {
    json!({"envelope_id": id, "type": kind, "payload": payload, "accepts_response_payload": false})
}

fn event(event: Value) -> Value {
    json!({"type": "event_callback", "event_id": "Ev1", "event": event})
}

fn mention(ts: &str, thread: Option<&str>, text: &str) -> Value {
    let mut mention =
        json!({"type": "app_mention", "user": "U1", "text": text, "ts": ts, "channel": "C1", "event_ts": ts});
    if let Some(thread) = thread {
        mention["thread_ts"] = json!(thread);
    }
    event(mention)
}

fn direct(ts: &str, extra: Value) -> Value {
    let mut message =
        json!({"type": "message", "channel_type": "im", "channel": "D1", "user": "U1", "text": "hi", "ts": ts});
    if let (Value::Object(message), Value::Object(extra)) = (&mut message, extra) {
        message.extend(extra);
    }
    event(message)
}

fn in_thread(conversation: &str, thread: Option<&str>) -> Destination {
    Destination {
        conversation: ConversationId(conversation.into()),
        thread: thread.map(|ts| ThreadId(ts.into())),
    }
}

fn message(ts: &str, at: Destination, sender: &str, text: &str) -> Inbound {
    Inbound::Message {
        id: MessageId(ts.into()),
        at,
        sender: UserId(sender.into()),
        text: text.into(),
    }
}

#[tokio::test]
async fn a_mention_is_acknowledged_and_passed_on_without_the_bot_mention() {
    let mut slack = FakeSlack::start().await;
    let mut running = run(channel(&slack));
    let mut socket = slack.socket().await;
    socket.send(json!({"type": "hello", "num_connections": 1})).await;
    let text = "<@UBOT> pods in &lt;ns&gt; &amp; logs";
    socket
        .send(envelope("e1", "events_api", mention("1.000100", None, text)))
        .await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));
    assert_eq!(
        running.next().await,
        message("1.000100", in_thread("C1", None), "U1", "pods in <ns> & logs")
    );
    assert_eq!(slack.calls("auth.test")[0].authorization, format!("Bearer {BOT_TOKEN}"));
    assert_eq!(
        slack.calls("apps.connections.open")[0].authorization,
        format!("Bearer {APP_TOKEN}")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_mention_in_a_thread_names_the_thread() {
    let mut slack = FakeSlack::start().await;
    let mut running = run(channel(&slack));
    let mut socket = slack.socket().await;
    socket
        .send(envelope(
            "e1",
            "events_api",
            mention("1.000200", Some("1.000100"), "<@UBOT> more"),
        ))
        .await;
    assert_eq!(
        running.next().await,
        message("1.000200", in_thread("C1", Some("1.000100")), "U1", "more")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn direct_messages_are_passed_on_but_not_the_bots_own_or_edits() {
    let mut slack = FakeSlack::start().await;
    let mut running = run(channel(&slack));
    let mut socket = slack.socket().await;
    let ignored = [
        direct("2.1", json!({"bot_id": "BBOT"})),
        direct("2.2", json!({"user": "UBOT"})),
        direct("2.3", json!({"subtype": "message_changed"})),
        event(
            json!({"type": "message", "channel_type": "channel", "channel": "C1", "user": "U1", "text": "chatter", "ts": "2.4"}),
        ),
    ];
    for (n, payload) in ignored.into_iter().enumerate() {
        let id = format!("ignored{n}");
        socket.send(envelope(&id, "events_api", payload)).await;
        assert_eq!(socket.receive().await, json!({ "envelope_id": id }));
    }
    socket
        .send(envelope("e1", "events_api", direct("2.5", json!({}))))
        .await;
    assert_eq!(running.next().await, message("2.5", in_thread("D1", None), "U1", "hi"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_redelivered_message_is_passed_on_once() {
    let mut slack = FakeSlack::start().await;
    let mut running = run(channel(&slack));
    let mut socket = slack.socket().await;
    socket
        .send(envelope("e1", "events_api", mention("3.1", None, "<@UBOT> once")))
        .await;
    socket
        .send(envelope("e2", "events_api", mention("3.1", None, "<@UBOT> once")))
        .await;
    socket
        .send(envelope("e3", "events_api", mention("3.2", None, "<@UBOT> next")))
        .await;
    for id in ["e1", "e2", "e3"] {
        assert_eq!(socket.receive().await, json!({ "envelope_id": id }));
    }
    assert_eq!(
        running.next().await,
        message("3.1", in_thread("C1", None), "U1", "once")
    );
    assert_eq!(
        running.next().await,
        message("3.2", in_thread("C1", None), "U1", "next")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_button_click_becomes_a_decision_in_the_cards_thread() {
    let mut slack = FakeSlack::start().await;
    let mut running = run(channel(&slack));
    let mut socket = slack.socket().await;
    let click = |action: &str| {
        json!({
            "type": "block_actions",
            "user": {"id": "U2"},
            "channel": {"id": "C1"},
            "container": {"type": "message", "message_ts": "4.2", "channel_id": "C1", "thread_ts": "4.1"},
            "message": {"ts": "4.2", "thread_ts": "4.1"},
            "actions": [{"action_id": action, "value": "A1", "type": "button"}],
        })
    };
    socket
        .send(envelope("i0", "interactive", click("something_else")))
        .await;
    socket.send(envelope("i1", "interactive", click("zx_deny"))).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "i0"}));
    assert_eq!(socket.receive().await, json!({"envelope_id": "i1"}));
    assert_eq!(
        running.next().await,
        Inbound::Decision {
            at: in_thread("C1", Some("4.1")),
            user: UserId("U2".into()),
            approval: ApprovalId("A1".into()),
            choice: Choice::Deny,
        }
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_click_finds_its_thread_in_the_button_value() {
    let mut slack = FakeSlack::start().await;
    let mut running = run(channel(&slack));
    let mut socket = slack.socket().await;
    // Only fields the block_actions reference documents: no thread anywhere but the value.
    let click = json!({
        "type": "block_actions",
        "user": {"id": "U2"},
        "container": {"type": "message", "message_ts": "4.2", "channel_id": "C1"},
        "message": {"ts": "4.2"},
        "actions": [{"action_id": "zx_allow_once", "value": "4.1/A2", "type": "button"}],
    });
    socket.send(envelope("i1", "interactive", click)).await;
    assert_eq!(
        running.next().await,
        Inbound::Decision {
            at: in_thread("C1", Some("4.1")),
            user: UserId("U2".into()),
            approval: ApprovalId("A2".into()),
            choice: Choice::AllowOnce,
        }
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn an_event_without_its_wrapper_is_passed_on_too() {
    let mut slack = FakeSlack::start().await;
    let mut running = run(channel(&slack));
    let mut socket = slack.socket().await;
    // The Socket Mode page's own example puts the bare event in the payload.
    let bare = json!({"type": "app_mention", "user": "U1", "text": "<@UBOT> bare", "ts": "4.5", "channel": "C1"});
    socket.send(envelope("e1", "events_api", bare)).await;
    assert_eq!(
        running.next().await,
        message("4.5", in_thread("C1", None), "U1", "bare")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_disconnect_message_makes_the_channel_reconnect() {
    let mut slack = FakeSlack::start().await;
    let mut running = run(channel(&slack));
    let mut first = slack.socket().await;
    first
        .send(json!({"type": "disconnect", "reason": "refresh_requested"}))
        .await;
    let mut second = slack.socket().await;
    second
        .send(envelope("e1", "events_api", mention("5.1", None, "<@UBOT> still here")))
        .await;
    assert_eq!(
        running.next().await,
        message("5.1", in_thread("C1", None), "U1", "still here")
    );
    assert_eq!(slack.calls("apps.connections.open").len(), 2);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_rejected_bot_token_stops_the_channel_with_a_configuration_error() {
    let slack = FakeSlack::start().await;
    slack.script("auth.test", vec![Answer::Error("invalid_auth")]);
    let running = run(channel(&slack));
    match running.task.await.unwrap() {
        Err(ImError::Config(error)) => assert!(error.contains("invalid_auth"), "{error}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_rejected_app_token_stops_the_channel_with_a_configuration_error() {
    let slack = FakeSlack::start().await;
    slack.script("apps.connections.open", vec![Answer::Error("not_allowed_token_type")]);
    let running = run(channel(&slack));
    match running.task.await.unwrap() {
        Err(ImError::Config(error)) => assert!(error.contains("not_allowed_token_type"), "{error}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[tokio::test]
async fn text_is_posted_in_the_thread_with_entities_escaped() {
    let slack = FakeSlack::start().await;
    let channel = channel(&slack);
    let to = in_thread("C1", Some("6.1"));
    let posted = channel
        .send(&to, &Outbound::Text("a < b & c > d".into()))
        .await
        .unwrap();
    let call = &slack.calls("chat.postMessage")[0];
    assert_eq!(call.authorization, format!("Bearer {BOT_TOKEN}"));
    assert_eq!(
        call.body,
        json!({"channel": "C1", "thread_ts": "6.1", "text": "a &lt; b &amp; c &gt; d"})
    );
    assert_eq!(posted.at, to);
    assert_eq!(posted.id, MessageId("1700000000.000001".into()));
}

#[tokio::test]
async fn long_text_continues_in_the_thread_of_its_first_part() {
    let slack = FakeSlack::start().await;
    let channel = channel(&slack);
    let text = "x".repeat(4000 * 2 + 10);
    let posted = channel
        .send(&in_thread("C1", None), &Outbound::Text(text))
        .await
        .unwrap();
    let calls = slack.calls("chat.postMessage");
    let lengths: Vec<usize> = calls
        .iter()
        .map(|call| call.body["text"].as_str().unwrap().len())
        .collect();
    assert_eq!(lengths, [4000, 4000, 10]);
    assert_eq!(calls[0].body.get("thread_ts"), None);
    for call in &calls[1..] {
        assert_eq!(call.body["thread_ts"], json!(posted.id.0));
    }
}

#[tokio::test]
async fn an_approval_card_has_two_buttons_that_carry_the_approval_id() {
    let slack = FakeSlack::start().await;
    let channel = channel(&slack);
    let card = Outbound::ApprovalRequest {
        id: ApprovalId("A1".into()),
        tool: "bash".into(),
        summary: "kubectl delete pod x".into(),
        reason: "needs approval".into(),
    };
    channel.send(&in_thread("C1", Some("7.1")), &card).await.unwrap();
    let body = &slack.calls("chat.postMessage")[0].body;
    let request = body["blocks"][0]["text"]["text"].as_str().unwrap();
    assert!(
        request.contains("kubectl delete pod x") && request.contains("needs approval"),
        "{request}"
    );
    let buttons: Vec<(&str, &str)> = body["blocks"][1]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|button| (button["action_id"].as_str().unwrap(), button["value"].as_str().unwrap()))
        .collect();
    // The value carries the thread as well, as the click payload documents no thread.
    assert_eq!(buttons, [("zx_allow_once", "7.1/A1"), ("zx_deny", "7.1/A1")]);
}

#[tokio::test]
async fn a_closed_card_keeps_its_request_and_shows_who_decided() {
    let slack = FakeSlack::start().await;
    let channel = channel(&slack);
    let card = Outbound::ApprovalRequest {
        id: ApprovalId("A1".into()),
        tool: "bash".into(),
        summary: "kubectl delete pod x".into(),
        reason: "needs approval".into(),
    };
    let posted = channel.send(&in_thread("C1", Some("8.1")), &card).await.unwrap();
    let request = slack.calls("chat.postMessage")[0].body["blocks"][0].clone();
    let closed = Outbound::ApprovalResolved {
        id: ApprovalId("A1".into()),
        user: Some(UserId("U2".into())),
        result: ApprovalResult::AllowOnce,
    };
    channel.update(&posted, &closed).await.unwrap();
    let body = &slack.calls("chat.update")[0].body;
    assert_eq!((&body["channel"], &body["ts"]), (&json!("C1"), &json!(posted.id.0)));
    let blocks = body["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 2, "no buttons are left: {blocks:?}");
    assert_eq!(blocks[0], request);
    assert_eq!(blocks[1]["text"]["text"], json!("已批准（<@U2>）"));
}

#[tokio::test]
async fn a_status_update_replaces_the_text_and_leaves_out_blocks() {
    let slack = FakeSlack::start().await;
    let channel = channel(&slack);
    let posted = channel
        .send(&in_thread("C1", Some("9.1")), &Outbound::Text("working".into()))
        .await
        .unwrap();
    channel.update(&posted, &Outbound::Text("done".into())).await.unwrap();
    assert_eq!(
        slack.calls("chat.update")[0].body,
        json!({"channel": "C1", "ts": posted.id.0, "text": "done"})
    );
}

#[tokio::test]
async fn a_rate_limited_post_waits_and_is_retried() {
    let slack = FakeSlack::start().await;
    slack.script("chat.postMessage", vec![Answer::RateLimited { retry_after_secs: 0 }]);
    let channel = channel(&slack);
    channel
        .send(&in_thread("C1", Some("10.1")), &Outbound::Text("hello".into()))
        .await
        .unwrap();
    assert_eq!(slack.calls("chat.postMessage").len(), 2);
}

#[tokio::test]
async fn posts_to_one_chat_are_spaced_out() {
    let slack = FakeSlack::start().await;
    let interval = Duration::from_millis(300);
    let channel = channel_with(&slack, interval);
    let to = in_thread("C1", Some("11.1"));
    let started = tokio::time::Instant::now();
    channel.send(&to, &Outbound::Text("one".into())).await.unwrap();
    channel.send(&to, &Outbound::Text("two".into())).await.unwrap();
    assert!(started.elapsed() >= interval, "{:?}", started.elapsed());
}

#[tokio::test]
async fn a_failed_connection_attempt_is_reported_and_retried() {
    let mut slack = FakeSlack::start().await;
    slack.script("apps.connections.open", vec![Answer::Error("internal_error")]);
    let (reported, mut reports) = mpsc::unbounded_channel();
    let mut config = SlackConfig::new(APP_TOKEN.into(), BOT_TOKEN.into());
    config.api_base = slack.api_base.clone();
    config.on_error = Some(Arc::new(move |error: &ImError| {
        reported.send(error.to_string()).ok();
    }));
    let running = run(Arc::new(SlackChannel::new(config).unwrap()));
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("internal_error"), "{report}");
    // The next attempt connects.
    slack.socket().await;
    running.stop().await.unwrap();
}

#[tokio::test]
async fn stopping_closes_the_connection() {
    let mut slack = FakeSlack::start().await;
    let running = run(channel(&slack));
    let mut socket = slack.socket().await;
    running.stop().await.unwrap();
    assert!(socket.closed_by_client().await);
}
