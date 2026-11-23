//! What a run waits for and takes in: the run and conversation limits, steering, the bounds on
//! what waits and what is steered, and the record of each run that ends.

use super::*;

#[tokio::test]
async fn each_run_that_ends_is_recorded_with_what_it_used() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("answer")).await;
    let record = h.ran().await;
    // One answer of the scripted model: 10 input and 5 output tokens.
    assert_eq!(
        (
            record.key,
            record.end,
            record.model_requests,
            record.usage.input,
            record.usage.output
        ),
        (key("m1"), RunEnd::Prompt(RunOutcome::Completed), 1, 10, 5)
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_run_limit_makes_other_threads_wait_their_turn() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("a done"), FauxStep::text("b done")],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("a", None, "task a").await;
    let status_a = h.posted(&thread("a"), working.clone()).await;
    // Thread a now waits for an approval while holding the only run slot.
    let (id, card) = h.card(&thread("a")).await;
    h.say("b", None, "task b").await;
    h.posted(&thread("b"), text(&h.replies.queued)).await;
    h.click("a", APPROVER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.posted(&thread("a"), text("a done")).await;
    h.updated(&status_a, text(&h.replies.answer_below)).await;
    let status_b = h.posted(&thread("b"), working).await;
    h.updated(&status_b, text("b done")).await;
    assert_eq!(h.requests()[2], ["user:U-asker: task b"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_during_a_run_is_steered_into_it() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("final")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.say("m2", Some("m1"), "also check the logs").await;
    h.acknowledged(&thread("m1"), "m2").await;
    h.click("m1", APPROVER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.posted(&thread("m1"), text("final")).await;
    h.updated(&status, text(&h.replies.answer_below)).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:U-asker: run echo",
            "assistant:calls[c1]",
            "tool:c1=done",
            "user:U-asker: also check the logs"
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_answer_the_run_goes_on_from_is_posted_before_the_next_one() {
    let gate = Gate::new();
    let mut h = start(Setup {
        steps: vec![FauxStep::text("first answer"), FauxStep::text("second answer")],
        answer_gate: Some(gate.clone()),
        ..Setup::default()
    });
    h.say("m1", None, "write it").await;
    let first = h.posted(&thread("m1"), text(&h.replies.working)).await;
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    // Steered after the model finished its answer: the run goes on to answer this too.
    h.say("m2", Some("m1"), "and one more thing").await;
    h.acknowledged(&thread("m1"), "m2").await;
    gate.release.cancel();
    h.updated(&first, text("first answer")).await;
    let second = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&second, text("second answer")).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:U-asker: write it",
            "assistant:first answer",
            "user:U-asker: and one more thing"
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_slow_session_setup_does_not_block_another_conversations_messages() {
    let gate = Gate::new();
    let mut h = start(Setup {
        steps: vec![FauxStep::text("hi")],
        setup_gate: Some(gate.clone()),
        ..Setup::default()
    });
    h.say_as(ASKER, "m2", "slow-thread", Addressee::Bot, "please").await;
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    h.say_directly("d1", "hello").await;
    // The direct chat is answered while the thread's setup is still held. Only WAIT, the
    // harness's failure bound, limits this wait; no scheduler-speed assumption is needed.
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("hi")).await;
    gate.release.cancel();
    h.stop().await.unwrap();
}

#[tokio::test]
async fn input_left_by_a_run_waits_behind_a_conversation_that_queued_first() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("b done"), FauxStep::text("a done")],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    h.posted(&direct(), text(&replies.working)).await;
    // The run waits for an approval, holding the only slot.
    h.card(&direct()).await;
    h.say("b1", None, "the other thread").await;
    h.posted(&thread("b1"), text(&replies.queued)).await;
    h.say_directly("d2", "more").await;
    h.acknowledged(&direct(), "d2").await;
    // Closing the session ends the run and leaves "more" to answer in the session opened
    // again.
    let session = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&session));
    let done: Replacement = text("a done");
    while !matches!(h.next().await, Sent::Update { message, .. } if message == done) {}

    let requests = h.requests();
    assert!(
        requests[1].iter().any(|message| message.contains("the other thread")),
        "the thread that queued first runs first: {requests:?}"
    );
    assert!(
        requests[2].iter().any(|message| message.contains("more")),
        "{requests:?}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_after_a_release_waits_for_a_run_slot_to_open_the_session() {
    let gate = Gate::disarmed();
    let mut h = start(Setup {
        steps: vec![FauxStep::text("first"), FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        setup_gate: Some(gate.clone()),
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let released = ServerEvent::Released {
        key: key("slow-thread"),
    };
    // The bot takes part in the thread, whose session is then released.
    h.say_as(ASKER, "m2", "slow-thread", Addressee::Bot, "first").await;
    let status = h.posted(&thread("slow-thread"), text(&replies.working)).await;
    h.updated(&status, text("first")).await;
    while h.event().await != released {}
    gate.arm();
    // The direct chat's run holds the only slot.
    h.say_directly("d1", "run").await;
    let running = h.posted(&direct(), text(&replies.working)).await;

    // Talk in the thread waits for the slot instead of setting up the session at once, so
    // the thread's worker still answers a command meanwhile.
    h.say_as(OTHER, "m3", "slow-thread", Addressee::Nobody, "said meanwhile")
        .await;
    h.say_as(ASKER, "m4", "slow-thread", Addressee::Bot, "/cancel").await;
    h.posted(&thread("slow-thread"), text(&replies.nothing_to_cancel)).await;

    // Once the slot is free, the session is set up and what was said is recorded.
    h.say_directly("d2", "/cancel").await;
    h.updated(&running, text(&replies.cancelled)).await;
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    gate.release.cancel();
    while h.event().await != released {}
    let id = h.sessions.of(&key("slow-thread")).await.unwrap().remove(0);
    let records = h.sessions.storage.read(&id).await.unwrap().records;
    assert!(
        records.iter().any(|record| record.contains("said meanwhile")),
        "{records:?}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_conversation_waiting_for_a_run_slot_does_not_set_up_a_session() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::new(1).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say("m1", None, "run").await;
    h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.say_directly("d1", "wait").await;
    h.posted(&direct(), text(&h.replies.queued)).await;
    let opened = h.sessions.opened.load(Ordering::SeqCst);
    h.stop().await.unwrap();
    assert_eq!(
        opened, 1,
        "a queued conversation must not start a container or hold a session"
    );
}

#[tokio::test]
async fn the_conversation_limit_refuses_new_workers_before_session_setup() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_conversations: NonZeroUsize::new(1).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say("m1", None, "run").await;
    h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.say_directly("d1", "another conversation").await;
    h.posted(&direct(), text(&h.replies.overloaded)).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_waiting_conversation_bounds_its_messages_and_still_accepts_cancel() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::new(1).unwrap(),
            max_pending_messages: NonZeroUsize::new(1).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say("m1", None, "run").await;
    h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.say_directly("d1", "wait").await;
    h.posted(&direct(), text(&h.replies.queued)).await;
    h.say_directly("d2", "too much").await;
    h.posted(&direct(), text(&h.replies.overloaded)).await;
    h.say_directly("d3", "/cancel").await;
    h.posted(&direct(), text(&h.replies.cancelled)).await;
    h.say_directly("d4", "wait again").await;
    h.posted(&direct(), text(&h.replies.queued)).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 1);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn talk_refused_for_capacity_gets_no_reply_and_a_request_does() {
    let gate = Gate::new();
    let mut h = start(Setup {
        config: ServerConfig {
            max_pending_messages: NonZeroUsize::new(1).unwrap(),
            ..Setup::default().config
        },
        setup_gate: Some(gate.clone()),
        ..Setup::default()
    });
    let overloaded: Post = text(&h.replies.overloaded);
    h.say_as(ASKER, "m1", "slow-thread", Addressee::Bot, "first").await;
    // The first message keeps its share of the budget while its session is set up.
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    h.say_as(OTHER, "m2", "slow-thread", Addressee::Nobody, "among people")
        .await;
    h.say_as(ASKER, "m3", "slow-thread", Addressee::Bot, "second").await;
    // The first reply is the one to the request: the talk before it got none.
    h.posted(&thread("slow-thread"), overloaded.clone()).await;
    gate.release.cancel();
    let (sent, events) = h.stop_and_take().await;

    assert!(
        !sent
            .iter()
            .any(|sent| matches!(sent, Sent::Post { message, .. } if *message == overloaded)),
        "{sent:?}"
    );
    let refused = events
        .iter()
        .filter(|event| matches!(event, ServerEvent::Failed { error, .. } if error.contains("max_pending_messages")))
        .count();
    assert_eq!(refused, 2, "the operator hears of both: {events:?}");
}

#[tokio::test]
async fn steering_is_bounded_by_utf8_bytes_without_acknowledging_rejected_messages() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_pending_bytes: NonZeroUsize::new(6).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say_directly("d1", "run").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.say_directly("d2", "中文").await;
    h.acknowledged(&direct(), "d2").await;
    h.say_directly("d3", "x").await;
    h.posted(&direct(), text(&h.replies.overloaded)).await;
    h.say_directly("d4", "/cancel").await;
    h.updated(&status, text(&h.replies.cancelled)).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_oversized_message_is_refused_without_opening_a_session() {
    let mut h = start(Setup {
        config: ServerConfig {
            max_pending_bytes: NonZeroUsize::new(5).unwrap(),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    h.say_directly("d1", "中文").await;
    h.posted(&direct(), text(&h.replies.overloaded)).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}
