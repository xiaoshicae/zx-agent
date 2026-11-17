//! The chain that runs the registered hooks: in stage order, each call bounded by its timeout
//! and the run's cancellation, with panics turned into failures.

use std::borrow::Cow;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{
    ContextEdit, DEFAULT_HOOK_TIMEOUT, FinishedStep, Hook, HookContext, HookError, HookFailure, HookPhase, HookResult,
    HookStage, RequestPlan, RequestSettings, StepDecision,
};
use crate::ai::{Message, ToolCall};
use crate::events::Event;
use crate::guard::{Priority, Stopped, bounded};
use crate::tools::ToolOutput;

/// One guarded call of a registered hook. A failure names the call by `name` and `phase`.
pub(crate) struct HookCall<'a> {
    pub(crate) name: &'a str,
    pub(crate) phase: HookPhase,
    pub(crate) timeout: Duration,
}

impl HookCall<'_> {
    fn error(&self, failure: HookFailure) -> HookError {
        HookError {
            hook: self.name.to_string(),
            phase: self.phase,
            failure,
        }
    }

    /// Wait for `future`, bounded by the timeout and by `cancel`; a rejection, a panic, a
    /// timeout and a cancellation each become a failure of their own kind.
    pub(crate) async fn run<T>(
        &self,
        future: impl Future<Output = HookResult<T>>,
        cancel: &CancellationToken,
    ) -> Result<T, HookError> {
        // Ready observers still see the final event after cancellation. Pending
        // observers stop immediately; operational hooks give cancellation priority.
        let priority = if self.phase == HookPhase::OnEvent {
            Priority::Completion
        } else {
            Priority::Cancellation
        };
        match bounded(future, self.timeout, cancel, priority).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(reason)) => Err(self.error(HookFailure::Rejected(reason))),
            Err(stopped) => Err(self.error(HookFailure::Stopped(stopped))),
        }
    }
}

#[derive(Clone)]
struct RegisteredHook {
    name: String,
    stage: HookStage,
    // Shared, not owned: the hooks of an agent serve every session it opens, and the
    // sessions run at the same time.
    hook: Arc<dyn Hook>,
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

    /// Run one callback of this hook in `phase`, bounded by the hook's timeout and the
    /// run's cancellation.
    async fn run<T>(
        &self,
        phase: HookPhase,
        future: impl Future<Output = HookResult<T>>,
        ctx: &HookContext,
    ) -> Result<T, HookError> {
        self.call(phase).run(future, ctx.cancel.token()).await
    }

    /// The error of this hook failing in `phase`.
    fn error(&self, phase: HookPhase, failure: HookFailure) -> HookError {
        self.call(phase).error(failure)
    }
}

/// A chain of hooks, kept in stage order, and borrowed by concurrent tool futures. A clone
/// is a chain of the same hooks: an agent's chain is cloned for each of its sessions, whose
/// own hooks are then added to the clone.
#[derive(Clone, Default)]
pub struct HookChain {
    hooks: Vec<RegisteredHook>,
}

