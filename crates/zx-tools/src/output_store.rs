//! Persistent, bounded tool results and a UUID-only paging tool.
//!
//! Install the spill hook after redaction. Stored text is the tool's already bounded
//! result, not data discarded by that tool's own truncation. Files are not auto-deleted.
//! When storing fails, the result stays inline: storage is an optimisation, not a filter.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agent::{Effects, HookContext, HookResult, HookStage, Hooks, Tool, ToolContext, ToolOutput};
use crate::ai::{ToolCall, ToolDef};
use crate::text::prefix_within;
use crate::tools::{parse_args, unless_cancelled};

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

/// Immutable filesystem address/configuration. The writer hook and reading tool own
/// independent copies; no open file or mutable in-memory catalog is shared between them.
#[derive(Clone)]
pub struct OutputStore {
    config: Config,
}

impl OutputStore {
    /// Resolve a relative directory against cwd and validate, without creating files.
    pub fn new(cwd: &Path, mut config: Config) -> Result<Self> {
        config.validate()?;
        config.directory = cwd.join(config.directory);
        Ok(Self { config })
    }

    /// Create the directory with its ignore file.
    async fn prepare_directory(&self) -> Result<(), String> {
        let directory = &self.config.directory;
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
        // Cancellation may leave an unreferenced partial file. No reference is returned
        // until the complete write succeeds; UUID names are never reused or overwritten.
        Ok(json!({"outputId":id,"path":path.to_string_lossy(),"bytes":text.len(),"readWith":TOOL_NAME}).to_string())
    }

    fn path(&self, id: &str) -> Result<PathBuf, String> {
        let parsed = uuid::Uuid::parse_str(id)
            .map_err(|error| format!("invalid output ID: {error}; use outputId from a tool result"))?;
        if parsed.to_string() != id {
            return Err("output ID must be a canonical UUID from a tool result".into());
        }
        Ok(self.config.directory.join(format!("{id}.txt")))
    }

    async fn read(&self, path: &Path) -> Result<String, String> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|error| format!("cannot read tool output {}: {error}", path.display()))?;
        let meta = file
            .metadata()
            .await
            .map_err(|error| format!("cannot inspect tool output {}: {error}", path.display()))?;
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
}

/// How long storing one result may take before the result stays inline instead. It is under
/// the default hook timeout, so a slow disk keeps the result rather than losing it to that
/// timeout.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// The hook that stores a long tool result as a file and leaves a reference in its place.
///
/// Register it after secret filtering, and exempt the paging tools so that a page does not
/// become another file reference; the hook chain bounds and cancels its I/O.
pub struct SpillOutputs {
    store: OutputStore,
    exempt_tools: Vec<String>,
    write_timeout: Duration,
}

impl SpillOutputs {
    /// The host must also register ReadOutputTool with the same store configuration.
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
impl Hooks for SpillOutputs {
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
                Ok(reference) => output.content = reference,
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

/// Reads only UUID-named files inside the configured output directory. Offsets and
/// pages are UTF-8 bytes; one result, including its continuation footer, fits the threshold.
pub struct ReadOutputTool {
    store: OutputStore,
}

impl ReadOutputTool {
    /// Use the same store as the writer hook, including across resumed sessions.
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
            description: "Read a persisted tool result by outputId (UUID). Use the returned UTF-8 byte offset for \
                          the next page. Only files in the configured output directory are readable."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "outputId from a tool result" },
                    "offset": { "type": "integer", "minimum": 0 }
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
        let remaining = text.get(args.offset..).ok_or_else(|| {
            "output offset is out of range or inside a UTF-8 character; use the returned offset".to_string()
        })?;
        let limit = self.store.config.threshold_bytes.unwrap_or(DEFAULT_PAGE_BYTES) - FOOTER_BYTES;
        let page = prefix_within(remaining, limit);
        let mut content = page.to_string();
        let next = args.offset + page.len();
        if next < text.len() {
            content.push_str(&format!(
                "\n[Use read_output with id={:?}, offset={next} to continue.]",
                args.id
            ));
        }
        Ok(ToolOutput::ok(content))
    }
}
