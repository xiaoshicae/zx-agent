//! Ordered, bounded extension points. Hook failures never grant tool access.

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use super::{AgentEvent, Decision, Emit, Event, ToolOutput};
use crate::ai::{AssistantMessage, Message, ToolCall, UserMessageKind};

/// Default bound for each individual hook invocation.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// Callback errors are supplied as text; the host adds the hook, phase and failure kind.
pub type HookResult<T> = Result<T, String>;

/// What every hook call knows about the run it belongs to. Fields are only ever added, so
/// the struct is `#[non_exhaustive]`: read its fields, and outside the crate build one with
/// [`HookContext::new`]. In JSON the cancellation signal is left out.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HookContext {
    /// The session's ID; absent when `run_loop` runs without a session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The run: one prompt or one manual compaction, as in the events' `runId`.
    pub run_id: String,
    /// The tools' working directory.
    pub cwd: PathBuf,
    /// Set when the run is cancelled; the host then stops waiting for the hook.
    #[serde(skip)]
    pub cancel: CancelSignal,
}

impl HookContext {
    /// A context for a new run with a fresh ID, cancelled with `cancel`.
    pub fn new(session_id: Option<String>, cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            session_id,
            run_id: uuid::Uuid::new_v4().to_string(),
            cwd,
            cancel: CancelSignal(cancel),
        }
    }
}

/// A run's cancellation as a hook sees it: a hook can wait for it, check it, and derive a
/// token for work of its own, but cannot cancel the run, which only its caller does.
///
/// ```compile_fail
/// fn stop(ctx: &zx_agent::agent::HookContext) {
///     ctx.cancel.cancel();
/// }
/// ```
#[derive(Clone, Debug)]
pub struct CancelSignal(CancellationToken);

impl CancelSignal {
    /// Whether the run has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Completes once the run is cancelled.
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.0.cancelled()
    }

    /// A token cancelled with the run, for work of the hook's own; cancelling it stops only
    /// that work.
    pub fn child_token(&self) -> CancellationToken {
        self.0.child_token()
    }

    /// The run's own token, for the host that waits on hooks.
    pub(crate) fn token(&self) -> &CancellationToken {
        &self.0
    }
}

const SYSTEM_APPEND: &str =
    "the system prompt comes from the configuration, so hooks cannot add system messages; add other context instead";

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
pub enum ContextEdit {
    Append { message: Message },
    ReplaceText { index: usize, text: String },
    Omit { index: usize },
}

const SUMMARY_EDIT: &str = "a context summary cannot be edited or omitted; edit its source messages instead";

impl ContextEdit {
    pub(crate) fn apply(&self, messages: &mut Vec<Message>) -> Result<(), String> {
        match self {
            Self::Append { message } => {
                if matches!(message, Message::System(_)) {
                    return Err(SYSTEM_APPEND.into());
                }
                if is_summary(message) {
                    return Err("only the session creates context summaries".into());
                }
                messages.push(message.clone());
            }
            Self::ReplaceText { index, text } => {
                let message = messages
                    .get_mut(*index)
                    .ok_or_else(|| format!("context index {index} is out of bounds"))?;
                match message {
                    Message::System(_) => return Err("append system prompt changes instead of editing them".into()),
                    Message::User {
                        kind: UserMessageKind::Summary,
                        ..
                    } => return Err(SUMMARY_EDIT.into()),
                    Message::Assistant(assistant) if !assistant.tool_calls().is_empty() => {
                        return Err("text replacement cannot erase assistant tool calls".into());
                    }
                    Message::User { .. } | Message::Assistant(_) | Message::ToolResult(_) => {}
                }
                *message = message.with_replaced_text(text);
            }
            Self::Omit { index } => {
                let message = messages
                    .get(*index)
                    .ok_or_else(|| format!("context index {index} is out of bounds"))?;
                if matches!(message, Message::System(_)) {
                    return Err("system prompt history cannot be omitted".into());
                }
                if is_summary(message) {
                    return Err(SUMMARY_EDIT.into());
                }
                messages.remove(*index);
            }
        }
        Ok(())
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

/// Whether the normal loop already has work after this response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnDisposition {
    Continue,
    Stop,
    Terminated,
}

/// A successful model turn, after tool results have been processed.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSummary<'a> {
    pub response: &'a AssistantMessage,
    pub messages: &'a [Message],
    pub turn: u32,
    pub continuations: u32,
    pub remaining_turns: Option<u32>,
    pub disposition: TurnDisposition,
}

