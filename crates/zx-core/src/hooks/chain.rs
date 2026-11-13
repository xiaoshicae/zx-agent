//! The chain that runs the registered hooks: in stage order, each call bounded by its timeout
//! and the run's cancellation, with panics turned into failures.

use std::borrow::Cow;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{
    ContextEdit, DEFAULT_HOOK_TIMEOUT, FinishedStep, Hook, HookContext, HookError, HookFailure, HookPoint, HookResult,
    HookStage, RequestPlan, RequestSettings, StepDecision,
};
use crate::ai::{Message, ToolCall};
use crate::events::Event;
use crate::guard::{Priority, bounded};
use crate::tools::ToolOutput;

/// One guarded call of a registered hook. A failure names the call by `name` and `phase`.
pub(crate) struct HookCall<'a> {
    pub(crate) name: &'a str,
    pub(crate) phase: HookPoint,
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
        let priority = if self.phase == HookPoint::OnEvent {
            Priority::Completion
        } else {
            Priority::Cancellation
        };
        match bounded(future, self.timeout, cancel, priority).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(reason)) => Err(self.error(HookFailure::Rejected(reason))),
            Err(stopped) => Err(self.error(stopped.into())),
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
    hook: Box<dyn Hook>,
    timeout: Duration,
}

impl RegisteredHook {
    fn call(&self, phase: HookPoint) -> HookCall<'_> {
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
        phase: HookPoint,
        future: impl Future<Output = HookResult<T>>,
        ctx: &HookContext,
    ) -> Result<T, HookError> {
        self.call(phase).run(future, ctx.cancel.token()).await
    }

    /// The error of this hook failing in `phase`.
    fn error(&self, phase: HookPoint, failure: HookFailure) -> HookError {
        self.call(phase).error(failure)
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
    pub fn push(&mut self, hook: impl Hook + 'static) {
        self.push_with(hook, HookOptions::default());
    }

    /// Register an owned hook. It runs in the stage it declares, after the hooks already
    /// registered in that stage.
    pub fn push_with<H: Hook + 'static>(&mut self, hook: H, options: HookOptions) {
        let name = options.name.unwrap_or_else(|| std::any::type_name::<H>().to_string());
        self.insert(name, Box::new(hook), options.timeout);
    }

    /// Register a hook an extension supplies, under the extension's name and with the default
    /// timeout. `extensions::register` has checked its stage.
    pub(crate) fn push_extension_hook(&mut self, name: &str, hook: Box<dyn Hook>) {
        self.insert(name.to_string(), hook, HookOptions::default().timeout);
    }

    fn insert(&mut self, name: String, hook: Box<dyn Hook>, timeout: Duration) {
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

    pub fn with(mut self, hook: impl Hook + 'static) -> Self {
        self.push(hook);
        self
    }

    /// Whether no hook is registered: nothing then observes the events.
    pub(crate) fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub async fn on_input(&self, mut text: String, ctx: &HookContext) -> Result<String, HookError> {
        for entry in &self.hooks {
            text = entry
                .run(HookPoint::OnInput, entry.hook.on_input(text, ctx), ctx)
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
            let phase = HookPoint::TransformContext;
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
    pub async fn prepare_request(
        &self,
        messages: &[Message],
        step: u32,
        settings: RequestSettings,
        ctx: &HookContext,
    ) -> Result<RequestSettings, HookError> {
        let mut settings = settings;
        for entry in &self.hooks {
            let phase = HookPoint::PrepareRequest;
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

    pub async fn after_tool(
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
                .run(HookPoint::AfterTool, entry.hook.after_tool(call, output, ctx), ctx)
                .await?;
        }
        Ok(output)
    }

    pub async fn finish_step(&self, step: &FinishedStep<'_>, ctx: &HookContext) -> Result<StepDecision, HookError> {
        let mut decision = StepDecision::Default;
        for entry in &self.hooks {
            let next = entry
                .run(HookPoint::FinishStep, entry.hook.finish_step(step, ctx), ctx)
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
    pub async fn on_event(&self, event: &Event, ctx: &HookContext) -> Vec<HookError> {
        let mut errors = Vec::new();
        for entry in &self.hooks {
            // Synchronous extension code: a panic is a failure of this hook, not of the run.
            match std::panic::catch_unwind(AssertUnwindSafe(|| entry.hook.observes(&event.event))) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => {
                    errors.push(entry.error(HookPoint::OnEvent, HookFailure::Panicked));
                    continue;
                }
            }
            if let Err(error) = entry
                .run(HookPoint::OnEvent, entry.hook.on_event(event, ctx), ctx)
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
}
