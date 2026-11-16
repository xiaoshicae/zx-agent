//! Thread membership, input admission and context recorded without starting a run.

use super::{CANCEL_COMMAND, Incoming, Membership, NEW_COMMAND, Phase, Worker, WorkerInput};
use crate::{
    Addressee, Session,
    outlet::{Asked, said},
};

impl Worker {
    /// Unknown threads are looked up here, never on the server's routing loop.
    async fn joined(&mut self, addressee: Addressee) -> bool {
        if addressee == Addressee::Bot {
            self.membership = Membership::Joined;
        } else if self.membership == Membership::Unknown {
            let lookup = self.out.shared.bounded(
                Some(&self.out.key),
                "look up the conversation's sessions",
                self.out.shared.latest_session(&self.out.key),
            );
            let found = tokio::select! {
                () = self.stop.cancelled() => None,
                found = lookup => found,
            };
            self.membership = match found {
                Some(Some(_)) => Membership::Joined,
                Some(None) => Membership::Absent,
                None => Membership::Unknown,
            };
        }
        self.membership == Membership::Joined
    }

    fn has_capacity(&self, bytes: usize) -> bool {
        let (mut count, mut total) = match &self.phase {
            Phase::Waiting { asked, .. } => (asked.len(), asked.iter().map(|asked| asked.text.len()).sum::<usize>()),
            Phase::Running { open, .. } => open.session.queues().pending_size(),
            Phase::Idle { .. } => (0, 0),
        };
        count = count.saturating_add(self.heard.len());
        total = self
            .heard
            .iter()
            .fold(total, |total, message| total.saturating_add(message.content.len()));
        count < self.out.shared.max_pending_messages && bytes <= self.out.shared.max_pending_bytes.saturating_sub(total)
    }

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
        if !self.joined(incoming.addressee).await {
            return;
        }
        let answer = self.answers(&incoming);
        if answer {
            match incoming.text.trim() {
                CANCEL_COMMAND => return self.cancel().await,
                NEW_COMMAND => return self.renew().await,
                _ => {}
            }
        }
        let content = self.out.attributed(&incoming.sender, &incoming.text).await;
        if !self.has_capacity(content.len()) {
            self.out.post(self.out.replies().overloaded.clone()).await;
            self.out
                .failed("pending input capacity exceeded; message was not accepted".into());
            return;
        }
        if !answer {
            self.answering = None;
            self.heard.push(said(content));
            self.record_heard().await;
            return;
        }
        self.answering = Some(incoming.sender.clone());
        let asked = Asked {
            text: content,
            actor: Some(incoming.sender.0.clone()),
        };
        let steered = match &mut self.phase {
            Phase::Running { open, .. } => {
                open.session.queues().steer(asked.into_input());
                true
            }
            Phase::Waiting { asked: waiting, .. } => {
                waiting.push(asked);
                false
            }
            Phase::Idle { open, .. } => {
                let open = open.take();
                self.begin(open, asked, &incoming.message).await;
                false
            }
        };
        if steered {
            self.out.acknowledge(&incoming.message).await;
        }
    }

    /// Write the messages heard but not answered into the session; a run that holds it
    /// leaves them for its end. A released session is opened again for them.
    pub(super) async fn record_heard(&mut self) {
        if self.heard.is_empty() || matches!(self.phase, Phase::Running { .. }) {
            return;
        }
        if let Phase::Idle { open: None, .. } = self.phase {
            match self.out.open(None).await {
                Ok(opened) => self.phase = Phase::idle(Some(opened)),
                Err(error) => {
                    self.out.failed(format!(
                        "cannot open the session; {} message(s) not recorded: {error}",
                        self.heard.len()
                    ));
                    self.heard.clear();
                    return;
                }
            }
        }
        let session = match &self.phase {
            Phase::Waiting { open: Some(open), .. } | Phase::Idle { open: Some(open), .. } => open.session.clone(),
            // Opened above, and no run started since.
            Phase::Idle { open: None, .. } | Phase::Waiting { open: None, .. } | Phase::Running { .. } => return,
        };
        self.record_in(&session).await;
    }

    pub(super) async fn record_in(&mut self, session: &Session) {
        for message in std::mem::take(&mut self.heard) {
            if let Err(error) = session.append_custom_message(message).await {
                self.out.failed(format!("cannot record a message: {error}"));
            }
        }
    }
}
