//! Ordered, bounded extension points. Hook failures never grant tool access.
//!
//! - `chain`: [`HookChain`], which runs the registered hooks in stage order, each call
//!   bounded by its timeout and the run's cancellation

mod chain;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::ai::{AssistantMessage, Message, Reasoning, ToolCall, UserMessageKind};
use crate::events::{Event, EventKind, HookPoint};
use crate::guard::{CancelSignal, Stopped};
use crate::input::InputQueues;
use crate::tools::{ActiveTools, ToolOutput};
use crate::transcript::context_edit_refusal;

#[cfg(test)]
use chain::HookCall;
pub use chain::{HookChain, HookOptions};

/// Default bound for each individual hook invocation.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// Callback errors are supplied as text; the chain adds the hook, phase and failure kind.
pub type HookResult<T> = Result<T, String>;

/// What every hook call knows about the run it belongs to. Fields are only ever added, so
/// the struct is `#[non_exhaustive]`: read its fields, and outside the crate build one with
/// [`HookContext::new`]. In JSON the cancellation signal, the queues and the tool set are
/// left out.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HookContext {
    /// The ID of the session the run belongs to.
    pub session_id: String,
    /// The run: one prompt or one manual compaction, as in the events' `runId`.
    pub run_id: String,
    /// The tools' working directory.
    pub cwd: PathBuf,
    /// Set when the run is cancelled; the chain then stops waiting for the hook.
    #[serde(skip)]
    pub cancel: CancelSignal,
    /// The run's queues: a hook sends the model a message with `steer_message` or
    /// `follow_up_message` (pi: `sendMessage`) and records state with `append_entry` (pi:
    /// `appendEntry`). In a context built with [`HookContext::new`] they belong to no loop.
    #[serde(skip)]
    pub queues: Arc<InputQueues>,
    /// The tools the next request offers (pi: `setActiveTools`), shared with the run.
    #[serde(skip)]
    pub active_tools: Arc<ActiveTools>,
}

impl HookContext {
    /// A context for a new run of session `session_id` with a fresh ID, cancelled with
    /// `cancel`, with queues and a tool set of its own.
    pub fn new(session_id: String, cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            session_id,
            run_id: uuid::Uuid::new_v4().to_string(),
            cwd,
            cancel: CancelSignal::new(cancel),
            queues: Arc::default(),
            active_tools: Arc::default(),
        }
    }

    /// The same context, sharing the loop's queues and tool set.
    pub(crate) fn with_shared(self, queues: Arc<InputQueues>, active_tools: Arc<ActiveTools>) -> Self {
        Self {
            queues,
            active_tools,
            ..self
        }
    }
}

/// Audit-friendly changes, applied in order against the current projection.
///
/// Indexes refer to the projection after preceding edits. System messages cannot be
/// appended, edited or omitted: the system prompt comes from the configuration, and a section
/// it does not have would be removed again when the next attempt starts. Replacing assistant
/// text must not erase tool calls. Context summaries (`UserMessageKind::Summary`) are built
/// by the session, so they can be neither edited, omitted nor appended; editing one of their
/// sources invalidates the summary instead.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "edit", rename_all = "snake_case", rename_all_fields = "camelCase")]
#[expect(
    clippy::large_enum_variant,
    reason = "an appended message is carried whole; a plan holds a few edits, and boxing the message would change the hook interface"
)]
pub enum ContextEdit {
    Append { message: Message },
    ReplaceText { index: usize, text: String },
    Omit { index: usize },
}

impl ContextEdit {
    /// Apply the edit to `messages`, or say why it is refused. The chain applies a whole
    /// plan to a copy of the context this way, before any of its edits is recorded.
    pub(crate) fn apply(&self, messages: &mut Vec<Message>) -> Result<(), String> {
        match self {
            Self::Append { message } => {
                if matches!(message, Message::System(_)) {
                    return Err("the system prompt comes from the configuration, so hooks cannot add \
                                system messages; add other context instead"
                        .into());
                }
                if is_summary(message) {
                    return Err("only the session creates context summaries".into());
                }
                messages.push(message.clone());
            }
            Self::ReplaceText { index, text } => {
                let message = editable(messages, *index, true)?;
                *message = message.with_replaced_text(text);
            }
            Self::Omit { index } => {
                editable(messages, *index, false)?;
                messages.remove(*index);
            }
        }
        Ok(())
    }
}

