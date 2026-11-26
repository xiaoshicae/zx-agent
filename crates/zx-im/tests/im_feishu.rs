//! The Feishu channel against a fake open platform: the long connection (pings, pongs,
//! acknowledgements, fragments, redeliveries, reconnection), messages and card clicks,
//! tokens, replies in topics, edits, cards, rate limits, the images of messages, files posted
//! and the stop.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use zx_agent::events::ApprovalResult;
use zx_im::{
    Addressee, ApprovalCard, ApprovalId, Attachment, ChatKind, Choice, Destination, FeishuChannel, FeishuConfig,
    ImChannel, ImError, ImageRef, Inbound, MessageId, MessageRef, Post, Replacement, UserId,
};
use zx_testing::WAIT;
use zx_testing::fake_feishu::{Answer, FakeFeishu, TestFrame};
use zx_testing::http::silent_listener;

use self::common::im::{FullBacklog, at, earlier, run_channel, run_channel_with};

fn config(fake: &FakeFeishu) -> FeishuConfig {
    let mut config = FeishuConfig::new("cli_test".into(), "secret-test".into());
    config.api_base = fake.api_base.clone();
    config.token_url = fake.token_url.clone();
    config.post_interval = Duration::ZERO;
    config
}

fn channel(fake: &FakeFeishu) -> Arc<FeishuChannel> {
    Arc::new(FeishuChannel::new(config(fake)).unwrap())
}

/// A channel that passes each error it reports on as text.
fn reporting_channel(fake: &FakeFeishu) -> (Arc<FeishuChannel>, mpsc::UnboundedReceiver<String>) {
    let (reported, reports) = mpsc::unbounded_channel();
    let mut config = config(fake);
    config.on_error = Some(Arc::new(move |error: &ImError| {
        reported.send(error.to_string()).ok();
    }));
    (Arc::new(FeishuChannel::new(config).unwrap()), reports)
}

fn payload(event_type: &str, event: &Value) -> Vec<u8> {
    json!({"schema": "2.0", "header": {"event_id": "ev1", "event_type": event_type}, "event": event})
        .to_string()
        .into_bytes()
}

fn text_message(id: &str, chat: &str, chat_type: &str, root: Option<&str>, text: &str, mentions: &Value) -> Value {
    let mut message = json!({
        "message_id": id, "chat_id": chat, "chat_type": chat_type, "message_type": "text",
        "content": json!({"text": text}).to_string(), "mentions": mentions,
    });
    if let Some(root) = root {
        message["root_id"] = json!(root);
    }
    json!({"sender": {"sender_id": {"open_id": "ou_u1"}, "sender_type": "user"}, "message": message})
}

fn bot_mention() -> Value {
    json!([{"key": "@_user_1", "mentioned_type": "bot", "name": "zx", "id": {"open_id": "ou_bot"}}])
}

fn received(event: &Value) -> Vec<u8> {
    payload("im.message.receive_v1", event)
}

/// A message from ou_u1 in a group that mentions the bot.
fn message(id: &str, at: Destination, text: &str) -> Inbound {
    said(id, at, ChatKind::Group, Addressee::Bot, text)
}

/// A message from ou_u1 in the direct chat oc_d.
fn direct(id: &str, text: &str) -> Inbound {
    said(id, at("oc_d", None), ChatKind::Direct, Addressee::Bot, text)
}

fn said(id: &str, at: Destination, chat: ChatKind, addressee: Addressee, text: &str) -> Inbound {
    Inbound::Message {
        id: MessageId(id.into()),
        at,
        chat,
        addressee,
        sender: UserId("ou_u1".into()),
        text: text.into(),
        images: Vec::new(),
    }
}

