//! One turn of a session: the prompt that starts it, its run read to its end with the events
//! sent to the client, and the stop reason that answers `session/prompt`.

use std::borrow::Cow;

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{ContentBlock, PromptResponse, SessionId, StopReason};
use anyhow::Result;
use tokio_util::sync::CancellationToken;
use zx_agent::ai::Image;
use zx_agent::builtin::images;
use zx_agent::compaction::{self, Outcome};
use zx_agent::events::{Budget, EventKind, RunOutcome, RunReport};
use zx_agent::input::Input;
use zx_agent::{Run, Session};
use zx_app::shutdown::next_event;

use super::permissions;
use super::updates::Updates;

/// What a turn runs: the task a prompt starts, or the compaction `/compact` starts.
pub(super) enum Running {
    Task(Run),
    Compaction(Run<compaction::Report>),
}

/// One turn of a session: the conversation, and the client it reports to and asks.
pub(super) struct Turn {
    session: Session,
    updates: Updates,
    cx: ConnectionTo<Client>,
    id: SessionId,
    /// Cancelled when the process is told to stop.
    stop: CancellationToken,
}

impl Turn {
    /// The turn of `session`, reported to the client `cx`, which knows the session as `id`;
    /// `stop` is cancelled when the process is told to stop.
    pub(super) fn new(session: Session, cx: ConnectionTo<Client>, id: SessionId, stop: CancellationToken) -> Self {
        Self {
            updates: Updates::new(cx.clone(), id.clone(), session.cwd().to_path_buf()),
            session,
            cx,
            id,
            stop,
        }
    }

    /// The response to `session/prompt` once `running`, the turn, has ended: its stop reason,
    /// or its error. A compaction that found nothing old enough to summarize says so, since
    /// nothing else would show that it ran.
    pub(super) async fn answer(mut self, running: Running) -> Result<PromptResponse> {
        let reason = match running {
            Running::Task(mut run) => {
                self.read(&mut run).await?;
                stop_reason(&run.wait().await)?
            }
            Running::Compaction(mut run) => {
                self.read(&mut run).await?;
                let report = run.wait().await;
                if report.outcome == Outcome::Noop {
                    self.updates.nothing_to_compact();
                }
                compaction_stop(&report)?
            }
        };
        Ok(PromptResponse::new(reason))
    }

    /// Read `run` to its end: its events go to the client as session updates, and a call
    /// that waits for approval becomes a permission request. A process that is told to stop
    /// cancels the run, which is still read to its end.
    async fn read<T>(&mut self, run: &mut Run<T>) -> Result<()> {
        while let Some(event) = next_event(run, &self.session, &self.stop).await {
            self.updates.emit(&event);
            if let EventKind::ApprovalRequested {
                approval_id,
                tool_call_id,
                tool_name,
                args,
                ..
            } = event.event
            {
                let request = permissions::Request {
                    approval_id,
                    tool_call_id,
                    tool_name,
                    args,
                };
                permissions::ask(&self.cx, &self.id, &self.session, request)?;
            }
        }
        Ok(())
    }
}

/// What a prompt gives the model: its text, and its images fitted to the limits of the model
/// APIs. The agent advertises images, and neither audio nor embedded context, so clients send
/// nothing else besides the text and the resource links every agent takes.
pub(super) struct Prompt {
    pub(super) text: String,
    pub(super) images: Vec<Image>,
}

impl Prompt {
    /// The prompt of `blocks`, its images fitted one by one. An image that cannot be fitted
    /// fails the prompt, with words for the client that say which and why.
    pub(super) async fn of(blocks: &[ContentBlock]) -> Result<Self, String> {
        let mut fitted = Vec::new();
        for block in blocks {
            if let ContentBlock::Image(image) = block {
                let number = fitted.len() + 1;
                let image = images::fit_base64(&image.data)
                    .await
                    .map_err(|reason| format!("image {number} of the prompt cannot be used: {reason}"))?;
                fitted.push(image.image);
            }
        }
        Ok(Self {
            text: prompt_text(blocks),
            images: fitted,
        })
    }

    /// Whether the prompt has neither text nor images.
    pub(super) fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    /// The input that starts the prompt's run.
    pub(super) fn into_input(self) -> Input {
        Input::User {
            text: self.text,
            images: self.images,
            actor: None,
        }
    }
}

/// The text of a prompt: its text blocks, one per line, and a line naming each resource link.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(Cow::Borrowed(text.text.as_str())),
            ContentBlock::ResourceLink(link) => Some(Cow::Owned(format!("[resource {} ({})]", link.uri, link.name))),
            // Images go beside the text; the protocol may add block kinds, the enum is
            // non-exhaustive.
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a finished run answers `session/prompt`. Limits that end a turn have a stop reason;
/// a failed or timed-out run is a request error, with the run's message.
fn stop_reason(report: &RunReport) -> Result<StopReason> {
    match report.outcome {
        RunOutcome::Completed => Ok(StopReason::EndTurn),
        // The protocol has no reason for a cost limit; tokens are what a cost is paid for.
        RunOutcome::Truncated
        | RunOutcome::BudgetExhausted {
            budget: Budget::Tokens | Budget::Cost,
            ..
        } => Ok(StopReason::MaxTokens),
        RunOutcome::BudgetExhausted {
            budget: Budget::ModelRequests,
            ..
        } => Ok(StopReason::MaxTurnRequests),
        RunOutcome::Cancelled => Ok(StopReason::Cancelled),
        RunOutcome::Failed | RunOutcome::TimedOut => {
            Err(anyhow::Error::msg(report.error.clone().unwrap_or_else(|| {
                format!("the run ended with {}", report.outcome.name())
            })))
        }
    }
}

