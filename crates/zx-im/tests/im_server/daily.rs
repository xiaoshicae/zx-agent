//! What the work of a day may use: once it is used up, in all chats or in one, a message and
//! `/compact` start no work there and are told so; with a usage store, the day counts on from
//! what it kept.

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

/// What `tokens` of work in the group chat count.
fn in_the_group(tokens: u64) -> ChatUsage {
    ChatUsage {
        platform: PLATFORM.into(),
        chat: ConversationId(CHAT.into()),
        tokens,
        cost: 0.0,
    }
}

/// A usage store that holds `held` for whatever day it is asked about, and records the days
/// asked about and what is added.
#[derive(Default)]
struct KeptUsage {
    held: Vec<ChatUsage>,
    asked: Mutex<Vec<u64>>,
    added: Mutex<Vec<(u64, ChatUsage)>>,
}

#[async_trait]
impl UsageStore for KeptUsage {
    async fn day(&self, day: u64) -> Result<Vec<ChatUsage>, String> {
        self.asked.lock().unwrap().push(day);
        Ok(self.held.clone())
    }

    async fn add(&self, day: u64, used: &ChatUsage) -> Result<(), String> {
        self.added.lock().unwrap().push((day, used.clone()));
        Ok(())
    }
}

/// A usage store whose database is gone.
struct UnreachableUsage;

#[async_trait]
impl UsageStore for UnreachableUsage {
    async fn day(&self, _day: u64) -> Result<Vec<ChatUsage>, String> {
        Err("the database is gone".into())
    }

    async fn add(&self, _day: u64, _used: &ChatUsage) -> Result<(), String> {
        Err("the database is gone".into())
    }
}

#[tokio::test]
async fn the_day_counts_on_from_what_the_usage_store_kept_and_the_work_is_added_to_it() {
    let store = Arc::new(KeptUsage {
        held: vec![in_the_group(ONE_ANSWER)],
        ..KeptUsage::default()
    });
    let mut setup = Setup {
        steps: vec![FauxStep::text("first")],
        ..Setup::default()
    };
    setup.config.daily = tokens(2 * ONE_ANSWER);
    setup.config.usage = Some(store.clone());
    let mut h = start(setup);
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("first")).await;

    // An earlier process used half of the day: with this answer, it is used up.
    h.say("m2", None, "again").await;
    h.posted(&thread("m2"), text(&h.replies.daily_limit)).await;
    h.stop().await.unwrap();
    let asked = store.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "read once, as the server starts");
    assert_eq!(*store.added.lock().unwrap(), [(asked[0], in_the_group(ONE_ANSWER))]);
}

#[tokio::test]
async fn a_usage_store_that_cannot_say_what_the_day_used_stops_the_start() {
    let mut setup = Setup::default();
    setup.config.usage = Some(Arc::new(UnreachableUsage));
    let h = start(setup);
    let Err(ImError::Config(error)) = h.stop().await else {
        panic!("the start fails as a configuration problem");
    };
    assert_eq!(error, "cannot read the day's usage: the database is gone");
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
