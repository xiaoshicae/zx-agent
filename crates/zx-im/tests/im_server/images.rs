//! The images of messages: fetched and fitted for the messages the bot answers, named in the
//! text when they cannot be used, and named only for the messages it does not answer.

use zx_agent::ai::Image;

use super::*;

/// A PNG of `width` by `height` black pixels.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::new_rgb8(width, height)
        .write_to(std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    bytes
}

/// The image the platform keeps as `key`, named `name`.
fn attached(key: &str, name: &str) -> ImageRef {
    ImageRef {
        key: key.into(),
        name: name.into(),
    }
}

/// `message` with `images` attached.
fn with_images(mut message: Inbound, attached: Vec<ImageRef>) -> Inbound {
    if let Inbound::Message { images, .. } = &mut message {
        *images = attached;
    }
    message
}

/// The text and images of the last user message of the model's request `index`.
fn asked(h: &Harness, index: usize) -> (String, Vec<Image>) {
    let requests = h.sessions.provider.requests();
    let last = requests[index].messages.iter().rev().find_map(|message| match message {
        Message::User { content, images, .. } => Some((content.clone(), images.clone())),
        _ => None,
    });
    last.expect("the request asks something")
}

#[tokio::test]
async fn the_images_of_a_message_the_bot_answers_reach_the_model() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("a black square")],
        images: HashMap::from([("k1".to_string(), Ok(png(2, 2)))]),
        ..Setup::default()
    });
    let said = message(CHAT, "m1", None, "what is this?");
    h.inject
        .send(with_images(said, vec![attached("k1", "shot.png")]))
        .await
        .unwrap();
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("a black square")).await;

    let (content, images) = asked(&h, 0);
    assert_eq!(content, "U-asker: what is this?");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_image_that_cannot_be_used_is_named_in_the_text_and_reported() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("no picture")],
        images: HashMap::from([
            ("k1".to_string(), Err("HTTP 403".to_string())),
            ("k2".to_string(), Ok(b"not an image".to_vec())),
        ]),
        ..Setup::default()
    });
    let said = message(CHAT, "m1", None, "look");
    let images = vec![attached("k1", "shot.png"), attached("k2", "notes.png")];
    h.inject.send(with_images(said, images)).await.unwrap();
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("no picture")).await;

    let (content, images) = asked(&h, 0);
    assert_eq!(
        content,
        "U-asker: look\n[image shot.png could not be used: cannot fetch it: HTTP 403]\n\
         [image notes.png could not be used: not a PNG, JPEG, GIF or WebP image]"
    );
    assert!(images.is_empty());
    match h.event().await {
        ServerEvent::Failed { error, .. } => {
            assert_eq!(error, "image shot.png of message m1: cannot fetch it: HTTP 403");
        }
        other => panic!("expected the failed fetch, got {other:?}"),
    }
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_message_gives_the_model_at_most_four_images() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("five")],
        images: (1..=5).map(|n| (format!("k{n}"), Ok(png(1, 1)))).collect(),
        ..Setup::default()
    });
    let images = (1..=5)
        .map(|n| attached(&format!("k{n}"), &format!("{n}.png")))
        .collect();
    h.inject
        .send(with_images(message(CHAT, "m1", None, "all of them"), images))
        .await
        .unwrap();
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("five")).await;

    let (content, images) = asked(&h, 0);
    assert_eq!(images.len(), 4);
    assert!(
        content.ends_with("\n[image 5.png could not be used: the bot takes at most 4 images of a message]"),
        "{content}"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_image_larger_than_the_bot_fetches_is_refused() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("too large")],
        images: HashMap::from([("k1".to_string(), Ok(vec![0; 20 * 1024 * 1024 + 1]))]),
        ..Setup::default()
    });
    h.inject
        .send(with_images(
            message(CHAT, "m1", None, "huge"),
            vec![attached("k1", "big.png")],
        ))
        .await
        .unwrap();
    let status = h.posted(&thread("m1"), text(&h.replies.working)).await;
    h.updated(&status, text("too large")).await;

    let (content, images) = asked(&h, 0);
    assert!(images.is_empty());
    assert_eq!(
        content,
        "U-asker: huge\n[image big.png could not be used: cannot fetch it: the file is larger than 20971520 bytes]"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn the_images_of_a_message_the_bot_does_not_answer_are_only_named() {
    let mut h = start(Setup {
        steps: vec![FauxStep::text("one"), FauxStep::text("two")],
        images: HashMap::from([("k1".to_string(), Ok(png(1, 1)))]),
        ..Setup::default()
    });
    let working: Post = text(&h.replies.working);
    h.say("m1", None, "first").await;
    let status = h.posted(&thread("m1"), working.clone()).await;
    h.updated(&status, text("one")).await;
    // Someone else posts a picture in the thread, to the others.
    let heard = group_message(CHAT, "m2", Some("m1"), OTHER, Addressee::Others, "mine");
    h.inject
        .send(with_images(heard, vec![attached("k1", "theirs.png")]))
        .await
        .unwrap();
    h.say("m3", Some("m1"), "and now?").await;
    let status = h.posted(&thread("m1"), working).await;
    h.updated(&status, text("two")).await;

    assert!(
        h.requests()[1].contains(&"user:U-other: mine\n[image theirs.png not shown]".to_string()),
        "{:?}",
        h.requests()[1]
    );
    let requests = h.sessions.provider.requests();
    let fetched: usize = requests[1]
        .messages
        .iter()
        .map(|message| match message {
            Message::User { images, .. } => images.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(fetched, 0, "nothing is fetched for a message the bot does not answer");
    h.stop().await.unwrap();
}
