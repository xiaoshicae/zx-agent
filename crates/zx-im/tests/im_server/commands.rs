//! The commands of a conversation, `/new`, `/cancel` and `/compact`, with nothing running and
//! during a run or a compaction.

use super::*;

#[tokio::test]
async fn cancel_in_the_thread_stops_the_run_and_closes_its_card() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("unused")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.say("m2", Some("m1"), " /cancel ").await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: None,
            result: ApprovalResult::Cancelled,
        },
    )
    .await;
    h.updated(&status, text(&h.replies.cancelled)).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.requests().len(), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn compact_summarizes_the_earlier_talk_as_work_of_its_own() {
    let mut h = start(Setup {
        steps: vec![
            FauxStep::text("first"),
            FauxStep::text("second"),
            FauxStep::compute(|request| FauxStep::text(zx_testing::summary_of_the_source(request))),
            FauxStep::text("third"),
        ],
        // Everything before the latest prompt is old enough to summarize.
        keep_recent_bytes: Some(1),
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    for (id, prompt, answer) in [("d1", "one", "first"), ("d2", "two", "second")] {
        h.say_directly(id, prompt).await;
        let status = h.posted(&direct(), working.clone()).await;
        h.updated(&status, text(answer)).await;
    }

    h.say_directly("d3", " /compact ").await;

    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text(&h.replies.compacted)).await;
    h.say_directly("d4", "three").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("third")).await;
    // The command went to no model as a prompt, and the next answer read the summary in
    // place of the first turn.
    let requests = h.requests();
    assert_eq!(requests.len(), 4);
    let read = requests[3].join("\n");
    assert!(!read.contains("user:one") && !read.contains("/compact"), "{read}");
    assert!(read.contains("user:two") && read.contains("Old observation"), "{read}");
    h.stop().await.unwrap();
}

#[tokio::test]
async fn compact_says_when_there_is_nothing_to_summarize() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("first")],
        ..Setup::default()
    });
    let nothing: Post = text(&h.replies.nothing_to_compact);
    // A conversation without a session gets none for the command.
    h.say("m1", None, "/compact").await;
    h.posted(&thread("m1"), nothing).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 0);
    let working: Post = text(&h.replies.working);
    h.say_directly("d1", "one").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("first")).await;

    h.say_directly("d2", "/compact").await;

    // One turn is the latest talk, which a summary leaves as it is: no request is made.
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text(&h.replies.nothing_to_compact)).await;
    assert_eq!(h.requests().len(), 1);
    h.stop().await.unwrap();
}

