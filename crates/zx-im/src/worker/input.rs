//! Input admission, and context recorded without starting a run.

use zx_core::input::Input;

use super::{AfterRun, CANCEL_COMMAND, Incoming, NEW_COMMAND, Phase, Worker, WorkerInput};
use crate::{
    Addressee, Session,
    outlet::{Asked, said},
};

impl Worker {
    pub(super) async fn input(&mut self, input: WorkerInput) {
        match input {
            WorkerInput::Message(incoming) => self.message(incoming).await,
            WorkerInput::Decision { user, approval, choice } => self.decide(user, approval, choice).await,
        }
    }

    /// Whether the bot answers `incoming`: it mentions the bot (every direct message does), or
    /// it mentions no one and comes from the person the bot just answered.
    pub(super) fn answers(&self, incoming: &Incoming) -> bool {
        match incoming.addressee {
            Addressee::Bot => true,
            Addressee::Nobody => self.answering.as_ref() == Some(&incoming.sender),
            Addressee::Others => false,
        }
    }

    async fn message(&mut self, incoming: Incoming) {
        let answer = self.answers(&incoming);
        if answer {
            match incoming.text.trim() {
                CANCEL_COMMAND => return self.cancel().await,
                NEW_COMMAND => return self.renew().await,
                _ => {}
            }
        }
        let content = self.out.attributed(&incoming.sender, &incoming.text).await;
        let input = if answer {
            Input::User {
                text: content,
                actor: Some(incoming.sender.0.clone()),
            }
        } else {
            Input::Custom(said(content))
        };
        let admitted = match incoming.permit {
            Some(permit) => permit.attach(input),
            None => self.budget.try_admit(input),
        };
        let Some(pending) = admitted else {
            self.out
                .failed("pending input capacity exceeded; message was not accepted".into());
            // Talk the bot only records is refused without a word in the conversation.
            if answer {
                self.out.post(self.out.replies().overloaded.clone()).await;
            }
            return;
        };
        if !answer {
            self.answering = None;
            self.heard.push(pending);
            self.record_heard().await;
            return;
        }
        self.answering = Some(incoming.sender.clone());
        let asked = Asked(pending);
        let taken = match &mut self.phase {
            // A run that `/cancel` or `/new` stopped takes no more input: what is asked now
            // runs next.
            Phase::Running {
                after: AfterRun::Cancel | AfterRun::Renew,
                asked: later,
                ..
            } => {
                later.push(asked);
                true
            }
            Phase::Running { open, .. } => {
                open.session.queues().steer(asked.0);
                true
            }
            Phase::Waiting { asked: waiting, .. } => {
                waiting.push(asked);
                false
            }
            Phase::Idle { open, .. } => {
                let open = open.take();
                self.begin(open, vec![asked], Some(incoming.message.clone())).await;
                false
            }
        };
        // Steered into the run, or kept for the next one: either way it will be answered.
        if taken {
            self.out.acknowledge(&incoming.message).await;
        }
    }

    /// Write the messages heard but not answered into the session when the conversation holds
    /// it. A run that holds it, or waits for a slot, writes them when it next opens or ends; a
    /// released session is opened again for them once a run slot is free, as for a run.
    pub(super) async fn record_heard(&mut self) {
        if self.heard.is_empty() {
            return;
        }
        match &self.phase {
            Phase::Idle { open: Some(open), .. } => {
                let session = open.session.clone();
                self.record_in(&session).await;
            }
            Phase::Idle { open: None, .. } => self.begin(None, Vec::new(), None).await,
            Phase::Waiting { .. } | Phase::Running { .. } => {}
        }
    }

    pub(super) async fn record_in(&mut self, session: &Session) {
        for pending in std::mem::take(&mut self.heard) {
            if let Input::Custom(message) = pending.input()
                && let Err(error) = session.append_custom_message(message.clone()).await
            {
                self.out.failed(format!("cannot record a message: {error}"));
            }
        }
    }
}
