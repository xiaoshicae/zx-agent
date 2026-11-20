//! Images through the public SDK: a person's images and a tool's reach the model with the
//! message they came with and are recorded with it, and each request says whether its model
//! takes images.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use zx_agent::AgentConfig;
use zx_agent::ai::faux::{FauxProvider, FauxStep};
use zx_agent::ai::{Image, Message, ModelMeta, ToolDef};
use zx_agent::events::RunOutcome;
use zx_agent::input::Input;
use zx_agent::tools::{Effects, Tool, ToolContext, ToolOutput};
use zx_agent::transcript::Transcript;
use zx_testing::{RunnerSetup, config};

/// An image with a stand-in for its data, which the kernel does not read.
fn png() -> Image {
    Image {
        mime_type: "image/png".into(),
        data: "iVBORw0KGgo=".into(),
    }
}

/// A tool that returns a screenshot.
struct Screenshot;

#[async_trait]
impl Tool for Screenshot {
    fn name(&self) -> &str {
        "screenshot"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "screenshot".into(),
            description: "Take a screenshot.".into(),
            parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    fn effects(&self) -> Effects {
        Effects::ReadOnly
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        Ok(ToolOutput::ok("the screen").with_images(vec![png()]))
    }
}

/// The role of a session whose model, `test-model`, answers as `provider` scripts and takes
/// images when `takes_images` says so.
fn role(provider: &Arc<FauxProvider>, takes_images: bool) -> AgentConfig {
    let mut config = config(provider.clone(), vec![Arc::new(Screenshot)]);
    let meta = ModelMeta {
        takes_images,
        ..ModelMeta::default()
    };
    config.models.insert("test-model".into(), meta);
    config
}

/// A person's question with an image.
fn asked_with_a_picture() -> Input {
    Input::User {
        text: "what is this?".into(),
        images: vec![png()],
        actor: None,
    }
}

#[tokio::test]
async fn a_persons_images_reach_the_model_and_are_recorded_with_the_question() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("a cat")]));
    let (session, _) = RunnerSetup::new(role(&provider, true)).build().await;

    let report = session.run(asked_with_a_picture()).await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    let requests = provider.requests();
    assert!(requests[0].takes_images);
    let Some(Message::User { content, images, .. }) = requests[0].messages.last() else {
        panic!("the question is the last message: {:?}", requests[0].messages);
    };
    assert_eq!((content.as_str(), images.as_slice()), ("what is this?", &[png()][..]));
    let recorded = session.with_transcript(Transcript::messages).unwrap();
    assert!(
        recorded
            .iter()
            .any(|message| matches!(message, Message::User { images, .. } if *images == [png()])),
        "{recorded:?}"
    );
}

#[tokio::test]
async fn a_tools_images_reach_the_model_in_its_result() {
    let provider = Arc::new(FauxProvider::new([
        FauxStep::tool_call("c1", "screenshot", json!({})),
        FauxStep::text("done"),
    ]));
    let (session, _) = RunnerSetup::new(role(&provider, true)).build().await;

    let report = session.run("look at the screen").await;

    assert_eq!(report.outcome, RunOutcome::Completed);
    let requests = provider.requests();
    let Some(Message::ToolResult(result)) = requests[1].messages.last() else {
        panic!("the result is the last message: {:?}", requests[1].messages);
    };
    assert_eq!(result.content, "the screen");
    assert_eq!(result.images, [png()]);
}

#[tokio::test]
async fn a_model_that_takes_no_images_is_asked_so_and_the_session_keeps_them() {
    let provider = Arc::new(FauxProvider::new([FauxStep::text("cannot see it")]));
    let (session, _) = RunnerSetup::new(role(&provider, false)).build().await;

    session.run(asked_with_a_picture()).await;

    // The provider sends a note in place of the images; the record keeps them.
    assert!(!provider.requests()[0].takes_images);
    let recorded = session.with_transcript(Transcript::messages).unwrap();
    assert!(
        recorded
            .iter()
            .any(|message| matches!(message, Message::User { images, .. } if *images == [png()]))
    );
}
