//! The hook that redacts known secrets from tool output before it is recorded. It matches
//! exact values only; it does not discover unknown credentials or filter inputs (the
//! boundaries are in the README's security model).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{HookContext, HookResult, HookStage, Hooks, ToolOutput};
use crate::ai::ToolCall;
use crate::sync::lock;
use crate::text::read_regular_file;

/// The largest secret file read, such as a token: anything bigger is not a single secret, and
/// it could never be matched whole in the bounded output of a built-in tool.
const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;

/// Earlier values read from secret files that stay redacted after the files change, oldest
/// dropped first; current values are always redacted. A rotated credential, such as a
/// ServiceAccount token, stays valid until it expires, so the values it replaced must stay
/// hidden as well; the bound keeps a long-running process from growing.
const MAX_FILE_SECRETS: usize = 32;

/// Redacts exact occurrences of configured secrets from tool output before persistence:
/// from the content, and from the strings and object keys of the details. Two keys that
/// differ only by a secret become one, and numbers in the details are not redacted.
///
/// Empty secrets are ignored. Longer matches run first so overlapping secrets do not
/// leave a suffix visible.
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
    /// covered and earlier values stay redacted; surrounding whitespace is ignored. Content
    /// that is not UTF-8 is matched as the built-in tools print it, decoded lossily. A file
    /// that is missing, or that this process may not read, is skipped: commands running as
    /// the same user cannot read it either. Any other failure, such as a file over 64 KiB or
    /// one that is not a regular file, fails the hook, which discards the tool output; check
    /// the files with [`check_secret_files`] before use to report those at startup.
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
                match read_regular_file(&file, MAX_SECRET_FILE_BYTES) {
                    // The built-in tools decode output the same way, so a value that is not
                    // UTF-8 appears in their output exactly like this.
                    Ok(bytes) => current.push(String::from_utf8_lossy(&bytes).trim().to_string()),
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
    /// After every extension has changed the output, so none can bring a secret back.
    fn stage(&self) -> HookStage {
        HookStage::Redaction
    }

    async fn after_tool(&self, _call: &ToolCall, mut output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        let file_secrets = self.file_secrets()?;
        let mut secrets: Vec<&str> = self.secrets.iter().chain(&file_secrets).map(String::as_str).collect();
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        output.content = redact_text(&output.content, &secrets);
        if let Some(details) = &mut output.details {
            redact_json(details, &secrets);
        }
        Ok(output)
    }
}

/// Replace each of `secrets`, longest first as given, with a marker.
fn redact_text(text: &str, secrets: &[&str]) -> String {
    let mut text = text.to_string();
    for secret in secrets {
        if text.contains(secret) {
            text = text.replace(secret, "[REDACTED]");
        }
    }
    text
}

fn redact_json(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::String(text) => *text = redact_text(text, secrets),
        Value::Array(items) => items.iter_mut().for_each(|item| redact_json(item, secrets)),
        Value::Object(map) => {
            *map = std::mem::take(map)
                .into_iter()
                .map(|(key, mut item)| {
                    redact_json(&mut item, secrets);
                    (redact_text(&key, secrets), item)
                })
                .collect();
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Check before use that the secret files registered at `paths`, given as to
/// [`RedactSecrets::with_secret_files`], can be redacted: each must be a regular file of at
/// most 64 KiB. A larger file could never be matched whole in the output of a built-in tool,
/// and it usually holds structured data, whose single values should be registered instead.
/// Content that is not UTF-8 is fine. Files that are missing or that this process may not
/// read are not errors here: the redaction skips them. Returns one message naming every file
/// that cannot be used; a file that only becomes unusable later fails the redaction instead.
pub fn check_secret_files(paths: &[PathBuf]) -> Result<(), String> {
    let mut problems = Vec::new();
    for path in paths {
        for file in secret_files_at(path)? {
            match read_regular_file(&file, MAX_SECRET_FILE_BYTES) {
                Ok(_) => {}
                Err(error) if is_unreadable(&error) => {}
                Err(error) => problems.push(format!("{error:#}")),
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "cannot redact these secret files from tool output: {}; register files that each hold a single value of at most {MAX_SECRET_FILE_BYTES} bytes",
            problems.join("; ")
        ))
    }
}

/// Values shorter than this many characters are still redacted, but every occurrence of the
/// same text is replaced too, wherever it appears. GitLab masks only values of at least 8
/// characters (<https://docs.gitlab.com/ci/variables/>).
pub const MIN_SECRET_CHARS: usize = 8;

/// The files at `paths`, given as to [`RedactSecrets::with_secret_files`], whose value is
/// shorter than [`MIN_SECRET_CHARS`] characters, for a notice at startup: such a value is still
/// redacted, and so is every other occurrence of the same text in tool output. Blank values,
/// which redact nothing, are left out, as are files that cannot be read: [`check_secret_files`]
/// reports those.
pub fn short_secret_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut short = Vec::new();
    for path in paths {
        // A directory that cannot be listed holds no value to report; check_secret_files
        // reports the failure.
        let Ok(files) = secret_files_at(path) else {
            continue;
        };
        for file in files {
            // Likewise for a file that cannot be read.
            let Ok(bytes) = read_regular_file(&file, MAX_SECRET_FILE_BYTES) else {
                continue;
            };
            let chars = String::from_utf8_lossy(&bytes).trim().chars().count();
            if chars > 0 && chars < MIN_SECRET_CHARS {
                short.push(file);
            }
        }
    }
    short
}

/// Whether a secret file is missing or not readable by this process.
fn is_unreadable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| matches!(error.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied))
}
