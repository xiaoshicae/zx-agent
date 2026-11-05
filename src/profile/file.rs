//! The TOML layer of a profile: what a file may set, and how one layer goes over another.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, ensure};
use serde::Deserialize;

use super::skills::Skill;
use super::{Builtin, Execution, MAX_FILE_BYTES, McpServer, Profile};
use crate::compaction;
use crate::executor::Access;
use crate::files::read_text_file;
use crate::policy::ProgramOptions;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RawProfile {
    name: Option<String>,
    pub(super) extends: Option<Builtin>,
    pub(super) instructions: Option<PathBuf>,
    tools: Option<Vec<String>>,
    // Unknown backends fail to deserialize instead of being ignored.
    execution: Option<Execution>,
    container: RawContainer,
    rules: RawRules,
    limits: RawLimits,
    extensions: RawExtensions,
    context: RawContext,
    skills: Option<Vec<Skill>>,
    compaction: RawCompaction,
    tool_output: RawToolOutput,
    mcp: Option<Vec<McpServer>>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawCompaction {
    trigger_bytes: Option<usize>,
    keep_recent_bytes: Option<usize>,
    max_summary_tokens: Option<u32>,
    timeout_secs: Option<u64>,
    /// `full` (the default) or `incremental`.
    mode: Option<compaction::SummaryMode>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawToolOutput {
    directory: Option<PathBuf>,
    threshold_bytes: Option<usize>,
    max_file_bytes: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawContainer {
    image: Option<String>,
    user: Option<String>,
    memory: Option<String>,
    pids_limit: Option<u32>,
    network: Option<String>,
    workspace: Option<Access>,
    root: Option<Access>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawRules {
    allow: Option<Vec<String>>,
    deny: Vec<String>,
    auto_approve: Option<Vec<String>>,
    auto_approve_except: Vec<String>,
    auto_approve_options: BTreeMap<String, ProgramOptions>,
    ask_tools: Option<Vec<String>>,
    deny_tools: Vec<String>,
    approve_all: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawLimits {
    max_model_requests: Option<ModelRequestLimit>,
    timeout_secs: Option<u64>,
    /// 0 disables a budget that an earlier layer set.
    token_budget: Option<u64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ModelRequestLimit {
    Count(u32),
    Unlimited(Unlimited),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Unlimited {
    Unlimited,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawExtensions {
    redaction: Option<bool>,
    continue_on_length: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawContext {
    discover: Option<bool>,
    files: Vec<PathBuf>,
}

impl RawProfile {
    /// Lay this file's values over `profile`. Paths are resolved against `parent`, the
    /// file's directory; lists marked as accumulating in [`super::Rules`] are appended, the
    /// other values replaced.
    pub(super) fn apply(self, profile: &mut Profile, parent: &Path) -> Result<()> {
        if let Some(name) = self.name {
            profile.name = name;
        }
        if let Some(path) = self.instructions {
            profile.instructions = read_text(&parent.join(path))?;
        }
        if let Some(tools) = self.tools {
            profile.tools = tools;
        }
        if let Some(allow) = self.rules.allow {
            profile.rules.allow = allow;
        }
        profile.rules.deny.extend(self.rules.deny);
        if let Some(prefixes) = self.rules.auto_approve {
            profile.rules.auto_approve = prefixes;
        }
        profile.rules.auto_approve_except.extend(self.rules.auto_approve_except);
        profile
            .rules
            .auto_approve_options
            .extend(self.rules.auto_approve_options);
        if let Some(tools) = self.rules.ask_tools {
            profile.rules.ask_tools = tools;
        }
        profile.rules.deny_tools.extend(self.rules.deny_tools);
        if let Some(approve_all) = self.rules.approve_all {
            profile.rules.approve_all = approve_all;
        }
        if let Some(turns) = self.limits.max_model_requests {
            profile.limits.max_model_requests = match turns {
                ModelRequestLimit::Count(count) => Some(count),
                ModelRequestLimit::Unlimited(Unlimited::Unlimited) => None,
            };
        }
        if let Some(secs) = self.limits.timeout_secs {
            profile.limits.timeout = (secs > 0).then(|| Duration::from_secs(secs));
        }
        if let Some(tokens) = self.limits.token_budget {
            profile.limits.token_budget = (tokens > 0).then_some(tokens);
        }
        if let Some(count) = self.extensions.continue_on_length {
            profile.continue_on_length = count;
        }
        ensure!(self.extensions.redaction != Some(false), "redaction cannot be disabled");
        if let Some(discover) = self.context.discover {
            profile.context.discover = discover;
        }
        profile
            .context
            .files
            .extend(self.context.files.into_iter().map(|path| parent.join(path)));
        if let Some(skills) = self.skills {
            profile.skills = skills
                .into_iter()
                .map(|mut skill| {
                    skill.path = parent.join(skill.path);
                    skill
                })
                .collect();
        }
        if let Some(bytes) = self.compaction.trigger_bytes {
            profile.compaction.trigger_bytes = (bytes > 0).then_some(bytes);
        }
        if let Some(bytes) = self.compaction.keep_recent_bytes {
            profile.compaction.keep_recent_bytes = bytes;
        }
        if let Some(tokens) = self.compaction.max_summary_tokens {
            profile.compaction.max_summary_tokens = tokens;
        }
        if let Some(secs) = self.compaction.timeout_secs {
            profile.compaction.timeout = Duration::from_secs(secs);
        }
        if let Some(mode) = self.compaction.mode {
            profile.compaction.mode = mode;
        }
        if let Some(path) = self.tool_output.directory {
            ensure!(!path.as_os_str().is_empty(), "toolOutput.directory must not be empty");
            profile.tool_output.directory = parent.join(path);
        }
        if let Some(bytes) = self.tool_output.threshold_bytes {
            profile.tool_output.threshold_bytes = (bytes > 0).then_some(bytes);
        }
        if let Some(bytes) = self.tool_output.max_file_bytes {
            profile.tool_output.max_file_bytes = bytes;
        }
        if let Some(execution) = self.execution {
            profile.execution = execution;
        }
        let container = self.container;
        for (value, layer) in [
            (&mut profile.container.image, container.image),
            (&mut profile.container.user, container.user),
            (&mut profile.container.memory, container.memory),
            (&mut profile.container.network, container.network),
        ] {
            if layer.is_some() {
                *value = layer;
            }
        }
        if container.pids_limit.is_some() {
            profile.container.pids_limit = container.pids_limit;
        }
        if container.workspace.is_some() {
            profile.container.workspace = container.workspace;
        }
        if container.root.is_some() {
            profile.container.root = container.root;
        }
        if let Some(servers) = self.mcp {
            profile.mcp = servers;
        }
        Ok(())
    }
}

/// Read a regular UTF-8 configuration file within [`MAX_FILE_BYTES`]. This is bounded,
/// synchronous startup I/O; invalid files and read failures preserve their path in errors.
pub fn read_text(path: &Path) -> Result<String> {
    read_text_file(path, MAX_FILE_BYTES)
}
