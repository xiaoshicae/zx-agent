//! Persistent, bounded tool results and a UUID-only paging tool.
//!
//! The spill hook stores what redaction left: it runs at the storage stage, after redaction,
//! however it is registered. Stored text is the tool's already bounded result, not data
//! discarded by that tool's own truncation. Files are not auto-deleted.
//! When storing fails, the result stays inline: storage is an optimisation, not a filter.
//!
//! Only the agent stores results, but the host may put their directory in the working
//! directory, where commands write too, those in a container included. So nothing is read
//! or stored that a link there leads to: not a file that is one, and not a directory that
//! is reached through one below the working directory.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::ai::{ToolCall, ToolDef};
use crate::tools::paging::{Links, Pager, read_text};
use crate::tools::{READ_OUTPUT, parse_args, unless_cancelled};
use zx_core::hooks::{Hook, HookContext, HookResult, HookStage};
use zx_core::tools::{Effects, Tool, ToolContext, ToolOutput};

/// The largest stored result, and so the largest [`OutputStoreConfig::max_file_bytes`].
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// The smallest [`OutputStoreConfig::threshold_bytes`]: a page holds the threshold, footer
/// included, and the footer alone takes about a hundred bytes.
pub const MIN_THRESHOLD_BYTES: usize = 256;
/// The threshold by default, and the size of a page when nothing is stored.
const DEFAULT_PAGE_BYTES: usize = 8 * 1024;
/// Written into the output directory when the store creates it: stored results may contain
/// sensitive data, and the directory may lie inside a working tree under version control.
const GITIGNORE: &str = "# Stored tool results may contain sensitive data.\n*\n";

/// How results are stored: where, from what size, and up to what size. The host chooses the
/// directory; build the settings with [`OutputStoreConfig::new`] and struct update syntax.
#[derive(Clone, Debug)]
pub struct OutputStoreConfig {
    /// Where the results are stored; a relative path is relative to the working directory of
    /// the session ([`OutputStore::new`]).
    pub directory: PathBuf,
    /// Results larger than this size are stored; `None` stores nothing, while results stored
    /// before can still be read.
    pub threshold_bytes: Option<usize>,
    /// Hard file size limit, at most [`MAX_FILE_BYTES`]. It applies to both writes and actual
    /// reads.
    pub max_file_bytes: usize,
}

impl OutputStoreConfig {
    /// Settings that store results in `directory`: those over 8 KiB, up to 1 MiB each.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            threshold_bytes: Some(DEFAULT_PAGE_BYTES),
            max_file_bytes: MAX_FILE_BYTES,
        }
    }

    /// Check the settings: the directory is named, the file size limit is within
    /// [`MAX_FILE_BYTES`] and the threshold, when there is one, is at least
    /// [`MIN_THRESHOLD_BYTES`] and within that limit.
    pub fn validate(&self) -> Result<(), InvalidOutputStoreConfig> {
        if self.directory.as_os_str().is_empty() {
            return Err(InvalidOutputStoreConfig::EmptyDirectory);
        }
        if self.max_file_bytes == 0 || self.max_file_bytes > MAX_FILE_BYTES {
            return Err(InvalidOutputStoreConfig::MaxFileBytes(self.max_file_bytes));
        }
        if let Some(threshold) = self.threshold_bytes
            && !(MIN_THRESHOLD_BYTES..=self.max_file_bytes).contains(&threshold)
        {
            return Err(InvalidOutputStoreConfig::ThresholdBytes {
                threshold,
                max_file_bytes: self.max_file_bytes,
            });
        }
        Ok(())
    }
}

/// A setting of an [`OutputStoreConfig`] that cannot be used. `Display` names the field as the
/// struct does; a host that reads the settings from a file of its own matches the variant to
/// name its own key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidOutputStoreConfig {
    /// `directory` is empty.
    EmptyDirectory,
    /// `max_file_bytes`, given here, is zero or over [`MAX_FILE_BYTES`].
    MaxFileBytes(usize),
    /// `threshold_bytes` is under [`MIN_THRESHOLD_BYTES`] or over `max_file_bytes`.
    ThresholdBytes { threshold: usize, max_file_bytes: usize },
}