fn content(call_body: &Value) -> Value {
    serde_json::from_str(call_body["content"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn the_channel_connects_with_the_app_credentials_and_pings_at_once() {
    let mut fake = FakeFeishu::start(120).await;
    let running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let ping = socket.receive().await;
    assert_eq!((ping.method, ping.service, ping.header("type")), (0, 33, Some("ping")));
    let call = &fake.calls("endpoint")[0];
    assert_eq!(
        call.body,
        json!({"AppID": "cli_test", "AppSecret": "secret-test", "ClientAssertion": ""})
    );
    assert_eq!(call.locale.as_deref(), Some("zh"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_group_mention_is_acknowledged_and_passed_on_without_the_bot() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let event = TestFrame::event(
        "f1",
        1,
        0,
        &received(&text_message(
            "om_1",
            "oc_1",
            "group",
            None,
            "@_user_1 pods",
            &bot_mention(),
        )),
    );
    socket.send(&event).await;
    let ack = socket.receive_data().await;
    assert_eq!(
        (ack.seq_id, ack.log_id, ack.service, ack.method, ack.log_id_new.as_str()),
        (7, 8, 33, 1, "n1")
    );
    assert_eq!(ack.headers[..event.headers.len()], event.headers[..]);
    assert_eq!(ack.header("biz_rt"), Some("0"));
    assert_eq!(
        serde_json::from_slice::<Value>(&ack.payload).unwrap(),
        json!({"code": 200, "headers": null, "data": null})
    );
    assert_eq!(running.next().await, message("om_1", at("oc_1", None), "pods"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_reply_names_the_root_of_its_thread() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let event = text_message(
        "om_2",
        "oc_1",
        "group",
        Some("om_root"),
        "@_user_1 more",
        &bot_mention(),
    );
    socket.send(&TestFrame::event("f1", 1, 0, &received(&event))).await;
    assert_eq!(
        running.next().await,
        message("om_2", at("oc_1", Some("om_root")), "more")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn only_what_people_address_to_the_bot_is_passed_on() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let unmentioned = text_message("om_1", "oc_1", "group", None, "chatter", &json!([]));
    let mut from_bot = text_message("om_2", "oc_d", "p2p", None, "echo", &json!([]));
    from_bot["sender"]["sender_type"] = json!("bot");
    let mut file = text_message("om_3", "oc_d", "p2p", None, "", &json!([]));
    file["message"]["message_type"] = json!("file");
    let wanted = text_message("om_4", "oc_d", "p2p", None, "hi", &json!([]));
    for (n, event) in [unmentioned, from_bot, file, wanted].into_iter().enumerate() {
        socket
            .send(&TestFrame::event(&format!("f{n}"), 1, 0, &received(&event)))
            .await;
        socket.receive_data().await;
    }
    assert_eq!(running.next().await, direct("om_4", "hi"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn replies_in_a_topic_are_passed_on_with_whom_they_mention() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let person = json!([{"key": "@_user_1", "mentioned_type": "user", "name": "Li", "id": {"open_id": "ou_li"}}]);
    let root = Some("om_root");
    let events = [
        text_message("om_1", "oc_1", "group", None, "chatter in the group", &json!([])),
        text_message("om_2", "oc_1", "group", root, "and then?", &json!([])),
        text_message("om_3", "oc_1", "group", root, "@_user_1 over to you", &person),
    ];
    for (n, event) in events.into_iter().enumerate() {
        socket
            .send(&TestFrame::event(&format!("f{n}"), 1, 0, &received(&event)))
            .await;
        socket.receive_data().await;
    }
    let topic = || at("oc_1", root);
    assert_eq!(
        running.next().await,
        said("om_2", topic(), ChatKind::Group, Addressee::Nobody, "and then?")
    );
    assert_eq!(
        running.next().await,
        said("om_3", topic(), ChatKind::Group, Addressee::Others, "@Li over to you")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_redelivered_message_is_passed_on_once() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let once = text_message("om_1", "oc_d", "p2p", None, "once", &json!([]));
    let next = text_message("om_2", "oc_d", "p2p", None, "next", &json!([]));
    for (frame_id, event) in [("f1", once.clone()), ("f2", once), ("f3", next)] {
        socket.send(&TestFrame::event(frame_id, 1, 0, &received(&event))).await;
        assert_eq!(socket.receive_data().await.header("message_id"), Some(frame_id));
    }
    assert_eq!(running.next().await, direct("om_1", "once"));
    assert_eq!(running.next().await, direct("om_2", "next"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_that_cannot_be_kept_is_reported_and_left_unacknowledged() {
    let mut fake = FakeFeishu::start(120).await;
    let (channel, mut reports) = reporting_channel(&fake);
    let mut running = run_channel_with(channel, Arc::new(FullBacklog), WAIT);
    let mut socket = fake.socket().await;
    let kept = text_message("om_1", "oc_d", "p2p", None, "keep me", &json!([]));
    socket.send(&TestFrame::event("f1", 1, 0, &received(&kept))).await;
    // An event with no message to keep is acknowledged; the first one was not.
    let other = payload("im.chat.updated_v1", &json!({}));
    socket.send(&TestFrame::event("f2", 1, 0, &other)).await;
    assert_eq!(socket.receive_data().await.header("message_id"), Some("f2"));
    let report = reports.recv().await.unwrap();
    assert!(
        report.contains("cannot keep message om_1: no space left on device"),
        "{report}"
    );
    assert!(running.nothing_passed());
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_fragmented_event_is_joined_and_acknowledged_once() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let whole = received(&text_message("om_1", "oc_d", "p2p", None, "joined", &json!([])));
    let (first, second) = whole.split_at(whole.len() / 2);
    socket.send(&TestFrame::event("f9", 2, 1, second)).await;
    socket.send(&TestFrame::event("f9", 2, 0, first)).await;
    let ack = socket.receive_data().await;
    assert_eq!((ack.header("message_id"), ack.header("seq")), (Some("f9"), Some("0")));
    assert_eq!(running.next().await, direct("om_1", "joined"));
    // The next data frame is the acknowledgement of a later event: the first fragment had none.
    let later = text_message("om_2", "oc_d", "p2p", None, "later", &json!([]));
    socket.send(&TestFrame::event("f10", 1, 0, &received(&later))).await;
    assert_eq!(socket.receive_data().await.header("message_id"), Some("f10"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_card_click_becomes_a_decision_in_its_thread() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let click = json!({
        "operator": {"open_id": "ou_u2"},
        "action": {"tag": "button", "value": {"zx": "deny", "approval": "A1", "chat": "oc_1", "thread": "om_root"}},
        "context": {"open_message_id": "om_card", "open_chat_id": "oc_1"},
    });
    socket
        .send(&TestFrame::event("f1", 1, 0, &payload("card.action.trigger", &click)))
        .await;
    socket.receive_data().await;
    assert_eq!(
        running.next().await,
        Inbound::Decision {
            at: at("oc_1", Some("om_root")),
            user: UserId("ou_u2".into()),
            approval: ApprovalId("A1".into()),
            choice: Choice::Deny,
        }
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_pong_can_shorten_the_ping_interval() {
    let mut fake = FakeFeishu::start(120).await;
    let running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    assert_eq!(socket.receive().await.header("type"), Some("ping"));
    let pong = TestFrame {
        service: 33,
        headers: vec![("type".into(), "pong".into())],
        payload: br#"{"PingInterval":1}"#.to_vec(),
        ..TestFrame::default()
    };
    socket.send(&pong).await;
    // Without the new interval the next ping would come after 120 seconds.
    assert_eq!(socket.receive().await.header("type"), Some("ping"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_refused_endpoint_stops_the_channel_with_a_configuration_error() {
    let fake = FakeFeishu::start(120).await;
    fake.script("endpoint", vec![Answer::Code(19001)]);
    let running = run_channel(channel(&fake), WAIT);
    match running.ended().await {
        Err(ImError::Config(error)) => assert!(error.contains("19001"), "{error}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

/// The configuration error of a channel made with the settings that `set` changes.
fn refused(set: impl FnOnce(&mut FeishuConfig)) -> String {
    let mut config = FeishuConfig::new("cli_test".into(), "secret-test".into());
    set(&mut config);
    match FeishuChannel::new(config) {
        Err(ImError::Config(error)) => error,
        Err(other) => panic!("expected a configuration error, got {other:?}"),
        Ok(_) => panic!("a channel was made"),
    }
}

#[test]
fn a_channel_is_not_made_with_an_address_that_calls_cannot_go_to() {
    for (address, expected) in [
        ("example.com", "api_base is not a URL"),
        ("localhost:8080", "api_base must be an http or https URL"),
        ("https://example.com/open?tenant=1", "api_base must end with its path"),
    ] {
        let error = refused(|config| config.api_base = address.into());
        assert!(error.starts_with(expected), "{address}: {error}");
    }
    for (address, expected) in [
        ("example.com/token", "token_url is not a URL"),
        ("ftp://example.com/token", "token_url must be an http or https URL"),
    ] {
        let error = refused(|config| config.token_url = address.into());
        assert!(error.starts_with(expected), "{address}: {error}");
    }
}

#[tokio::test]
async fn a_dropped_connection_is_opened_again() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    fake.socket().await.close().await;
    let mut second = fake.socket().await;
    let event = text_message("om_1", "oc_d", "p2p", None, "back", &json!([]));
    second.send(&TestFrame::event("f1", 1, 0, &received(&event))).await;
    assert_eq!(running.next().await, direct("om_1", "back"));
    assert_eq!(fake.calls("endpoint").len(), 2);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_silent_connection_is_reported_and_opened_again() {
    // Pings every second: three seconds of silence end the connection.
    let mut fake = FakeFeishu::start(1).await;
    let (channel, mut reports) = reporting_channel(&fake);
    let running = run_channel(channel, WAIT);
    // Answers nothing, not even the pings.
    let first = fake.socket().await;
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("silent"), "{report}");
    let second = fake.socket().await;
    assert_eq!(fake.calls("endpoint").len(), 2);
    drop((first, second));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_lost_connection_is_reported_and_opened_again() {
    let mut fake = FakeFeishu::start(120).await;
    let (channel, mut reports) = reporting_channel(&fake);
    let running = run_channel(channel, WAIT);
    // Gone without a close frame.
    drop(fake.socket().await);
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("lost"), "{report}");
    let second = fake.socket().await;
    assert_eq!(fake.calls("endpoint").len(), 2);
    drop(second);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn text_in_a_thread_is_a_reply_in_its_topic() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let to = at("oc_1", Some("om_root"));
    let posted = channel.send(&to, &Post::Text("hi".into())).await.unwrap();
    let call = &fake.calls("reply")[0];
    assert_eq!(call.path, "/open-apis/im/v1/messages/om_root/reply");
    assert_eq!(call.authorization, "Bearer t-1");
    assert_eq!(
        call.body,
        json!({"msg_type": "text", "content": "{\"text\":\"hi\"}", "reply_in_thread": true})
    );
    assert_eq!(
        posted,
        MessageRef {
            at: to,
            id: MessageId("om_out1".into())
        }
    );
}

#[tokio::test]
async fn text_without_a_thread_starts_a_message_and_the_token_is_reused() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    for text in ["one", "two"] {
        channel.send(&at("oc_1", None), &Post::Text(text.into())).await.unwrap();
    }
    let creates = fake.calls("create");
    assert_eq!(creates.len(), 2);
    assert_eq!(creates[0].path, "/open-apis/im/v1/messages?receive_id_type=chat_id");
    assert_eq!(
        creates[0].body,
        json!({"receive_id": "oc_1", "msg_type": "text", "content": "{\"text\":\"one\"}"})
    );
    let tokens = fake.calls("token");
    assert_eq!(tokens.len(), 1);
    assert_eq!(
        tokens[0].body,
        json!({"grant_type": "client_credentials", "client_id": "cli_test", "client_secret": "secret-test"})
    );
}

#[tokio::test]
async fn replies_in_a_direct_chat_stay_out_of_topics() {
    let mut fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let mut running = run_channel(Arc::clone(&channel), WAIT);
    let mut socket = fake.socket().await;
    let event = text_message("om_d1", "oc_d", "p2p", None, "hi", &json!([]));
    socket.send(&TestFrame::event("f1", 1, 0, &received(&event))).await;
    running.next().await;
    channel
        .send(&at("oc_d", Some("om_d1")), &Post::Text("answer".into()))
        .await
        .unwrap();
    assert_eq!(fake.calls("reply")[0].body["reply_in_thread"], json!(false));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_refused_secret_is_a_configuration_error() {
    let fake = FakeFeishu::start(120).await;
    fake.script("token", vec![Answer::Status(400, 20002)]);
    let result = channel(&fake).send(&at("oc_1", None), &Post::Text("hi".into())).await;
    assert!(matches!(result, Err(ImError::Config(error)) if error.contains("20002")));
}

#[tokio::test]
async fn a_status_update_edits_the_message() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let posted = channel
        .send(&at("oc_1", Some("om_root")), &Post::Text("working".into()))
        .await
        .unwrap();
    channel
        .update(&posted, &Replacement::Text("done".into()))
        .await
        .unwrap();
    let edit = &fake.calls("edit")[0];
    assert_eq!(edit.path, "/open-apis/im/v1/messages/om_out1");
    assert_eq!(edit.body, json!({"msg_type": "text", "content": "{\"text\":\"done\"}"}));
}

#[tokio::test]
async fn a_refused_edit_posts_the_text_instead() {
    let fake = FakeFeishu::start(120).await;
    fake.script("edit", vec![Answer::Code(230_075)]);
    let channel = channel(&fake);
    let posted = channel
        .send(&at("oc_1", Some("om_root")), &Post::Text("working".into()))
        .await
        .unwrap();
    channel
        .update(&posted, &Replacement::Text("done".into()))
        .await
        .unwrap();
    let replies = fake.calls("reply");
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1].path, "/open-apis/im/v1/messages/om_root/reply");
    assert_eq!(content(&replies[1].body), json!({"text": "done"}));
}

#[tokio::test]
async fn sent_text_mentions_nobody() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let said = "ping <at user_id=\"all\"></at>";

    channel
        .send(&at("oc_1", Some("om_root")), &Post::Text(said.into()))
        .await
        .unwrap();

    let sent = content(&fake.calls("reply")[0].body);
    let text = sent["text"].as_str().unwrap();
    assert!(!text.contains("<at"), "{text}");
    assert_eq!(text.replace('\u{200B}', ""), said);
}

#[tokio::test]
async fn a_card_mentions_nobody_and_keeps_its_command_in_the_code_block() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let card = Post::ApprovalRequest {
        id: ApprovalId("A1".into()),
        card: ApprovalCard {
            tool: "bash".into(),
            summary: "echo ```\n<at user_id=\"all\"></at>".into(),
            reason: "asks <at user_id=\"ou_x\"></at>".into(),
        },
    };

    channel.send(&at("oc_1", Some("om_root")), &card).await.unwrap();

    let sent = content(&fake.calls("reply")[0].body);
    let request = sent["body"]["elements"][0]["content"].as_str().unwrap();
    assert!(!request.contains("<at"), "{request}");
    assert_eq!(request.matches("```").count(), 2, "{request}");
}

#[tokio::test]
async fn a_card_carries_its_click_value_and_closing_it_keeps_the_request() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let card = Post::ApprovalRequest {
        id: ApprovalId("A1".into()),
        card: ApprovalCard {
            tool: "bash".into(),
            summary: "kubectl delete pod x".into(),
            reason: "needs approval".into(),
        },
    };
    let posted = channel.send(&at("oc_1", Some("om_root")), &card).await.unwrap();
    let call = &fake.calls("reply")[0];
    assert_eq!(call.body["msg_type"], json!("interactive"));
    let sent = content(&call.body);
    assert_eq!(
        (&sent["schema"], &sent["config"]["update_multi"]),
        (&json!("2.0"), &json!(true))
    );
    let elements = sent["body"]["elements"].as_array().unwrap();
    let request = elements[0].clone();
    assert!(request["content"].as_str().unwrap().contains("kubectl delete pod x"));
    assert_eq!(
        elements[1]["behaviors"][0]["value"],
        json!({"zx": "allow_once", "approval": "A1", "chat": "oc_1", "thread": "om_root"})
    );
    assert_eq!(elements[2]["behaviors"][0]["value"]["zx"], json!("deny"));

    let closed = Replacement::ApprovalResolved {
        card: ApprovalCard {
            tool: "bash".into(),
            summary: "kubectl delete pod x".into(),
            reason: "needs approval".into(),
        },
        id: ApprovalId("A1".into()),
        user: Some(UserId("ou_u2".into())),
        result: ApprovalResult::AllowOnce,
    };
    channel.update(&posted, &closed).await.unwrap();
    let patch = &fake.calls("patch")[0];
    assert_eq!(patch.path, format!("/open-apis/im/v1/messages/{}", posted.id.0));
    let elements = content(&patch.body)["body"]["elements"].clone();
    assert_eq!(
        elements,
        json!([request, {"tag": "markdown", "content": "Allowed (ou_u2)"}])
    );
}

#[tokio::test]
async fn a_rate_limited_call_waits_and_is_retried() {
    let fake = FakeFeishu::start(120).await;
    fake.script("create", vec![Answer::RateLimited { reset_secs: 0 }]);
    channel(&fake)
        .send(&at("oc_1", None), &Post::Text("hi".into()))
        .await
        .unwrap();
    assert_eq!(fake.calls("create").len(), 2);
}

#[tokio::test]
async fn a_call_rate_limited_for_longer_than_a_call_waits_fails_at_once_as_rate_limited() {
    let fake = FakeFeishu::start(120).await;
    fake.script("create", vec![Answer::RateLimited { reset_secs: 45 }]);
    let sent = tokio::time::timeout(WAIT, channel(&fake).send(&at("oc_1", None), &Post::Text("hi".into())))
        .await
        .expect("the call waited out a rate limit longer than a call waits");
    let error = sent.unwrap_err().to_string();
    assert!(
        error.ends_with("rate limited, for 45s: longer than a call waits (20s in all)"),
        "{error}"
    );
    assert_eq!(fake.calls("create").len(), 1);
}

#[tokio::test]
async fn a_busy_endpoint_is_reported_and_retried() {
    let mut fake = FakeFeishu::start(120).await;
    fake.script("endpoint", vec![Answer::Code(1)]);
    let (channel, mut reports) = reporting_channel(&fake);
    let running = run_channel(channel, WAIT);
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("code 1"), "{report}");
    // The next attempt connects.
    fake.socket().await;
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_frame_that_cannot_be_decoded_is_reported_and_the_connection_goes_on() {
    let mut fake = FakeFeishu::start(120).await;
    let (channel, mut reports) = reporting_channel(&fake);
    let mut running = run_channel(channel, WAIT);
    let mut socket = fake.socket().await;
    // Wire type 3, which the frame codec does not read.
    socket.send_bytes(vec![0x0b]).await;
    let report = tokio::time::timeout(WAIT, reports.recv()).await.unwrap().unwrap();
    assert!(report.contains("malformed frame"), "{report}");
    let event = text_message("om_1", "oc_d", "p2p", None, "still here", &json!([]));
    socket.send(&TestFrame::event("f1", 1, 0, &received(&event))).await;
    assert_eq!(running.next().await, direct("om_1", "still here"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn stopping_does_not_wait_for_a_connection_being_opened() {
    let (address, mut connections) = silent_listener().await;
    let mut config = FeishuConfig::new("cli_test".into(), "secret-test".into());
    config.api_base = format!("http://{address}");
    config.token_url = format!("http://{address}/oauth/v3/token");
    // Longer than the test waits, so that only the stop can end the attempt in time.
    config.request_timeout = 3 * WAIT;
    let running = run_channel(Arc::new(FeishuChannel::new(config).unwrap()), WAIT);
    connections.recv().await.unwrap();
    tokio::time::timeout(WAIT, running.stop())
        .await
        .expect("the stop waited for the connection attempt")
        .unwrap();
}

#[tokio::test]
async fn stopping_closes_the_connection() {
    let mut fake = FakeFeishu::start(120).await;
    let running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    running.stop().await.unwrap();
    assert!(socket.closed_by_client().await);
}

/// The direct message `id` of ou_u1 of the kind `kind` with `content`.
fn direct_of_kind(id: &str, kind: &str, content: &Value) -> Value {
    let mut event = text_message(id, "oc_d", "p2p", None, "", &json!([]));
    event["message"]["message_type"] = json!(kind);
    event["message"]["content"] = json!(content.to_string());
    event
}

/// The image `key`, as the channel passes it on.
fn image(key: &str) -> ImageRef {
    ImageRef {
        key: key.into(),
        name: key.into(),
    }
}

#[tokio::test]
async fn an_image_and_rich_text_are_passed_on_with_their_images() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run_channel(channel(&fake), WAIT);
    let mut socket = fake.socket().await;
    let picture = direct_of_kind("om_1", "image", &json!({"image_key": "img_1"}));
    let rich = direct_of_kind(
        "om_2",
        "post",
        &json!({"title": "Crash", "content": [
            [{"tag": "text", "text": "what is this?"}],
            [{"tag": "img", "image_key": "img_2"}],
        ]}),
    );
    for (n, event) in [picture, rich].into_iter().enumerate() {
        socket
            .send(&TestFrame::event(&format!("f{n}"), 1, 0, &received(&event)))
            .await;
        socket.receive_data().await;
    }
    let with_images = |id: &str, text: &str, images: Vec<ImageRef>| {
        let Inbound::Message {
            id,
            at,
            chat,
            addressee,
            sender,
            text,
            ..
        } = direct(id, text)
        else {
            unreachable!("a message")
        };
        Inbound::Message {
            id,
            at,
            chat,
            addressee,
            sender,
            text,
            images,
        }
    };
    assert_eq!(running.next().await, with_images("om_1", "", vec![image("img_1")]));
    assert_eq!(
        running.next().await,
        with_images("om_2", "Crash\nwhat is this?", vec![image("img_2")])
    );
    running.stop().await.unwrap();
}

/// The message `om_1` of the direct chat, whose images are fetched.
fn posted() -> MessageRef {
    MessageRef {
        at: at("oc_d", None),
        id: MessageId("om_1".into()),
    }
}

#[tokio::test]
async fn an_image_is_fetched_by_the_message_resource_api() {
    let fake = FakeFeishu::start(120).await;
    fake.image("img_1", b"png bytes".to_vec());
    let bytes = channel(&fake).image(&posted(), &image("img_1"), 1024).await.unwrap();
    assert_eq!(bytes, b"png bytes");
    let call = &fake.calls("resource")[0];
    assert_eq!(call.path, "/open-apis/im/v1/messages/om_1/resources/img_1?type=image");
    assert_eq!(call.authorization, "Bearer t-1");
}

#[tokio::test]
async fn an_image_feishu_refuses_or_one_too_large_is_an_error() {
    let fake = FakeFeishu::start(120).await;
    fake.image("img_1", vec![0; 100]);
    let channel = channel(&fake);
    let refused = channel.image(&posted(), &image("img_2"), 1024).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        "feishu /open-apis/im/v1/messages/om_1/resources/img_2?type=image: HTTP 400 Bad Request, code 234003: File not in msg."
    );
    let large = channel.image(&posted(), &image("img_1"), 99).await.unwrap_err();
    assert_eq!(
        large.to_string(),
        "feishu /open-apis/im/v1/messages/om_1/resources/img_1?type=image: the file is larger than 99 bytes"
    );
}

/// A message as the message APIs give it: `id`, sent at `sent_ms` by `sender`, of `kind` with
/// `content`, mentioning `mentions`.
fn listed(id: &str, sent_ms: &str, sender: Value, kind: &str, content: &Value, mentions: Value) -> Value {
    let mut item = json!({
        "message_id": id, "create_time": sent_ms, "msg_type": kind, "chat_id": "oc_1", "deleted": false,
        "body": {"content": content.to_string()},
    });
    item["sender"] = sender;
    item["mentions"] = mentions;
    item
}

/// [`listed`] for a text message.
fn listed_text(id: &str, sent_ms: &str, sender: Value, text: &str, mentions: Value) -> Value {
    listed(id, sent_ms, sender, "text", &json!({"text": text}), mentions)
}

/// A person as the sender of a message, by open_id.
fn person(open_id: &str) -> Value {
    json!({"id": open_id, "id_type": "open_id", "sender_type": "user", "tenant_key": "t1"})
}

/// An app as the sender of a message, by App ID.
fn app(app_id: &str) -> Value {
    json!({"id": app_id, "id_type": "app_id", "sender_type": "app", "tenant_key": "t1"})
}

/// A mention of the bot as the message APIs give it: by its open_id, with no type.
fn listed_bot_mention() -> Value {
    json!([{"key": "@_user_1", "id": "ou_bot", "id_type": "open_id", "name": "zx", "tenant_key": "t1"}])
}

/// An answer of the message APIs with these messages.
fn messages(items: Vec<Value>) -> Answer {
    let mut answer = json!({"code": 0, "msg": "success", "data": {"has_more": false}});
    answer["data"]["items"] = Value::Array(items);
    Answer::Json(answer)
}

/// The mention of the bot `id` posted in the group oc_1 itself.
fn group_mention(id: &str) -> MessageRef {
    MessageRef {
        at: at("oc_1", None),
        id: MessageId(id.into()),
    }
}

#[tokio::test]
async fn a_group_is_read_before_a_mention_up_to_its_time() {
    let fake = FakeFeishu::start(120).await;
    let mention = listed_text(
        "om_m",
        "1700000000500",
        person("ou_u1"),
        "@_user_1 why?",
        listed_bot_mention(),
    );
    fake.script("get", vec![messages(vec![mention.clone()])]);
    let recalled = json!({
        "message_id": "om_gone", "create_time": "1700000000300", "msg_type": "text", "deleted": true,
        "body": {"content": "This message was recalled"}, "sender": person("ou_u2"), "mentions": [],
    });
    let rich = json!({"title": "", "content": [[{"tag": "text", "text": "deploy failed"}]]});
    fake.script(
        "list",
        vec![messages(vec![
            // The latest first, as asked.
            listed_text("om_after", "1700000000900", person("ou_u2"), "just after", json!([])),
            mention,
            listed_text(
                "om_own",
                "1700000000400",
                app("cli_test"),
                "an earlier answer",
                json!([]),
            ),
            recalled,
            listed(
                "om_img",
                "1700000000200",
                person("ou_u2"),
                "image",
                &json!({"image_key": "img_1"}),
                json!([]),
            ),
            listed("om_post", "1700000000100", person("ou_u3"), "post", &rich, json!([])),
            listed_text(
                "om_asked",
                "1700000000050",
                person("ou_u1"),
                "@_user_1 done?",
                listed_bot_mention(),
            ),
            listed_text("om_alert", "1700000000010", app("cli_other"), "disk full", json!([])),
        ])],
    );
    let read = channel(&fake).chat_history(&group_mention("om_m"), 50).await.unwrap();
    // What came after the mention within its second, the mention, this app's own answer, the
    // recalled message and the image are left out; another app's message is never for the bot.
    assert_eq!(
        read,
        [
            earlier("om_alert", "cli_other", "disk full", Addressee::Others),
            earlier("om_asked", "ou_u1", "@zx done?", Addressee::Bot),
            earlier("om_post", "ou_u3", "deploy failed", Addressee::Nobody),
        ]
    );
    assert_eq!(fake.calls("get")[0].path, "/open-apis/im/v1/messages/om_m");
    let list = &fake.calls("list")[0];
    assert_eq!(
        list.path,
        "/open-apis/im/v1/messages?sort_type=ByCreateTimeDesc&page_size=50&container_id_type=chat&container_id=oc_1&end_time=1700000001"
    );
    assert_eq!((list.authorization.as_str(), &list.body), ("Bearer t-1", &Value::Null));
    assert_eq!(fake.calls("bot").len(), 1);
}

#[tokio::test]
async fn a_group_read_before_a_mention_gives_its_latest_messages_up_to_the_limit() {
    let fake = FakeFeishu::start(120).await;
    fake.script(
        "list",
        vec![messages(vec![
            listed_text("om_3", "0", person("ou_u1"), "third", json!([])),
            listed_text("om_2", "0", person("ou_u1"), "second", json!([])),
            listed_text("om_1", "0", person("ou_u1"), "first", json!([])),
        ])],
    );
    // The fake gives the mention as sent at 0.
    let read = channel(&fake).chat_history(&group_mention("om_0"), 2).await.unwrap();
    let ids: Vec<&str> = read.iter().map(|said| said.id.0.as_str()).collect();
    assert_eq!(ids, ["om_2", "om_3"]);
}

#[tokio::test]
async fn a_topic_is_read_from_its_root_oldest_first() {
    let fake = FakeFeishu::start(120).await;
    let mut root = listed_text("om_r", "1700000000000", person("ou_u2"), "the deploy failed", json!([]));
    root["thread_id"] = json!("omt_1");
    fake.script("get", vec![messages(vec![root.clone()])]);
    fake.script(
        "list",
        vec![messages(vec![
            listed_text(
                "om_3",
                "1700000000300",
                person("ou_u1"),
                "@_user_1 why?",
                listed_bot_mention(),
            ),
            listed_text("om_2", "1700000000200", app("cli_test"), "an earlier answer", json!([])),
            listed_text("om_1", "1700000000100", person("ou_u3"), "me too", json!([])),
            // Read by itself, the root is not read twice.
            root,
        ])],
    );
    let read = channel(&fake)
        .thread_history(&at("oc_1", Some("om_r")), 50)
        .await
        .unwrap();
    assert_eq!(
        read,
        [
            earlier("om_r", "ou_u2", "the deploy failed", Addressee::Nobody),
            earlier("om_1", "ou_u3", "me too", Addressee::Nobody),
            earlier("om_3", "ou_u1", "@zx why?", Addressee::Bot),
        ]
    );
    assert_eq!(fake.calls("get")[0].path, "/open-apis/im/v1/messages/om_r");
    assert_eq!(
        fake.calls("list")[0].path,
        "/open-apis/im/v1/messages?sort_type=ByCreateTimeDesc&page_size=50&container_id_type=thread&container_id=omt_1"
    );
}

#[tokio::test]
async fn a_reply_outside_topics_reads_only_its_root() {
    let fake = FakeFeishu::start(120).await;
    let root = listed_text("om_r", "1700000000000", person("ou_u2"), "the deploy failed", json!([]));
    fake.script("get", vec![messages(vec![root])]);
    let read = channel(&fake)
        .thread_history(&at("oc_1", Some("om_r")), 50)
        .await
        .unwrap();
    assert_eq!(read, [earlier("om_r", "ou_u2", "the deploy failed", Addressee::Nobody)]);
    assert!(fake.calls("list").is_empty());
}

#[tokio::test]
async fn a_group_feishu_refuses_to_list_is_an_error_with_its_code() {
    let fake = FakeFeishu::start(120).await;
    // As without the permission to read every group message.
    fake.script("list", vec![Answer::Code(230_027)]);
    let refused = channel(&fake)
        .chat_history(&group_mention("om_0"), 50)
        .await
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "feishu /open-apis/im/v1/messages: HTTP 200 OK, code 230027: refused"
    );
}

#[tokio::test]
async fn without_the_bots_open_id_no_message_read_is_taken_as_meant_for_it() {
    let fake = FakeFeishu::start(120).await;
    fake.script("bot", vec![Answer::Code(99_991_663)]);
    fake.script(
        "list",
        vec![messages(vec![listed_text(
            "om_1",
            "0",
            person("ou_u1"),
            "@_user_1 done?",
            listed_bot_mention(),
        )])],
    );
    let (channel, mut reports) = reporting_channel(&fake);
    let read = channel.chat_history(&group_mention("om_0"), 50).await.unwrap();
    assert_eq!(read, [earlier("om_1", "ou_u1", "@zx done?", Addressee::Others)]);
    let report = reports.try_recv().unwrap();
    assert!(
        report.contains("/open-apis/bot/v3/info") && report.contains("99991663"),
        "{report}"
    );
}

/// A Markdown file named in Chinese, as people name theirs.
fn report() -> Attachment {
    Attachment {
        name: "报告.md".into(),
        bytes: b"# Report".to_vec(),
    }
}

#[tokio::test]
async fn a_file_is_uploaded_then_posted_as_a_file_message_in_its_topic() {
    let fake = FakeFeishu::start(120).await;
    channel(&fake)
        .send_file(&at("oc_1", Some("om_root")), &report())
        .await
        .unwrap();
    let upload = &fake.calls("upload")[0];
    let content_type = upload.content_type.clone().unwrap();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );
    let form = String::from_utf8(upload.bytes.clone()).unwrap();
    for part in [
        "name=\"file_type\"\r\n\r\nstream\r\n",
        "name=\"file_name\"\r\n\r\n报告.md\r\n",
        "name=\"file\"; filename=\"报告.md\"",
        "\r\n\r\n# Report\r\n",
    ] {
        assert!(form.contains(part), "{part:?} in {form}");
    }
    let reply = &fake.calls("reply")[0];
    assert_eq!(reply.path, "/open-apis/im/v1/messages/om_root/reply");
    assert_eq!(
        reply.body,
        json!({"msg_type": "file", "content": "{\"file_key\":\"file_v2_1\"}", "reply_in_thread": true})
    );
}

#[tokio::test]
async fn a_file_feishu_refuses_to_take_fails_with_its_code_and_posts_nothing() {
    let fake = FakeFeishu::start(120).await;
    fake.script("upload", vec![Answer::Code(230_027)]);
    let error = channel(&fake)
        .send_file(&at("oc_1", None), &report())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("code 230027"), "{error}");
    assert!(fake.calls("create").is_empty() && fake.calls("reply").is_empty());
}
