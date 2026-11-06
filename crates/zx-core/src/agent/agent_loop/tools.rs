//! Tool validation, approval, execution and result recording.

use std::borrow::Cow;
use std::fmt::Display;

use serde_json::Value;

use super::{
    AgentEvent, Approval, ApprovalRequest, ApprovalResult, AssertUnwindSafe, Decision, Effects, Engine, ExecutionMode,
    FutureExt, Halt, HookCall, HookFailure, HookPhase, Instant, MAX_DETAILS_BYTES, MAX_QUEUED_PROGRESS, Message, Tool,
    ToolCall, ToolContext, ToolOutput, ToolResultMessage, ToolVerdict, duration_ms, join_all, now_ms,
};

impl<'a> Engine<'a, '_> {
    fn find_tool(&self, name: &str) -> Option<&'a dyn Tool> {
        self.config
            .spec
            .tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(AsRef::as_ref)
    }

    /// Resolve `call` and check its arguments, then run the policy and ask for approval
    /// when needed. A blocked call carries the result the model sees instead.
    async fn check<'c>(&mut self, call: &'c ToolCall) -> Checked<'a, 'c> {
        // Resolve and validate before running policy or asking the host.
        let Some(tool) = self.find_tool(&call.name) else {
            return Checked::blocked(format!("Tool {} not found", call.name));
        };
        if !self.config.active_tools.is_active(&call.name) {
            return Checked::blocked(format!("Tool {} is not active", call.name));
        }
        if let Some(error) = &call.arguments_error {
            return Checked::Blocked(invalid_arguments(call, error));
        }
        let ran = match std::panic::catch_unwind(AssertUnwindSafe(|| tool.prepare_arguments(&call.arguments))) {
            Ok(None) => Cow::Borrowed(call),
            Ok(Some(arguments)) => Cow::Owned(ToolCall {
                arguments,
                ..call.clone()
            }),
            Err(_) => return Checked::blocked(format!("Tool {} argument preparation panicked", call.name)),
        };
        if let Err(outcome) = self.validate_arguments(tool, &ran) {
            return Checked::Blocked(outcome);
        }
        if let Err(outcome) = self.authorize(&ran).await {
            return Checked::Blocked(outcome);
        }
        if self.cancel.is_cancelled() {
            return Checked::blocked(NOT_STARTED);
        }
        Checked::Ready(Runnable { tool, call: ran })
    }

    /// Check the arguments `call` runs with: a JSON object that fits the tool's declared
    /// schema and passes the tool's own validation.
    fn validate_arguments(&self, tool: &dyn Tool, call: &ToolCall) -> Result<(), Outcome> {
        if !call.arguments.is_object() {
            return Err(invalid_arguments(call, "expected a JSON object"));
        }
        if let Some(validator) = self.validators.get(&call.name)
            && let Err(error) = validator.validate(&call.arguments)
        {
            return Err(invalid_arguments(call, error));
        }
        match std::panic::catch_unwind(AssertUnwindSafe(|| tool.validate_arguments(&call.arguments))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(invalid_arguments(call, error)),
            Err(_) => Err(Outcome::blocked(format!(
                "Tool {} argument validation panicked",
                call.name
            ))),
        }
    }

    /// Run the policy on `call`, then ask the interaction host when it says so. An error is
    /// the result of a call that may not run.
    async fn authorize(&mut self, call: &ToolCall) -> Result<(), Outcome> {
        let verdict = match self.config.hooks.before_tool(call, self.trace.context()).await {
            Ok(verdict) => verdict,
            Err(error) => {
                let reason = self.report_hook_failure(error);
                self.decision(call, ToolVerdict::Deny, Some(reason.clone())).await;
                return Err(blocked_by_policy(&reason));
            }
        };
        match verdict {
            Decision::Allow => self.decision(call, ToolVerdict::Allow, None).await,
            Decision::Deny(reason) => {
                self.decision(call, ToolVerdict::Deny, Some(reason.clone())).await;
                return Err(blocked_by_policy(&reason));
            }
            Decision::Ask(reason) => {
                self.decision(call, ToolVerdict::Ask, Some(reason.clone())).await;
                if self.approvals.allows(call) {
                    self.approval_resolved(call, ApprovalResult::Remembered, None, 0).await;
                } else if let Err(denied) = self.ask(call, reason).await {
                    return Err(Outcome::blocked(format!("Approval denied: {denied}")));
                }
            }
        }
        Ok(())
    }

    /// Ask the interaction host to approve `call`. Returns why the call may not run.
    async fn ask(&mut self, call: &ToolCall, reason: String) -> Result<(), String> {
        self.emit(AgentEvent::ApprovalRequested {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            reason: reason.clone(),
        })
        .await;
        let request = ApprovalRequest {
            call: call.clone(),
            reason,
        };
        let guard = HookCall {
            name: "Interaction::confirm",
            phase: HookPhase::Approval,
            timeout: self.config.interaction_timeout,
        };
        let asked = Instant::now();
        let answer = guard.run(self.config.interaction.confirm(&request), self.cancel).await;
        let waited = duration_ms(asked.elapsed());
        match answer {
            Ok(Approval::AllowOnce) => {
                self.approval_resolved(call, ApprovalResult::AllowOnce, None, waited)
                    .await;
                Ok(())
            }
            Ok(Approval::AllowSession) => {
                self.approvals.remember(call);
                self.approval_resolved(call, ApprovalResult::AllowSession, None, waited)
                    .await;
                Ok(())
            }
            Ok(Approval::Deny(reason)) => {
                self.approval_resolved(call, ApprovalResult::Denied, Some(reason.clone()), waited)
                    .await;
                Err(reason)
            }
            Err(error) => {
                let result = match error.failure {
                    HookFailure::TimedOut(_) => ApprovalResult::TimedOut,
                    HookFailure::Cancelled => ApprovalResult::Cancelled,
                    HookFailure::Rejected(_) | HookFailure::Panicked => ApprovalResult::Failed,
                };
                let reason = self.report_hook_failure(error);
                self.approval_resolved(call, result, Some(reason.clone()), waited).await;
                Err(reason)
            }
        }
    }

    async fn decision(&self, call: &ToolCall, verdict: ToolVerdict, reason: Option<String>) {
        self.emit(AgentEvent::ToolDecision {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            verdict,
            reason,
        })
        .await;
    }

    async fn approval_resolved(&self, call: &ToolCall, approval: ApprovalResult, reason: Option<String>, wait_ms: u64) {
        self.emit(AgentEvent::ApprovalResolved {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            approval,
            reason,
            wait_ms,
        })
        .await;
    }

    pub(super) async fn execute_tools(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        let sequential = self.config.spec.sequential_tools
            || calls.iter().any(|call| {
                self.find_tool(&call.name)
                    .is_some_and(|tool| tool.execution_mode() == ExecutionMode::Sequential)
            });
        if sequential {
            self.sequential(calls).await
        } else {
            self.parallel(calls).await
        }
    }

    async fn sequential(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        let mut results = Vec::new();
        let mut terminate = true;
        for call in calls {
            self.tool_start(call).await;
            let checked = self.check(call).await;
            if let Checked::Ready(runnable) = &checked {
                self.record_start(runnable).await?;
            }
            let outcome = self.execute_checked(call, checked).await;
            terminate &= outcome.output.terminate;
            results.push(self.record_result(call, outcome).await?);
            if self.cancel.is_cancelled() {
                break;
            }
        }
        Ok(Batch {
            terminate: terminate && !results.is_empty(),
            results,
        })
    }

    async fn parallel(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        // Phase 1: all checks are ordered. Store verdicts, then borrow the static
        // tools during execution after the mutable approval borrow ends.
        let mut slots = Vec::new();
        for call in calls {
            self.tool_start(call).await;
            let checked = self.check(call).await;
            slots.push((call, checked));
            if self.cancel.is_cancelled() {
                break;
            }
        }
        // Record starts in call order before any call runs, so that recovery after a crash
        // can tell the calls that began from those that never did.
        for (_, checked) in &slots {
            if let Checked::Ready(runnable) = checked {
                self.record_start(runnable).await?;
            }
        }
        // Phase 2: borrow the runtime; futures do not need spawn or shared mutable state.
        let engine = &*self;
        let outcomes = join_all(
            slots
                .into_iter()
                .map(|(call, checked)| async move { (call, engine.execute_checked(call, checked).await) }),
        )
        .await;
        // Phase 3: record results in model call order, regardless of completion order.
        let terminate = !outcomes.is_empty() && outcomes.iter().all(|(_, result)| result.output.terminate);
        let mut results = Vec::with_capacity(outcomes.len());
        for (call, outcome) in outcomes {
            results.push(self.record_result(call, outcome).await?);
        }
        Ok(Batch { results, terminate })
    }

    /// Record the start of a call with side effects; an error means it must not run.
    /// Read-only calls are not recorded.
    async fn record_start(&mut self, runnable: &Runnable<'_, '_>) -> Result<(), Halt> {
        match runnable.tool.effects() {
            Effects::SideEffects => self
                .session
                .record_tool_call_started(&runnable.call)
                .await
                .map_err(Halt::Unrecorded),
            Effects::ReadOnly => Ok(()),
        }
    }

    /// Run a checked call, or take the result of a blocked one, then pass the result through
    /// the output hooks and report the call's end. The hooks see the call as it ran, with
    /// the arguments `Tool::prepare_arguments` rewrote.
    async fn execute_checked<'c>(&self, call: &'c ToolCall, checked: Checked<'a, 'c>) -> Outcome {
        let (ran, outcome) = match checked {
            Checked::Blocked(outcome) => (Cow::Borrowed(call), outcome),
            Checked::Ready(Runnable { tool, call: ran }) => {
                let outcome = execute(self, tool, &ran).await;
                (ran, outcome)
            }
        };
        let outcome = self.apply_output_hooks(&ran, outcome).await;
        self.tool_end(call, &outcome).await;
        outcome
    }

    /// Record the result of `call`, then report it.
    async fn record_result(&mut self, call: &ToolCall, outcome: Outcome) -> Result<ToolResultMessage, Halt> {
        let result = to_result(call, outcome);
        self.emit(AgentEvent::MessageStart { role: "toolResult" }).await;
        self.record_finished_message(&Message::ToolResult(result.clone()))
            .await?;
        Ok(result)
    }

    async fn apply_output_hooks(&self, call: &ToolCall, outcome: Outcome) -> Outcome {
        let mut output = match self
            .config
            .hooks
            .after_tool(call, outcome.output, self.trace.context())
            .await
        {
            Ok(output) => output,
            // Discard the raw output on every failure, including cancellation.
            Err(error) => ToolOutput::error(format!("Tool output withheld: {}", self.report_hook_failure(error))),
        };
        // Bounded here, after every hook, so that the event and the record carry the same.
        output.details = output.details.filter(details_fit);
        Outcome {
            output,
            duration_ms: outcome.duration_ms,
        }
    }

    pub(super) async fn reject_truncated_tools(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        let mut results = Vec::new();
        for call in calls {
            self.tool_start(call).await;
            let blocked = Outcome::blocked(format!(
                "Tool call {:?} was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            ));
            let outcome = self.execute_checked(call, Checked::Blocked(blocked)).await;
            results.push(self.record_result(call, outcome).await?);
        }
        Ok(Batch {
            results,
            terminate: false,
        })
    }

    async fn tool_start(&self, call: &ToolCall) {
        self.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
        })
        .await;
    }

    async fn tool_end(&self, call: &ToolCall, outcome: &Outcome) {
        self.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            is_error: outcome.output.is_error,
            duration_ms: outcome.duration_ms,
            details: outcome.output.details.clone(),
        })
        .await;
    }
}