impl fmt::Display for InvalidOutputStoreConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDirectory => f.write_str("directory is empty"),
            Self::MaxFileBytes(bytes) => write!(f, "max_file_bytes is {bytes}; it must be 1..={MAX_FILE_BYTES}"),
            Self::ThresholdBytes {
                threshold,
                max_file_bytes,
            } => write!(
                f,
                "threshold_bytes is {threshold}; it must be {MIN_THRESHOLD_BYTES}..={max_file_bytes} (max_file_bytes), or None to store nothing"
            ),
        }
    }
}

impl std::error::Error for InvalidOutputStoreConfig {}

/// The filesystem address and settings of the stored results. The writer hook and the
/// reading tool own independent copies; no open file or in-memory catalog is shared between
/// them.
#[derive(Clone)]
pub struct OutputStore {
    config: OutputStoreConfig,
    /// The directory and the directories above it, up to the working directory and without
    /// it, outermost first; none for a directory outside the working directory. Commands
    /// write the working directory, so none of these may be a symbolic link.
    below_cwd: Vec<PathBuf>,
}

impl OutputStore {
    /// The store of a session that works in `cwd`, against which a relative directory is
    /// resolved; checks the settings and creates no files. Errors: settings that
    /// [`OutputStoreConfig::validate`] refuses.
    pub fn new(cwd: &Path, mut config: OutputStoreConfig) -> Result<Self, InvalidOutputStoreConfig> {
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

    /// The stored result at `path`, read without following a link: whoever can write the
    /// directory can put one there, and it would lead to any file this process can read.
    async fn read(&self, path: &Path) -> Result<String, String> {
        self.check_directory().await?;
        let max = u64::try_from(self.config.max_file_bytes)
            .map_err(|error| format!("output byte limit is out of range: {error}"))?;
        read_text(path, max, Links::Refuse).await
    }

    /// How the stored result `id` is paged: a page with its footer fits the threshold.
    fn pager<'a>(&self, id: &'a str) -> Pager<'a> {
        Pager {
            tool: READ_OUTPUT,
            argument: ("id", id),
            max_bytes: self.config.threshold_bytes.unwrap_or(DEFAULT_PAGE_BYTES),
        }
    }
}

/// How long storing one result may take before the result stays inline instead. It is under
/// the default hook timeout, so a slow disk keeps the result rather than losing it to that
/// timeout.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// The hook that stores a long tool result as a file and leaves its first page in its
/// place, as [`ReadOutputTool`] would return it: with a footer that says how to read on.
///
/// Exempt the tools that page their own output, so that a page is not stored again. It runs
/// after secret filtering by its stage, and the hook chain bounds and cancels its I/O.
pub struct SpillOutputs {
    store: OutputStore,
    exempt_tools: Vec<String>,
    write_timeout: Duration,
}

impl SpillOutputs {
    /// The hook that stores in `store`, and the [`ReadOutputTool`] that pages what it stores:
    /// the host registers both, since the first page of a stored result tells the model to
    /// read on with that tool. `exempt_tools` are the tools whose results are never stored.
    pub fn with_reader(store: OutputStore, exempt_tools: Vec<String>) -> (Self, ReadOutputTool) {
        let hook = Self {
            store: store.clone(),
            exempt_tools,
            write_timeout: WRITE_TIMEOUT,
        };
        (hook, ReadOutputTool::new(store))
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
                Ok(id) => output.content = self.store.pager(&id).page(&output.content, 0)?,
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
/// following a symbolic link. Offsets are UTF-8 bytes; pages are whole lines where they fit
/// and, with their continuation footer, fit the threshold.
pub struct ReadOutputTool {
    store: OutputStore,
}

impl ReadOutputTool {
    /// A reader of what was stored in `store` before, without the hook that stores; the two
    /// together come from [`SpillOutputs::with_reader`]. Keep the same settings across
    /// resumed sessions.
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
        READ_OUTPUT
    }
    fn effects(&self) -> Effects {
        Effects::ReadOnly
    }
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: READ_OUTPUT.into(),
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
        let text = read
            .ok_or_else(|| "output read cancelled".to_string())?
            .map_err(|error| format!("cannot read tool output: {error}"))?;
        Ok(ToolOutput::ok(self.store.pager(&args.id).page(&text, args.offset)?))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_support::make_fifo;

    #[tokio::test]
    async fn a_named_pipe_in_place_of_an_output_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let reader = ReadOutputTool::new(OutputStore::new(dir.path(), OutputStoreConfig::new("outputs")).unwrap());
        let outputs = dir.path().join("outputs");
        std::fs::create_dir_all(&outputs).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = outputs.join(format!("{id}.txt"));
        make_fifo(&path);
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
