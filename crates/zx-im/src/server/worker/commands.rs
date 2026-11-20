//! The conversation's commands, `/cancel`, `/new` and `/compact`: what each is, and what the
//! worker does for it.

use std::sync::Arc;

use super::{AfterRun, OpenFailure, Phase, Worker};

/// A conversation's commands, each sent alone.
#[derive(Clone, Copy)]
pub(in crate::server) enum Command {
    /// `/cancel`: cancels the conversation's run.
    Cancel,
    /// `/new`: starts a new session for the conversation, as chat's `/new` does.
    New,
    /// `/compact`: summarizes the earlier part of the conversation, as chat's `/compact` does.
    Compact,
}

impl Command {
    /// The command `text` is, alone but for the whitespace around it; `None` for anything
    /// else, which is to be answered.
    pub(in crate::server) fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "/cancel" => Some(Self::Cancel),
            "/new" => Some(Self::New),
            "/compact" => Some(Self::Compact),
            _ => None,
        }
    }
}

impl Worker {
    /// `/cancel`: cancel the run in progress, or what waits for a run slot.
    pub(super) async fn cancel(&mut self) {
        if let Phase::Running(running) = &mut self.phase {
            // The run ends as aborted, and its status message then says so. What was steered
            // into it is dropped with it, also when the run ended just before; what is asked
            // from now on waits for the next run.
            running.open.session.cancel();
            if running.after == AfterRun::Continue {
                running.after = AfterRun::Cancel;
            }
            return;
        }
        let asked = self.stop_waiting().unwrap_or_default();
        let replies = self.out.replies();
        let text = if asked.is_empty() {
            replies.nothing_to_cancel.clone()
        } else {
            replies.cancelled.clone()
        };
        self.out.post(text).await;
        // Waiting only to record what was heard is not something to cancel: that waits again.
        self.record_heard().await;
    }

    /// `/new`: the conversation goes on in a new session. A run in progress is cancelled
    /// first, and its end starts the new session; input that waited for a run slot is
    /// dropped with the session it was meant for.
    pub(super) async fn renew(&mut self) {
        self.answering = None;
        self.heard.clear();
        let open = match self.phase.take() {
            Phase::Running(mut running) => {
                running.after = AfterRun::Renew;
                running.open.session.cancel();
                self.phase = Phase::Running(running);
                return;
            }
            Phase::Waiting(waiting) => waiting.open,
            Phase::Idle { open, .. } => open,
        };
        let open = self.out.start_over(open, &self.stop).await;
        self.phase = Phase::idle(open);
    }

    /// `/compact`: summarize the earlier part of the conversation, as work of its own that
    /// holds a run slot, shows its status, and that `/cancel`, `/new` and the stop end like a
    /// run. Only while the conversation is idle and a slot is free: otherwise it says that it
    /// is busy, and the command is to be sent again. A conversation without a session has
    /// nothing to summarize, and none is made for the command.
    pub(super) async fn compact(&mut self) {
        let free = match self.phase.take() {
            Phase::Idle { open, .. } => {
                if open.is_none() && self.out.shared.session_of(&self.out.key).is_none() {
                    self.out.post(self.out.replies().nothing_to_compact.clone()).await;
                    return;
                }
                match Arc::clone(&self.out.shared.permits).try_acquire_owned() {
                    Ok(slot) => Ok((open, slot)),
                    Err(_) => Err(Phase::idle(open)),
                }
            }
            busy @ (Phase::Waiting(_) | Phase::Running(_)) => Err(busy),
        };
        let (open, slot) = match free {
            Ok(free) => free,
            Err(busy) => {
                self.phase = busy;
                self.out.post(self.out.replies().busy.clone()).await;
                return;
            }
        };
        let open = match self.opened(open, None).await {
            Ok(open) => open,
            Err(OpenFailure::Stopped) => return,
            Err(OpenFailure::Failed(error)) => return self.out.cannot_open(&error).await,
        };
        let work = self.out.compact(&open.session, self.stop.clone());
        self.start(open, slot, work).await;
    }
}