impl HookChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `hook` as `name`, which `hook_failed` events and traces show, each of its
    /// calls bounded by [`DEFAULT_HOOK_TIMEOUT`]. It runs in the stage it declares, after the
    /// hooks already registered in that stage.
    pub fn push(&mut self, name: impl Into<String>, hook: Arc<dyn Hook>) {
        self.push_with_timeout(name, hook, DEFAULT_HOOK_TIMEOUT);
    }

    /// Register `hook` as [`push`](Self::push) does, with `timeout` bounding each call.
    pub fn push_with_timeout(&mut self, name: impl Into<String>, hook: Arc<dyn Hook>, timeout: Duration) {
        self.insert(RegisteredHook {
            name: name.into(),
            stage: hook.stage(),
            hook,
            timeout,
        });
    }

    /// Register the hooks of `other`, each after the hooks this chain already has in its
    /// stage, with the name and the timeout it was registered with.
    pub(crate) fn extend(&mut self, other: HookChain) {
        for entry in other.hooks {
            self.insert(entry);
        }
    }

    fn insert(&mut self, entry: RegisteredHook) {
        let position = self.hooks.partition_point(|existing| existing.stage <= entry.stage);
        self.hooks.insert(position, entry);
    }

    /// Whether no hook is registered: nothing then observes the events.
    pub(crate) fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Run `on_input` of every hook, each seeing the text the previous ones returned.
    pub(crate) async fn on_input(&self, mut text: String, ctx: &HookContext) -> Result<String, HookError> {
        for entry in &self.hooks {
            text = entry
                .run(HookPhase::OnInput, entry.hook.on_input(text, ctx), ctx)
                .await?;
        }
        Ok(text)
    }

    /// The context edits of every hook, each hook seeing the projection after the edits of
    /// the ones before it. An edit that does not apply fails the hook that planned it.
    pub(crate) async fn transform_context(
        &self,
        messages: &[Message],
        ctx: &HookContext,
    ) -> Result<Vec<ContextEdit>, HookError> {
        // A separate projection validates the complete ordered plan before any write.
        let mut projected = Cow::Borrowed(messages);
        let mut edits = Vec::new();
        for entry in &self.hooks {
            let phase = HookPhase::TransformContext;
            let changes = entry
                .run(phase, entry.hook.transform_context(&projected, ctx), ctx)
                .await?;
            for change in &changes {
                change
                    .apply(projected.to_mut())
                    .map_err(|reason| entry.error(phase, HookFailure::Rejected(reason)))?;
            }
            edits.extend(changes);
        }
        Ok(edits)
    }

    /// Run `prepare_request` of every hook, each seeing the settings the previous ones chose.
    pub(crate) async fn prepare_request(
        &self,
        messages: &[Message],
        step: u32,
        settings: RequestSettings,
        ctx: &HookContext,
    ) -> Result<RequestSettings, HookError> {
        let mut settings = settings;
        for entry in &self.hooks {
            let phase = HookPhase::PrepareRequest;
            let plan = RequestPlan {
                messages,
                step,
                settings: &settings,
            };
            if let Some(next) = entry.run(phase, entry.hook.prepare_request(&plan, ctx), ctx).await? {
                if next.model.trim().is_empty() {
                    return Err(entry.error(phase, HookFailure::Rejected("chose an empty model".into())));
                }
                settings = next;
            }
        }
        Ok(settings)
    }

    /// Run `after_tool` of every hook, each seeing the output the previous ones returned.
    pub(crate) async fn after_tool(
        &self,
        call: &ToolCall,
        output: ToolOutput,
        ctx: &HookContext,
    ) -> Result<ToolOutput, HookError> {
        self.after_tool_in(|_| true, call, output, ctx).await
    }

    /// Filter transient progress through redaction hooks without invoking final-output hooks
    /// or spilling it to storage. A failed redactor withholds the update.
    pub(crate) async fn filter_tool_progress(
        &self,
        call: &ToolCall,
        output: ToolOutput,
        ctx: &HookContext,
    ) -> Result<ToolOutput, HookError> {
        self.after_tool_in(|stage| stage == HookStage::Redaction, call, output, ctx)
            .await
    }

    /// Run `after_tool` of the hooks in the stages `selected` accepts, in order, each
    /// seeing the preceding hook's output.
    async fn after_tool_in(
        &self,
        selected: impl Fn(HookStage) -> bool,
        call: &ToolCall,
        mut output: ToolOutput,
        ctx: &HookContext,
    ) -> Result<ToolOutput, HookError> {
        for entry in self.hooks.iter().filter(|entry| selected(entry.stage)) {
            output = entry
                .run(HookPhase::AfterTool, entry.hook.after_tool(call, output, ctx), ctx)
                .await?;
        }
        Ok(output)
    }

    /// What follows a step: the first hook that stops ends the chain; otherwise the first
    /// that asks to continue the answer or to follow up decides.
    pub(crate) async fn finish_step(
        &self,
        step: &FinishedStep<'_>,
        ctx: &HookContext,
    ) -> Result<StepDecision, HookError> {
        let mut decision = StepDecision::Default;
        for entry in &self.hooks {
            let next = entry
                .run(HookPhase::FinishStep, entry.hook.finish_step(step, ctx), ctx)
                .await?;
            match next {
                StepDecision::Stop => return Ok(StepDecision::Stop),
                StepDecision::Default => {}
                StepDecision::ContinueAnswer(_) | StepDecision::FollowUp(_) => {
                    if decision == StepDecision::Default {
                        decision = next;
                    }
                }
            }
        }
        Ok(decision)
    }

    /// Call `on_event` of every hook that observes `event`, in stage and registration order.
    pub(crate) async fn on_event(&self, event: &Event, ctx: &HookContext) -> Vec<HookError> {
        let mut errors = Vec::new();
        for entry in &self.hooks {
            // Synchronous extension code: a panic is a failure of this hook, not of the run.
            match std::panic::catch_unwind(AssertUnwindSafe(|| entry.hook.observes(&event.event))) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => {
                    errors.push(entry.error(HookPhase::OnEvent, HookFailure::Stopped(Stopped::Panicked)));
                    continue;
                }
            }
            if let Err(error) = entry
                .run(HookPhase::OnEvent, entry.hook.on_event(event, ctx), ctx)
                .await
            {
                // Cancellation is a normal shutdown, not an observer failure.
                if error.failure != HookFailure::Stopped(Stopped::Cancelled) {
                    errors.push(error);
                }
            }
        }
        errors
    }
}
