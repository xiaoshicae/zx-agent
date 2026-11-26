//! The lists of im.toml, read again while the program runs: on SIGHUP, its `[access]`
//! replaces the server's lists, once its server's sections pass the checks of a start. A
//! replica that waits for its turn to serve reloads too, and serves with what it read. What
//! chats have of their own stays as the start read it: a chat the new lists no longer hold
//! only leaves its settings unused, in the replica that leads and in the one that takes over.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::config_file::read_file;
use super::log;
use crate::Access;

/// Read im.toml at `path` again on each SIGHUP until `cancel`, and give `access` its lists;
/// the configured `platforms` stay those the server runs. The signal is listened for before
/// this returns, so that a SIGHUP from then on reloads rather than ends the process. A file
/// that cannot be read, or whose server's sections fail the checks, leaves the lists as they
/// are, and the log says why. Without a way to listen for the signal, that is logged, nothing
/// reloads and there is no task.
pub(super) fn on_hangup(
    path: PathBuf,
    platforms: Vec<String>,
    access: Access,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    match signal(SignalKind::hangup()) {
        Ok(hangups) => Some(tokio::spawn(reload_on(hangups, path, platforms, access, cancel))),
        Err(error) => {
            log::failed(&format!(
                "cannot listen for SIGHUP; {} is read again only by a restart: {error}",
                path.display()
            ));
            None
        }
    }
}

/// Reload the file at `path` on each of `hangups`, until `cancel`.
async fn reload_on(
    mut hangups: Signal,
    path: PathBuf,
    platforms: Vec<String>,
    access: Access,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            received = hangups.recv() => {
                if received.is_none() {
                    return;
                }
            }
        }
        // A small, bounded read of the file, as rare as the signal.
        reload(&path, &platforms, &access);
    }
}

/// Read im.toml at `path` again and give `access` its lists, or log why not.
fn reload(path: &Path, platforms: &[String], access: &Access) {
    let names: HashSet<&str> = platforms.iter().map(String::as_str).collect();
    match read_file(path).and_then(|file| file.server_sections(&names)) {
        Ok(sections) => {
            log::reloaded(sections.conversations.len(), sections.approvers.len());
            access.replace(sections.conversations, sections.approvers);
        }
        Err(error) => log::failed(&format!(
            "cannot reload {}; the lists stay as they were: {error:#}",
            path.display()
        )),
    }
}
