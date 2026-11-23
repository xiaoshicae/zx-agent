//! What a chat has of its own: an agent that answers its conversations, also after a restart,
//! approvers who alone decide there, and settings the server refuses.

use super::*;

/// An agent of a chat's own: it answers with `steps` of a model of its own, and keeps its
/// sessions in `storage`, where the server's agent keeps them.
fn own_agent(storage: &Arc<MemoryStorage>, steps: Vec<FauxStep>) -> (Agent, Arc<FauxProvider>) {
    let provider = Arc::new(FauxProvider::new(steps));
    let mut config = config(provider.clone(), Vec::new());
    config.retry = fast_retry(0);
    config.limits = test_limits();
    config.storage = storage.clone();
    (Agent::new(config).unwrap(), provider)
}

/// What `chat` of the fake platform has of its own: `agent`, and `approver` as its only
/// approver.
fn own_settings(chat: &str, agent: Option<Agent>, approver: Option<&str>) -> ChatConfig {
    ChatConfig {
        platform: PLATFORM.into(),
        chat: ConversationId(chat.into()),
        agent,
        approvers: approver.map(|user| vec![UserId(user.into())]),
    }
}

/// A server over `storage` whose model answers with `steps`, where the direct chat has the
/// agent `own` of its own.
fn start_with_own_agent(storage: &Arc<MemoryStorage>, steps: Vec<FauxStep>, own: Agent) -> Harness {
    let mut setup = Setup {
        steps,
        storage: Some(Arc::clone(storage)),
        ..Setup::default()
    };
    setup.config.chats = vec![own_settings(DIRECT, Some(own), None)];
    start(setup)
}

#[tokio::test]
async fn a_chat_with_an_agent_of_its_own_is_answered_by_it() {
    let storage = Arc::new(MemoryStorage::default());
    let (own, provider) = own_agent(&storage, vec![FauxStep::text("from the chat's agent")]);
    let mut h = start_with_own_agent(&storage, vec![FauxStep::text("from the server's agent")], own);
    h.say_directly("d1", "hello").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("from the chat's agent")).await;
    // The other chat keeps the server's agent.
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("from the server's agent")).await;
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(h.requests(), [["user:U-asker: hello"]]);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_conversation_with_a_chat_s_own_agent_goes_on_in_its_session_after_a_restart() {
    let storage = Arc::new(MemoryStorage::default());
    let (own, _) = own_agent(&storage, vec![FauxStep::text("one")]);
    let mut h = start_with_own_agent(&storage, Vec::new(), own);
    h.say_directly("d1", "first").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("one")).await;
    h.stop().await.unwrap();

    // A new server over the same storage, as after a restart: the session the chat's agent
    // made is found where the server's agent looks.
    let (own, provider) = own_agent(&storage, vec![FauxStep::text("two")]);
    let mut h = start_with_own_agent(&storage, Vec::new(), own);
    h.say_directly("d2", "second").await;
    let status = h.posted(&direct(), text(&h.replies.working)).await;
    h.updated(&status, text("two")).await;
    assert_eq!(
        summarize(&provider.requests()[0].messages),
        ["user:first", "assistant:one", "user:second"]
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn in_a_chat_with_approvers_of_its_own_only_they_decide() {
    let mut setup = Setup {
        steps: vec![echo_call(), FauxStep::text("finished")],
        ..Setup::default()
    };
    setup.config.chats = vec![own_settings(CHAT, None, Some(OTHER))];
    let mut h = start(setup);
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    // The server's approver is not one of the chat's.
    h.click("m1", APPROVER, &id, Choice::AllowOnce).await;
    h.posted(&thread("m1"), text(&h.replies.not_approver)).await;
    h.click("m1", OTHER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: Some(UserId(OTHER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.posted(&thread("m1"), text("finished")).await;
    h.updated(&status, text(&h.replies.answer_below)).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn settings_of_a_chat_not_served_or_given_twice_stop_the_server() {
    let cases = [
        (
            vec![own_settings("C9", None, Some(OTHER))],
            "the chat fake:C9 has settings of its own but is not served",
        ),
        (
            vec![
                own_settings(CHAT, None, Some(OTHER)),
                own_settings(CHAT, None, Some(APPROVER)),
            ],
            "the chat fake:C1 has settings of its own more than once",
        ),
    ];
    for (chats, expected) in cases {
        let mut setup = Setup::default();
        setup.config.chats = chats;
        let error = start(setup).stop().await.unwrap_err();
        assert!(matches!(&error, ImError::Config(text) if text == expected), "{error:?}");
    }
}
