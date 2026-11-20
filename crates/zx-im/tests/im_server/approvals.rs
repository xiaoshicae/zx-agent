//! Approvals: the card, a click by an approver or by someone else, and an approval nobody
//! answers.

use super::*;

#[tokio::test]
async fn an_approver_allows_a_call_and_the_card_shows_the_decision() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("finished")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.click("m1", APPROVER, &id, Choice::AllowOnce).await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
            id: id.clone(),
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.updated(&status, text("finished")).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.requests()[1],
        ["user:U-asker: run echo", "assistant:calls[c1]", "tool:c1=done"]
    );
    match h.event().await {
        ServerEvent::Approval(record) => {
            assert_eq!(
                (
                    record.tool_call_id.as_str(),
                    record.tool.as_str(),
                    record.approval,
                    record.user,
                    record.result
                ),
                (
                    "c1",
                    "echo",
                    Some(id),
                    Some(UserId(APPROVER.into())),
                    ApprovalResult::AllowOnce
                )
            );
            assert!(record.ended_ms > 0);
        }
        other => panic!("expected the approval record, got {other:?}"),
    }
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_click_by_someone_who_is_not_an_approver_decides_nothing() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("not run")],
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.click("m1", OTHER, &id, Choice::AllowOnce).await;
    h.posted(&thread("m1"), text(&h.replies.not_approver)).await;
    h.click("m1", APPROVER, &id, Choice::Deny).await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::Denied,
        },
    )
    .await;
    h.updated(&status, text("not run")).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_unanswered_approval_times_out_and_its_card_says_so() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("gave up")],
        approval_timeout: Duration::from_millis(50),
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.updated(
        &card,
        Replacement::ApprovalResolved {
            card: echo_card(),
            id,
            user: None,
            result: ApprovalResult::TimedOut,
        },
    )
    .await;
    h.updated(&status, text("gave up")).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}