pub(super) struct Batch {
    pub(super) results: Vec<ToolResultMessage>,
    pub(super) terminate: bool,
}

struct Outcome {
    output: ToolOutput,
    duration_ms: Option<u64>,
}

impl Outcome {
    fn blocked(text: impl Into<String>) -> Self {
        Self {
            output: ToolOutput::error(text),
            duration_ms: None,
        }
    }
}

/// The result of a call whose arguments are not valid for its tool.
fn invalid_arguments(call: &ToolCall, reason: impl Display) -> Outcome {
    Outcome::blocked(format!("Invalid arguments for tool {:?}: {reason}", call.name))
}

/// The result of a call the policy denied.
fn blocked_by_policy(reason: &str) -> Outcome {
    Outcome::blocked(format!("Blocked by policy: {reason}"))
}

/// A call after its checks.
enum Checked<'a, 'c> {
    /// The call does not run; this is its result.
    Blocked(Outcome),
    Ready(Runnable<'a, 'c>),
}

impl Checked<'_, '_> {
    fn blocked(text: impl Into<String>) -> Self {
        Self::Blocked(Outcome::blocked(text))
    }
}

/// A call that may run: its tool, and the call as it runs, owned when
/// `Tool::prepare_arguments` rewrote its arguments.
struct Runnable<'a, 'c> {
    tool: &'a dyn Tool,
    call: Cow<'c, ToolCall>,
}

