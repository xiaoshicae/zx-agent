//! Built-in policies implemented through the same Hooks interface as SDK extensions.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::agent::ToolOutput;
use crate::agent::hooks::{HookResult, Hooks, TurnDecision, TurnDisposition, TurnSummary};
use crate::ai::{StopReason, ToolCall};
use crate::profile::read_text;
use crate::sync::lock;

/// The unattended continuation text; the runtime does not depend on this wording.
pub const CONTINUE_PROMPT: &str = "Your previous response was cut off at the output token limit. Continue exactly where it \
stopped, without repeating earlier text, and keep the rest concise.";

/// Stateless continuation policy. The runtime supplies a per-run continuation count.
pub struct ContinueOnLength {
    max_continuations: u32,
}

impl ContinueOnLength {
    pub fn new(max_continuations: u32) -> Self {
        Self { max_continuations }
    }
}

#[async_trait]
impl Hooks for ContinueOnLength {
    async fn finish_turn(&self, turn: &TurnSummary<'_>) -> HookResult<TurnDecision> {
        if turn.disposition == TurnDisposition::Stop
            && turn.response.stop_reason == StopReason::Length
            && turn.remaining_turns != Some(0)
            && turn.continuations < self.max_continuations
        {
            Ok(TurnDecision::ContinueAnswer(CONTINUE_PROMPT.to_string()))
        } else {
            Ok(TurnDecision::Default)
        }
    }
}

/// Earlier values read from secret files that stay redacted after the files change, oldest
/// dropped first; current values are always redacted. A rotated credential, such as a
/// ServiceAccount token, stays valid until it expires, so the values it replaced must stay
/// hidden as well; the bound keeps a long-running process from growing.
const MAX_FILE_SECRETS: usize = 32;

/// Redacts exact occurrences of configured secrets from tool output before persistence.
///
/// Empty secrets are ignored. Longer matches run first so overlapping secrets do not
/// leave a suffix visible. This does not discover unknown credentials or filter inputs.
pub struct RedactSecrets {
    secrets: Vec<String>,
    files: Vec<PathBuf>,
    /// Values read from `files`, newest last: the current ones and at most
    /// [`MAX_FILE_SECRETS`] earlier ones.
    file_secrets: Mutex<Vec<String>>,
}

impl RedactSecrets {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        Self {
            secrets: secrets.into_iter().filter(|secret| !secret.is_empty()).collect(),
            files: Vec::new(),
            file_secrets: Mutex::new(Vec::new()),
        }
    }

    /// Also redact the contents of `files`, each holding one secret such as the token of a
    /// Kubernetes ServiceAccount. A directory stands for every file directly inside it whose
    /// name does not start with a dot, so a whole Kubernetes Secret volume can be given and keys
    /// added to it later are covered. Every redaction reads them again, so a rotated value is
    /// covered and earlier values stay redacted; surrounding whitespace is ignored. A file
    /// that is missing, or that this process may not read, is skipped: commands running as
    /// the same user cannot read it either. Any other failure, such as a file that is not
    /// regular UTF-8 text within 64 KiB, fails the hook, which discards the tool output.
    pub fn with_secret_files(mut self, files: impl IntoIterator<Item = PathBuf>) -> Self {
        self.files.extend(files);
        self
    }

    /// The current values of the secret files, and earlier ones within [`MAX_FILE_SECRETS`].
    fn file_secrets(&self) -> Result<Vec<String>, String> {
        let mut current = Vec::new();
        for path in &self.files {
            for file in secret_files_at(path)? {
                // Small, bounded local reads, synchronous like the other configuration reads.
                match read_text(&file) {
                    Ok(text) => current.push(text.trim().to_string()),
                    Err(error) if is_unreadable(&error) => {}
                    Err(error) => return Err(format!("cannot read a secret file to redact tool output: {error:#}")),
                }
            }
        }
        current.retain(|value| !value.is_empty());
        let mut seen = lock(&self.file_secrets);
        for value in &current {
            if !seen.contains(value) {
                seen.push(value.clone());
            }
        }
        // Current values always stay; of the earlier ones, the oldest go first.
        let mut earlier = seen.iter().filter(|value| !current.contains(value)).count();
        seen.retain(|value| {
            let drop = earlier > MAX_FILE_SECRETS && !current.contains(value);
            if drop {
                earlier -= 1;
            }
            !drop
        });
        // A copy, so the lock is not held while the output is rewritten.
        Ok(seen.clone())
    }
}

/// The files to read for a registered path: the path itself, or for a directory the entries
/// directly inside it whose names do not start with a dot. That skips the `..data` links that
/// Kubernetes keeps in Secret volumes; subdirectories are skipped too. A directory that is
/// missing or that this process may not list yields nothing, like an unreadable file.
fn secret_files_at(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.is_dir() {
        // A file, or nothing at all: reading it decides, with the same rules as for files.
        return Ok(vec![path.to_path_buf()]);
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("cannot list secret files in {}: {error}", path.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot list secret files in {}: {error}", path.display()))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let file = entry.path();
        // is_dir follows links, as the entries of a Secret volume are links to the data.
        if !file.is_dir() {
            files.push(file);
        }
    }
    files.sort();
    Ok(files)
}

#[async_trait]
impl Hooks for RedactSecrets {
    async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput) -> HookResult<ToolOutput> {
        let file_secrets = self.file_secrets()?;
        let mut secrets: Vec<&str> = self.secrets.iter().chain(&file_secrets).map(String::as_str).collect();
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        for secret in secrets {
            output.content = output.content.replace(secret, "[REDACTED]");
        }
        Ok(output)
    }
}

/// Whether a secret file is missing or not readable by this process.
fn is_unreadable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| matches!(error.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied))
}
