//! Persistent, bounded tool results and a UUID-only paging tool.
//!
//! Install the spill hook after redaction. Stored text is the tool's already bounded
//! result, not data discarded by that tool's own truncation. Files are not auto-deleted.
//! When storing fails, the result stays inline: storage is an optimisation, not a filter.
//!
//! Only the agent stores results, but by default their directory lies in the working
//! directory, where commands write too, those in a container included. So nothing is read
//! or stored that a link there leads to: not a file that is one, and not a directory that
//! is reached through one below the working directory.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ai::{ToolCall, ToolDef};
use crate::text::prefix_within;
use crate::tools::{parse_args, unless_cancelled};
use zx_core::hooks::{Hook, HookContext, HookResult, HookStage};
use zx_core::tools::{Effects, Tool, ToolContext, ToolOutput};

/// Paging tool name; enabling spilling requires registering this tool too.
pub const TOOL_NAME: &str = "read_output";
const DEFAULT_PAGE_BYTES: usize = 8 * 1024;
const MAX_FILE_BYTES: usize = 1024 * 1024;
// Enough for the fixed footer, a UUID and any usize offset, including on 128-bit targets.
const FOOTER_BYTES: usize = 160;
/// Written into the output directory: stored results may contain sensitive data, and the
/// default directory lies inside the user's working tree.
const GITIGNORE: &str = "# Created by zx-agent: stored tool results may contain sensitive data.\n*\n";

/// Storage settings. The host maps its own notation to them, such as a zero threshold to
/// `None`.
#[derive(Clone, Debug)]
pub struct Config {
    /// Relative to the tool cwd unless the host already resolved an explicit path.
    pub directory: PathBuf,
    /// Results larger than this size spill; None disables writes, retaining paged reads.
    pub threshold_bytes: Option<usize>,
    /// Hard file size limit, at most 1 MiB. It applies to both writes and actual reads.
    pub max_file_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            directory: PathBuf::from(".zx-agent/outputs"),
            threshold_bytes: Some(DEFAULT_PAGE_BYTES),
            max_file_bytes: MAX_FILE_BYTES,
        }
    }
}

impl Config {
    /// Check the settings: the directory is named, the file size limit is within 1 MiB and
    /// the threshold, when there is one, is at least 256 bytes and within that limit. Errors
    /// name the field; the host adds its own name for the settings.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.directory.as_os_str().is_empty(), "directory must not be empty");
        ensure!(
            self.max_file_bytes > 0 && self.max_file_bytes <= MAX_FILE_BYTES,
            "max_file_bytes must be 1..={MAX_FILE_BYTES}"
        );
        if let Some(bytes) = self.threshold_bytes {
            ensure!(
                bytes >= 256 && bytes <= self.max_file_bytes,
                "threshold_bytes must be 256..=max_file_bytes, or unset to disable spilling"
            );
        }
        Ok(())
    }
}

/// The filesystem address and settings of the stored results. The writer hook and the
/// reading tool own independent copies; no open file or in-memory catalog is shared between
/// them.
#[derive(Clone)]
pub struct OutputStore {
    config: Config,
    /// The directory and the directories above it, up to the working directory and without
    /// it, outermost first; none for a directory outside the working directory. Commands
    /// write the working directory, so none of these may be a symbolic link.
    below_cwd: Vec<PathBuf>,
}

impl OutputStore {
    /// Resolve a relative directory against cwd and validate, without creating files.
    pub fn new(cwd: &Path, mut config: Config) -> Result<Self> {
        config.validate()?;
        config.directory = cwd.join(config.directory);
        let below_cwd = match config.directory.strip_prefix(cwd) {
            // A path that goes up again is not below the working directory.
            Ok(below) if !below.components().any(|part| part == Component::ParentDir) => below
                .components()
                .scan(cwd.to_path_buf(), |path, part| {
                    path.push(part);
                    Some(path.clone())
                })
                .collect(),
            Ok(_) | Err(_) => Vec::new(),
        };
        Ok(Self { config, below_cwd })
    }

    /// Refuse a directory that is reached through a symbolic link below the working
    /// directory. Whoever can write there, such as a command in a container that mounts the
    /// working directory, can put a link to another directory in the directory's place or
    /// above it; that directory is neither read nor written. A link at or above the working
    /// directory, or on the way to a directory configured outside it, is the user's own.
    async fn check_directory(&self) -> Result<(), String> {
        for path in &self.below_cwd {
            match tokio::fs::symlink_metadata(path).await {
                Ok(found) if found.file_type().is_symlink() => {
                    return Err(format!(
                        "output directory {} is reached through the symbolic link {}; nothing is read or stored \
                         through it",
                        self.config.directory.display(),
                        path.display()
                    ));
                }
                Ok(_) => {}
                // Missing, or not to be inspected: no link leads on from here. What is missing
                // is created as a directory, and creating or opening says what else is wrong.
                Err(_) => return Ok(()),
            }
        }
        Ok(())
    }