/// Whether `details` fit in an event. A value that cannot be measured is dropped too.
fn details_fit(details: &Value) -> bool {
    serde_json::to_string(details).is_ok_and(|json| json.len() <= MAX_DETAILS_BYTES)
}

/// The result of a call that never started because the run was cancelled first.
const NOT_STARTED: &str = "Not executed: the run was cancelled before this call started, so it had no effect.";

/// The result of a call that the cancellation interrupted. Like a call that a crash
/// interrupted, one with side effects may have changed state before it stopped.
fn interrupted(effects: Effects) -> &'static str {
    match effects {
        Effects::SideEffects => {
            "Interrupted: the run was cancelled while this call was running. It may have changed state; check the current state before running it again."
        }
        Effects::ReadOnly => {
            "Interrupted: the run was cancelled while this read-only call was running. It changed nothing; run it again if the result is still needed."
        }
    }
}

async fn execute(engine: &Engine<'_, '_>, tool: &dyn Tool, call: &ToolCall) -> Outcome {
    let cancel = engine.cancel;
    if cancel.is_cancelled() {
        return Outcome::blocked(NOT_STARTED);
    }
    // A token of the call's own: cancelled with the run, while a tool that cancels it to
    // stop work of its own does not stop the run.
    let (progress, mut updates) = tokio::sync::mpsc::channel(MAX_QUEUED_PROGRESS);
    let ctx = ToolContext {
        cwd: engine.config.spec.cwd.clone(),
        cancel: cancel.child_token(),
        tool_call_id: Some(call.id.clone()),
        progress: Some(progress),
    };
    let started = Instant::now();
    let future = AssertUnwindSafe(tool.execute(call.arguments.clone(), &ctx)).catch_unwind();
    tokio::pin!(future);
    let result = loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Outcome {
                    output: ToolOutput::error(interrupted(tool.effects())),
                    duration_ms: Some(duration_ms(started.elapsed())),
                };
            }
            update = updates.recv() => {
                if let Some(update) = update {
                    emit_progress(engine, call, update).await;
                }
            }
            result = &mut future => break result,
        }
    };
    updates.close();
    while let Ok(update) = updates.try_recv() {
        emit_progress(engine, call, update).await;
    }
    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => ToolOutput::error(error),
        Err(_) => ToolOutput::error(format!("Tool {} panicked", tool.name())),
    };
    Outcome {
        output,
        duration_ms: Some(duration_ms(started.elapsed())),
    }
}

async fn emit_progress(engine: &Engine<'_, '_>, call: &ToolCall, text: String) {
    match engine
        .config
        .hooks
        .filter_tool_progress(call, ToolOutput::ok(text), engine.trace.context())
        .await
    {
        Ok(output) => {
            engine
                .emit(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: output.content,
                })
                .await;
        }
        Err(error) => {
            // The update is withheld; the failure is reported, not the update's text.
            engine.report_hook_failure(error);
        }
    }
}

fn to_result(call: &ToolCall, outcome: Outcome) -> ToolResultMessage {
    let Outcome { output, duration_ms } = outcome;
    let content = match (output.content.is_empty(), output.is_error) {
        (false, _) => output.content,
        (true, true) => "(tool failed without output)".to_string(),
        (true, false) => "(no output)".to_string(),
    };
    ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content,
        is_error: output.is_error,
        timestamp: now_ms(),
        duration_ms,
        details: output.details,
    }
}