/// A direct chat of two turns whose `/compact` waits at `gate` for its summary: the server,
/// and the status of the compaction under way.
async fn compacting(gate: &Arc<Gate>, more: Vec<FauxStep>) -> (Harness, MessageRef) {
    let mut steps = vec![FauxStep::text("first"), FauxStep::text("second")];
    steps.extend(more);
    let mut h = start(Setup {
        steps,
        keep_recent_bytes: Some(1),
        summary_gate: Some(gate.clone()),
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    for (id, prompt, answer) in [("d1", "one", "first"), ("d2", "two", "second")] {
        h.say_directly(id, prompt).await;
        let status = h.posted(&direct(), working.clone()).await;
        h.updated(&status, text(answer)).await;
    }
    h.say_directly("d3", "/compact").await;
    let status = h.posted(&direct(), working).await;
    gate.entered.notified().await;
    (h, status)
}

#[tokio::test]
async fn cancel_stops_a_compaction_like_a_run() {
    let gate = Gate::new();
    let (mut h, status) = compacting(&gate, Vec::new()).await;

    h.say_directly("d4", "/cancel").await;

    h.updated(&status, text(&h.replies.cancelled)).await;
    // Nothing was summarized: the next answer reads the talk as it was.
    assert_eq!(h.requests().len(), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_sent_during_a_compaction_is_answered_after_it() {
    let gate = Gate::new();
    let (mut h, status) = compacting(&gate, vec![FauxStep::text("third")]).await;

    h.say_directly("d4", "three").await;

    h.acknowledged(&direct(), "d4").await;
    gate.release.cancel();
    h.updated(&status, text(&h.replies.compacted)).await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("third")).await;
    let read = h.requests()[2].join("\n");
    assert!(read.contains("user:three") && !read.contains("user:one"), "{read}");
    h.stop().await.unwrap();
}

#[tokio::test]
async fn compact_during_a_run_says_that_the_conversation_is_busy() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("unused")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (_, _card) = h.card(&thread("m1")).await;

    h.say("m2", Some("m1"), "/compact").await;

    h.posted(&thread("m1"), text(&h.replies.busy)).await;
    // The run goes on as it was: it still ends by `/cancel`.
    h.say("m3", Some("m1"), "/cancel").await;
    loop {
        match h.next().await {
            Sent::Update { posted, message } if posted == status => {
                assert_eq!(message, text(&h.replies.cancelled));
                break;
            }
            Sent::Update { .. } => {}
            other => panic!("expected the run to be cancelled, got {other:?}"),
        }
    }
    h.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_with_nothing_running_says_so() {
    let mut h = start(Setup::default());
    h.say("m1", None, "/cancel").await;
    h.posted(&thread("m1"), text(&h.replies.nothing_to_cancel)).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn new_starts_a_new_session_and_keeps_the_earlier_one() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say_directly("d2", " /new ").await;
    h.posted(&direct(), text(&h.replies.new_session)).await;
    h.say_directly("d3", "second").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:second"]);
    assert_eq!(h.sessions.of(&direct_key()).await.unwrap().len(), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_conversation_released_after_new_goes_on_in_the_new_session() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    let released = ServerEvent::Released { key: direct_key() };
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    // The idle time passes before `/new`, so the new session is later than the first one's
    // last record by more than the clock's resolution.
    assert_eq!(h.event().await, released);
    h.say_directly("d2", "/new").await;
    h.posted(&direct(), text(&h.replies.new_session)).await;
    // Released again, the conversation remembers nothing: only the new session, which it
    // held until now, says that `/new` was asked for.
    assert_eq!(h.event().await, released);
    h.say_directly("d3", "second").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:second"]);
    assert_eq!(h.sessions.of(&direct_key()).await.unwrap().len(), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn new_reports_a_session_it_cannot_create() {
    let mut h = start(Setup {
        fail_open: Some("disk full".into()),
        ..Setup::default()
    });
    h.say_directly("d1", "/new").await;
    let refused = format!("{}cannot set up the session: disk full", h.replies.cannot_open);
    h.posted(&direct(), text(&refused)).await;
    match h.event().await {
        ServerEvent::Failed { key: Some(key), error } => {
            assert_eq!(key, direct_key());
            assert!(error.contains("disk full"), "{error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(h.sessions.of(&direct_key()).await.unwrap().is_empty());
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_stop_does_not_wait_for_the_session_new_is_making() {
    let gate = Gate::new();
    let h = start(Setup {
        setup_gate: Some(gate.clone()),
        ..Setup::default()
    });
    let backlog = h.backlog.clone();
    // The new session's setup waits at the gate, which stays shut.
    h.say_as(ASKER, "m1", "slow-thread", Addressee::Bot, "/new").await;
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    tokio::time::timeout(WAIT, h.stop()).await.unwrap().unwrap();
    // Nothing was made: the command waits for the next start.
    assert_eq!(pending(&backlog).await, ["m1"]);
}

#[tokio::test]
async fn new_during_a_run_cancels_it_and_then_starts_over() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("fresh")],
        ..Setup::default()
    });
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    // The card shows that the run is waiting, its request answered.
    let (id, card) = h.card(&direct()).await;
    h.say_directly("d2", "/new").await;
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.updated(&status, text(&h.replies.cancelled)).await;
    h.posted(&direct(), text(&h.replies.new_session)).await;
    h.say_directly("d3", "again").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("fresh")).await;
    assert_eq!(h.requests()[1], ["user:again"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_that_arrives_while_new_cancels_a_run_is_answered_in_the_new_session() {
    let release = CancellationToken::new();
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("fresh")],
        updates: Updates::Held(release.clone()),
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    let (id, card) = h.card(&direct()).await;
    h.say_directly("d2", "/new").await;
    // The cancelled run closes its card, and the platform holds that update back: the
    // conversation is busy with it, its run not yet seen to have ended.
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.say_directly("d3", "again").await;
    // The router passes messages on in the order it got them: once it has refused this one,
    // the one before it waits in the conversation's inbox.
    h.say_in("C2", "m1", None, "hello").await;
    let elsewhere = Destination {
        conversation: ConversationId("C2".into()),
        thread: Some(ThreadId("m1".into())),
    };
    h.posted(&elsewhere, text(&replies.unauthorized)).await;
    release.cancel();
    h.acknowledged(&direct(), "d3").await;
    h.updated(&status, text(&replies.cancelled)).await;
    h.posted(&direct(), text(&replies.new_session)).await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("fresh")).await;
    assert_eq!(h.requests()[1], ["user:again"]);
    assert_eq!(h.sessions.of(&direct_key()).await.unwrap().len(), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn new_starts_its_session_also_when_the_one_it_leaves_was_closed_meanwhile() {
    let release = CancellationToken::new();
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("fresh")],
        updates: Updates::Held(release.clone()),
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    let (id, card) = h.card(&direct()).await;
    h.say_directly("d2", "/new").await;
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    // Closed under the conversation while its cancelled run ends, as a session closes itself
    // when it can no longer write its records.
    let left = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&left));
    release.cancel();
    h.updated(&status, text(&replies.cancelled)).await;
    h.posted(&direct(), text(&replies.new_session)).await;
    h.say_directly("d3", "again").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("fresh")).await;
    assert_eq!(h.requests()[1], ["user:again"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_that_arrives_while_cancel_stops_a_run_is_answered_after_it() {
    let release = CancellationToken::new();
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("fresh")],
        updates: Updates::Held(release.clone()),
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    let (id, card) = h.card(&direct()).await;
    h.say_directly("d2", "/cancel").await;
    // The cancelled run closes its card, and the platform holds that update back: the
    // conversation is busy with it, its run not yet seen to have ended.
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.say_directly("d3", "again").await;
    // The router passes messages on in the order it got them: once it has refused this one,
    // the one before it waits in the conversation's inbox.
    h.say_in("C2", "m1", None, "hello").await;
    let elsewhere = Destination {
        conversation: ConversationId("C2".into()),
        thread: Some(ThreadId("m1".into())),
    };
    h.posted(&elsewhere, text(&replies.unauthorized)).await;
    release.cancel();
    h.acknowledged(&direct(), "d3").await;
    h.updated(&status, text(&replies.cancelled)).await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("fresh")).await;
    let second = &h.requests()[1];
    assert_eq!(second.last().map(String::as_str), Some("user:again"), "{second:?}");
    h.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_drops_what_was_steered_into_the_run_it_cancels() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let working: Post = text(&replies.working);
    h.say_directly("d1", "run").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.say_directly("d2", "also this").await;
    h.acknowledged(&direct(), "d2").await;
    h.say_directly("d3", "/cancel").await;
    h.updated(&status, text(&replies.cancelled)).await;

    let (sent, _) = h.stop_and_take().await;

    assert!(
        !sent
            .iter()
            .any(|sent| matches!(sent, Sent::Post { message, .. } if *message == working)),
        "no run for the steering the cancelled run did not take in: {sent:?}"
    );
}