/// Stop dominates; otherwise the first non-default decision wins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", content = "text", rename_all = "snake_case")]
pub enum TurnDecision {
    Default,
    Stop,
    /// Continue the same answer; the final report joins adjacent answer fragments.
    ContinueAnswer(String),
    /// Request a new answer without joining the preceding answer.
    FollowUp(String),
}

/// Where a hook runs among the others. Every hook method runs the stages in this order;
/// within a stage, hooks run in the order they were registered. A hook declares its stage
/// with [`Hooks::stage`], so the guarantees below hold however a host registers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookStage {
    /// Built-in tool policies: they decide before any extension sees a call.
    Policy,
    /// Extensions and other hooks; the default.
    Extension,
    /// Removes known secrets from tool output, after every extension has changed it.
    Redaction,
    /// Saves long tool output, after redaction.
    Storage,
}

/// Hooks must yield while waiting. Blocking work prevents cooperative timeouts.
/// Every method has a no-op default so implementers override only what they use.
#[async_trait]
pub trait Hooks: Send + Sync {
    async fn on_input(&self, text: String, _ctx: &HookContext) -> HookResult<String> {
        Ok(text)
    }

    async fn transform_context(&self, _messages: &[Message], _ctx: &HookContext) -> HookResult<Vec<ContextEdit>> {
        Ok(Vec::new())
    }

    async fn before_tool(&self, _call: &ToolCall, _ctx: &HookContext) -> HookResult<Decision> {
        Ok(Decision::Allow)
    }

    async fn after_tool(&self, _call: &ToolCall, output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        Ok(output)
    }

    async fn finish_turn(&self, _turn: &TurnSummary<'_>, _ctx: &HookContext) -> HookResult<TurnDecision> {
        Ok(TurnDecision::Default)
    }

    /// Read-only observation of the events this hook [`observes`](Hooks::observes), after
    /// persistence and event delivery. Diagnostic events reporting observer errors are not
    /// sent back through this method.
    ///
    /// Each call is awaited before the run goes on, in stage and registration order. While
    /// tools run in parallel, their events arrive concurrently, so calls of one hook can
    /// overlap and finish out of `seq` order. Once the run is cancelled, a call that is
    /// still pending is stopped, so an observer can miss the remaining events, `run_end`
    /// among them. An exporter that needs every event, in order, uses
    /// [`AgentSession::subscribe_bounded`](crate::AgentSession::subscribe_bounded).
    async fn on_event(&self, _event: &Event, _ctx: &HookContext) -> HookResult<()> {
        Ok(())
    }

    /// Whether `on_event` receives `event`. Every call of `on_event` is awaited, so the
    /// default skips the frequent streaming deltas (`message_update`); override to see them.
    /// A panic here counts as not observing the event and is reported as a hook failure.
    fn observes(&self, event: &AgentEvent) -> bool {
        !matches!(event, AgentEvent::MessageUpdate { .. })
    }

