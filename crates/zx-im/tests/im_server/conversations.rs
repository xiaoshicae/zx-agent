//! Which conversation a message belongs to and when it is answered: threads, the direct chat,
//! following a conversation without a mention, talk between people, and what was said in a
//! thread before the bot's first session there.

use super::*;

#[tokio::test]
async fn a_message_in_the_chat_is_answered_in_a_thread_under_it() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("answer")).await;
    assert_eq!(h.requests(), [["user:U-asker: hello"]]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_reply_in_the_thread_continues_its_session() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say("m2", Some("m1"), "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        ["user:U-asker: first", "assistant:one", "user:U-asker: second"]
    );
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn each_thread_has_a_session_of_its_own() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say("m2", None, "unrelated").await;
    let status = h.posted(&thread("m2"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:U-asker: unrelated"]);
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_chat_that_is_not_served_gets_a_refusal_and_nothing_else() {
    let mut h = start(Setup::default());
    h.say_in("C2", "m1", None, "hello").await;
    let refused = Destination {
        conversation: ConversationId("C2".into()),
        thread: Some(ThreadId("m1".into())),
    };
    h.posted(&refused, text(&h.replies.unauthorized)).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 0);
    assert!(h.requests().is_empty());
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_operator_hears_where_a_refused_message_came_from_and_who_wrote_it() {
    let mut h = start(Setup::default());
    let refused = |root: &str| Destination {
        conversation: ConversationId("C2".into()),
        thread: Some(ThreadId(root.into())),
    };
    h.say_in("C2", "m1", None, "hello").await;
    h.posted(&refused("m1"), text(&h.replies.unauthorized)).await;
    // Talk between people there is not refused, so the operator hears nothing of it; the
    // router takes messages in order, so the refusal after it shows that it was passed over.
    let talk = group_message("C2", "m2", Some("m1"), "U-other", Addressee::Nobody, "lunch?");
    h.inject.send(talk).await.unwrap();
    h.say_in("C2", "m3", None, "again").await;
    h.posted(&refused("m3"), text(&h.replies.unauthorized)).await;
    let (_, events) = h.stop_and_take().await;
    let unauthorized = |root: &str| ServerEvent::Unauthorized {
        key: SessionKey {
            platform: PLATFORM.into(),
            conversation: ConversationId("C2".into()),
            thread: Some(ThreadId(root.into())),
        },
        user: UserId(ASKER.into()),
    };
    assert_eq!(events, [unauthorized("m1"), unauthorized("m3")]);
}

#[tokio::test]
async fn a_direct_chat_is_one_conversation_answered_in_the_chat() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say_directly("d2", "second").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    // One person: no names before their words.
    assert_eq!(h.requests()[1], ["user:first", "assistant:one", "user:second"]);
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_session_records_the_sender_of_each_message_it_answers_as_its_actor() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say_as(OTHER, "m2", "m1", Addressee::Bot, "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;

    let id = h.sessions.of(&key("m1")).await.unwrap().remove(0);
    let stored = Transcript::snapshot(h.sessions.storage.read(&id).await.unwrap()).unwrap();
    let actors: Vec<Option<&str>> = stored
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message(entry) if matches!(entry.message, Message::User { .. }) => Some(entry.actor.as_deref()),
            _ => None,
        })
        .collect();
    assert_eq!(actors, [Some(ASKER), Some(OTHER)]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_in_a_thread_the_bot_never_joined_reaches_neither_a_worker_nor_the_storage() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("hi")],
        ..Setup::default()
    });
    for id in ["m2", "m3", "m4"] {
        h.say_as(OTHER, id, "m1", Addressee::Nobody, "talk among people").await;
    }
    // A mention in the direct chat, answered once the router has passed the talk by: it
    // routes what arrives in order.
    h.say_directly("d1", "hello").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("hi")).await;

    assert_eq!(h.sessions.indexed.load(Ordering::SeqCst), 1, "read once, at the start");
    assert_eq!(h.sessions.listed.load(Ordering::SeqCst), 0);
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1, "the direct chat's session");
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_conversation_of_an_earlier_process_is_followed_without_a_mention() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("first")],
        ..Setup::default()
    });
    h.say_as(ASKER, "m2", "m1", Addressee::Bot, "first").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("first")).await;
    let storage = Arc::clone(&h.sessions.storage);
    h.stop().await.unwrap();

    // A new server over the same storage, as after a restart.
    let mut h = start(Setup {
        steps: vec![FauxStep::text("second")],
        storage: Some(storage),
        ..Setup::default()
    });
    h.say_as(OTHER, "m3", "m1", Addressee::Nobody, "said while the bot was away")
        .await;
    h.say_as(ASKER, "m4", "m1", Addressee::Bot, "again").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("second")).await;
    let request = &h.requests()[0];
    assert!(
        request
            .iter()
            .any(|message| message.contains("said while the bot was away")),
        "{request:?}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_thread_the_bot_joins_after_talk_it_passed_by_is_followed_from_then_on() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say_as(OTHER, "m2", "m1", Addressee::Nobody, "before the bot").await;
    h.say_as(ASKER, "m3", "m1", Addressee::Bot, "first").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    while h.event().await != (ServerEvent::Released { key: key("m1") }) {}

    // The bot took part now: talk there that does not mention it reaches the conversation,
    // as context its next answer reads, whether the router still has the thread's worker or
    // looks the thread up again.
    h.say_as(OTHER, "m4", "m1", Addressee::Nobody, "the cache was flushed")
        .await;
    h.say_as(ASKER, "m5", "m1", Addressee::Bot, "second").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("two")).await;
    let request = &h.requests()[1];
    assert!(
        request.iter().any(|message| message.contains("the cache was flushed")),
        "{request:?}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_person_just_answered_goes_on_without_a_mention() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say_as(ASKER, "m2", "m1", Addressee::Nobody, "and then?").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        ["user:U-asker: first", "assistant:one", "user:U-asker: and then?"]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_between_people_in_the_thread_is_recorded_but_not_answered() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "why is checkout slow?").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    // The asker turns to someone else, who answers; then the asker speaks without a mention.
    h.say_as(ASKER, "m2", "m1", Addressee::Others, "@Li any idea?").await;
    h.say_as(OTHER, "m3", "m1", Addressee::Nobody, "the cache was flushed")
        .await;
    h.say_as(ASKER, "m4", "m1", Addressee::Nobody, "makes sense").await;
    // Only a mention brings the bot back; what it did not answer is in its context.
    h.say("m5", Some("m1"), "so what now?").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:U-asker: why is checkout slow?",
            "assistant:one",
            "user:U-asker: @Li any idea?",
            "user:Li: the cache was flushed",
            "user:U-asker: makes sense",
            "user:U-asker: so what now?",
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_where_the_bot_never_took_part_is_left_alone() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        ..Setup::default()
    });
    h.say_as(OTHER, "m1", "t1", Addressee::Nobody, "lunch?").await;
    let elsewhere = group_message("C2", "m2", Some("t2"), OTHER, Addressee::Nobody, "lunch?");
    h.inject.send(elsewhere).await.unwrap();
    h.say("m3", None, "hello").await;
    // The first thing the server does is answer the mention: no refusal, no run before it.
    let status = h.posted(&thread("m3"), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

/// A message that `sender` posted before the bot was mentioned, meant for nobody in
/// particular.
fn earlier(id: &str, sender: &str, text: &str) -> Said {
    Said {
        id: MessageId(id.into()),
        sender: UserId(sender.into()),
        text: text.into(),
        addressee: Addressee::Nobody,
    }
}

/// [`earlier`], meant for `addressee`.
fn earlier_to(addressee: Addressee, id: &str, sender: &str, text: &str) -> Said {
    Said {
        addressee,
        ..earlier(id, sender, text)
    }
}

#[tokio::test]
async fn the_first_mention_in_a_thread_reads_what_was_said_there() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        history: vec![earlier("t1", OTHER, "the deploy failed"), earlier("m2", ASKER, "why?")],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    h.say("m2", Some("t1"), "why?").await;
    let status = h.posted(&thread("t1"), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    // The mention itself is the prompt, not part of what came before.
    assert_eq!(h.requests()[0], ["user:Li: the deploy failed", "user:U-asker: why?"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_first_mention_in_a_thread_reads_only_what_was_said_before_it() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        // Read once a run slot was free: by then the thread went on after the mention.
        history: vec![
            earlier("t1", OTHER, "the deploy failed"),
            earlier("m2", ASKER, "why?"),
            earlier("m3", OTHER, "any news?"),
        ],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    h.say("m2", Some("t1"), "why?").await;
    let status = h.posted(&thread("t1"), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.requests()[0], ["user:Li: the deploy failed", "user:U-asker: why?"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_mention_in_the_chat_reads_what_was_said_there_since_the_bot_was_last_mentioned() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        chat: vec![
            earlier("c1", OTHER, "lunch?"),
            earlier_to(Addressee::Bot, "c2", ASKER, "is the deploy done?"),
            earlier("c3", OTHER, "the deploy failed"),
            earlier_to(Addressee::Others, "c4", OTHER, "@Wang can you look?"),
        ],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    h.say("m5", None, "why?").await;
    let status = h.posted(&thread("m5"), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    // The earlier mention, and what its conversation read, are left out.
    assert_eq!(
        h.requests()[0],
        [
            "user:Li: the deploy failed",
            "user:Li: @Wang can you look?",
            "user:U-asker: why?"
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_mention_in_a_chat_the_bot_was_never_mentioned_in_reads_all_that_the_platform_gives() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        chat: vec![
            earlier("c1", OTHER, "lunch?"),
            earlier("c2", OTHER, "the deploy failed"),
        ],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    h.say("m3", None, "why?").await;
    let status = h.posted(&thread("m3"), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    assert_eq!(
        h.requests()[0],
        ["user:Li: lunch?", "user:Li: the deploy failed", "user:U-asker: why?"]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_direct_message_reads_nothing_said_before_it() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        chat: vec![earlier("c1", OTHER, "the deploy failed")],
        ..Setup::default()
    });
    h.say_directly("m2", "why?").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.requests()[0], ["user:why?"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_session_started_by_new_does_not_read_what_was_said_in_the_thread() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        history: vec![earlier("t1", OTHER, "the deploy failed")],
        ..Setup::default()
    });
    h.say("m2", Some("t1"), "/new").await;
    h.posted(&thread("t1"), text(&h.replies.new_session)).await;
    // Released, the conversation opens the session as it would any other of its own.
    assert_eq!(h.event().await, ServerEvent::Released { key: key("t1") });
    h.say("m3", Some("t1"), "why?").await;
    let status = h.posted(&thread("t1"), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.requests()[0], ["user:U-asker: why?"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_in_a_thread_after_its_session_was_released_is_still_recorded() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.event().await, ServerEvent::Released { key: key("m1") });
    h.say_as(OTHER, "m2", "m1", Addressee::Nobody, "fixed it").await;
    h.say("m3", Some("m1"), "confirm?").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:U-asker: first",
            "assistant:one",
            "user:U-other: fixed it",
            "user:U-asker: confirm?"
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_in_a_thread_whose_session_closed_is_recorded_in_the_session_opened_again() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    // Closed under the conversation, as a session closes itself when it can no longer write
    // its records.
    let session = h.sessions.of(&key("m1")).await.unwrap().remove(0);
    assert!(h.agent.close_session(&session));
    h.say_as(OTHER, "m2", "m1", Addressee::Nobody, "fixed it").await;
    h.say("m3", Some("m1"), "confirm?").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:U-asker: first",
            "assistant:one",
            "user:U-other: fixed it",
            "user:U-asker: confirm?"
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_heard_before_a_thread_has_a_session_is_read_with_the_thread_at_the_next_mention() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one")],
        // The thread as the platform gives it at the question: the talk heard is in it.
        history: vec![
            earlier("t1", OTHER, "the deploy failed"),
            earlier("m3", OTHER, "any news?"),
        ],
        names: HashMap::from([(OTHER.to_string(), "Li".to_string())]),
        ..Setup::default()
    });
    // A mention that makes no session.
    h.say("m2", Some("t1"), "/cancel").await;
    h.posted(&thread("t1"), text(&h.replies.nothing_to_cancel)).await;
    h.say_as(OTHER, "m3", "t1", Addressee::Nobody, "any news?").await;
    h.say("m4", Some("t1"), "why?").await;
    let status = h.posted(&thread("t1"), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    assert_eq!(
        h.requests()[0],
        ["user:Li: the deploy failed", "user:Li: any news?", "user:U-asker: why?"]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_first_session_is_made_only_once_its_thread_is_read() {
    let gate = Gate::new();
    let mut h = start(Setup {
        steps: vec![FauxStep::text("hi")],
        history_gate: Some(gate.clone()),
        ..Setup::default()
    });
    h.say_as(ASKER, "m2", "m1", Addressee::Bot, "please").await;
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    assert_eq!(
        h.sessions.opened.load(Ordering::SeqCst),
        0,
        "a session that starts before its thread is read could be left without it"
    );
    gate.release.cancel();
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("hi")).await;
    h.stop().await.unwrap();
}
