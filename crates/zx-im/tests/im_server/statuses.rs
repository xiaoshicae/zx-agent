//! The statuses of running work kept for a restart: kept, forgotten, replaced by the answer,
//! left by an earlier process, and a store that fails.

use super::*;

/// A status that an earlier process left in thread `root`.
fn left_in(root: &str, id: &str) -> MessageRef {
    MessageRef {
        at: thread(root),
        id: MessageId(id.into()),
    }
}

fn store_failed(root: &str, doing: &str) -> ServerEvent {
    ServerEvent::Failed {
        key: Some(key(root)),
        error: format!("cannot {doing}: {STORE_ERROR}"),
    }
}

#[tokio::test]
async fn a_running_status_is_kept_and_forgotten_before_the_answer_replaces_it() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    h.updated(&status, text("answer")).await;
    let statuses = h.statuses.clone().unwrap();
    h.stop().await.unwrap();
    assert!(statuses.kept().is_empty());
}

#[tokio::test]
async fn a_status_left_by_an_earlier_process_is_replaced_and_forgotten() {
    let left = left_in("m0", "s0");
    let mut h = start(Setup {
        statuses: Some(vec![(key("m0"), left.clone())]),
        ..Setup::default()
    });
    h.updated(&left, text(&h.replies.interrupted)).await;
    assert_eq!(h.next().await, Sent::Forgot(left));
    let statuses = h.statuses.clone().unwrap();
    h.stop().await.unwrap();
    assert!(statuses.kept().is_empty());
}

#[tokio::test]
async fn a_left_status_the_platform_does_not_replace_is_reported_and_forgotten() {
    let left = left_in("m0", "s0");
    let mut h = start(Setup {
        statuses: Some(vec![(key("m0"), left.clone())]),
        updates: Updates::Refused,
        ..Setup::default()
    });
    h.updated(&left, text(&h.replies.interrupted)).await;
    assert_eq!(h.next().await, Sent::Forgot(left));
    assert_eq!(
        h.event().await,
        ServerEvent::Failed {
            key: Some(key("m0")),
            error: "cannot replace status s0 that an earlier process left: update refused".into(),
        }
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_left_status_of_a_platform_not_served_is_reported_and_forgotten() {
    let gone = SessionKey {
        platform: "gone".into(),
        ..key("m0")
    };
    let left = left_in("m0", "s0");
    let mut h = start(Setup {
        statuses: Some(vec![(gone.clone(), left.clone())]),
        ..Setup::default()
    });
    assert_eq!(h.next().await, Sent::Forgot(left));
    assert_eq!(
        h.event().await,
        ServerEvent::Failed {
            key: Some(gone),
            error: "cannot replace status s0 that an earlier process left: no channel is named \"gone\"".into(),
        }
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_stop_gives_up_replacing_left_statuses_and_keeps_them() {
    let left = left_in("m0", "s0");
    let mut h = start(Setup {
        statuses: Some(vec![(key("m0"), left.clone())]),
        updates: Updates::Hang,
        ..Setup::default()
    });
    // The platform never answers this update.
    h.updated(&left, text(&h.replies.interrupted)).await;
    let statuses = h.statuses.clone().unwrap();
    h.stop().await.unwrap();
    assert_eq!(statuses.kept(), [(key("m0"), left)]);
}

#[tokio::test]
async fn a_status_that_cannot_be_kept_is_reported_and_the_run_goes_on() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        broken: Some(Broken::Insert),
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(
        h.event().await,
        store_failed("m1", &format!("keep status {}", status.id))
    );
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    h.updated(&status, text("answer")).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_status_that_cannot_be_forgotten_is_reported_and_still_replaced() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        broken: Some(Broken::Remove),
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    assert_eq!(
        h.event().await,
        store_failed("m1", &format!("forget status {}", status.id))
    );
    h.updated(&status, text("answer")).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_answer_that_cannot_replace_its_status_is_posted_instead() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        updates: Updates::Refused,
        ..Setup::default()
    });
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    h.updated(&status, text("answer")).await;
    h.posted(&thread("m1"), text("answer")).await;
    assert_eq!(
        h.event().await,
        ServerEvent::Failed {
            key: Some(key("m1")),
            error: format!("cannot update message {}: update refused", status.id),
        }
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn statuses_that_cannot_be_read_are_reported_and_the_server_serves() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("answer")],
        statuses: Some(Vec::new()),
        broken: Some(Broken::List),
        ..Setup::default()
    });
    assert_eq!(
        h.event().await,
        ServerEvent::Failed {
            key: None,
            error: format!("cannot read the statuses an earlier process left: {STORE_ERROR}"),
        }
    );
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    assert_eq!(h.next().await, Sent::Kept(status.clone()));
    assert_eq!(h.next().await, Sent::Forgot(status.clone()));
    h.updated(&status, text("answer")).await;
    h.stop().await.unwrap();
}