/// The message at `index`, if an edit may change it: it exists, is not a summary, which only
/// the session builds, and is one the session accepts an edit for ([`context_edit_refusal`]).
fn editable(messages: &mut [Message], index: usize, replaces_text: bool) -> Result<&mut Message, String> {
    let message = messages
        .get_mut(index)
        .ok_or_else(|| format!("context index {index} is out of bounds"))?;
    if is_summary(message) {
        return Err("a context summary cannot be edited or omitted; edit its source messages instead".into());
    }
    match context_edit_refusal(message, replaces_text) {
        Some(reason) => Err(format!("context message {index} {reason}")),
        None => Ok(message),
    }
}

fn is_summary(message: &Message) -> bool {
    matches!(
        message,
        Message::User {
            kind: UserMessageKind::Summary,
            ..
        }
    )
}

/// The settings of one task request that [`Hook::prepare_request`] may change: the model,
/// with the same provider, and the reasoning level. The context is not among them: what the
/// model sees is always the session projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestSettings {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
}

/// What [`Hook::prepare_request`] sees of the request about to be sent.
#[derive(Clone, Copy, Debug)]
pub struct RequestPlan<'a> {
    /// The context the request sends, as recorded in the session.
    pub messages: &'a [Message],
    pub step: u32,
    /// The settings so far: the configured ones, as changed by earlier hooks.
    pub settings: &'a RequestSettings,
}

/// What the loop does after a step if no finish hook decides otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepDisposition {
    /// Another step follows: tool results await the model, or input is queued.
    Continue,
    /// The attempt ends: the response called no tool and no input is queued.
    Stop,
    /// The attempt ends because every call of the response asked to end it
    /// ([`ToolOutput::terminate`]), and no input is queued.
    Terminated,
}

/// A successful model step, after its tool results have been processed, as
/// [`Hook::finish_step`] sees it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishedStep<'a> {
    pub response: &'a AssistantMessage,
    pub messages: &'a [Message],
    pub step: u32,
    /// Answer continuations already scheduled in this run.
    pub answer_continuations: u32,
    pub remaining_model_requests: Option<u32>,
    pub disposition: StepDisposition,
}

/// What a finish hook asks for after a step; [`Hook::finish_step`] says how the decisions
/// of several hooks combine. Input queued while the hooks ran takes precedence over
/// `ContinueAnswer` and `FollowUp`, and either of them ends the attempt instead when the
/// request budget leaves no request for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", content = "text", rename_all = "snake_case")]
pub enum StepDecision {
    /// Leave it to the loop, which does what the step's [`StepDisposition`] says.
    Default,
    /// End the attempt now, even when tool results await the model. Queued input stays
    /// queued for the next run.
    Stop,
    /// Continue the same answer, with this text as the next input; the final report joins
    /// adjacent answer fragments. Honoured only when the loop would stop
    /// ([`StepDisposition::Stop`]).
    ContinueAnswer(String),
    /// Request a new answer, with this text as the next input, without joining the preceding
    /// answer. Honoured unless another step follows anyway ([`StepDisposition::Continue`]).
    FollowUp(String),
}

/// Where a hook runs among the others. Every hook method runs the stages in this order;
/// within a stage, hooks run in the order they were registered. A hook declares its stage
/// with [`Hook::stage`], so the guarantees below hold however a host registers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookStage {
    /// Extensions and other hooks; the default.
    Extension,
    /// Removes known secrets from tool output, after every extension has changed it.
    Redaction,
    /// Saves long tool output, after redaction.
    Storage,
}