    /// Create the directory with its ignore file.
    async fn prepare_directory(&self) -> Result<(), String> {
        let directory = &self.config.directory;
        // Before creating: creating goes through a link that is in the way, and would make
        // the missing directories where it leads.
        self.check_directory().await?;
        let mut builder = tokio::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder
            .create(directory)
            .await
            .map_err(|error| format!("cannot create output directory {}: {error}", directory.display()))?;
        let ignore = directory.join(".gitignore");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&ignore).await {
            Ok(mut file) => file
                .write_all(GITIGNORE.as_bytes())
                .await
                .map_err(|error| format!("cannot write {}: {error}", ignore.display())),
            // Keep an existing file: the user may have edited it.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(format!("cannot create {}: {error}", ignore.display())),
        }
    }

    /// Store `text` in a new file and return its output ID.
    async fn write(&self, text: &str) -> HookResult<String> {
        if text.len() > self.config.max_file_bytes {
            return Err(format!(
                "tool result is {} bytes, over the {} byte file limit",
                text.len(),
                self.config.max_file_bytes
            ));
        }
        self.prepare_directory().await?;
        let id = uuid::Uuid::new_v4().to_string();
        let path = self.config.directory.join(format!("{id}.txt"));
        let mut options = tokio::fs::OpenOptions::new();
        // A new file only: nothing already at the path is written to, a link least of all.
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&path)
            .await
            .map_err(|error| format!("cannot create tool output {}: {error}", path.display()))?;
        let written = match file.write_all(text.as_bytes()).await {
            Ok(()) => file.flush().await,
            Err(error) => Err(error),
        };
        drop(file);
        if let Err(error) = written {
            // Best effort: the write already failed and the partial file is unreferenced.
            let _ = tokio::fs::remove_file(&path).await;
            return Err(format!("cannot write tool output {}: {error}", path.display()));
        }
        // Cancellation may leave an unreferenced partial file. No ID is returned until the
        // complete write succeeds; UUID names are never reused or overwritten.
        Ok(id)
    }

    fn path(&self, id: &str) -> Result<PathBuf, String> {
        let parsed = uuid::Uuid::parse_str(id)
            .map_err(|error| format!("invalid output ID: {error}; use the id from the footer of a tool result"))?;
        if parsed.to_string() != id {
            return Err("output ID must be a canonical UUID from the footer of a tool result".into());
        }
        Ok(self.config.directory.join(format!("{id}.txt")))
    }

    async fn read(&self, path: &Path) -> Result<String, String> {
        let unreadable = |error: &dyn std::fmt::Display| format!("cannot read tool output {}: {error}", path.display());
        self.check_directory().await.map_err(|error| unreadable(&error))?;
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true);
        // A link named like an output would lead to any file this process can read, and
        // whoever can write the directory can put one there: links are not followed. Without
        // blocking, so that a FIFO put there is refused below instead of waited for.
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(path).await.map_err(|error| unreadable(&error))?;
        let meta = file
            .metadata()
            .await
            .map_err(|error| format!("cannot inspect tool output {}: {error}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("tool output {} is not a regular file", path.display()));
        }
        let max = u64::try_from(self.config.max_file_bytes)
            .map_err(|error| format!("output byte limit is out of range: {error}"))?;
        if meta.len() > max {
            return Err(format!("tool output exceeds the {max} byte file limit"));
        }
        let mut bytes = Vec::new();
        file.take(max + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| format!("cannot read tool output {}: {error}", path.display()))?;
        if bytes.len() > self.config.max_file_bytes {
            return Err(format!("tool output grew over the {max} byte file limit"));
        }
        String::from_utf8(bytes).map_err(|error| format!("tool output is not UTF-8: {error}"))
    }

    /// One page of the stored result `id`: the start of `rest`, its text from `offset` on,
    /// and, when more follows, a footer that says how to read on. A page with its footer
    /// fits the threshold.
    fn page(&self, id: &str, rest: &str, offset: usize) -> String {
        let limit = self.config.threshold_bytes.unwrap_or(DEFAULT_PAGE_BYTES) - FOOTER_BYTES;
        let page = prefix_within(rest, limit);
        let mut content = page.to_string();
        if page.len() < rest.len() {
            let next = offset + page.len();
            content.push_str(&format!(
                "\n[Use read_output with id={id:?}, offset={next} to continue.]"
            ));
        }
        content
    }
}

/// How long storing one result may take before the result stays inline instead. It is under
/// the default hook timeout, so a slow disk keeps the result rather than losing it to that
/// timeout.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// The hook that stores a long tool result as a file and leaves its first page in its
/// place, as [`ReadOutputTool`] would return it: with a footer that says how to read on.
///
/// Register it after secret filtering, and exempt the tools that page their own output, so
/// that a page is not stored again; the hook chain bounds and cancels its I/O.
pub struct SpillOutputs {
    store: OutputStore,
    exempt_tools: Vec<String>,
    write_timeout: Duration,
}

