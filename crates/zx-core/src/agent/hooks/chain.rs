//! The chain that runs the registered hooks: in stage order, each call bounded by its timeout
//! and the run's cancellation, with panics turned into failures.

use std::borrow::Cow;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use super::{
    ContextEdit, DEFAULT_HOOK_TIMEOUT, HookContext, HookError, HookFailure, HookPhase, HookResult, HookStage, Hooks,
    RequestPlan, RequestSettings, TurnDecision, TurnSummary,
};
use crate::agent::{AgentEvent, Decision, Event, EventTarget, ToolOutput};
use crate::ai::{Message, ToolCall};

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
                () = cancel.cancelled() => None,
            }
        } else {
            tokio::select! {
                biased;
                () = cancel.cancelled() => None,
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
        let name = options.name.unwrap_or_else(|| std::any::type_name::<H>().to_string());
        self.insert(name, Box::new(hook), options.timeout);
    }

    /// Register a hook an extension supplies, under the extension's name and with the default
    /// timeout. `extensions::register` has checked its stage.
    pub(crate) fn push_extension_hook(&mut self, name: &str, hook: Box<dyn Hooks>) {
        self.insert(name.to_string(), hook, HookOptions::default().timeout);
    }

    fn insert(&mut self, name: String, hook: Box<dyn Hooks>, timeout: Duration) {
        let stage = hook.stage();
        let position = self.hooks.partition_point(|entry| entry.stage <= stage);
        self.hooks.insert(
            position,
            RegisteredHook {
                name,
                stage,
                hook,
                timeout,
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

    /// Run `prepare_request` of every hook, each seeing the settings the previous ones chose.
    pub async fn prepare_request(
        &self,
        messages: &[Message],
        turn: u32,
        settings: RequestSettings,
        ctx: &HookContext,
    ) -> Result<RequestSettings, HookError> {
        let mut settings = settings;
        for entry in &self.hooks {
            let call = entry.call(HookPhase::PrepareRequest);
            let plan = RequestPlan {
                messages,
                turn,
                settings: &settings,
            };
            if let Some(next) = call
                .run(entry.hook.prepare_request(&plan, ctx), ctx.cancel.token())
                .await?
            {
                if next.model.trim().is_empty() {
                    return Err(call.error(HookFailure::Rejected("chose an empty model".into())));
                }
                settings = next;
            }
        }
        Ok(settings)
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

    /// Filter transient progress through redaction hooks without invoking final-output hooks
    /// or spilling it to storage. A failed redactor withholds the update.
    pub(crate) async fn filter_tool_progress(
        &self,
        tool: &ToolCall,
        mut output: ToolOutput,
        ctx: &HookContext,
    ) -> Result<ToolOutput, HookError> {
        for entry in &self.hooks {
            if entry.stage != HookStage::Redaction {
                continue;
            }
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
    pub(crate) async fn emit(&self, event: AgentEvent, target: EventTarget<'_>) {
        // What the event reports is already recorded; the sink sees it before observers do.
        let event = target.trace.stamp(event);
        target.sink.emit(&event);
        for error in self.on_event(&event, target.trace.context()).await {
            report_failure(error, target);
        }
    }
}

/// Report a hook failure as a `hook_failed` event. Returns its description.
pub(crate) fn report_failure(error: HookError, target: EventTarget<'_>) -> String {
    let text = error.to_string();
    // Diagnostics bypass hooks, preventing recursive observer failures.
    target.emit_unobserved(AgentEvent::HookFailed {
        hook: error.hook,
        phase: error.phase.as_str(),
        error: text.clone(),
    });
    text
}