    /// The stage this hook runs in. Only the kernel's tool policy adapter, secret redaction
    /// and output storage declare a stage other than [`HookStage::Extension`].
    fn stage(&self) -> HookStage {
        HookStage::Extension
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookPhase {
    OnInput,
    TransformContext,
    BeforeTool,
    AfterTool,
    FinishTurn,
    OnEvent,
    Approval,
}

impl HookPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnInput => "on_input",
            Self::TransformContext => "transform_context",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::FinishTurn => "finish_turn",
            Self::OnEvent => "on_event",
            Self::Approval => "approval",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookFailure {
    Rejected(String),
    TimedOut(Duration),
    Panicked,
    Cancelled,
}

/// Structured host-side failure for one callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookError {
    pub hook: String,
    pub phase: HookPhase,
    pub failure: HookFailure,
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ", self.hook, self.phase.as_str())?;
        match &self.failure {
            HookFailure::Rejected(reason) => write!(f, "failed: {reason}"),
            HookFailure::TimedOut(timeout) => write!(f, "timed out after {}ms", timeout.as_millis()),
            HookFailure::Panicked => f.write_str("panicked"),
            HookFailure::Cancelled => f.write_str("was cancelled"),
        }
    }
}

impl std::error::Error for HookError {}

pub(crate) struct HookCall<'a> {
    pub name: &'a str,
    pub phase: HookPhase,
    pub timeout: Duration,
}

impl HookCall<'_> {
    fn error(&self, failure: HookFailure) -> HookError {
        HookError {
            hook: self.name.to_string(),
            phase: self.phase,
            failure,
        }
    }

    pub(crate) async fn run<T>(
        &self,
        future: impl Future<Output = HookResult<T>>,
        cancel: &CancellationToken,
    ) -> Result<T, HookError> {
        let guarded = tokio::time::timeout(self.timeout, AssertUnwindSafe(future).catch_unwind());
        tokio::pin!(guarded);
        // Ready observers still see the final event after cancellation. Pending
        // observers stop immediately; operational hooks give cancellation priority.
        let result = if self.phase == HookPhase::OnEvent {
            tokio::select! {
                biased;
                result = &mut guarded => Some(result),
                _ = cancel.cancelled() => None,
            }
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                result = &mut guarded => Some(result),
            }
        };
        match result {
            Some(Ok(Ok(Ok(value)))) => Ok(value),
            Some(Ok(Ok(Err(reason)))) => Err(self.error(HookFailure::Rejected(reason))),
            Some(Ok(Err(_))) => Err(self.error(HookFailure::Panicked)),
            Some(Err(_)) => Err(self.error(HookFailure::TimedOut(self.timeout))),
            None => Err(self.error(HookFailure::Cancelled)),
        }
    }
}

/// How a hook is registered. Build it from `Default` with struct update syntax.
#[derive(Clone, Debug)]
pub struct HookOptions {
    /// The hook's name in `hook_failed` events and traces; `None` uses its type name.
    pub name: Option<String>,
    /// Bound for each call of the hook.
    pub timeout: Duration,
}

impl Default for HookOptions {
    fn default() -> Self {
        Self {
            name: None,
            timeout: DEFAULT_HOOK_TIMEOUT,
        }
    }
}

struct RegisteredHook {
    name: String,
    stage: HookStage,
    hook: Box<dyn Hooks>,
    timeout: Duration,
}

impl RegisteredHook {
    fn call(&self, phase: HookPhase) -> HookCall<'_> {
        HookCall {
            name: &self.name,
            phase,
            timeout: self.timeout,
        }
    }
}

/// A chain owns its hooks, kept in stage order, and is borrowed by concurrent tool futures.
#[derive(Default)]
pub struct HookChain {
    hooks: Vec<RegisteredHook>,
}