impl SpillOutputs {
    /// The host must also register a [`ReadOutputTool`] with a copy of the same store.
    pub fn new(store: OutputStore, exempt_tools: Vec<String>) -> Self {
        Self {
            store,
            exempt_tools,
            write_timeout: WRITE_TIMEOUT,
        }
    }

    /// Keep a result inline when storing it takes longer than `timeout` (3 seconds by
    /// default). Keep it below the timeout this hook is registered with (5 seconds by
    /// default): when that one fires first, the tool's output is withheld. A write that runs
    /// out of time may still finish later and leave its file behind.
    pub fn write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }
}

#[async_trait]
impl Hook for SpillOutputs {
    /// After redaction, so that saved output contains no known secret.
    fn stage(&self) -> HookStage {
        HookStage::Storage
    }

    async fn after_tool(&self, call: &ToolCall, mut output: ToolOutput, _ctx: &HookContext) -> HookResult<ToolOutput> {
        if !self.exempt_tools.contains(&call.name)
            && self
                .store
                .config
                .threshold_bytes
                .is_some_and(|limit| output.content.len() > limit)
        {
            let stored = match tokio::time::timeout(self.write_timeout, self.store.write(&output.content)).await {
                Ok(stored) => stored,
                Err(_) => Err(format!("storing took longer than {:?}", self.write_timeout)),
            };
            match stored {
                // The first page, not only a reference: the model sees how the result starts
                // without another request, and reads on only when it needs the rest.
                Ok(id) => output.content = self.store.page(&id, &output.content, 0),
                // This hook runs after redaction, so the bounded result is already what the
                // session would record without spilling; hiding it would only lose evidence.
                Err(reason) => output
                    .content
                    .push_str(&format!("\n[not stored: {reason}; the result is shown inline]")),
            }
        }
        Ok(output)
    }
}

/// Reads only UUID-named regular files inside the configured output directory, without
/// following a symbolic link. Offsets and pages are UTF-8 bytes; one result, including its
/// continuation footer, fits the threshold.
pub struct ReadOutputTool {
    store: OutputStore,
}

impl ReadOutputTool {
    /// Use a copy of the writer hook's store, and the same settings across resumed sessions.
    pub fn new(store: OutputStore) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    id: String,
    #[serde(default)]
    offset: usize,
}

#[async_trait]
impl Tool for ReadOutputTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn effects(&self) -> Effects {
        Effects::ReadOnly
    }
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: TOOL_NAME.into(),
            description: "Read on in a tool result that was too long to show whole. Such a result ends with a \
                          footer that names its id and the UTF-8 byte offset of the next page; pass both. Only \
                          results this agent stored are readable."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The id in the footer of the tool result" },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The offset in the footer; 0, the default, reads from the start"
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        let args: Args = parse_args(args)?;
        self.store.path(&args.id).map(|_| ())
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let args: Args = parse_args(&args)?;
        let path = self.store.path(&args.id)?;
        let read = unless_cancelled(ctx, self.store.read(&path)).await;
        let text = read.ok_or_else(|| "output read cancelled".to_string())??;
        let rest = text.get(args.offset..).ok_or_else(|| {
            "output offset is out of range or inside a UTF-8 character; use the returned offset".to_string()
        })?;
        Ok(ToolOutput::ok(self.store.page(&args.id, rest, args.offset)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    use tokio_util::sync::CancellationToken;

    use super::*;

    #[tokio::test]
    async fn a_named_pipe_in_place_of_an_output_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let reader = ReadOutputTool::new(OutputStore::new(dir.path(), Config::default()).unwrap());
        let outputs = dir.path().join(".zx-agent/outputs");
        std::fs::create_dir_all(&outputs).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = outputs.join(format!("{id}.txt"));
        assert!(
            tokio::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .await
                .unwrap()
                .success()
        );
        let writer_path = path.clone();
        // A reader that waits for a writer gets this one and then reads what it wrote, so
        // that such a reader fails the test instead of blocking it.
        let writer = tokio::task::spawn_blocking(move || {
            let mut file = std::fs::OpenOptions::new().write(true).open(writer_path).unwrap();
            file.write_all(b"pipe contents")
        });
        let ctx = ToolContext::new(dir.path().to_path_buf(), CancellationToken::new());

        let result = reader.execute(json!({"id": id}), &ctx).await;

        // Release the writer if the pipe was not open for reading long enough for it.
        let _unblock = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        // A broken pipe is expected when the refusal closed the pipe first.
        let _ = writer.await.unwrap();
        let error = result.unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }
}