/// The callbacks an extension runs at the loop's extension points. Every method has a no-op
/// default, so implementers override only what they use.
///
/// A hook must yield while waiting: blocking work prevents cooperative timeouts. Hooks run
/// in stage order, then registration order. Operational callbacks are bounded by their
/// configured timeout and cancellation; observer failures are reported as diagnostics.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Transform prompt text before it enters the model context. Transformations chain in
    /// order. With a session, the original is recorded and a rewrite is recorded as an edit.
    /// A failure rejects the input and stops the attempt; cancellation records no input.
    async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
        Ok(text)
    }

    /// Propose ordered edits before a task request. Each hook sees preceding edits; the full
    /// plan is validated before it is applied. With a session, edits are recorded and affect
    /// the session projection. Failure stops the attempt before the request.
    async fn transform_context(&self, _messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        Ok(Vec::new())
    }

    /// Change the model or reasoning level of the next task request, for example to use a
    /// cheaper model for a simple step. The context is not among
    /// the settings: what the model sees is always the session projection. Return `None` to keep the
    /// settings. Changes chain in hook order and last for this request only; the request
    /// audit records the settings each request used. An empty model and a failure both stop
    /// the attempt before the request; summary requests do not call this hook.
    async fn prepare_request(
        &self,
        _plan: &RequestPlan<'_>,
        _ctx: &HookContext,
    ) -> HookResult<Option<RequestSettings>> {
        Ok(None)
    }

    /// Transform a tool's result before it is delivered and recorded. Results chain in hook
    /// order, including results for blocked calls. Failure withholds the raw output and yields
    /// an error result; it cannot undo effects of a tool that has already run.
    async fn after_tool(&self, _call: &ToolCall, output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        Ok(output)
    }

    /// Decide what follows a successful task response, after tool results and `StepEnd`.
    /// Stop wins; otherwise the first non-default decision wins. A failure stops the attempt.
    /// There is no corresponding decision hook at step start; observe `StepStart` in `on_event`.
    async fn finish_step(&self, _step: &FinishedStep<'_>, _ctx: &HookContext) -> HookResult<StepDecision> {
        Ok(StepDecision::Default)
    }

    /// Read-only observation of the events this hook [`observes`](Hook::observes), once
    /// what they report is recorded and they are queued for the run's reader. A hook's
    /// failure is itself an event, `hook_failed`, which the hooks observe too; a failure to
    /// observe that one is not reported again.
    ///
    /// Each call is awaited before the run goes on, in stage and registration order. While
    /// tools run in parallel, their events arrive concurrently, so calls of one hook can
    /// overlap and finish out of `seq` order. Once the run is cancelled, a call that is
    /// still pending is stopped, so an observer can miss the remaining events, `run_end`
    /// among them. An exporter that needs every event, in order, reads them from the
    /// [`Run`](crate::session::Run).
    async fn on_event(&self, _event: &Event, _ctx: &HookContext) -> HookResult<()> {
        Ok(())
    }

    /// Whether `on_event` receives `event`. Every call of `on_event` is awaited, so the
    /// default skips the frequent streaming deltas (`message_update`); override to see them.
    /// A panic here counts as not observing the event and is reported as a hook failure.
    fn observes(&self, event: &EventKind) -> bool {
        !matches!(event, EventKind::MessageUpdate { .. })
    }

    /// The stage this hook runs in. Only secret redaction and output storage, which live in
    /// the zx-tools crate, declare a stage other than [`HookStage::Extension`].
    fn stage(&self) -> HookStage {
        HookStage::Extension
    }
}

/// How one callback failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HookFailure {
    /// The callback returned this error.
    #[error("failed: {0}")]
    Rejected(String),
    /// The callback did not finish within its timeout.
    #[error("timed out after {}ms", .0.as_millis())]
    TimedOut(Duration),
    #[error("panicked")]
    Panicked,
    /// The run was cancelled while the callback ran.
    #[error("was cancelled")]
    Cancelled,
}

impl From<Stopped> for HookFailure {
    fn from(stopped: Stopped) -> Self {
        match stopped {
            Stopped::TimedOut(timeout) => Self::TimedOut(timeout),
            Stopped::Panicked => Self::Panicked,
            Stopped::Cancelled => Self::Cancelled,
        }
    }
}

/// The failure of one callback, as the chain reports it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{hook} {phase} {failure}")]
pub struct HookError {
    pub hook: String,
    pub phase: HookPoint,
    pub failure: HookFailure,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{SectionChange, StopReason};
    use serde_json::json;

