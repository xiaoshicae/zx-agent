//! Built-in policies implemented through the same Hooks interface as SDK extensions.

use std::io::ErrorKind;
use std::path::PathBuf;
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

/// Values read from secret files that stay redacted, oldest dropped first. A rotated
/// credential, such as a ServiceAccount token, stays valid until it expires, so the values
/// it replaced must stay hidden as well; the bound keeps a long-running process from growing.
const MAX_FILE_SECRETS: usize = 32;

/// Redacts exact occurrences of configured secrets from tool output before persistence.
///
/// Empty secrets are ignored. Longer matches run first so overlapping secrets do not
/// leave a suffix visible. This does not discover unknown credentials or filter inputs.
pub struct RedactSecrets {
    secrets: Vec<String>,
    files: Vec<PathBuf>,
    /// Values read from `files`, newest last, at most [`MAX_FILE_SECRETS`].
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
    /// Kubernetes ServiceAccount. Every redaction reads them again, so a rotated value is
    /// covered and earlier values stay redacted; surrounding whitespace is ignored. A file
    /// that is missing, or that this process may not read, is skipped: commands running as
    /// the same user cannot read it either. Any other failure, such as a file that is not
    /// regular UTF-8 text within 64 KiB, fails the hook, which discards the tool output.
    pub fn with_secret_files(mut self, files: impl IntoIterator<Item = PathBuf>) -> Self {
        self.files.extend(files);
        self
    }

    /// The current and earlier values of the secret files.
    fn file_secrets(&self) -> Result<Vec<String>, String> {
        let mut current = Vec::new();
        for path in &self.files {
            // Small, bounded local reads, synchronous like the other configuration reads.
            match read_text(path) {
                Ok(text) => current.push(text.trim().to_string()),
                Err(error) if is_unreadable(&error) => {}
                Err(error) => return Err(format!("cannot read a secret file to redact tool output: {error:#}")),
            }
        }
        let mut seen = lock(&self.file_secrets);
        for value in current {
            if !value.is_empty() && !seen.contains(&value) {
                seen.push(value);
            }
        }
        while seen.len() > MAX_FILE_SECRETS {
            seen.remove(0);
        }
        // A copy, so the lock is not held while the output is rewritten.
        Ok(seen.clone())
    }
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
