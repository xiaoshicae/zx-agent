//! Tool validation, approval, execution and result recording.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Display;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use futures::FutureExt;
use futures::future::join_all;
use serde_json::Value;

use super::{Halt, StepLoop};
use crate::ai::{Message, ToolCall, ToolResultMessage, duration_ms, now_ms};
use crate::events::{ApprovalResult, EventKind};
use crate::policy::{Answer, ApprovalDecision, ApprovalMode, NO_HUMAN, PolicyDecision};
use crate::tools::{Effects, ExecutionMode, MAX_DETAILS_BYTES, MAX_QUEUED_PROGRESS, Tool, ToolContext, ToolOutput};

impl StepLoop<'_> {
    /// The tool the model calls `name`, and what it declared it changes.
    fn find_tool(&self, name: &str) -> Option<(Arc<dyn Tool>, Effects)> {
        let tools = &self.run.config.tools;
        Some((tools.find(name)?.clone(), tools.effects(name)?))
    }

    /// Resolve `call` and check its arguments, then run the policy and ask for approval
    /// when needed. A blocked call carries the result the model sees instead.
    async fn check<'c>(&mut self, call: &'c ToolCall) -> Checked<'c> {
        // Resolve and validate before running policy or asking the host.
        let Some((tool, effects)) = self.find_tool(&call.name) else {
            return Checked::blocked(format!("Tool {} not found", call.name));
        };
        if !self.run.config.active_tools.is_active(&call.name) {
            return Checked::blocked(format!("Tool {} is not active", call.name));
        }
        if let Some(error) = &call.arguments_error {
            return Checked::blocked(invalid_arguments(call, error));
        }
        let ran = match std::panic::catch_unwind(AssertUnwindSafe(|| tool.prepare_arguments(&call.arguments))) {
            Ok(None) => Cow::Borrowed(call),
            Ok(Some(arguments)) => Cow::Owned(ToolCall {
                arguments,
                ..call.clone()
            }),
            Err(_) => return Checked::blocked(format!("Tool {} argument preparation panicked", call.name)),
        };
        if let Err(text) = self.validate_arguments(tool.as_ref(), &ran) {
            return Checked::blocked(text);
        }
        let authorized = self.authorize(&ran).await;
        // Cancelled while the policies ran or the call waited for approval: the call does not
        // start, whatever their answer came to, and nobody denied it.
        if self.run.target.trace.cancel().is_cancelled() {
            return Checked::NotStarted;
        }
        if let Err(text) = authorized {
            self.accounting.denied_calls = self.accounting.denied_calls.saturating_add(1);
            return Checked::blocked(text);
        }
        Checked::Ready(Runnable {
            tool,
            effects,
            call: ran,
        })
    }

    /// Check `call` as the next call of an answer whose calls so far have the IDs `seen`.
    /// One that repeats an ID is blocked without being checked: its start record and its
    /// result could not be told from the first call's. A cancelled run starts no call, and
    /// asks no tool, no policy and no person about one.
    async fn check_next<'c>(&mut self, call: &'c ToolCall, seen: &mut HashSet<&'c str>) -> Checked<'c> {
        if self.run.target.trace.cancel().is_cancelled() {
            return Checked::NotStarted;
        }
        if seen.insert(&call.id) {
            return self.check(call).await;
        }
        Checked::blocked(format!(
            "Tool call {:?} was not executed: an earlier call of this answer has the same ID {:?}. Give every call an ID of its own.",
            call.name, call.id
        ))
    }

    /// Check the arguments `call` runs with: a JSON object that fits the tool's declared
    /// schema and passes the tool's own validation. An error is the text of the blocked result.
    fn validate_arguments(&self, tool: &dyn Tool, call: &ToolCall) -> Result<(), String> {
        if !call.arguments.is_object() {
            return Err(invalid_arguments(call, "expected a JSON object"));
        }
        if let Some(mismatch) = self.run.config.tools.schema_mismatch(&call.name, &call.arguments) {
            return Err(invalid_arguments(call, mismatch));
        }
        match std::panic::catch_unwind(AssertUnwindSafe(|| tool.validate_arguments(&call.arguments))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(invalid_arguments(call, error)),
            Err(_) => Err(format!("Tool {} argument validation panicked", call.name)),
        }
    }

    /// Run the policy on `call`, then ask for the host's approval when it says so. An error
    /// is the text of the result of a call that may not run.
    async fn authorize(&mut self, call: &ToolCall) -> Result<(), String> {
        let verdict = self
            .run
            .config
            .policies
            .check(call, &self.run.target.trace.policy_context())
            .await;
        self.decision(call, verdict.clone()).await;
        match verdict {
            PolicyDecision::Allow => {}
            PolicyDecision::Deny(reason) => return Err(blocked_by_policy(&reason)),
            PolicyDecision::Ask(reason) => {
                if self.approvals.allows(call) {
                    let remembered = Resolved {
                        approval_id: None,
                        approval: ApprovalResult::Remembered,
                        reason: None,
                        wait_ms: 0,
                    };
                    self.approval_resolved(call, remembered).await;
                } else if let Err(denied) = self.ask(call, reason).await {
                    return Err(format!("Approval denied: {denied}"));
                }
            }
        }
        Ok(())
    }

    /// Ask for the host's approval of `call`: announce the request, then wait at the gate
    /// for its answer. Returns why the call may not run.
    async fn ask(&mut self, call: &ToolCall, reason: String) -> Result<(), String> {
        let mut request = self.approvals.open();
        let approval_id = request.id().to_string();
        self.emit(EventKind::ApprovalRequested {
            approval_id: approval_id.clone(),
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
            reason,
        })
        .await;
        let asked = Instant::now();
        let answer = match self.run.config.approvals {
            ApprovalMode::Deny => Answer::Decided(ApprovalDecision::Deny(NO_HUMAN.into())),
            ApprovalMode::Wait(timeout) => request.answer(timeout, self.run.target.trace.cancel()).await,
        };
        // Withdrawn before it is reported: an answer that comes now finds no request.
        drop(request);
        let (approval, denial) = match answer {
            Answer::Decided(ApprovalDecision::AllowOnce) => (ApprovalResult::AllowOnce, None),
            Answer::Decided(ApprovalDecision::AllowSession) => {
                self.approvals.remember(call);
                (ApprovalResult::AllowSession, None)
            }
            Answer::Decided(ApprovalDecision::Deny(reason)) => (ApprovalResult::Denied, Some(reason)),
            Answer::TimedOut(timeout) => (
                ApprovalResult::TimedOut,
                Some(format!(
                    "nobody answered the approval request within {}s",
                    timeout.as_secs()
                )),
            ),
            Answer::Cancelled => (
                ApprovalResult::Cancelled,
                Some("the run was cancelled while the call waited for approval".to_string()),
            ),
        };
        let resolved = Resolved {
            approval_id: Some(approval_id),
            approval,
            reason: denial.clone(),
            wait_ms: duration_ms(asked.elapsed()),
        };
        self.approval_resolved(call, resolved).await;
        denial.map_or(Ok(()), Err)
    }

    /// Report the policies' verdict on `call`.
    async fn decision(&self, call: &ToolCall, decision: PolicyDecision) {
        self.emit(EventKind::ToolDecision {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            decision,
        })
        .await;
    }

    async fn approval_resolved(&self, call: &ToolCall, resolved: Resolved) {
        self.emit(EventKind::ApprovalResolved {
            approval_id: resolved.approval_id,
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            approval: resolved.approval,
            reason: resolved.reason,
            wait_ms: resolved.wait_ms,
        })
        .await;
    }

    /// Check, run and record the calls of one response. Each call's start is recorded before
    /// it runs (invariant 10), and the two orders keep that true differently: in sequence,
    /// each call is checked, started and run before the next is checked; in parallel, every
    /// call is checked, approvals included, before any start is recorded, so that a crash
    /// while one call waits for approval leaves no start for calls that never ran. Every call
    /// gets its result here, in a cancelled run too: the calls it did not reach are recorded
    /// as not started, so that only a run that died leaves calls for the next one to answer.
    pub(super) async fn execute_tools(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        let sequential = calls
            .iter()
            .any(|call| self.run.config.tools.execution_mode(&call.name) == Some(ExecutionMode::Sequential));
        if sequential {
            self.sequential(calls).await
        } else {
            self.parallel(calls).await
        }
    }

    async fn sequential(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        let mut batch = Batch::default();
        let mut seen = HashSet::new();
        for call in calls {
            self.tool_start(call).await;
            let checked = self.check_next(call, &mut seen).await;
            if let Checked::Ready(runnable) = &checked {
                self.record_start(runnable).await?;
            }
            let outcome = self.execute_checked(call, checked).await;
            batch.add(self.record_result(call, outcome).await?);
        }
        Ok(batch)
    }

    async fn parallel(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        // Phase 1: check every call in order. Approvals borrow the loop mutably, so all of
        // them are settled here, before any call runs.
        let mut slots = Vec::new();
        let mut seen = HashSet::new();
        for call in calls {
            self.tool_start(call).await;
            let checked = self.check_next(call, &mut seen).await;
            slots.push((call, checked));
        }
        // Record starts in call order before any call runs, so that recovery after a crash
        // can tell the calls that began from those that never did. A run cancelled during
        // the checks starts none of them.
        if !self.run.target.trace.cancel().is_cancelled() {
            for (_, checked) in &slots {
                if let Checked::Ready(runnable) = checked {
                    self.record_start(runnable).await?;
                }
            }
        }
        // Phase 2: the calls run concurrently on one shared borrow of the loop; the futures
        // need neither spawn nor shared mutable state.
        let this = &*self;
        let outcomes = join_all(
            slots
                .into_iter()
                .map(|(call, checked)| async move { (call, this.execute_checked(call, checked).await) }),
        )
        .await;
        // Phase 3: record results in model call order, regardless of completion order.
        let mut batch = Batch::default();
        for (call, outcome) in outcomes {
            batch.add(self.record_result(call, outcome).await?);
        }
        Ok(batch)
    }

    /// Record the start of a call with side effects; an error means it must not run.
    /// Read-only calls are not recorded.
    async fn record_start(&mut self, runnable: &Runnable<'_>) -> Result<(), Halt> {
        let call = &runnable.call;
        match runnable.effects {
            Effects::SideEffects => self
                .run
                .transcript
                .append_tool_call_started(&call.id, &call.name)
                .await
                .map(|_| ())
                .with_context(|| format!("cannot persist the start of tool call {:?}", call.id))
                .map_err(Halt::Unrecorded),
            Effects::ReadOnly => Ok(()),
        }
    }

    /// Run a checked call, or take the result of one that does not run, then pass the result
    /// through the output hooks and report the call's end. The hooks see the call as it ran,
    /// with the arguments `Tool::prepare_arguments` rewrote; what the kernel says of a call
    /// that the run's cancellation kept from starting or interrupted goes past them.
    async fn execute_checked<'c>(&self, call: &'c ToolCall, checked: Checked<'c>) -> CallOutput {
        let (ran, ended) = match checked {
            Checked::Blocked(outcome) => (Cow::Borrowed(call), Ended::Output(outcome)),
            Checked::NotStarted => (Cow::Borrowed(call), Ended::not_started()),
            Checked::Ready(Runnable {
                tool,
                effects,
                call: ran,
            }) => {
                let ended = self.execute(tool.as_ref(), effects, &ran).await;
                (ran, ended)
            }
        };
        let outcome = match ended {
            Ended::Output(outcome) => self.apply_output_hooks(&ran, outcome).await,
            Ended::Cancelled(outcome) => outcome,
        };
        self.tool_end(call, &outcome).await;
        outcome
    }

    /// Run `tool`, which declared it has `effects`, on `call` until it finishes or the run is
    /// cancelled, reporting its progress on the way. A failure or a panic of the tool becomes
    /// an error result.
    async fn execute(&self, tool: &dyn Tool, effects: Effects, call: &ToolCall) -> Ended {
        let cancel = self.run.target.trace.cancel();
        if cancel.is_cancelled() {
            return Ended::not_started();
        }
        let (progress, mut updates) = tokio::sync::mpsc::channel(MAX_QUEUED_PROGRESS);
        let ctx = ToolContext {
            cwd: self.run.config.cwd.clone(),
            // A token of the call's own: cancelled with the run, while a tool that cancels it
            // to stop work of its own does not stop the run.
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
                    return Ended::Cancelled(CallOutput {
                        output: ToolOutput::error(interrupted(effects)),
                        duration_ms: Some(duration_ms(started.elapsed())),
                    });
                }
                update = updates.recv() => {
                    if let Some(update) = update {
                        self.emit_progress(call, update).await;
                    }
                }
                result = &mut future => break result,
            }
        };
        updates.close();
        while let Ok(update) = updates.try_recv() {
            self.emit_progress(call, update).await;
        }
        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => ToolOutput::error(error),
            Err(_) => ToolOutput::error(format!("Tool {} panicked", tool.name())),
        };
        Ended::Output(CallOutput {
            output,
            duration_ms: Some(duration_ms(started.elapsed())),
        })
    }

    /// Pass one progress update of `call` through the redaction hooks, then report it.
    async fn emit_progress(&self, call: &ToolCall, text: String) {
        match self
            .run
            .config
            .hooks
            .filter_tool_progress(call, ToolOutput::ok(text), &self.run.target.trace.context())
            .await
        {
            Ok(output) => {
                self.emit(EventKind::ToolExecutionUpdate {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: output.content,
                })
                .await;
            }
            Err(error) => {
                // The update is withheld; the failure is reported, not the update's text.
                self.report_hook_failure(error).await;
            }
        }
    }

    /// Record the result of `call`, then report it and count what the tool spent. Returns
    /// whether the tool asked to end the attempt.
    async fn record_result(&mut self, call: &ToolCall, outcome: CallOutput) -> Result<bool, Halt> {
        let terminate = outcome.output.terminate;
        let usage = outcome.output.usage.clone();
        self.append(Message::ToolResult(to_result(call, outcome))).await?;
        if let Some(usage) = &usage {
            self.accounting.count_tool_usage(&call.name, usage);
        }
        Ok(terminate)
    }

    async fn apply_output_hooks(&self, call: &ToolCall, outcome: CallOutput) -> CallOutput {
        let mut output = match self
            .run
            .config
            .hooks
            .after_tool(call, outcome.output, &self.run.target.trace.context())
            .await
        {
            Ok(output) => output,
            // Discard the raw output on every failure, including cancellation.
            Err(error) => {
                let failure = self.report_hook_failure(error).await;
                ToolOutput::error(format!("Tool output withheld: {failure}"))
            }
        };
        // Bounded here, after every hook, so that the event and the record carry the same.
        output.details = output.details.filter(details_fit);
        CallOutput {
            output,
            duration_ms: outcome.duration_ms,
        }
    }

    /// Answer every call of a response cut off at the output token limit without running it
    /// (invariant 4). Calls that never ran cannot end the attempt, whatever an output hook
    /// sets on their results.
    pub(super) async fn reject_truncated_tools(&mut self, calls: &[ToolCall]) -> Result<Batch, Halt> {
        for call in calls {
            self.tool_start(call).await;
            let blocked = CallOutput::blocked(format!(
                "Tool call {:?} was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            ));
            let outcome = self.execute_checked(call, Checked::Blocked(blocked)).await;
            self.record_result(call, outcome).await?;
        }
        Ok(Batch::default())
    }

    async fn tool_start(&self, call: &ToolCall) {
        self.emit(EventKind::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
        })
        .await;
    }

    async fn tool_end(&self, call: &ToolCall, outcome: &CallOutput) {
        self.emit(EventKind::ToolExecutionEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            is_error: outcome.output.is_error,
            duration_ms: outcome.duration_ms,
            details: outcome.output.details.clone(),
            usage: outcome.output.usage.clone(),
        })
        .await;
    }
}

