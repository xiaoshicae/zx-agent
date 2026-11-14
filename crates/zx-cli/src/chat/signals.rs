//! Persistent signal registrations: cancelling one prompt leaves the REPL running.

use std::io;
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};

pub(super) enum Interrupt {
    Cancel,
    Terminate,
}

pub(super) struct Signals {
    #[cfg(unix)]
    interrupt: Signal,
    #[cfg(unix)]
    terminate: Signal,
    #[cfg(not(unix))]
    interrupt: tokio::signal::windows::CtrlC,
}

impl Signals {
    pub(super) fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    pub(super) async fn next(&mut self) -> Interrupt {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Interrupt::Cancel,
                _ = self.terminate.recv() => Interrupt::Terminate,
            }
        }
        #[cfg(not(unix))]
        {
            match self.interrupt.recv().await {
                Some(()) => Interrupt::Cancel,
                None => Interrupt::Terminate,
            }
        }
    }
}