    fn context() -> HookContext {
        HookContext::new("session".into(), PathBuf::from("."), CancellationToken::new())
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "echo".into(),
            arguments: json!({}),
            arguments_error: None,
        }
    }

    struct Suffix(&'static str);
    #[async_trait]
    impl Hook for Suffix {
        async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
            Ok(text + self.0)
        }
        async fn after_tool(
            &self,
            _call: &ToolCall,
            mut output: ToolOutput,
            _ctx: &HookContext,
        ) -> HookResult<ToolOutput> {
            output.content.push_str(self.0);
            Ok(output)
        }
        async fn transform_context(&self, messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
            let Message::User { content, .. } = &messages[0] else {
                panic!("user input")
            };
            Ok(vec![ContextEdit::ReplaceText {
                index: 0,
                text: content.clone() + self.0,
            }])
        }
    }

    #[tokio::test]
    async fn input_hooks_compose_in_registration_order_including_empty_unicode_input() {
        let chain = HookChain::new().with(Suffix("甲")).with(Suffix("乙"));
        assert_eq!(chain.on_input(String::new(), &context()).await.unwrap(), "甲乙");
    }

    #[tokio::test]
    async fn output_hooks_receive_the_preceding_hooks_result() {
        let chain = HookChain::new().with(Suffix("甲")).with(Suffix("乙"));
        let output = chain.after_tool(&call(), ToolOutput::ok(""), &context()).await.unwrap();
        assert_eq!(output.content, "甲乙");
    }

    #[tokio::test]
    async fn context_hooks_see_the_projection_after_preceding_edits() {
        let chain = HookChain::new().with(Suffix("甲")).with(Suffix("乙"));
        let original = vec![Message::user("")];
        let edits = chain.transform_context(&original, &context()).await.unwrap();
        let mut projected = original.clone();
        for edit in edits {
            edit.apply(&mut projected).unwrap();
        }
        assert!(matches!(&projected[0], Message::User { content, .. } if content == "甲乙"));
        assert!(matches!(&original[0], Message::User { content, .. } if content.is_empty()));
    }

    struct Finish(StepDecision);
    #[async_trait]
    impl Hook for Finish {
        async fn finish_step(&self, _step: &FinishedStep<'_>, _ctx: &HookContext) -> HookResult<StepDecision> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn finish_stop_dominates_and_otherwise_first_request_wins() {
        let response = AssistantMessage::failure("p", "m", StopReason::Stop, "unused");
        let step = FinishedStep {
            response: &response,
            messages: &[],
            step: 1,
            answer_continuations: 0,
            remaining_model_requests: None,
            disposition: StepDisposition::Stop,
        };
        let first = StepDecision::ContinueAnswer("first".into());
        let chain = HookChain::new()
            .with(Finish(first.clone()))
            .with(Finish(StepDecision::FollowUp("second".into())));
        assert_eq!(chain.finish_step(&step, &context()).await.unwrap(), first);
        let chain = chain.with(Finish(StepDecision::Stop));
        assert_eq!(chain.finish_step(&step, &context()).await.unwrap(), StepDecision::Stop);
    }

    fn guard() -> HookCall<'static> {
        HookCall {
            name: "test",
            phase: HookPoint::AfterTool,
            timeout: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn callback_errors_preserve_the_hook_and_phase() {
        let error = guard()
            .run(async { Err::<(), _>("rejected".into()) }, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            HookError {
                hook: "test".into(),
                phase: HookPoint::AfterTool,
                failure: HookFailure::Rejected("rejected".into())
            }
        );
    }

    #[tokio::test]
    async fn callback_panics_are_caught() {
        let error = guard()
            .run::<()>(async { panic!("callback bug") }, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.failure, HookFailure::Panicked);
    }

    #[tokio::test]
    async fn pending_callbacks_are_bounded_by_their_timeout() {
        let error = guard()
            .run::<()>(std::future::pending(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.failure, HookFailure::TimedOut(Duration::ZERO));
    }

    #[tokio::test]
    async fn cancellation_interrupts_callbacks_before_their_timeout() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let guard = HookCall {
            timeout: Duration::from_secs(60),
            ..guard()
        };
        let error = guard.run::<()>(std::future::pending(), &cancel).await.unwrap_err();
        assert_eq!(error.failure, HookFailure::Cancelled);
    }

    #[test]
    fn context_edits_preserve_system_history_and_tool_calls() {
        let system = Message::System(crate::ai::SystemMessage {
            sections: vec![SectionChange {
                name: "preamble".into(),
                content: Some("system".into()),
            }],
            timestamp: 0,
        });
        let mut messages = vec![system];
        assert!(ContextEdit::Omit { index: 0 }.apply(&mut messages).is_err());
        assert!(
            ContextEdit::ReplaceText {
                index: 0,
                text: "changed".into()
            }
            .apply(&mut messages)
            .is_err()
        );
        assert!(ContextEdit::Omit { index: 1 }.apply(&mut messages).is_err());
        let mut response = AssistantMessage::failure("p", "m", StopReason::ToolUse, "unused");
        response.content = vec![crate::ai::ContentBlock::ToolCall(crate::ai::ToolCall {
            id: "c".into(),
            name: "echo".into(),
            arguments: json!({}),
            arguments_error: None,
        })];
        messages.push(Message::Assistant(response));
        assert!(
            ContextEdit::ReplaceText {
                index: 1,
                text: "changed".into()
            }
            .apply(&mut messages)
            .is_err()
        );
    }

    #[test]
    fn context_summaries_cannot_be_edited_omitted_or_appended() {
        let summary = Message::User {
            content: "<context_summary></context_summary>".into(),
            timestamp: 0,
            kind: crate::ai::UserMessageKind::Summary,
        };
        let mut messages = vec![summary.clone(), Message::user("kept")];
        for edit in [
            ContextEdit::ReplaceText {
                index: 0,
                text: "changed".into(),
            },
            ContextEdit::Omit { index: 0 },
            ContextEdit::Append { message: summary },
        ] {
            let error = edit.apply(&mut messages).unwrap_err();
            assert!(error.contains("summar"), "{error}");
        }
        assert_eq!(messages.len(), 2, "rejected edits leave the projection unchanged");
        assert!(ContextEdit::Omit { index: 1 }.apply(&mut messages).is_ok());
    }
}