/// Whether the recorded results of one response's calls end the attempt: only when every
/// call asked to.
#[derive(Default)]
pub(super) struct Batch {
    /// Whether a result was recorded.
    recorded: bool,
    /// Whether a recorded call did not ask to end the attempt.
    declined: bool,
}

impl Batch {
    /// Count a recorded result, and whether its tool asked to end the attempt.
    fn add(&mut self, terminate: bool) {
        self.recorded = true;
        self.declined |= !terminate;
    }

    /// Whether the results end the attempt: there was one, and every call asked to.
    pub(super) fn terminates(&self) -> bool {
        self.recorded && !self.declined
    }
}

/// What one call produced, before it becomes a result message.
struct CallOutput {
    output: ToolOutput,
    duration_ms: Option<u64>,
}

impl CallOutput {
    fn blocked(text: impl Into<String>) -> Self {
        Self {
            output: ToolOutput::error(text),
            duration_ms: None,
        }
    }
}

/// The result text of a call whose arguments are not valid for its tool.
fn invalid_arguments(call: &ToolCall, reason: impl Display) -> String {
    format!("Invalid arguments for tool {:?}: {reason}", call.name)
}

/// The result text of a call the policy denied.
fn blocked_by_policy(reason: &str) -> String {
    format!("Blocked by policy: {reason}")
}

