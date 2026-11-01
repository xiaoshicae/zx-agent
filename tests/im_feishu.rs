//! The Feishu channel against a fake open platform: the long connection (pings, pongs,
//! acknowledgements, fragments, redeliveries, reconnection), messages and card clicks,
//! tokens, replies in topics, edits, cards, rate limits and the stop.
mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use common::fake_feishu::{Answer, FakeFeishu, TestFrame};
use zx_agent::agent::ApprovalResult;
use zx_agent::im::feishu::{FeishuChannel, FeishuConfig};
use zx_agent::im::{
    ApprovalId, Choice, ConversationId, Destination, ImChannel, ImError, Inbound, MessageId, MessageRef, Outbound,
    ThreadId, UserId,
};

/// Only reached when an expected event never comes.
const WAIT: Duration = Duration::from_secs(10);

fn channel(fake: &FakeFeishu) -> Arc<FeishuChannel> {
    let mut config = FeishuConfig::new("cli_test".into(), "secret-test".into());
    config.api_base = fake.api_base.clone();
    config.token_url = fake.token_url.clone();
    config.post_interval = Duration::ZERO;
    Arc::new(FeishuChannel::new(config).unwrap())
}

struct Running {
    inbound: mpsc::Receiver<Inbound>,
    cancel: CancellationToken,
    task: JoinHandle<Result<(), ImError>>,
}

