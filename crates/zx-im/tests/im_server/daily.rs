//! What the work of a day may use: once it is used up, in all chats or in one, a message and
//! `/compact` start no work there and are told so.

use super::*;

/// What one answer of the scripted model uses: 10 input and 5 output tokens.
const ONE_ANSWER: u64 = 15;

/// A day's limit of `tokens` tokens.
fn tokens(tokens: u64) -> DailyLimit {
    DailyLimit {
        tokens: Some(tokens),
        cost: None,
    }
}

#[tokio::test]
async fn once_the_day_s_usage_is_used_up_a_message_and_compact_start_no_work() {
    let mut setup = Setup {
        steps: vec![FauxStep::text("first")],
        ..Setup::default()
    };
    setup.config.daily = tokens(ONE_ANSWER);
    let mut h = start(setup);
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("first")).await;

    // The first answer used up the day, for a new conversation and for the first one alike.
    h.say("m2", None, "again").await;
    h.posted(&thread("m2"), text(&h.replies.daily_limit)).await;
    h.say("m3", Some("m1"), "/compact").await;
    h.posted(&thread("m1"), text(&h.replies.daily_limit)).await;
    assert_eq!(h.requests().len(), 1);
    // Nothing waits: the refused messages are handled.
    let backlog = Arc::clone(&h.backlog);
    h.stop().await.unwrap();
    assert!(pending(&backlog).await.is_empty());
}

#[tokio::test]
async fn a_chat_that_used_up_its_day_leaves_the_other_chats_their_work() {
    let mut setup = Setup {
        steps: vec![FauxStep::text("direct"), FauxStep::text("in the group")],
        ..Setup::default()
    };
    setup.config.chats = vec![ChatConfig {
        platform: PLATFORM.into(),
        chat: ConversationId(DIRECT.into()),
        agent: None,
        approvers: None,
        daily: tokens(ONE_ANSWER),
    }];
    let mut h = start(setup);
    h.say_directly("d1", "hello").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("direct")).await;
    h.say_directly("d2", "again").await;
    h.posted(&direct(), text(&h.replies.daily_limit)).await;
    // The group chat has no limit of its own, and the service none.
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("in the group")).await;
    h.stop().await.unwrap();
}
