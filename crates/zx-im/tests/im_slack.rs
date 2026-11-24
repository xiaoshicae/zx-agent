//! The Slack channel against a fake Slack: Socket Mode messages and clicks, acknowledgement,
//! deduplication, reconnection, credential errors, posting, cards, rate limits, the images of
//! messages, files posted, the way through a proxy and the stop.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use zx_agent::ai::{Proxies, Proxy};
use zx_agent::events::ApprovalResult;
use zx_im::{
    Addressee, ApprovalCard, ApprovalId, Attachment, ChatKind, Choice, Destination, ImChannel, ImError, ImageRef,
    Inbound, MessageId, MessageRef, Post, Replacement, Said, SlackChannel, SlackConfig, UserId,
};
use zx_testing::WAIT;
use zx_testing::fake_proxy::FakeProxy;
use zx_testing::fake_slack::{Answer, BOT_TOKEN, FakeSlack};
use zx_testing::http::silent_listener;

use self::common::im::{FullBacklog, at, earlier, run_channel, run_channel_with};

const APP_TOKEN: &str = "xapp-test";

fn config(slack: &FakeSlack) -> SlackConfig {
    let mut config = SlackConfig::new(APP_TOKEN.into(), BOT_TOKEN.into());
    config.api_base = slack.api_base.clone();
    config.post_interval = Duration::ZERO;
    config
}

fn channel_with(slack: &FakeSlack, post_interval: Duration) -> Arc<SlackChannel> {
    let mut config = config(slack);
    config.post_interval = post_interval;
    Arc::new(SlackChannel::new(config).unwrap())
}

fn channel(slack: &FakeSlack) -> Arc<SlackChannel> {
    channel_with(slack, Duration::ZERO)
}

/// A channel for `config` that passes each error it reports on as text.
fn reporting(mut config: SlackConfig) -> (Arc<SlackChannel>, mpsc::UnboundedReceiver<String>) {
    let (reported, reports) = mpsc::unbounded_channel();
    config.on_error = Some(Arc::new(move |error: &ImError| {
        reported.send(error.to_string()).ok();
    }));
    (Arc::new(SlackChannel::new(config).unwrap()), reports)
}

fn envelope(id: &str, kind: &str, payload: &Value) -> Value {
    json!({"envelope_id": id, "type": kind, "payload": payload, "accepts_response_payload": false})
}

fn event(event: &Value) -> Value {
    json!({"type": "event_callback", "event_id": "Ev1", "event": event})
}

fn mention(ts: &str, thread: Option<&str>, text: &str) -> Value {
    let mut mention =
        json!({"type": "app_mention", "user": "U1", "text": text, "ts": ts, "channel": "C1", "event_ts": ts});
    if let Some(thread) = thread {
        mention["thread_ts"] = json!(thread);
    }
    event(&mention)
}

fn direct(ts: &str, extra: Value) -> Value {
    let mut message =
        json!({"type": "message", "channel_type": "im", "channel": "D1", "user": "U1", "text": "hi", "ts": ts});
    if let (Value::Object(message), Value::Object(extra)) = (&mut message, extra) {
        message.extend(extra);
    }
    event(&message)
}

/// A message from U1 in a channel that mentions the bot.
fn message(ts: &str, at: Destination, text: &str) -> Inbound {
    said(ts, at, ChatKind::Group, Addressee::Bot, text)
}

/// A message from U1 in the direct chat D1.
fn direct_message(ts: &str, text: &str) -> Inbound {
    said(ts, at("D1", None), ChatKind::Direct, Addressee::Bot, text)
}

/// The message `ts` from U1 at `at`, in a chat of kind `chat`, addressed to `addressee`.
fn said(ts: &str, at: Destination, chat: ChatKind, addressee: Addressee, text: &str) -> Inbound {
    Inbound::Message {
        id: MessageId(ts.into()),
        at,
        chat,
        addressee,
        sender: UserId("U1".into()),
        text: text.into(),
        images: Vec::new(),
    }
}

