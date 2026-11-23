//! Files the server posts: an answer too long to read as messages, as a Markdown file, and as
//! text when the platform cannot post it.

use super::*;

/// A server whose model answers with `answer`, and that posts answers longer than ten
/// characters as files, but for `refused`.
fn long_answers(answer: &str, refused: Option<&str>) -> Harness {
    let mut setup = Setup {
        steps: vec![FauxStep::text(answer)],
        refused_file: refused.map(String::from),
        ..Setup::default()
    };
    setup.config.long_answer_chars = NonZeroUsize::new(10).unwrap();
    start(setup)
}

/// The file `answer.md` holding `answer`.
fn answer_file(answer: &str) -> Attachment {
    Attachment {
        name: "answer.md".into(),
        bytes: answer.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn a_long_answer_is_posted_as_a_file_and_the_status_says_so() {
    let answer = "a report of more than ten characters";
    let mut h = long_answers(answer, None);
    h.say("m1", None, "report").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    match h.next().await {
        Sent::File { to, file } => assert_eq!((to, file), (thread("m1"), answer_file(answer))),
        other => panic!("expected the answer as a file, got {other:?}"),
    }
    h.updated(&status, text(&h.replies.answer_attached)).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_short_answer_stays_text() {
    let mut h = long_answers("ten chars!", None);
    h.say("m1", None, "hello").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("ten chars!")).await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_long_answer_the_platform_cannot_post_as_a_file_is_posted_as_text() {
    let answer = "a report of more than ten characters";
    let mut h = long_answers(answer, Some("answer.md"));
    h.say("m1", None, "report").await;
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    match h.next().await {
        Sent::File { file, .. } => assert_eq!(file.name, "answer.md"),
        other => panic!("expected an attempt to post the file, got {other:?}"),
    }
    h.updated(&status, text(answer)).await;
    match h.event().await {
        ServerEvent::Failed { key: failed_in, error } => {
            assert_eq!(failed_in, Some(key("m1")));
            assert!(error.contains("cannot post the answer as answer.md"), "{error}");
        }
        other => panic!("expected the failure, got {other:?}"),
    }
    h.stop().await.unwrap();
}