impl HookChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an owned hook under its type name, with the default per-call timeout.
    pub fn push(&mut self, hook: impl Hooks + 'static) {
        self.push_with(hook, HookOptions::default());
    }

    /// Register an owned hook. It runs in the stage it declares, after the hooks already
    /// registered in that stage.
    pub fn push_with<H: Hooks + 'static>(&mut self, hook: H, options: HookOptions) {
        let stage = hook.stage();
        let name = options.name.unwrap_or_else(|| std::any::type_name::<H>().to_string());
        let position = self.hooks.partition_point(|entry| entry.stage <= stage);
        self.hooks.insert(
            position,
            RegisteredHook {
                name,
                stage,
                hook: Box::new(hook),
                timeout: options.timeout,
            },
        );
    }

    pub fn with(mut self, hook: impl Hooks + 'static) -> Self {
        self.push(hook);
        self
    }

    pub async fn on_input(&self, mut text: String, ctx: &HookContext) -> Result<String, HookError> {
        for entry in &self.hooks {
            text = entry
                .call(HookPhase::OnInput)
                .run(entry.hook.on_input(text, ctx), ctx.cancel.token())
                .await?;
        }
        Ok(text)
    }

    pub async fn transform_context(
        &self,
        messages: &[Message],
        ctx: &HookContext,
    ) -> Result<Vec<ContextEdit>, HookError> {
        // A separate projection validates the complete ordered plan before any write.
        let mut projected = Cow::Borrowed(messages);
        let mut edits = Vec::new();
        for entry in &self.hooks {
            let call = entry.call(HookPhase::TransformContext);
            let changes = call
                .run(entry.hook.transform_context(&projected, ctx), ctx.cancel.token())
                .await?;
            for change in &changes {
                change
                    .apply(projected.to_mut())
                    .map_err(|reason| call.error(HookFailure::Rejected(reason)))?;
            }
            edits.extend(changes);
        }
        Ok(edits)
    }

    pub async fn before_tool(&self, tool: &ToolCall, ctx: &HookContext) -> Result<Decision, HookError> {
        let mut verdict = Decision::Allow;
        for entry in &self.hooks {
            let decision = entry
                .call(HookPhase::BeforeTool)
                .run(entry.hook.before_tool(tool, ctx), ctx.cancel.token())
                .await?;
            match decision {
                Decision::Deny(_) => return Ok(decision),
                Decision::Ask(_) if matches!(verdict, Decision::Allow) => verdict = decision,
                Decision::Ask(_) | Decision::Allow => {}
            }
        }
        Ok(verdict)
    }

    pub async fn after_tool(
        &self,
        tool: &ToolCall,
        mut output: ToolOutput,
        ctx: &HookContext,
    ) -> Result<ToolOutput, HookError> {
        for entry in &self.hooks {
            output = entry
                .call(HookPhase::AfterTool)
                .run(entry.hook.after_tool(tool, output, ctx), ctx.cancel.token())
                .await?;
        }
        Ok(output)
    }

    pub async fn finish_turn(&self, turn: &TurnSummary<'_>, ctx: &HookContext) -> Result<TurnDecision, HookError> {
        let mut decision = TurnDecision::Default;
        for entry in &self.hooks {
            let next = entry
                .call(HookPhase::FinishTurn)
                .run(entry.hook.finish_turn(turn, ctx), ctx.cancel.token())
                .await?;
            match next {
                TurnDecision::Stop => return Ok(TurnDecision::Stop),
                TurnDecision::Default => {}
                TurnDecision::ContinueAnswer(_) | TurnDecision::FollowUp(_) => {
                    if decision == TurnDecision::Default {
                        decision = next;
                    }
                }
            }
        }
        Ok(decision)
    }

    /// Call `on_event` of every hook that observes `event`, in stage and registration order.
    pub async fn on_event(&self, event: &Event, ctx: &HookContext) -> Vec<HookError> {
        let mut errors = Vec::new();
        for entry in &self.hooks {
            // Synchronous extension code: a panic is a failure of this hook, not of the run.
            match std::panic::catch_unwind(AssertUnwindSafe(|| entry.hook.observes(&event.event))) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => {
                    errors.push(entry.call(HookPhase::OnEvent).error(HookFailure::Panicked));
                    continue;
                }
            }
            if let Err(error) = entry
                .call(HookPhase::OnEvent)
                .run(entry.hook.on_event(event, ctx), ctx.cancel.token())
                .await
            {
                // Cancellation is a normal shutdown, not an observer failure.
                if error.failure != HookFailure::Cancelled {
                    errors.push(error);
                }
            }
        }
        errors
    }

    /// Stamp `event`, hand it to the sink, then to the observing hooks.
    pub(crate) async fn emit(&self, event: AgentEvent, to: Emit<'_>) {
        // What the event reports is already recorded; the sink sees it before observers do.
        let event = to.trace.stamp(event);
        to.sink.emit(&event);
        for error in self.on_event(&event, to.trace.context()).await {
            report_failure(error, to);
        }
    }
}