/// How a finished compaction answers the `/compact` that asked for it; a failed one is a
/// request error, with its message.
fn compaction_stop(report: &compaction::Report) -> Result<StopReason> {
    match report.outcome {
        Outcome::Completed | Outcome::Noop => Ok(StopReason::EndTurn),
        Outcome::Cancelled => Ok(StopReason::Cancelled),
        Outcome::Failed => Err(anyhow::Error::msg(
            report
                .error
                .clone()
                .unwrap_or_else(|| "the compaction failed".to_string()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::v1::schema::{ImageContent, ResourceLink, TextContent};
    use zx_agent::ai::Usage;

    use super::*;
    use crate::acp::protocol_error;
    use crate::test_support;

    fn report(outcome: RunOutcome, error: Option<&str>) -> RunReport {
        RunReport {
            error: error.map(str::to_string),
            ..test_support::report(outcome)
        }
    }

    /// The stop reason, or the message of the request error as the client gets it.
    fn answer(report: &RunReport) -> Result<StopReason, String> {
        stop_reason(report).map_err(|error| protocol_error(error).message)
    }

    #[test]
    fn outcomes_map_to_stop_reasons_and_failures_to_request_errors() {
        assert_eq!(answer(&report(RunOutcome::Completed, None)), Ok(StopReason::EndTurn));
        assert_eq!(answer(&report(RunOutcome::Truncated, None)), Ok(StopReason::MaxTokens));
        let exhausted = |budget, truncated| RunOutcome::BudgetExhausted { budget, truncated };
        assert_eq!(
            answer(&report(exhausted(Budget::Tokens, false), None)),
            Ok(StopReason::MaxTokens)
        );
        // Which budget stopped the turn decides, not whether its last answer was cut off.
        assert_eq!(
            answer(&report(exhausted(Budget::ModelRequests, true), None)),
            Ok(StopReason::MaxTurnRequests)
        );
        assert_eq!(answer(&report(RunOutcome::Cancelled, None)), Ok(StopReason::Cancelled));
        assert_eq!(
            answer(&report(RunOutcome::Failed, Some("boom"))),
            Err("boom".to_string())
        );
        assert_eq!(
            answer(&report(RunOutcome::TimedOut, None)),
            Err("the run ended with timed_out".to_string())
        );
    }

    #[test]
    fn a_compaction_ends_its_turn_and_a_failed_one_is_a_request_error() {
        let ended = |outcome, error: Option<&str>| {
            let report = compaction::Report {
                outcome,
                requests: 0,
                usage: Usage::default(),
                input_bytes: 0,
                output_bytes: 0,
                entry_id: None,
                error: error.map(str::to_string),
            };
            compaction_stop(&report).map_err(|error| error.to_string())
        };
        assert_eq!(ended(Outcome::Completed, None), Ok(StopReason::EndTurn));
        assert_eq!(ended(Outcome::Noop, None), Ok(StopReason::EndTurn));
        assert_eq!(ended(Outcome::Cancelled, Some("cancelled")), Ok(StopReason::Cancelled));
        assert_eq!(
            ended(Outcome::Failed, Some("invalid summary JSON")),
            Err("invalid summary JSON".to_string())
        );
    }

    #[test]
    fn a_prompt_is_the_text_of_its_text_blocks() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("one")),
            ContentBlock::Text(TextContent::new("two")),
        ];
        assert_eq!(prompt_text(&blocks), "one\ntwo");
        assert_eq!(prompt_text(&[]), "");
    }

    /// A PNG of one transparent pixel, in base64.
    const ONE_PIXEL_PNG: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[tokio::test]
    async fn a_prompt_takes_the_images_beside_its_text() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("what is this?")),
            ContentBlock::Image(ImageContent::new(ONE_PIXEL_PNG, "image/png")),
        ];
        let prompt = Prompt::of(&blocks).await.unwrap();
        assert_eq!(prompt.text, "what is this?");
        assert_eq!(prompt.images.len(), 1);
        assert_eq!(prompt.images[0].mime_type, "image/png");
        assert_eq!(prompt.images[0].data, ONE_PIXEL_PNG);

        let alone = Prompt::of(&[ContentBlock::Image(ImageContent::new(ONE_PIXEL_PNG, "image/png"))])
            .await
            .unwrap();
        assert!(!alone.is_empty(), "an image alone is a prompt");
        assert!(Prompt::of(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_image_that_cannot_be_used_fails_the_prompt_saying_which() {
        let blocks = vec![
            ContentBlock::Image(ImageContent::new(ONE_PIXEL_PNG, "image/png")),
            ContentBlock::Image(ImageContent::new("not base64!", "image/png")),
        ];
        let error = Prompt::of(&blocks).await.err().unwrap();
        assert!(
            error.starts_with("image 2 of the prompt cannot be used: the image is not valid base64"),
            "{error}"
        );
    }

    #[test]
    fn a_resource_link_in_a_prompt_reaches_the_model_as_a_line_naming_it() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("review this")),
            ContentBlock::ResourceLink(ResourceLink::new("main.rs", "file:///work/src/main.rs")),
        ];
        assert_eq!(
            prompt_text(&blocks),
            "review this\n[resource file:///work/src/main.rs (main.rs)]"
        );
    }
}
