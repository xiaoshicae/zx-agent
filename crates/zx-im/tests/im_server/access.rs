//! Who may use the server, replaced while it runs: the chats it answers in and the approvers.

use super::*;

/// The thread under message `root` in the chat `chat`.
fn thread_in(chat: &str, root: &str) -> Destination {
    Destination {
        conversation: ConversationId(chat.into()),
        thread: Some(ThreadId(root.into())),
    }
}

#[tokio::test]
async fn replaced_lists_serve_a_new_chat_and_take_another_approver() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("hello from C2"), echo_call(), FauxStep::text("finished")],
        ..Setup::default()
    });
    h.say_in("C2", "m1", None, "hello").await;
    h.posted(&thread_in("C2", "m1"), text(&h.replies.unauthorized)).await;

    h.access.replace(
        vec![
            (PLATFORM.into(), ConversationId(CHAT.into())),
            (PLATFORM.into(), ConversationId("C2".into())),
        ],
        vec![(PLATFORM.into(), UserId(OTHER.into()))],
    );
    h.say_in("C2", "m2", None, "hello").await;
    let status = h.posted(&thread_in("C2", "m2"), text(&h.replies.working)).await;
    h.updated(&status, text("hello from C2")).await;

    // The approver taken off the list decides nothing; the one put on it does.
    h.say("m3", None, "run echo").await;
    let status = h.posted(&thread("m3"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m3")).await;
    h.click("m3", APPROVER, &id, Choice::AllowOnce).await;
    h.posted(&thread("m3"), text(&h.replies.not_approver)).await;
    h.click("m3", OTHER, &id, Choice::AllowOnce).await;
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
    h.posted(&thread("m3"), text("finished")).await;
    h.updated(&status, text(&h.replies.answer_below)).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_chat_taken_off_the_list_is_no_longer_answered() {
    let mut h = start(Setup::default());
    h.access
        .replace(Vec::new(), vec![(PLATFORM.into(), UserId(APPROVER.into()))]);
    h.say("m1", None, "hello").await;
    h.posted(&thread("m1"), text(&h.replies.unauthorized)).await;
    assert!(h.requests().is_empty());
    h.stop().await.unwrap();
}
