//! Ordered, bounded extension points around the loop. Hooks observe and transform; only
//! policies decide whether a tool call runs, so no hook, failing or not, lets one run.
//!
//! - `chain`: [`HookChain`], which runs the registered hooks in stage order, each call
//!   bounded by its timeout and the run's cancellation

mod chain;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub use self::chain::HookChain;
use crate::ai::{AssistantMessage, Message, Reasoning, ToolCall, UserMessageKind};
use crate::events::{Event, EventKind, HookPhase};
use crate::guard::{CancelSignal, Stopped};
use crate::input::InputQueues;
use crate::tools::{ActiveTools, ToolOutput};
use crate::transcript::{context_edit_refusal, replace_text};

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
    /// The session's labels: what its host said about it when it was created, such as its
    /// tenant or the chat it belongs to. Left out of the JSON when there are none.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Who sent the input the run is answering: the actor of the latest prompt of the
    /// session ([`Input::User`](crate::input::Input::User)); `None` when the host named
    /// nobody, and after a `finish_step` hook asked for a follow-up, which nobody sent. In
    /// `on_input` it is the sender of that input. Left out of the JSON when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Set when the run is cancelled; the chain then stops waiting for the hook.
    #[serde(skip)]
    pub cancel: CancelSignal,
    /// The run's queues: a hook sends the model a message with `steer` or `follow_up` (pi:
    /// `sendMessage`) and records state with `append_custom_entry` (pi:
    /// `appendEntry`). In a context built with [`HookContext::new`] they belong to no loop.
    #[serde(skip)]
    pub queues: Arc<InputQueues>,
    /// The tools the next request offers (pi: `setActiveTools`), shared with the run.
    #[serde(skip)]
    pub active_tools: Arc<ActiveTools>,
}

impl HookContext {
    /// A context for a new run of session `session_id` with a fresh ID, cancelled with
    /// `cancel`, with queues and a tool set of its own, no labels and no actor.
    pub fn new(session_id: String, cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            session_id,
            run_id: uuid::Uuid::new_v4().to_string(),
            cwd,
            labels: BTreeMap::new(),
            actor: None,
            cancel: CancelSignal::new(cancel),
            queues: Arc::default(),
            active_tools: Arc::default(),
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
                if message.user_kind() == Some(UserMessageKind::Summary) {
                    return Err("only the session creates context summaries".into());
                }
                messages.push(message.clone());
            }
            Self::ReplaceText { index, text } => replace_text(editable(messages, *index, Some(text))?, text),
            Self::Omit { index } => {
                editable(messages, *index, None)?;
                messages.remove(*index);
            }
        }
        Ok(())
    }
}

/// The message at `index`, if an edit may change it to `replacement`, or omit it when that is
/// `None`: it exists, is not a summary, which only the session builds, and is one the session
/// accepts the edit for ([`context_edit_refusal`]).
fn editable<'m>(
    messages: &'m mut [Message],
    index: usize,
    replacement: Option<&str>,
) -> Result<&'m mut Message, String> {
    let message = messages.get_mut(index).ok_or_else(|| out_of_bounds(index))?;
    if message.user_kind() == Some(UserMessageKind::Summary) {
        return Err("a context summary cannot be edited or omitted; edit its source messages instead".into());
    }
    match context_edit_refusal(message, replacement) {
        Some(reason) => Err(format!("context message {index} {reason}")),
        None => Ok(message),
    }
}

/// Why an edit of the context at `index` cannot be made: the context has no such message.
pub(crate) fn out_of_bounds(index: usize) -> String {
    format!("context index {index} is out of bounds")
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
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
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
    /// Model requests the run may still make; `None` is unlimited. A decision that needs a
    /// request no budget leaves ends the run as out of that budget.
    pub remaining_model_requests: Option<u32>,
    pub disposition: StepDisposition,
}

/// What a finish hook asks for after a step; [`Hook::finish_step`] says how the decisions
/// of several hooks combine. Input queued while the hooks ran takes precedence over
/// `ContinueAnswer` and `FollowUp`, and either of them ends the attempt instead when a
/// budget leaves no room for the request it needs.
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
    /// Every hook that declares no other stage: the host's own and every extension's.
    Default,
    /// Removes known secrets from tool output, after every hook of the default stage has
    /// changed it.
    Redaction,
    /// Saves long tool output, after redaction.
    Storage,
}

impl HookStage {
    /// The stage as messages name it: `default`, `redaction` or `storage`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Redaction => "redaction",
            Self::Storage => "storage",
        }
    }
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
    /// order. The original is recorded, and a rewrite is recorded as an edit of it.
    /// A failure rejects the input and stops the attempt; cancellation records no input.
    async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
        Ok(text)
    }

    /// Propose ordered edits before a task request. Each hook sees preceding edits; the full
    /// plan is validated before it is applied. Edits are recorded and change the session's
    /// projection. Called again when the step's preparation compacted the context, before
    /// the same request: a hook that appends a message on every call appends it each time.
    /// Failure stops the attempt before the request.
    async fn transform_context(&self, _messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        Ok(Vec::new())
    }

    /// Change the model or reasoning level of the next task request ([`RequestSettings`]),
    /// for example to use a cheaper model for a simple step. Return `None` to keep the
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
    /// an error result; it cannot undo effects of a tool that has already run. Not called for
    /// a call that the run's cancellation kept from starting or interrupted: its result is
    /// the kernel's own words.
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
    /// the zx-builtin crate, declare a stage other than [`HookStage::Default`].
    fn stage(&self) -> HookStage {
        HookStage::Default
    }
}

/// How one callback failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HookFailure {
    /// The callback returned this error.
    #[error("failed: {0}")]
    Rejected(String),
    /// The callback gave no value: it timed out or panicked, or the run was cancelled while
    /// it ran.
    #[error("{0}")]
    Stopped(Stopped),
}

/// The failure of one callback, as the chain reports it in a `hook_failed` event.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{hook} {phase} {failure}")]
pub(crate) struct HookError {
    pub(crate) hook: String,
    pub(crate) phase: HookPhase,
    pub(crate) failure: HookFailure,
}

impl HookError {
    /// Whether the callback gave no value because the run was cancelled. That is a normal
    /// shutdown: it ends what the hook was asked for, and nobody reports it as the hook's
    /// failure.
    pub(crate) fn cancelled(&self) -> bool {
        self.failure == HookFailure::Stopped(Stopped::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ai::{ContentBlock, SectionChange, StopReason, SystemMessage};

    #[test]
    fn context_edits_preserve_system_history_and_tool_calls() {
        let system = Message::System(SystemMessage {
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
        let response = AssistantMessage::new(
            "p",
            "m",
            StopReason::ToolUse,
            vec![ContentBlock::ToolCall(ToolCall {
                id: "c".into(),
                name: "echo".into(),
                arguments: json!({}),
                arguments_error: None,
            })],
        );
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
            images: Vec::new(),
            timestamp: 0,
            kind: UserMessageKind::Summary,
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