/// How an approval ended, as its `approval_resolved` event reports it.
struct Resolved {
    /// The request that was answered; `None` when a session grant covered the call and
    /// nobody was asked.
    approval_id: Option<String>,
    approval: ApprovalResult,
    /// Why the call may not run, when it may not.
    reason: Option<String>,
    wait_ms: u64,
}

/// What a call came to, before the output hooks.
enum Ended {
    /// The tool's result, or the result of a blocked call: the output hooks see it, and one
    /// that fails withholds it.
    Output(CallOutput),
    /// The run was cancelled before the call started or while it ran. The result is the
    /// kernel's own words, with nothing of the tool's in them to withhold, and no hook sees
    /// it: a cancelled run waits for none.
    Cancelled(CallOutput),
}

impl Ended {
    /// The end of a call that never started because the run was cancelled first.
    fn not_started() -> Self {
        Self::Cancelled(CallOutput::blocked(NOT_STARTED))
    }
}

/// A call after its checks.
enum Checked<'c> {
    /// The call does not run; this is its result.
    Blocked(CallOutput),
    /// The call does not run: the run was cancelled before it could start.
    NotStarted,
    Ready(Runnable<'c>),
}

impl Checked<'_> {
    fn blocked(text: impl Into<String>) -> Self {
        Self::Blocked(CallOutput::blocked(text))
    }
}

/// A call that may run: its tool and what the tool declared it changes, and the call as it
/// runs, owned when `Tool::prepare_arguments` rewrote its arguments.
struct Runnable<'c> {
    tool: Arc<dyn Tool>,
    effects: Effects,
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

fn to_result(call: &ToolCall, outcome: CallOutput) -> ToolResultMessage {
    let CallOutput { output, duration_ms } = outcome;
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
        usage: output.usage,
    }
}
