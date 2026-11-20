//! Input admission, and context recorded without starting a run.

use zx_agent::Session;
use zx_agent::input::{Input, PendingInput};

use crate::Addressee;
use crate::server::outlet::{Asked, said};

use super::{AfterRun, Command, Heard, Incoming, Phase, Running, Worker, WorkerInput};

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
        if answer && let Some(command) = Command::parse(&incoming.text) {
            return match command {
                Command::Cancel => self.cancel().await,
                Command::New => self.renew().await,
                Command::Compact => self.compact().await,
            };
        }
        let content = self.out.attributed(&incoming.sender, &incoming.text).await;
        let permit = match incoming.permit {
            Some(permit) => permit.resize(content.len()),
            None => self.budget.try_reserve(content.len()),
        };
        let Some(permit) = permit else {
            self.out
                .failed("pending input capacity exceeded; message was not accepted".into());
            // Talk the bot only records is refused without a word in the conversation.
            if answer {
                self.out.post(self.out.replies().overloaded.clone()).await;
            }
            return;
        };
        let hold = (permit, incoming.receipt);
        if !answer {
            self.answering = None;
            self.heard.push(Heard {
                message: said(content),
                hold,
            });
            self.record_heard().await;
            return;
        }
        let input = Input::User {
            text: content,
            actor: Some(incoming.sender.0.clone()),
        };
        self.answering = Some(incoming.sender);
        let asked = Asked(PendingInput::held(input, hold));
        let taken = match &mut self.phase {
            // A run that `/cancel` or `/new` stopped takes no more input: what is asked now
            // runs next.
            Phase::Running(Running {
                after: AfterRun::Cancel | AfterRun::Renew,
                asked: later,
                ..
            }) => {
                later.push(asked);
                true
            }
            Phase::Running(running) => {
                running.open.session.queues().steer(asked.0);
                true
            }
            Phase::Waiting(waiting) => {
                waiting.asked.push(asked);
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
    /// released session, or one that closed while the conversation held it, is opened again
    /// for them once a run slot is free, as for a run. A conversation without a session yet,
    /// such as after a mention that was only `/cancel`, drops them: its first session, made
    /// for the next mention, starts with what the thread holds then, them included, on a
    /// platform that reads threads.
    pub(super) async fn record_heard(&mut self) {
        if self.heard.is_empty() {
            return;
        }
        match &self.phase {
            Phase::Idle { open: Some(open), .. } if open.still_open() => {
                let session = open.session.clone();
                self.record_in(&session).await;
            }
            Phase::Idle { .. } if self.out.shared.session_of(&self.out.key).is_some() => {
                self.begin(None, Vec::new(), None).await;
            }
            // Dropping them returns their share of the budget and takes them out of the backlog.
            Phase::Idle { .. } => self.heard.clear(),
            Phase::Waiting(_) | Phase::Running(_) => {}
        }
    }

    pub(super) async fn record_in(&mut self, session: &Session) {
        for Heard { message, hold } in std::mem::take(&mut self.heard) {
            if let Err(error) = session.append_custom_message(message).await {
                self.out.failed(format!("cannot record a message: {error}"));
            }
            // Recorded, or reported as not: the message is handled.
            drop(hold);
        }
    }
}
