//! The TOML layer of a profile: what a file may set, and how one layer goes over another.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use super::skills::Skill;
use super::{Builtin, ContainerSettings, Execution, MAX_FILE_BYTES, McpServer, Profile, Rules};
use crate::ai::{ModelCost, ModelMeta};
use crate::executor::Access;
use crate::files::read_text_file;
use crate::policy::ProgramOptions;
use crate::runner::Limits;
use crate::{compaction, output_store};

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
    models: Option<Vec<RawModel>>,
}

/// One `[[models]]` entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawModel {
    id: String,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    cost: Option<ModelCost>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawCompaction {
    trigger_bytes: Option<usize>,
    /// 0 disables the token trigger; unset leaves it to the model's context window.
    trigger_tokens: Option<u64>,
    reserve_tokens: Option<u64>,
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
    /// In the unit of the `[[models]]` prices; 0 disables a budget that an earlier layer set.
    cost_budget: Option<f64>,
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
        self.rules.apply(&mut profile.rules);
        self.limits.apply(&mut profile.limits)?;
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
        self.compaction.apply(&mut profile.compaction);
        self.tool_output.apply(&mut profile.tool_output, parent)?;
        if let Some(execution) = self.execution {
            profile.execution = execution;
        }
        self.container.apply(&mut profile.container);
        if let Some(servers) = self.mcp {
            profile.mcp = servers;
        }
        for model in self.models.unwrap_or_default() {
            model.apply(profile)?;
        }
        Ok(())
    }
}

impl RawModel {
    /// Check the entry and record it under its id.
    fn apply(self, profile: &mut Profile) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "models: an entry has no id");
        ensure!(
            self.context_window.is_none_or(|window| window > 0),
            "models.{}: contextWindow must be greater than zero",
            self.id
        );
        if let Some(cost) = &self.cost {
            ensure!(
                [cost.input, cost.output, cost.cache_read, cost.cache_write]
                    .iter()
                    .all(|price| price.is_finite() && *price >= 0.0),
                "models.{}: prices must be zero or more, per million tokens",
                self.id
            );
        }
        profile.models.insert(
            self.id,
            ModelMeta {
                context_window: self.context_window,
                cost: self.cost,
            },
        );
        Ok(())
    }
}

impl RawRules {
    fn apply(self, rules: &mut Rules) {
        if let Some(allow) = self.allow {
            rules.allow = allow;
        }
        rules.deny.extend(self.deny);
        if let Some(prefixes) = self.auto_approve {
            rules.auto_approve = prefixes;
        }
        rules.auto_approve_except.extend(self.auto_approve_except);
        rules.auto_approve_options.extend(self.auto_approve_options);
        if let Some(tools) = self.ask_tools {
            rules.ask_tools = tools;
        }
        rules.deny_tools.extend(self.deny_tools);
        if let Some(approve_all) = self.approve_all {
            rules.approve_all = approve_all;
        }
    }
}

impl RawLimits {
    fn apply(self, limits: &mut Limits) -> Result<()> {
        if let Some(turns) = self.max_model_requests {
            limits.max_model_requests = match turns {
                ModelRequestLimit::Count(count) => Some(count),
                ModelRequestLimit::Unlimited(Unlimited::Unlimited) => None,
            };
        }
        if let Some(secs) = self.timeout_secs {
            limits.timeout = (secs > 0).then(|| Duration::from_secs(secs));
        }
        if let Some(tokens) = self.token_budget {
            limits.token_budget = (tokens > 0).then_some(tokens);
        }
        if let Some(amount) = self.cost_budget {
            limits.cost_budget = cost_budget(amount).context("limits.costBudget")?;
        }
        Ok(())
    }
}

/// A cost budget as configured, in the unit of the prices: 0 disables it. Negative and
/// non-finite amounts are errors.
fn cost_budget(amount: f64) -> Result<Option<f64>> {
    ensure!(
        amount.is_finite() && amount >= 0.0,
        "a cost budget must be zero (none) or a positive amount, not {amount}"
    );
    Ok((amount > 0.0).then_some(amount))
}

impl RawCompaction {
    fn apply(self, config: &mut compaction::Config) {
        if let Some(bytes) = self.trigger_bytes {
            config.trigger_bytes = (bytes > 0).then_some(bytes);
        }
        if let Some(tokens) = self.trigger_tokens {
            config.trigger_tokens = (tokens > 0).then_some(tokens);
        }
        if let Some(tokens) = self.reserve_tokens {
            config.reserve_tokens = tokens;
        }
        if let Some(bytes) = self.keep_recent_bytes {
            config.keep_recent_bytes = bytes;
        }
        if let Some(tokens) = self.max_summary_tokens {
            config.max_summary_tokens = tokens;
        }
        if let Some(secs) = self.timeout_secs {
            config.timeout = Duration::from_secs(secs);
        }
        if let Some(mode) = self.mode {
            config.mode = mode;
        }
    }
}

impl RawToolOutput {
    fn apply(self, config: &mut output_store::Config, parent: &Path) -> Result<()> {
        if let Some(path) = self.directory {
            ensure!(!path.as_os_str().is_empty(), "toolOutput.directory must not be empty");
            config.directory = parent.join(path);
        }
        if let Some(bytes) = self.threshold_bytes {
            config.threshold_bytes = (bytes > 0).then_some(bytes);
        }
        if let Some(bytes) = self.max_file_bytes {
            config.max_file_bytes = bytes;
        }
        Ok(())
    }
}

impl RawContainer {
    fn apply(self, settings: &mut ContainerSettings) {
        for (value, layer) in [
            (&mut settings.image, self.image),
            (&mut settings.user, self.user),
            (&mut settings.memory, self.memory),
            (&mut settings.network, self.network),
        ] {
            if layer.is_some() {
                *value = layer;
            }
        }
        if self.pids_limit.is_some() {
            settings.pids_limit = self.pids_limit;
        }
        if self.workspace.is_some() {
            settings.workspace = self.workspace;
        }
        if self.root.is_some() {
            settings.root = self.root;
        }
    }
}

/// Read a regular UTF-8 configuration file within [`MAX_FILE_BYTES`]. This is bounded,
/// synchronous startup I/O; invalid files and read failures preserve their path in errors.
pub(super) fn read_text(path: &Path) -> Result<String> {
    read_text_file(path, MAX_FILE_BYTES)
}