#[tokio::test]
async fn a_mention_is_acknowledged_and_passed_on_without_the_bot_mention() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    socket.send(json!({"type": "hello", "num_connections": 1})).await;
    let text = "<@UBOT> pods in &lt;ns&gt; &amp; logs";
    socket
        .send(envelope("e1", "events_api", &mention("1.000100", None, text)))
        .await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "e1"}));
    assert_eq!(
        running.next().await,
        message("1.000100", at("C1", None), "pods in <ns> & logs")
    );
    assert_eq!(slack.calls("auth.test")[0].authorization, format!("Bearer {BOT_TOKEN}"));
    assert_eq!(
        slack.calls("apps.connections.open")[0].authorization,
        format!("Bearer {APP_TOKEN}")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn the_web_api_and_the_socket_mode_connection_go_through_the_proxy_with_its_credentials() {
    let mut slack = FakeSlack::start().await;
    let proxy = FakeProxy::start().await;
    let mut config = config(&slack);
    config.proxy = Proxy::Through(Proxies {
        http: Some(proxy.url(Some("bot:p%40ss"))),
        ..Proxies::default()
    });
    let mut running = run_channel(Arc::new(SlackChannel::new(config).unwrap()), WAIT);
    let mut socket = slack.socket().await;
    socket
        .send(envelope("e1", "events_api", &mention("1.000100", None, "<@UBOT> hi")))
        .await;
    assert_eq!(running.next().await, message("1.000100", at("C1", None), "hi"));
    running.stop().await.unwrap();

    // `bot:p@ss`, the user and password of the proxy's URL, decoded.
    let credentials = Some("Basic Ym90OnBAc3M=".to_string());
    let asked = proxy.asked();
    let api_call = format!("POST {}/auth.test", slack.api_base);
    assert!(
        asked
            .iter()
            .any(|asked| asked.line == api_call && asked.authorization == credentials),
        "{asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|asked| asked.line.starts_with("CONNECT 127.0.0.1:") && asked.authorization == credentials),
        "{asked:?}"
    );
}

#[tokio::test]
async fn a_mention_in_a_thread_names_the_thread() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    socket
        .send(envelope(
            "e1",
            "events_api",
            &mention("1.000200", Some("1.000100"), "<@UBOT> more"),
        ))
        .await;
    assert_eq!(
        running.next().await,
        message("1.000200", at("C1", Some("1.000100")), "more")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn direct_messages_are_passed_on_but_not_the_bots_own_or_edits() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    let ignored = [
        direct("2.1", json!({"bot_id": "BBOT"})),
        direct("2.2", json!({"user": "UBOT"})),
        direct("2.3", json!({"subtype": "message_changed"})),
        event(
            &json!({"type": "message", "channel_type": "channel", "channel": "C1", "user": "U1", "text": "chatter", "ts": "2.4"}),
        ),
    ];
    for (n, payload) in ignored.into_iter().enumerate() {
        let id = format!("ignored{n}");
        socket.send(envelope(&id, "events_api", &payload)).await;
        assert_eq!(socket.receive().await, json!({ "envelope_id": id }));
    }
    socket
        .send(envelope("e1", "events_api", &direct("2.5", json!({}))))
        .await;
    assert_eq!(running.next().await, direct_message("2.5", "hi"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_redelivered_message_is_passed_on_once() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    socket
        .send(envelope("e1", "events_api", &mention("3.1", None, "<@UBOT> once")))
        .await;
    socket
        .send(envelope("e2", "events_api", &mention("3.1", None, "<@UBOT> once")))
        .await;
    socket
        .send(envelope("e3", "events_api", &mention("3.2", None, "<@UBOT> next")))
        .await;
    for id in ["e1", "e2", "e3"] {
        assert_eq!(socket.receive().await, json!({ "envelope_id": id }));
    }
    assert_eq!(running.next().await, message("3.1", at("C1", None), "once"));
    assert_eq!(running.next().await, message("3.2", at("C1", None), "next"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_that_cannot_be_kept_is_reported_and_left_unacknowledged() {
    let mut slack = FakeSlack::start().await;
    let (channel, mut reports) = reporting(config(&slack));
    let mut running = run_channel_with(channel, Arc::new(FullBacklog), WAIT);
    let mut socket = slack.socket().await;
    socket
        .send(envelope("e1", "events_api", &mention("5.1", None, "<@UBOT> keep me")))
        .await;
    // An envelope with no message to keep is acknowledged; the first one was not.
    socket
        .send(envelope("e2", "events_api", &direct("5.2", json!({"bot_id": "BBOT"}))))
        .await;
    assert_eq!(socket.receive().await, json!({ "envelope_id": "e2" }));
    let report = reports.recv().await.unwrap();
    assert!(
        report.contains("cannot keep message 5.1: no space left on device"),
        "{report}"
    );
    assert!(running.nothing_passed());
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_button_click_becomes_a_decision_in_the_cards_thread() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
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
        .send(envelope("i0", "interactive", &click("something_else")))
        .await;
    socket.send(envelope("i1", "interactive", &click("zx_deny"))).await;
    assert_eq!(socket.receive().await, json!({"envelope_id": "i0"}));
    assert_eq!(socket.receive().await, json!({"envelope_id": "i1"}));
    assert_eq!(
        running.next().await,
        Inbound::Decision {
            at: at("C1", Some("4.1")),
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
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    // Only fields the block_actions reference documents: no thread anywhere but the value.
    let click = json!({
        "type": "block_actions",
        "user": {"id": "U2"},
        "container": {"type": "message", "message_ts": "4.2", "channel_id": "C1"},
        "message": {"ts": "4.2"},
        "actions": [{"action_id": "zx_allow_once", "value": "4.1/A2", "type": "button"}],
    });
    socket.send(envelope("i1", "interactive", &click)).await;
    assert_eq!(
        running.next().await,
        Inbound::Decision {
            at: at("C1", Some("4.1")),
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
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    // The Socket Mode page's own example puts the bare event in the payload.
    let bare = json!({"type": "app_mention", "user": "U1", "text": "<@UBOT> bare", "ts": "4.5", "channel": "C1"});
    socket.send(envelope("e1", "events_api", &bare)).await;
    assert_eq!(running.next().await, message("4.5", at("C1", None), "bare"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_disconnect_message_makes_the_channel_reconnect() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut first = slack.socket().await;
    first
        .send(json!({"type": "disconnect", "reason": "refresh_requested"}))
        .await;
    let mut second = slack.socket().await;
    second
        .send(envelope(
            "e1",
            "events_api",
            &mention("5.1", None, "<@UBOT> still here"),
        ))
        .await;
    assert_eq!(running.next().await, message("5.1", at("C1", None), "still here"));
    assert_eq!(slack.calls("apps.connections.open").len(), 2);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_rejected_bot_token_stops_the_channel_with_a_configuration_error() {
    let slack = FakeSlack::start().await;
    slack.script("auth.test", vec![Answer::Error("invalid_auth")]);
    let running = run_channel(channel(&slack), WAIT);
    match running.ended().await {
        Err(ImError::Config(error)) => assert!(error.contains("invalid_auth"), "{error}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_rejected_app_token_stops_the_channel_with_a_configuration_error() {
    let slack = FakeSlack::start().await;
    slack.script("apps.connections.open", vec![Answer::Error("not_allowed_token_type")]);
    let running = run_channel(channel(&slack), WAIT);
    match running.ended().await {
        Err(ImError::Config(error)) => assert!(error.contains("not_allowed_token_type"), "{error}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[tokio::test]
async fn text_is_posted_in_the_thread_with_entities_escaped() {
    let slack = FakeSlack::start().await;
    let channel = channel(&slack);
    let to = at("C1", Some("6.1"));
    let posted = channel.send(&to, &Post::Text("a < b & c > d".into())).await.unwrap();
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
    let posted = channel.send(&at("C1", None), &Post::Text(text)).await.unwrap();
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
    let card = Post::ApprovalRequest {
        id: ApprovalId("A1".into()),
        card: ApprovalCard {
            tool: "bash".into(),
            summary: "kubectl delete pod x".into(),
            reason: "needs approval".into(),
        },
    };
    channel.send(&at("C1", Some("7.1")), &card).await.unwrap();
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
    let card = Post::ApprovalRequest {
        id: ApprovalId("A1".into()),
        card: ApprovalCard {
            tool: "bash".into(),
            summary: "kubectl delete pod x".into(),
            reason: "needs approval".into(),
        },
    };
    let posted = channel.send(&at("C1", Some("8.1")), &card).await.unwrap();
    let request = slack.calls("chat.postMessage")[0].body["blocks"][0].clone();
    let closed = Replacement::ApprovalResolved {
        card: ApprovalCard {
            tool: "bash".into(),
            summary: "kubectl delete pod x".into(),
            reason: "needs approval".into(),
        },
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
    assert_eq!(blocks[1]["text"]["text"], json!("Allowed (<@U2>)"));
}

#[tokio::test]
async fn a_status_update_replaces_the_text_and_leaves_out_blocks() {
    let slack = FakeSlack::start().await;
    let channel = channel(&slack);
    let posted = channel
        .send(&at("C1", Some("9.1")), &Post::Text("working".into()))
        .await
        .unwrap();
    channel
        .update(&posted, &Replacement::Text("done".into()))
        .await
        .unwrap();
    assert_eq!(
        slack.calls("chat.update")[0].body,
        json!({"channel": "C1", "ts": posted.id.0, "text": "done"})
    );
}

#[tokio::test]
async fn a_refused_edit_posts_the_text_instead() {
    let slack = FakeSlack::start().await;
    slack.script("chat.update", vec![Answer::Error("message_not_found")]);
    let channel = channel(&slack);
    let posted = channel
        .send(&at("C1", Some("9.1")), &Post::Text("working".into()))
        .await
        .unwrap();

    channel
        .update(&posted, &Replacement::Text("done".into()))
        .await
        .unwrap();

    let posts = slack.calls("chat.postMessage");
    assert_eq!(posts.len(), 2, "{posts:?}");
    assert_eq!(
        posts[1].body,
        json!({"channel": "C1", "thread_ts": "9.1", "text": "done"})
    );
}

#[tokio::test]
async fn a_rate_limited_post_waits_and_is_retried() {
    let slack = FakeSlack::start().await;
    slack.script("chat.postMessage", vec![Answer::RateLimited { retry_after_secs: 0 }]);
    let channel = channel(&slack);
    channel
        .send(&at("C1", Some("10.1")), &Post::Text("hello".into()))
        .await
        .unwrap();
    assert_eq!(slack.calls("chat.postMessage").len(), 2);
}

#[tokio::test]
async fn a_post_rate_limited_for_longer_than_a_call_waits_fails_at_once_as_rate_limited() {
    let slack = FakeSlack::start().await;
    slack.script("chat.postMessage", vec![Answer::RateLimited { retry_after_secs: 45 }]);
    let channel = channel(&slack);
    let error = tokio::time::timeout(WAIT, channel.send(&at("C1", Some("10.1")), &Post::Text("hello".into())))
        .await
        .expect("the post waited out a rate limit longer than a call waits")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "slack chat.postMessage: rate limited, for 45s: longer than a call waits (20s in all)"
    );
    assert_eq!(slack.calls("chat.postMessage").len(), 1);
}

#[tokio::test]
async fn posts_to_one_chat_are_spaced_out() {
    let slack = FakeSlack::start().await;
    let interval = Duration::from_millis(300);
    let channel = channel_with(&slack, interval);
    let to = at("C1", Some("11.1"));
    let started = tokio::time::Instant::now();
    channel.send(&to, &Post::Text("one".into())).await.unwrap();
    channel.send(&to, &Post::Text("two".into())).await.unwrap();
    assert!(started.elapsed() >= interval, "{:?}", started.elapsed());
}

#[tokio::test]
async fn a_failed_connection_attempt_is_reported_and_retried() {
    let mut slack = FakeSlack::start().await;
    slack.script("apps.connections.open", vec![Answer::Error("internal_error")]);
    let (channel, mut reports) = reporting(config(&slack));
    let running = run_channel(channel, WAIT);
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("internal_error"), "{report}");
    // The next attempt connects.
    slack.socket().await;
    running.stop().await.unwrap();
}

/// `wss://` goes through rustls' process-wide crypto provider. A build in which two providers
/// are enabled makes the connection panic instead of failing, which only `wss://` addresses
/// reach; the fake's own socket is plain `ws://`.
#[tokio::test]
async fn a_failed_tls_handshake_is_reported_and_retried() {
    let tls = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tls_url = format!("wss://{}/link", tls.local_addr().unwrap());
    // Accepts and closes, so the handshake fails.
    tokio::spawn(async move {
        while let Ok((stream, _)) = tls.accept().await {
            drop(stream);
        }
    });
    let mut slack = FakeSlack::start().await;
    slack.script(
        "apps.connections.open",
        vec![Answer::Json(json!({"ok": true, "url": tls_url}))],
    );
    let (channel, mut reports) = reporting(config(&slack));
    let running = run_channel(channel, WAIT);
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("cannot open the Socket Mode connection"), "{report}");
    // The next attempt gets the fake's plain socket.
    slack.socket().await;
    running.stop().await.unwrap();
}

#[tokio::test]
async fn stopping_closes_the_connection() {
    let mut slack = FakeSlack::start().await;
    let running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    running.stop().await.unwrap();
    assert!(socket.closed_by_client().await);
}

#[tokio::test]
async fn stopping_does_not_wait_out_a_long_retry_after() {
    let slack = FakeSlack::start().await;
    slack.script(
        "apps.connections.open",
        vec![Answer::RateLimited {
            retry_after_secs: 100_000,
        }],
    );
    let running = run_channel(channel(&slack), WAIT);
    slack.wait_for("apps.connections.open", 1).await;
    tokio::time::timeout(WAIT, running.stop())
        .await
        .expect("the stop waited for the Retry-After")
        .unwrap();
}

#[tokio::test]
async fn stopping_does_not_wait_for_a_connection_being_opened() {
    let (address, mut connections) = silent_listener().await;
    let slack = FakeSlack::start().await;
    slack.script(
        "apps.connections.open",
        vec![Answer::Json(json!({"ok": true, "url": format!("ws://{address}/link")}))],
    );
    let mut config = config(&slack);
    // Longer than the test waits, so that only the stop can end the attempt in time.
    config.request_timeout = 3 * WAIT;
    let running = run_channel(Arc::new(SlackChannel::new(config).unwrap()), WAIT);
    connections.recv().await.unwrap();
    tokio::time::timeout(WAIT, running.stop())
        .await
        .expect("the stop waited for the connection attempt")
        .unwrap();
}

#[tokio::test]
async fn a_silent_connection_is_reported_and_opened_again() {
    let mut slack = FakeSlack::start().await;
    let mut config = config(&slack);
    config.ping_interval = Duration::from_millis(100);
    let (channel, mut reports) = reporting(config);
    let running = run_channel(channel, WAIT);
    // Answers nothing, not even the pings.
    let first = slack.socket().await;
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("silent"), "{report}");
    let second = slack.socket().await;
    assert_eq!(slack.calls("apps.connections.open").len(), 2);
    drop((first, second));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_lost_connection_is_reported_and_opened_again() {
    let mut slack = FakeSlack::start().await;
    let (channel, mut reports) = reporting(config(&slack));
    let running = run_channel(channel, WAIT);
    // Gone without a close frame.
    drop(slack.socket().await);
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("was lost"), "{report}");
    let second = slack.socket().await;
    assert_eq!(slack.calls("apps.connections.open").len(), 2);
    drop(second);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_without_text_is_not_passed_on() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    socket
        .send(envelope("e1", "events_api", &direct("2.6", json!({"text": null}))))
        .await;
    socket
        .send(envelope("e2", "events_api", &direct("2.7", json!({}))))
        .await;
    assert_eq!(running.next().await, direct_message("2.7", "hi"));
    running.stop().await.unwrap();
}

/// A message in channel C1 from U1, in the thread `thread` when given.
fn channel_message(ts: &str, thread: Option<&str>, text: &str) -> Value {
    let mut message =
        json!({"type": "message", "channel_type": "channel", "channel": "C1", "user": "U1", "text": text, "ts": ts});
    if let Some(thread) = thread {
        message["thread_ts"] = json!(thread);
    }
    event(&message)
}

#[tokio::test]
async fn replies_in_a_thread_are_passed_on_with_whom_they_mention() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    let thread = Some("6.0");
    let frames = [
        channel_message("6.1", None, "chatter in the channel"),
        channel_message("6.2", thread, "and then?"),
        channel_message("6.3", thread, "<@U2> over to you"),
        channel_message("6.4", thread, "<@UBOT> you too"),
    ];
    for (n, frame) in frames.iter().enumerate() {
        socket.send(envelope(&format!("e{n}"), "events_api", frame)).await;
    }
    let at = at("C1", thread);
    let group = |ts, addressee, text| said(ts, at.clone(), ChatKind::Group, addressee, text);
    assert_eq!(running.next().await, group("6.2", Addressee::Nobody, "and then?"));
    assert_eq!(
        running.next().await,
        group("6.3", Addressee::Others, "<@U2> over to you")
    );
    assert_eq!(running.next().await, group("6.4", Addressee::Bot, "you too"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_thread_reply_also_sent_to_the_channel_is_passed_on() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    let mut broadcast = channel_message("6.2", Some("6.0"), "and then?");
    broadcast["event"]["subtype"] = json!("thread_broadcast");

    socket.send(envelope("e1", "events_api", &broadcast)).await;

    let at = at("C1", Some("6.0"));
    assert_eq!(
        running.next().await,
        said("6.2", at, ChatKind::Group, Addressee::Nobody, "and then?")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn another_bots_reply_in_a_thread_is_heard_and_starts_nothing() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    let by_bot = |ts: &str, extra: Value| {
        let mut message = json!({"type": "message", "channel_type": "channel", "channel": "C1", "ts": ts});
        if let (Value::Object(message), Value::Object(extra)) = (&mut message, extra) {
            message.extend(extra);
        }
        event(&message)
    };
    let frames = [
        // An integration's post has no user, and may name the bot: it is heard, not answered.
        by_bot(
            "9.1",
            json!({"subtype": "bot_message", "bot_id": "B9", "text": "<@UBOT> resolved", "thread_ts": "9.0"}),
        ),
        // The bot's own posts, as Slack sends them back, with and without its user.
        by_bot(
            "9.2",
            json!({"subtype": "bot_message", "bot_id": "BBOT", "text": "mine", "thread_ts": "9.0"}),
        ),
        by_bot(
            "9.3",
            json!({"user": "UBOT", "bot_id": "BBOT", "text": "mine too", "thread_ts": "9.0"}),
        ),
        // Outside a thread a bot starts no conversation, even by a mention, nor in a direct chat.
        event(
            &json!({"type": "app_mention", "user": "UB9", "bot_id": "B9", "text": "<@UBOT> wake up", "ts": "9.4", "channel": "C1"}),
        ),
        direct("9.5", json!({"bot_id": "B9"})),
        channel_message("9.6", Some("9.0"), "anyone?"),
    ];
    for (n, frame) in frames.iter().enumerate() {
        socket.send(envelope(&format!("e{n}"), "events_api", frame)).await;
    }
    let at = at("C1", Some("9.0"));
    assert_eq!(
        running.next().await,
        Inbound::Message {
            id: MessageId("9.1".into()),
            at: at.clone(),
            chat: ChatKind::Group,
            addressee: Addressee::Others,
            sender: UserId("B9".into()),
            text: "resolved".into(),
            images: Vec::new(),
        }
    );
    assert_eq!(
        running.next().await,
        said("9.6", at, ChatKind::Group, Addressee::Nobody, "anyone?")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_mention_that_arrives_as_both_events_is_passed_on_once() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    socket
        .send(envelope("e1", "events_api", &mention("7.1", None, "<@UBOT> once")))
        .await;
    socket
        .send(envelope(
            "e2",
            "events_api",
            &channel_message("7.1", None, "<@UBOT> once"),
        ))
        .await;
    socket
        .send(envelope("e3", "events_api", &mention("7.2", None, "<@UBOT> next")))
        .await;
    assert_eq!(running.next().await, message("7.1", at("C1", None), "once"));
    assert_eq!(running.next().await, message("7.2", at("C1", None), "next"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_thread_is_read_without_the_messages_of_bots_or_edits() {
    let slack = FakeSlack::start().await;
    slack.script(
        "conversations.replies",
        vec![Answer::Json(json!({"ok": true, "messages": [
            {"type": "message", "user": "U1", "text": "root &amp; all", "ts": "8.0", "thread_ts": "8.0"},
            {"type": "message", "bot_id": "B9", "text": "a bot says", "ts": "8.1"},
            {"type": "message", "subtype": "channel_join", "user": "U3", "text": "joined", "ts": "8.2"},
            {"type": "message", "user": "U2", "text": "me too", "ts": "8.3"},
        ]}))],
    );
    let read = channel(&slack)
        .thread_history(&at("C1", Some("8.0")), 50)
        .await
        .unwrap();
    assert_eq!(
        read,
        [
            earlier("8.0", "U1", "root & all", Addressee::Nobody),
            earlier("8.3", "U2", "me too", Addressee::Nobody)
        ]
    );
    let calls = slack.calls("conversations.replies");
    assert_eq!(calls.len(), 1, "an answer without a cursor is the thread's last page");
    // A page holds as many messages as Slack recommends asking for, however few are kept.
    assert_eq!(
        (calls[0].arg("channel"), calls[0].arg("ts"), calls[0].arg("limit")),
        (Some("C1"), Some("8.0"), Some("200"))
    );
    assert_eq!(calls[0].arg("cursor"), None);
    assert_eq!(calls[0].authorization, format!("Bearer {BOT_TOKEN}"));
}

#[tokio::test]
async fn a_thread_is_read_with_what_other_bots_said_once_the_bot_knows_itself() {
    let mut slack = FakeSlack::start().await;
    slack.script(
        "conversations.replies",
        vec![Answer::Json(json!({"ok": true, "messages": [
            // An alert as integrations post it: no text of its own, what it says attached.
            {"type": "message", "subtype": "bot_message", "bot_id": "B9", "text": "", "ts": "8.0", "thread_ts": "8.0",
             "attachments": [
                {"fallback": "[FIRING:1] DiskFull | http://alerts", "title": "[FIRING:1] DiskFull", "text": "node-3 at 97% &amp; rising"},
                {"fallback": "silence it at http://alerts/silence"},
             ]},
            {"type": "message", "subtype": "bot_message", "bot_id": "BBOT", "text": "Working…", "ts": "8.1"},
            {"type": "message", "user": "UBOT", "bot_id": "BBOT", "text": "an earlier answer", "ts": "8.2"},
            {"type": "message", "user": "UB7", "bot_id": "B7", "text": "deploy 42 finished", "ts": "8.3"},
            {"type": "message", "user": "U2", "text": "what is this?", "ts": "8.4"},
        ]}))],
    );
    let channel = channel(&slack);
    // Connected, the channel has asked Slack who it is.
    let running = run_channel(channel.clone(), WAIT);
    let _socket = slack.socket().await;

    let read = channel.thread_history(&at("C1", Some("8.0")), 50).await.unwrap();

    assert_eq!(
        read,
        [
            earlier(
                "8.0",
                "B9",
                "[FIRING:1] DiskFull\nnode-3 at 97% & rising\nsilence it at http://alerts/silence",
                Addressee::Others
            ),
            earlier("8.3", "UB7", "deploy 42 finished", Addressee::Others),
            earlier("8.4", "U2", "what is this?", Addressee::Nobody),
        ]
    );
    running.stop().await.unwrap();
}

/// The mention of the bot posted in C1 itself at `ts`.
fn mention_at(ts: &str) -> MessageRef {
    MessageRef {
        at: at("C1", None),
        id: MessageId(ts.into()),
    }
}

#[tokio::test]
async fn a_channel_is_read_before_a_mention_oldest_first_with_whom_each_message_was_for() {
    let mut slack = FakeSlack::start().await;
    slack.script(
        "conversations.history",
        vec![Answer::Json(json!({"ok": true, "messages": [
            // The latest first, as Slack returns a channel.
            {"type": "message", "user": "U2", "text": "me &amp; too", "ts": "9.4"},
            {"type": "message", "user": "UBOT", "bot_id": "BBOT", "text": "an earlier answer", "ts": "9.3"},
            {"type": "message", "subtype": "channel_join", "user": "U3", "text": "joined", "ts": "9.2"},
            {"type": "message", "user": "U1", "text": "<@UBOT> is it done?", "ts": "9.1", "thread_ts": "9.1"},
            {"type": "message", "user": "UB7", "bot_id": "B7", "text": "<@UBOT> deploy 42 finished", "ts": "9.0"},
        ]}))],
    );
    let channel = channel(&slack);
    // Connected, the channel has asked Slack who it is.
    let running = run_channel(channel.clone(), WAIT);
    let _socket = slack.socket().await;

    let read = channel.chat_history(&mention_at("9.5"), 50).await.unwrap();

    // Another bot's mention starts nothing; the bot's own answer and the join are left out.
    assert_eq!(
        read,
        [
            earlier("9.0", "UB7", "<@UBOT> deploy 42 finished", Addressee::Others),
            earlier("9.1", "U1", "<@UBOT> is it done?", Addressee::Bot),
            earlier("9.4", "U2", "me & too", Addressee::Nobody),
        ]
    );
    let calls = slack.calls("conversations.history");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].arg("channel"), calls[0].arg("latest"), calls[0].arg("limit")),
        (Some("C1"), Some("9.5"), Some("200"))
    );
    assert_eq!(calls[0].authorization, format!("Bearer {BOT_TOKEN}"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_channel_read_before_a_mention_gives_its_latest_messages_up_to_the_limit() {
    let slack = FakeSlack::start().await;
    slack.script(
        "conversations.history",
        vec![Answer::Json(json!({"ok": true, "has_more": true, "messages": [
            {"type": "message", "user": "U1", "text": "third", "ts": "9.2"},
            {"type": "message", "user": "U1", "text": "second", "ts": "9.1"},
            {"type": "message", "user": "U1", "text": "first", "ts": "9.0"},
        ]}))],
    );
    let read = channel(&slack).chat_history(&mention_at("9.5"), 2).await.unwrap();
    assert_eq!(timestamps(&read), ["9.1", "9.2"]);
}

/// One page of `conversations.replies`: messages of U1 with these timestamps, oldest first
/// as Slack returns them, and the cursor of the next page, empty on the last one.
fn replies_page(timestamps: &[&str], next_cursor: &str) -> Answer {
    let messages: Vec<Value> = timestamps
        .iter()
        .map(|ts| json!({"type": "message", "user": "U1", "text": format!("said at {ts}"), "ts": ts}))
        .collect();
    Answer::Json(json!({
        "ok": true,
        "messages": messages,
        "has_more": !next_cursor.is_empty(),
        "response_metadata": {"next_cursor": next_cursor},
    }))
}

/// The timestamps of the messages read, in order.
fn timestamps(read: &[Said]) -> Vec<&str> {
    read.iter().map(|said| said.id.0.as_str()).collect()
}

#[tokio::test]
async fn a_thread_of_several_pages_is_read_to_its_end_for_its_latest_messages() {
    let slack = FakeSlack::start().await;
    slack.script(
        "conversations.replies",
        vec![
            replies_page(&["8.0", "8.1", "8.2"], "page-2"),
            replies_page(&["8.3", "8.4"], ""),
        ],
    );
    let read = channel(&slack).thread_history(&at("C1", Some("8.0")), 3).await.unwrap();
    assert_eq!(timestamps(&read), ["8.2", "8.3", "8.4"]);
    let calls = slack.calls("conversations.replies");
    let cursors: Vec<Option<&str>> = calls.iter().map(|call| call.arg("cursor")).collect();
    assert_eq!(cursors, [None, Some("page-2")]);
    assert_eq!((calls[1].arg("channel"), calls[1].arg("ts")), (Some("C1"), Some("8.0")));
}

#[tokio::test]
async fn a_later_page_that_cannot_be_read_leaves_the_messages_read_so_far() {
    let slack = FakeSlack::start().await;
    slack.script(
        "conversations.replies",
        vec![
            replies_page(&["8.0", "8.1"], "page-2"),
            // Waiting this out would never end: a later page is given up at once instead.
            Answer::RateLimited {
                retry_after_secs: 100_000,
            },
        ],
    );
    let (channel, mut reports) = reporting(config(&slack));
    let read = tokio::time::timeout(WAIT, channel.thread_history(&at("C1", Some("8.0")), 50))
        .await
        .expect("the rate limit of a later page was waited out")
        .unwrap();
    assert_eq!(timestamps(&read), ["8.0", "8.1"]);
    let report = reports.try_recv().unwrap();
    assert!(
        report.contains("conversations.replies") && report.contains("8.0") && report.contains("C1"),
        "{report}"
    );
}

#[tokio::test]
async fn a_thread_of_too_many_pages_gives_the_latest_messages_of_those_read() {
    let slack = FakeSlack::start().await;
    // One page more than the channel reads of a thread.
    let pages = ["8.0", "8.1", "8.2", "8.3", "8.4", "8.5"]
        .iter()
        .map(|ts| replies_page(&[ts], "more"))
        .collect();
    slack.script("conversations.replies", pages);
    let (channel, mut reports) = reporting(config(&slack));
    let read = channel.thread_history(&at("C1", Some("8.0")), 2).await.unwrap();
    assert_eq!(timestamps(&read), ["8.3", "8.4"]);
    assert_eq!(slack.calls("conversations.replies").len(), 5);
    let report = reports.try_recv().unwrap();
    assert!(report.contains("5 pages"), "{report}");
}

#[tokio::test]
async fn a_name_is_the_display_name_or_else_the_real_name() {
    let slack = FakeSlack::start().await;
    slack.script(
        "users.info",
        vec![
            Answer::Json(json!({"ok": true, "user": {"profile": {"display_name": "ann", "real_name": "Ann Lee"}}})),
            Answer::Json(json!({"ok": true, "user": {"profile": {"display_name": "", "real_name": "Bob Ray"}}})),
        ],
    );
    let channel = channel(&slack);
    let [ann, bob, nameless] = ["U1", "U2", "U3"].map(|user| UserId(user.into()));
    assert_eq!(channel.display_name(&ann).await.unwrap().as_deref(), Some("ann"));
    assert_eq!(channel.display_name(&bob).await.unwrap().as_deref(), Some("Bob Ray"));
    // The fake's own answer has no name at all.
    assert_eq!(channel.display_name(&nameless).await.unwrap(), None);
    assert_eq!(slack.calls("users.info")[0].arg("user"), Some("U1"));
}

#[tokio::test]
async fn an_integration_without_a_user_is_named_by_its_bot_record() {
    let slack = FakeSlack::start().await;
    slack.script("users.info", vec![Answer::Error("user_not_found")]);
    slack.script(
        "bots.info",
        vec![Answer::Json(
            json!({"ok": true, "bot": {"id": "B9", "deleted": false, "name": "alerts"}}),
        )],
    );
    let channel = channel(&slack);

    let name = channel.display_name(&UserId("B9".into())).await.unwrap();

    assert_eq!(name.as_deref(), Some("alerts"));
    assert_eq!(slack.calls("bots.info")[0].arg("bot"), Some("B9"));
}

#[tokio::test]
async fn an_acknowledgement_is_a_reaction_and_a_repeated_one_is_no_error() {
    let slack = FakeSlack::start().await;
    slack.script("reactions.add", vec![Answer::Error("already_reacted")]);
    let channel = channel(&slack);
    let steered = MessageRef {
        at: at("C1", Some("9.0")),
        id: MessageId("9.1".into()),
    };
    channel.acknowledge(&steered).await.unwrap();
    channel.acknowledge(&steered).await.unwrap();
    let calls = slack.calls("reactions.add");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].body,
        json!({"channel": "C1", "timestamp": "9.1", "name": "eyes"})
    );
}

/// A message of the direct chat D1, for fetching its images.
fn posted_message() -> MessageRef {
    MessageRef {
        at: at("D1", None),
        id: MessageId("1.000100".into()),
    }
}

#[tokio::test]
async fn a_message_passes_on_its_images_and_no_other_files() {
    let mut slack = FakeSlack::start().await;
    let mut running = run_channel(channel(&slack), WAIT);
    let mut socket = slack.socket().await;
    let shot = slack.file("shot.png", "image/png", b"png".to_vec());
    let files = json!({"subtype": "file_share", "text": "what is this?", "files": [
        {"id": "F1", "name": "shot.png", "mimetype": "image/png", "url_private": "https://unused.invalid/x", "url_private_download": shot},
        {"id": "F2", "name": "report.pdf", "mimetype": "application/pdf", "url_private": "https://files.slack.com/files-pri/T1-F2/report.pdf"},
        {"id": "F3", "name": "connect.png", "mimetype": "image/png", "file_access": "check_file_info"},
    ]});
    socket
        .send(envelope("e1", "events_api", &direct("1.000100", files)))
        .await;
    socket.receive().await;
    let Inbound::Message { text, images, .. } = running.next().await else {
        panic!("expected a message");
    };
    assert_eq!(text, "what is this?");
    assert_eq!(
        images,
        [ImageRef {
            key: shot,
            name: "shot.png".into()
        }]
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn an_image_is_fetched_with_the_bot_token() {
    let slack = FakeSlack::start().await;
    let key = slack.file("shot.png", "image/png", b"png bytes".to_vec());
    let image = ImageRef {
        key,
        name: "shot.png".into(),
    };
    let bytes = channel(&slack).image(&posted_message(), &image, 1024).await.unwrap();
    assert_eq!(bytes, b"png bytes");
    let fetched = slack.calls("/files-pri/T1-F1/shot.png");
    assert_eq!(fetched[0].authorization, format!("Bearer {BOT_TOKEN}"));
}

#[tokio::test]
async fn an_image_larger_than_asked_for_is_refused() {
    let slack = FakeSlack::start().await;
    let key = slack.file("big.png", "image/png", vec![0; 100]);
    let image = ImageRef {
        key,
        name: "big.png".into(),
    };
    let error = channel(&slack).image(&posted_message(), &image, 99).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "slack file big.png: the file is larger than 99 bytes"
    );
}

#[tokio::test]
async fn a_file_slack_does_not_keep_is_refused_without_sending_the_token() {
    let slack = FakeSlack::start().await;
    let image = ImageRef {
        key: "https://example.com/files-pri/T1-F1/shot.png".into(),
        name: "shot.png".into(),
    };
    let error = channel(&slack)
        .image(&posted_message(), &image, 1024)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "slack: https://example.com/files-pri/T1-F1/shot.png is not a URL of a file Slack keeps"
    );
    assert!(slack.calls("/files-pri/T1-F1/shot.png").is_empty());
}

#[tokio::test]
async fn a_sign_in_page_in_place_of_the_file_points_at_the_missing_scope() {
    let slack = FakeSlack::start().await;
    let key = slack.file("shot.png", "image/png", b"png".to_vec());
    let mut config = config(&slack);
    config.bot_token = "xoxb-without-files-read".into();
    let channel = SlackChannel::new(config).unwrap();
    let image = ImageRef {
        key,
        name: "shot.png".into(),
    };
    let error = channel.image(&posted_message(), &image, 1024).await.unwrap_err();
    assert!(
        error.to_string().contains("the bot token may lack files:read"),
        "{error}"
    );
}

#[tokio::test]
async fn a_file_is_uploaded_then_shared_in_its_thread() {
    let slack = FakeSlack::start().await;
    let file = Attachment {
        name: "report.md".into(),
        bytes: b"# Report".to_vec(),
    };
    channel(&slack)
        .send_file(&at("C1", Some("1700000001.000100")), &file)
        .await
        .unwrap();
    let ticket = &slack.calls("files.getUploadURLExternal")[0];
    assert_eq!(
        (
            ticket.arg("filename"),
            ticket.arg("length"),
            ticket.authorization.as_str()
        ),
        (Some("report.md"), Some("8"), format!("Bearer {BOT_TOKEN}").as_str())
    );
    // The bytes go to the address Slack gave, without the token.
    let upload = &slack.calls("/upload/v1/F1")[0];
    assert_eq!(
        (upload.bytes.as_slice(), upload.authorization.as_str()),
        (&b"# Report"[..], "")
    );
    let shared = &slack.calls("files.completeUploadExternal")[0];
    assert_eq!(
        shared.body,
        json!({"files": [{"id": "F1", "title": "report.md"}], "channel_id": "C1", "thread_ts": "1700000001.000100"})
    );
}

#[tokio::test]
async fn a_file_slack_refuses_fails_with_the_refusal_and_is_not_shared() {
    let slack = FakeSlack::start().await;
    slack.script("files.getUploadURLExternal", vec![Answer::Error("missing_scope")]);
    let file = Attachment {
        name: "report.md".into(),
        bytes: b"# Report".to_vec(),
    };
    let error = channel(&slack).send_file(&at("C1", None), &file).await.unwrap_err();
    assert!(
        error.to_string().contains("files.getUploadURLExternal: missing_scope"),
        "{error}"
    );
    assert!(slack.calls("files.completeUploadExternal").is_empty());
}
