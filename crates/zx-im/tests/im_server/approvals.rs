//! Approvals: the card, a click by an approver or by someone else, an approval nobody
//! answers, the answer that follows a card, and the record of each approval.

use zx_agent::ai::now_ms;

use super::*;

#[tokio::test]
async fn an_answer_after_a_card_is_posted_below_it_and_the_status_says_so() {
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
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    h.posted(&thread("m1"), text("finished")).await;
    h.updated(&status, text(&h.replies.answer_below)).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_answer_that_cannot_be_posted_below_a_card_takes_the_place_of_the_status() {
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("finished")],
        refused_post: Some("finished".into()),
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
            id,
            user: Some(UserId(APPROVER.into())),
            result: ApprovalResult::AllowOnce,
        },
    )
    .await;
    // Tried below the card, refused there.
    h.posted(&thread("m1"), text("finished")).await;
    h.updated(&status, text("finished")).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_answer_after_a_card_that_the_run_goes_on_from_is_posted_below_the_card() {
    let gate = Gate::new();
    let mut h = start(Setup {
        steps: vec![
            echo_call(),
            FauxStep::text("first answer"),
            FauxStep::text("second answer"),
        ],
        answer_gate: Some(gate.clone()),
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    let first = h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
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
    tokio::time::timeout(WAIT, gate.entered.notified()).await.unwrap();
    h.say("m2", Some("m1"), "and one more thing").await;
    h.acknowledged(&thread("m1"), "m2").await;
    gate.release.cancel();
    h.posted(&thread("m1"), text("first answer")).await;
    h.updated(&first, text(&h.replies.answer_below)).await;
    // The new status comes after the card: the next answer takes its place.
    let second = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&second, text("second answer")).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_approval_record_has_the_time_it_ended_not_when_its_card_closed() {
    let release = CancellationToken::new();
    let mut h = start(Setup {
        steps: vec![echo_call(), FauxStep::text("finished")],
        updates: Updates::Held(release.clone()),
        ..Setup::default()
    });
    h.say("m1", None, "run echo").await;
    h.posted(&thread("m1"), text(&h.replies.working)).await;
    let (id, card) = h.card(&thread("m1")).await;
    h.click("m1", APPROVER, &id, Choice::AllowOnce).await;
    match h.next().await {
        Sent::Update { posted, .. } => assert_eq!(posted, card),
        other => panic!("expected the card to close, got {other:?}"),
    }
    // The card closes only once released, a while after the approval ended: long enough for
    // the clock to tell the two apart.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let closing = now_ms();
    release.cancel();
    match h.event().await {
        ServerEvent::Approval(record) => assert!(
            record.ended_ms < closing,
            "ended at {}, not before the card closed at {closing}",
            record.ended_ms
        ),
        other => panic!("expected the approval record, got {other:?}"),
    }
    h.stop().await.unwrap();
}

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
    h.posted(&thread("m1"), text("finished")).await;
    h.updated(&status, text(&h.replies.answer_below)).await;
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
    h.posted(&thread("m1"), text("not run")).await;
    h.updated(&status, text(&h.replies.answer_below)).await;
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
    h.posted(&thread("m1"), text("gave up")).await;
    h.updated(&status, text(&h.replies.answer_below)).await;
    assert_eq!(h.sessions.echo.calls.load(Ordering::SeqCst), 0);
    h.stop().await.unwrap();
}