fn run(channel: Arc<FeishuChannel>) -> Running {
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

fn payload(event_type: &str, event: Value) -> Vec<u8> {
    json!({"schema": "2.0", "header": {"event_id": "ev1", "event_type": event_type}, "event": event})
        .to_string()
        .into_bytes()
}

fn text_message(id: &str, chat: &str, chat_type: &str, root: Option<&str>, text: &str, mentions: Value) -> Value {
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

fn received(event: Value) -> Vec<u8> {
    payload("im.message.receive_v1", event)
}

fn at(chat: &str, thread: Option<&str>) -> Destination {
    Destination {
        conversation: ConversationId(chat.into()),
        thread: thread.map(|root| ThreadId(root.into())),
    }
}

fn message(id: &str, at: Destination, text: &str) -> Inbound {
    Inbound::Message {
        id: MessageId(id.into()),
        at,
        sender: UserId("ou_u1".into()),
        text: text.into(),
    }
}

fn content(call_body: &Value) -> Value {
    serde_json::from_str(call_body["content"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn the_channel_connects_with_the_app_credentials_and_pings_at_once() {
    let mut fake = FakeFeishu::start(120).await;
    let running = run(channel(&fake));
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
    let mut running = run(channel(&fake));
    let mut socket = fake.socket().await;
    let event = TestFrame::event(
        "f1",
        1,
        0,
        &received(text_message(
            "om_1",
            "oc_1",
            "group",
            None,
            "@_user_1 pods",
            bot_mention(),
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
    let mut running = run(channel(&fake));
    let mut socket = fake.socket().await;
    let event = text_message("om_2", "oc_1", "group", Some("om_root"), "@_user_1 more", bot_mention());
    socket.send(&TestFrame::event("f1", 1, 0, &received(event))).await;
    assert_eq!(
        running.next().await,
        message("om_2", at("oc_1", Some("om_root")), "more")
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn only_text_from_people_addressed_to_the_bot_is_passed_on() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run(channel(&fake));
    let mut socket = fake.socket().await;
    let unmentioned = text_message("om_1", "oc_1", "group", None, "chatter", json!([]));
    let mut from_bot = text_message("om_2", "oc_d", "p2p", None, "echo", json!([]));
    from_bot["sender"]["sender_type"] = json!("bot");
    let mut image = text_message("om_3", "oc_d", "p2p", None, "", json!([]));
    image["message"]["message_type"] = json!("image");
    let wanted = text_message("om_4", "oc_d", "p2p", None, "hi", json!([]));
    for (n, event) in [unmentioned, from_bot, image, wanted].into_iter().enumerate() {
        socket
            .send(&TestFrame::event(&format!("f{n}"), 1, 0, &received(event)))
            .await;
        socket.receive_data().await;
    }
    assert_eq!(running.next().await, message("om_4", at("oc_d", None), "hi"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_redelivered_message_is_passed_on_once() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run(channel(&fake));
    let mut socket = fake.socket().await;
    let once = text_message("om_1", "oc_d", "p2p", None, "once", json!([]));
    let next = text_message("om_2", "oc_d", "p2p", None, "next", json!([]));
    for (frame_id, event) in [("f1", once.clone()), ("f2", once), ("f3", next)] {
        socket.send(&TestFrame::event(frame_id, 1, 0, &received(event))).await;
        assert_eq!(socket.receive_data().await.header("message_id"), Some(frame_id));
    }
    assert_eq!(running.next().await, message("om_1", at("oc_d", None), "once"));
    assert_eq!(running.next().await, message("om_2", at("oc_d", None), "next"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_fragmented_event_is_joined_and_acknowledged_once() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run(channel(&fake));
    let mut socket = fake.socket().await;
    let whole = received(text_message("om_1", "oc_d", "p2p", None, "joined", json!([])));
    let (first, second) = whole.split_at(whole.len() / 2);
    socket.send(&TestFrame::event("f9", 2, 1, second)).await;
    socket.send(&TestFrame::event("f9", 2, 0, first)).await;
    let ack = socket.receive_data().await;
    assert_eq!((ack.header("message_id"), ack.header("seq")), (Some("f9"), Some("0")));
    assert_eq!(running.next().await, message("om_1", at("oc_d", None), "joined"));
    // The next data frame is the acknowledgement of a later event: the first fragment had none.
    let later = text_message("om_2", "oc_d", "p2p", None, "later", json!([]));
    socket.send(&TestFrame::event("f10", 1, 0, &received(later))).await;
    assert_eq!(socket.receive_data().await.header("message_id"), Some("f10"));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_card_click_becomes_a_decision_in_its_thread() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run(channel(&fake));
    let mut socket = fake.socket().await;
    let click = json!({
        "operator": {"open_id": "ou_u2"},
        "action": {"tag": "button", "value": {"zx": "deny", "approval": "A1", "chat": "oc_1", "thread": "om_root"}},
        "context": {"open_message_id": "om_card", "open_chat_id": "oc_1"},
    });
    socket
        .send(&TestFrame::event("f1", 1, 0, &payload("card.action.trigger", click)))
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
    let running = run(channel(&fake));
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
    let running = run(channel(&fake));
    match running.task.await.unwrap() {
        Err(ImError::Config(error)) => assert!(error.contains("19001"), "{error}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_dropped_connection_is_opened_again() {
    let mut fake = FakeFeishu::start(120).await;
    let mut running = run(channel(&fake));
    fake.socket().await.close().await;
    let mut second = fake.socket().await;
    let event = text_message("om_1", "oc_d", "p2p", None, "back", json!([]));
    second.send(&TestFrame::event("f1", 1, 0, &received(event))).await;
    assert_eq!(running.next().await, message("om_1", at("oc_d", None), "back"));
    assert_eq!(fake.calls("endpoint").len(), 2);
    running.stop().await.unwrap();
}

#[tokio::test]
async fn text_in_a_thread_is_a_reply_in_its_topic() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let to = at("oc_1", Some("om_root"));
    let posted = channel.send(&to, &Outbound::Text("hi".into())).await.unwrap();
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
        channel
            .send(&at("oc_1", None), &Outbound::Text(text.into()))
            .await
            .unwrap();
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
    let mut running = run(Arc::clone(&channel));
    let mut socket = fake.socket().await;
    let event = text_message("om_d1", "oc_d", "p2p", None, "hi", json!([]));
    socket.send(&TestFrame::event("f1", 1, 0, &received(event))).await;
    running.next().await;
    channel
        .send(&at("oc_d", Some("om_d1")), &Outbound::Text("answer".into()))
        .await
        .unwrap();
    assert_eq!(fake.calls("reply")[0].body["reply_in_thread"], json!(false));
    running.stop().await.unwrap();
}

#[tokio::test]
async fn a_refused_secret_is_a_configuration_error() {
    let fake = FakeFeishu::start(120).await;
    fake.script("token", vec![Answer::Status(400, 20002)]);
    let result = channel(&fake)
        .send(&at("oc_1", None), &Outbound::Text("hi".into()))
        .await;
    assert!(matches!(result, Err(ImError::Config(error)) if error.contains("20002")));
}

#[tokio::test]
async fn a_status_update_edits_the_message() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let posted = channel
        .send(&at("oc_1", Some("om_root")), &Outbound::Text("working".into()))
        .await
        .unwrap();
    channel.update(&posted, &Outbound::Text("done".into())).await.unwrap();
    let edit = &fake.calls("edit")[0];
    assert_eq!(edit.path, "/open-apis/im/v1/messages/om_out1");
    assert_eq!(edit.body, json!({"msg_type": "text", "content": "{\"text\":\"done\"}"}));
}

#[tokio::test]
async fn a_refused_edit_posts_the_text_instead() {
    let fake = FakeFeishu::start(120).await;
    fake.script("edit", vec![Answer::Code(230075)]);
    let channel = channel(&fake);
    let posted = channel
        .send(&at("oc_1", Some("om_root")), &Outbound::Text("working".into()))
        .await
        .unwrap();
    channel.update(&posted, &Outbound::Text("done".into())).await.unwrap();
    let replies = fake.calls("reply");
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1].path, "/open-apis/im/v1/messages/om_root/reply");
    assert_eq!(content(&replies[1].body), json!({"text": "done"}));
}

#[tokio::test]
async fn a_card_carries_its_click_value_and_closing_it_keeps_the_request() {
    let fake = FakeFeishu::start(120).await;
    let channel = channel(&fake);
    let card = Outbound::ApprovalRequest {
        id: ApprovalId("A1".into()),
        tool: "bash".into(),
        summary: "kubectl delete pod x".into(),
        reason: "needs approval".into(),
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

    let closed = Outbound::ApprovalResolved {
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
        json!([request, {"tag": "markdown", "content": "已批准（ou_u2）"}])
    );
}

#[tokio::test]
async fn a_rate_limited_call_waits_and_is_retried() {
    let fake = FakeFeishu::start(120).await;
    fake.script("create", vec![Answer::RateLimited { reset_secs: 0 }]);
    channel(&fake)
        .send(&at("oc_1", None), &Outbound::Text("hi".into()))
        .await
        .unwrap();
    assert_eq!(fake.calls("create").len(), 2);
}

#[tokio::test]
async fn stopping_closes_the_connection() {
    let mut fake = FakeFeishu::start(120).await;
    let running = run(channel(&fake));
    let mut socket = fake.socket().await;
    running.stop().await.unwrap();
    assert!(socket.closed_by_client().await);
}