pub(crate) fn report_failure(error: HookError, to: Emit<'_>) -> String {
    let text = error.to_string();
    // Diagnostics bypass hooks, preventing recursive observer failures.
    to.direct(AgentEvent::HookFailed {
        hook: error.hook,
        phase: error.phase.as_str(),
        error: text.clone(),
    });
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{SectionChange, StopReason};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn context() -> HookContext {
        HookContext::new(None, PathBuf::from("."), CancellationToken::new())
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "echo".into(),
            arguments: json!({}),
            arguments_error: None,
        }
    }

    struct Verdict {
        decision: Decision,
        index: usize,
        seen: Arc<Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl Hooks for Verdict {
        async fn before_tool(&self, _call: &ToolCall, _ctx: &HookContext) -> HookResult<Decision> {
            self.seen.lock().unwrap().push(self.index);
            Ok(self.decision.clone())
        }
    }

    #[tokio::test]
    async fn before_tool_uses_the_strictest_verdict_in_registration_order() {
        for (decisions, expected, visited) in [
            (
                vec![
                    Decision::Allow,
                    Decision::Ask("first".into()),
                    Decision::Ask("second".into()),
                ],
                Decision::Ask("first".into()),
                vec![0, 1, 2],
            ),
            (
                vec![
                    Decision::Ask("ask".into()),
                    Decision::Allow,
                    Decision::Deny("deny".into()),
                ],
                Decision::Deny("deny".into()),
                vec![0, 1, 2],
            ),
            (
                vec![Decision::Deny("first".into()), Decision::Allow],
                Decision::Deny("first".into()),
                vec![0],
            ),
        ] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut chain = HookChain::new();
            for (index, decision) in decisions.into_iter().enumerate() {
                chain.push(Verdict {
                    decision,
                    index,
                    seen: seen.clone(),
                });
            }
            assert_eq!(chain.before_tool(&call(), &context()).await.unwrap(), expected);
            assert_eq!(*seen.lock().unwrap(), visited);
        }
    }

    struct Suffix(&'static str);
    #[async_trait]
    impl Hooks for Suffix {
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

    struct Finish(TurnDecision);
    #[async_trait]
    impl Hooks for Finish {
        async fn finish_turn(&self, _turn: &TurnSummary<'_>, _ctx: &HookContext) -> HookResult<TurnDecision> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn finish_stop_dominates_and_otherwise_first_request_wins() {
        let response = AssistantMessage::failure("p", "m", StopReason::Stop, "unused");
        let turn = TurnSummary {
            response: &response,
            messages: &[],
            turn: 1,
            continuations: 0,
            remaining_turns: None,
            disposition: TurnDisposition::Stop,
        };
        let first = TurnDecision::ContinueAnswer("first".into());
        let chain = HookChain::new()
            .with(Finish(first.clone()))
            .with(Finish(TurnDecision::FollowUp("second".into())));
        assert_eq!(chain.finish_turn(&turn, &context()).await.unwrap(), first);
        let chain = chain.with(Finish(TurnDecision::Stop));
        assert_eq!(chain.finish_turn(&turn, &context()).await.unwrap(), TurnDecision::Stop);
    }

    fn guard() -> HookCall<'static> {
        HookCall {
            name: "test",
            phase: HookPhase::BeforeTool,
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
                phase: HookPhase::BeforeTool,
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
        response.content = vec![crate::ai::ContentBlock::ToolCall {
            id: "c".into(),
            name: "echo".into(),
            arguments: json!({}),
            arguments_error: None,
        }];
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
