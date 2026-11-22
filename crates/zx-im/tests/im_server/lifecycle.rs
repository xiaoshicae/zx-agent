//! The server's life: idle release, the stop, the backlog a restart answers, sessions that
//! close or cannot be opened, and a channel that fails or is configured twice.

use super::*;

/// Keep `event` in `backlog` as the channel `PLATFORM` does as it receives it: what a server
/// that ended before handling it leaves behind.
async fn kept_before(backlog: &Arc<MemoryBacklog>, event: &Inbound) {
    let (unread, _) = mpsc::channel(1);
    let inlet = Inlet::new(PLATFORM, unread, backlog.clone());
    assert_eq!(inlet.keep(event).await.unwrap(), Kept::New);
}

#[tokio::test]
async fn an_idle_conversation_releases_its_session_and_reopens_it() {
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
    h.say("m2", Some("m1"), "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.requests()[1],
        ["user:U-asker: first", "assistant:one", "user:U-asker: second"]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_run_cancelled_by_the_stop_says_the_server_is_stopping() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        ..Setup::default()
    });
    h.say("m1", None, "wait forever").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.cancel.cancel();
    h.updated(&status, text(&h.replies.stopped)).await;
    h.server.await.unwrap().unwrap();
}

#[tokio::test]
async fn stopping_leaves_a_message_waiting_for_a_run_slot_in_the_backlog() {
    let mut h = start(Setup {
        steps: vec![FauxStep::Hang],
        config: ServerConfig {
            max_running: NonZeroUsize::MIN,
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let backlog = h.backlog.clone();
    h.say("a", None, "task a").await;
    let status_a = h.posted(&thread("a"), text(&replies.working)).await;
    h.say("b", None, "task b").await;
    h.posted(&thread("b"), text(&replies.queued)).await;
    let (sent, events) = h.stop_and_take().await;
    // The run had taken in all its input, so its thread only learns that the stop cancelled
    // it. Thread b never ran: its message waits for the next start.
    assert_eq!(
        sent,
        [Sent::Update {
            posted: status_a,
            message: text(&replies.stopped)
        }]
    );
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(pending(&backlog).await, ["b"]);
}

#[tokio::test]
async fn stopping_leaves_steering_its_run_did_not_take_in_in_the_backlog() {
    let mut h = start(Setup {
        steps: vec![echo_call()],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    let backlog = h.backlog.clone();
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.say("m2", Some("m1"), "also check the logs").await;
    h.acknowledged(&thread("m1"), "m2").await;
    // The worker answers this click only after it has steered the message before it.
    h.click("m1", OTHER, &id, Choice::AllowOnce).await;
    h.posted(&thread("m1"), text(&replies.not_approver)).await;
    let (sent, events) = h.stop_and_take().await;
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    assert_eq!(
        sent,
        [
            Sent::Update {
                posted: card,
                message: closed
            },
            Sent::Update {
                posted: status,
                message: text(&replies.stopped)
            }
        ]
    );
    assert!(matches!(events.as_slice(), [ServerEvent::Approval(_)]), "{events:?}");
    // The run recorded its prompt; the steering it never took waits for the next start.
    assert_eq!(pending(&backlog).await, ["m2"]);
}

#[tokio::test]
async fn a_message_received_as_the_server_stops_stays_in_the_backlog() {
    let h = start(Setup {
        late: vec![message(CHAT, "m1", None, "hello")],
        ..Setup::default()
    });
    let sessions = Arc::clone(&h.sessions);
    let backlog = h.backlog.clone();
    let (sent, events) = h.stop_and_take().await;
    assert!(sent.is_empty(), "{sent:?}");
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(
        sessions.opened.load(Ordering::SeqCst),
        0,
        "nothing runs during the stop"
    );
    assert_eq!(pending(&backlog).await, ["m1"]);
}

#[tokio::test]
async fn a_message_an_earlier_server_left_in_the_backlog_is_answered() {
    let backlog = Arc::new(MemoryBacklog::default());
    kept_before(&backlog, &message(CHAT, "m1", None, "left behind")).await;
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer"), FauxStep::text("answer")],
        backlog: backlog.clone(),
        ..Setup::default()
    });
    // Delivered again by the platform, it is known and not answered twice.
    h.say("m1", None, "left behind").await;
    h.say("m2", None, "new").await;
    // The two threads may run at the same time.
    let mut answers = 0;
    while answers < 2 {
        if let Sent::Update { message, .. } = h.next().await {
            assert_eq!(message, text("answer"));
            answers += 1;
        }
    }
    let mut requests = h.requests();
    requests.sort();
    assert_eq!(requests, [["user:U-asker: left behind"], ["user:U-asker: new"]]);
    let (sent, events) = h.stop_and_take().await;
    assert!(sent.is_empty(), "{sent:?}");
    assert!(events.is_empty(), "{events:?}");
    assert!(pending(&backlog).await.is_empty());
}

#[tokio::test]
async fn a_handled_message_delivered_again_is_not_answered_again() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    h.say("m1", None, "first").await;
    h.say("m2", Some("m1"), "second").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests().len(), 2);
    let backlog = h.backlog.clone();
    h.stop().await.unwrap();
    assert!(pending(&backlog).await.is_empty());
}

#[tokio::test]
async fn messages_answered_without_a_run_leave_the_backlog() {
    let mut h = start(Setup::default());
    let replies = h.replies.clone();
    h.say("m1", None, "/cancel").await;
    h.posted(&thread("m1"), text(&replies.nothing_to_cancel)).await;
    h.say_in("C-other", "m2", None, "hello").await;
    let elsewhere = Destination {
        conversation: ConversationId("C-other".into()),
        thread: Some(ThreadId("m2".into())),
    };
    h.posted(&elsewhere, text(&replies.unauthorized)).await;
    let backlog = h.backlog.clone();
    h.stop().await.unwrap();
    assert!(pending(&backlog).await.is_empty());
}

#[tokio::test]
async fn a_failing_channel_stops_the_server_with_its_error() {
    let h = start(Setup {
        fail_channel: Some("token rejected".into()),
        ..Setup::default()
    });
    match h.server.await.unwrap() {
        Err(ImError::Config(error)) => assert_eq!(error, "token rejected"),
        other => panic!("expected the channel's error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_conversation_whose_session_is_gone_goes_on_in_a_new_one() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        config: ServerConfig {
            idle: Duration::from_millis(20),
            ..Setup::default().config
        },
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say_directly("d1", "hello").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    assert_eq!(h.event().await, ServerEvent::Released { key: direct_key() });
    let gone = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    h.sessions.forget(&gone);

    h.say_directly("d2", "again").await;

    match h.event().await {
        ServerEvent::Failed { error, .. } => assert!(error.contains(&gone), "{error}"),
        other => panic!("expected the lost session to be reported, got {other:?}"),
    }
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1], ["user:again"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_session_that_cannot_be_opened_is_reported_and_tried_again() {
    let mut h = start(Setup {
        fail_open: Some("disk full".into()),
        ..Setup::default()
    });
    let refused: Post = text(&format!(
        "{}cannot set up the session: disk full",
        h.replies.cannot_open
    ));
    h.say("m1", None, "hello").await;
    h.posted(&thread("m1"), refused.clone()).await;
    match h.event().await {
        ServerEvent::Failed { key: Some(key), error } => {
            assert_eq!(key.thread, Some(ThreadId("m1".into())));
            assert!(error.contains("disk full"), "{error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    h.say("m2", Some("m1"), "again").await;
    h.posted(&thread("m1"), refused).await;
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn two_channels_with_one_name_are_a_configuration_error() {
    let channel = || -> Arc<dyn ImChannel> {
        let (sent, _) = mpsc::unbounded_channel();
        Arc::new(FakeChannel {
            name: PLATFORM,
            events: Mutex::new(None),
            late: Mutex::new(Vec::new()),
            sent,
            next: AtomicUsize::new(0),
            fail: None,
            updates: Updates::default(),
            refused_post: None,
            history: Vec::new(),
            history_gate: None,
            chat: Vec::new(),
            names: HashMap::new(),
            images: HashMap::new(),
        })
    };
    let h = start(Setup::default());
    let server = Server::new(ServerConfig::default(), h.agent.clone(), Arc::new(NullServerSink));
    let result = server.run(vec![channel(), channel()], &CancellationToken::new()).await;
    assert!(matches!(result, Err(ImError::Config(error)) if error.contains("fake")));
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_session_closed_under_its_conversation_is_opened_again_for_the_next_message() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), working.clone()).await;
    h.updated(&status, text("one")).await;
    // Closed by someone other than the conversation, which still holds it: a session also
    // closes itself this way when it can no longer write its records.
    let id = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&id));
    h.say_directly("d2", "second").await;
    let status = h.posted(&direct(), working).await;
    h.updated(&status, text("two")).await;
    // The message that found the session closed is answered in the session opened again.
    assert_eq!(h.requests()[1], ["user:first", "assistant:one", "user:second"]);
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_session_closed_during_its_run_is_let_go_and_opened_again_for_the_next_message() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("two")],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    // The run waits for an approval, so it is in progress when its session is closed.
    let (id, card) = h.card(&direct()).await;
    let session = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&session));
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.updated(&status, text(&replies.cancelled)).await;
    // The approval that the closing cancelled is reported before the release.
    while h.event().await != (ServerEvent::Released { key: direct_key() }) {}
    h.say_directly("d2", "again").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1].last().map(String::as_str), Some("user:again"));
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn what_was_asked_during_a_run_whose_session_closed_is_answered_in_the_session_opened_again() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("two")],
        ..Setup::default()
    });
    let replies = h.replies.clone();
    h.say_directly("d1", "run echo").await;
    let status = h.posted(&direct(), text(&replies.working)).await;
    let (id, card) = h.card(&direct()).await;
    // Asked while the run waits for its approval: acknowledged, and queued for the run.
    h.say_directly("d2", "meanwhile").await;
    h.acknowledged(&direct(), "d2").await;
    // Closed under the conversation, as a session closes itself when it can no longer write
    // its records: the run ends without taking what was queued.
    let session = h.sessions.of(&direct_key()).await.unwrap().remove(0);
    assert!(h.agent.close_session(&session));
    let closed = Replacement::ApprovalResolved {
        card: echo_card(),
        id,
        user: None,
        result: ApprovalResult::Cancelled,
    };
    h.updated(&card, closed).await;
    h.updated(&status, text(&replies.cancelled)).await;

    let status = h.posted(&direct(), text(&replies.working)).await;
    h.updated(&status, text("two")).await;
    assert_eq!(h.requests()[1].last().map(String::as_str), Some("user:meanwhile"));
    assert_eq!(h.sessions.opened.load(Ordering::SeqCst), 2);
    h.stop().await.unwrap();
}
