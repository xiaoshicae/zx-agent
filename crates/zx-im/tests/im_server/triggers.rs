use super::*;

/// The chat itself, where a trigger's title opens its thread.
fn the_chat() -> Destination {
    Destination {
        conversation: ConversationId(CHAT.into()),
        thread: None,
    }
}

/// The trigger of an hourly schedule in the served chat.
fn hourly() -> Trigger {
    Trigger {
        platform: PLATFORM.into(),
        chat: ConversationId(CHAT.into()),
        title: "Hourly check".into(),
        prompt: "look around".into(),
        source: "schedule:hourly".into(),
    }
}

#[tokio::test]
async fn a_trigger_opens_a_thread_with_its_title_and_answers_its_prompt_there() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("all is well")],
        ..Setup::default()
    });
    assert!(h.triggers.start(hourly()).await);
    let root = h.posted(&the_chat(), text("Hourly check")).await;
    let status = h.posted(&thread(&root.id.0), text(&h.replies.working)).await;
    h.updated(&status, text("all is well")).await;
    assert_eq!(
        h.event().await,
        ServerEvent::Triggered {
            key: key(&root.id.0),
            source: "schedule:hourly".into()
        }
    );
    // The prompt is the source's, under its own name: nobody's on the platform.
    assert_eq!(h.requests()[0], ["user:schedule:hourly: look around"]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_mention_in_a_triggered_thread_goes_on_in_its_session() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("all is well"), FauxStep::text("nothing new")],
        ..Setup::default()
    });
    h.triggers.start(hourly()).await;
    let root = h.posted(&the_chat(), text("Hourly check")).await;
    let status = h.posted(&thread(&root.id.0), text(&h.replies.working)).await;
    h.updated(&status, text("all is well")).await;
    h.say("m2", Some(&root.id.0), "anything else?").await;
    let status = h.posted(&thread(&root.id.0), text(&h.replies.working)).await;
    h.updated(&status, text("nothing new")).await;
    assert_eq!(
        h.requests()[1],
        [
            "user:schedule:hourly: look around",
            "assistant:all is well",
            "user:U-asker: anything else?"
        ]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_trigger_for_a_chat_not_served_posts_nothing_and_is_reported() {
    let mut h = start(Setup::default());
    let elsewhere = Trigger {
        chat: ConversationId("C-other".into()),
        ..hourly()
    };
    h.triggers.start(elsewhere).await;
    match h.event().await {
        ServerEvent::Failed { key: None, error } => assert!(error.contains("not served"), "{error}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    let (sent, _) = h.stop_and_take().await;
    assert!(sent.is_empty(), "{sent:?}");
}

#[tokio::test]
async fn a_trigger_whose_title_cannot_be_posted_is_reported_and_dropped() {
    let mut h = start(Setup {
        refused_post: Some("Hourly check".into()),
        ..Setup::default()
    });
    h.triggers.start(hourly()).await;
    h.posted(&the_chat(), text("Hourly check")).await;
    match h.event().await {
        ServerEvent::Failed { key: None, error } => assert!(error.contains("open a thread in C1"), "{error}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    // Without its thread, the trigger asks nothing.
    assert!(h.requests().is_empty());
    let (sent, _) = h.stop_and_take().await;
    assert!(sent.is_empty(), "{sent:?}");
}

#[tokio::test]
async fn a_trigger_is_refused_once_the_server_has_stopped() {
    let h = start(Setup::default());
    let triggers = h.triggers.clone();
    h.stop().await.unwrap();
    assert!(!triggers.start(hourly()).await);
}
