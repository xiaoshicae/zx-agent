//! An agent file: TOML front matter between a first line `+++` and the next line `+++`, then
//! the instructions. One rule lays a file over the agent it extends: every key the file sets
//! replaces the inherited value whole, lists included, and the tables `permissions`, `limits`,
//! `context` and `compaction` go key by key.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use zx_agent::agent::RunLimits;
use zx_agent::compaction::{self, BYTES_PER_TOKEN, InvalidConfig, MAX_SOURCE_BYTES, MAX_SUMMARY_TOKENS, TokenTrigger};

use super::permissions::RawPermissions;
use super::skills::Skill;
use super::{AgentDefinition, Builtin};

/// The line that opens and closes the front matter.
const DELIMITER: &str = "+++";

/// A parsed agent file, not yet laid over anything.
pub(super) struct AgentFile {
    front: FrontMatter,
    /// The instructions: the text after the closing `+++`, without the empty lines that start
    /// it.
    body: String,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct FrontMatter {
    extends: Option<Builtin>,
    tools: Option<Vec<String>>,
    continue_on_length: Option<u32>,
    permissions: RawPermissions,
    limits: RawLimits,
    context: RawContext,
    skills: Option<Vec<Skill>>,
    compaction: RawCompaction,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawCompaction {
    trigger_bytes: Option<usize>,
    /// 0 disables the token trigger; unset leaves it to the context window of the model in
    /// use ([`settle_token_trigger`]).
    trigger_tokens: Option<u64>,
    /// What a token trigger that follows the context window keeps free below it.
    reserve_tokens: Option<u64>,
    keep_recent_bytes: Option<usize>,
    max_summary_tokens: Option<u32>,
    timeout_secs: Option<u64>,
}

/// The `[limits]` of an agent file, and the budgets a host gives over an agent's
/// ([`Overrides`](crate::Overrides)): each one given replaces the agent's.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct RawLimits {
    pub(crate) max_model_requests: Option<ModelRequestLimit>,
    /// 0 disables a time limit that the extended agent set.
    pub(crate) timeout_secs: Option<u64>,
    /// 0 disables a budget that the extended agent set.
    pub(crate) token_budget: Option<u64>,
    /// In the unit of the `[[models]]` prices; 0 disables a budget that the extended agent
    /// set.
    pub(crate) cost_budget: Option<f64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum ModelRequestLimit {
    Count(u32),
    Unlimited(Unlimited),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Unlimited {
    Unlimited,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct RawContext {
    discover: Option<bool>,
    files: Option<Vec<PathBuf>>,
}

impl AgentFile {
    /// Split `text` into front matter and instructions, and read the front matter.
    pub(super) fn parse(text: &str) -> Result<Self> {
        let mut lines = text.split_inclusive('\n');
        ensure!(
            lines.next().is_some_and(|line| line.trim_end() == DELIMITER),
            "the first line must be {DELIMITER}, which opens the TOML front matter"
        );
        let mut front = String::new();
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim_end() == DELIMITER {
                closed = true;
                break;
            }
            front.push_str(line);
        }
        ensure!(closed, "no line {DELIMITER} closes the front matter");
        let body = lines.collect::<String>().trim_start_matches(['\n', '\r']).to_string();
        let front = toml::from_str(&front).context("invalid front matter")?;
        Ok(Self { front, body })
    }

    /// The built-in agent the file extends.
    pub(super) fn extends(&self) -> Option<Builtin> {
        self.front.extends
    }

    /// Whether the file has instructions of its own.
    pub(super) fn has_instructions(&self) -> bool {
        !self.body.trim().is_empty()
    }

    /// Lay this file over `agent`. Paths are resolved against `parent`, the file's directory.
    pub(super) fn apply(self, agent: &mut AgentDefinition, parent: &Path) -> Result<()> {
        if self.has_instructions() {
            agent.instructions = self.body;
        }
        let front = self.front;
        if let Some(tools) = front.tools {
            agent.tools = tools;
        }
        if let Some(count) = front.continue_on_length {
            agent.continue_on_length = count;
        }
        front.permissions.apply(&mut agent.permissions)?;
        front.limits.apply(&mut agent.limits)?;
        if let Some(discover) = front.context.discover {
            agent.context.discover = discover;
        }
        if let Some(files) = front.context.files {
            agent.context.files = files.into_iter().map(|path| parent.join(path)).collect();
        }
        if let Some(skills) = front.skills {
            agent.skills = skills
                .into_iter()
                .map(|mut skill| {
                    skill.path = parent.join(skill.path);
                    skill
                })
                .collect();
        }
        front.compaction.apply(&mut agent.compaction);
        Ok(())
    }
}

impl RawLimits {
    /// Lay these over `limits`. A cost budget that is negative or not finite is an error.
    pub(crate) fn apply(self, limits: &mut RunLimits) -> Result<()> {
        if let Some(requests) = self.max_model_requests {
            limits.max_model_requests = match requests {
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

/// A cost budget, in the unit of the prices, as an agent file or a host gives it: 0 disables
/// it. Negative and non-finite amounts are errors.
pub(crate) fn cost_budget(amount: f64) -> Result<Option<f64>> {
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
            config.trigger_tokens = (tokens > 0).then_some(TokenTrigger::Above(tokens));
        }
        // Only a token trigger that no file set follows the window, below its reserve.
        if let Some(tokens) = self.reserve_tokens
            && let Some(TokenTrigger::BelowWindow { reserve_tokens }) = &mut config.trigger_tokens
        {
            *reserve_tokens = tokens;
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
    }
}

/// A `[compaction]` setting that the kernel refuses, said with its key in the agent file,
/// where 0 turns a trigger off.
pub(super) fn compaction_error(error: &InvalidConfig) -> anyhow::Error {
    let message = match error {
        InvalidConfig::ZeroKeepRecentBytes => "compaction.keepRecentBytes must be greater than zero".to_string(),
        // `triggerTokens = 0` turns the token trigger off, so no file comes here.
        InvalidConfig::ZeroTriggerTokens => {
            "compaction.triggerTokens must be greater than zero (0 turns the token trigger off)".to_string()
        }
        InvalidConfig::TriggerTokensWithinKept => format!(
            "compaction.triggerTokens must exceed compaction.keepRecentBytes / {BYTES_PER_TOKEN}, the tokens the kept recent messages take"
        ),
        InvalidConfig::TriggerBytesWithinKept => {
            "compaction.triggerBytes must exceed compaction.keepRecentBytes (0 turns the byte trigger off)".to_string()
        }
        InvalidConfig::TriggerBytesBeyondSource => format!(
            "compaction.triggerBytes may exceed compaction.keepRecentBytes by at most {MAX_SOURCE_BYTES} bytes, the most a summary reads"
        ),
        InvalidConfig::MaxSummaryTokens => format!("compaction.maxSummaryTokens must be 1..={MAX_SUMMARY_TOKENS}"),
        InvalidConfig::ZeroTimeout => "compaction.timeoutSecs must be greater than zero".to_string(),
    };
    anyhow::anyhow!(message)
}

/// Settle the token trigger of `config` once every file of an agent is applied. One that no
/// file set follows the context window of the model in use
/// ([`TokenTrigger::BelowWindow`]) while the byte trigger is on, and is off otherwise: then
/// nothing turns automatic compaction on.
pub(super) fn settle_token_trigger(config: &mut compaction::Config) {
    if config.trigger_bytes.is_none() && matches!(config.trigger_tokens, Some(TokenTrigger::BelowWindow { .. })) {
        config.trigger_tokens = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_front_matter_is_between_the_first_two_delimiter_lines() {
        let file = AgentFile::parse("+++\nextends = \"assistant\"\n+++\n\nDo the task.\n+++\n").unwrap();
        assert_eq!(file.extends(), Some(Builtin::Assistant));
        assert_eq!(file.body, "Do the task.\n+++\n");
        let crlf = AgentFile::parse("+++\r\ntools = []\r\n+++\r\nBody\r\n").unwrap();
        assert_eq!(crlf.body, "Body\r\n");
        assert!(!AgentFile::parse("+++\n+++\n \n").unwrap().has_instructions());
    }

    #[test]
    fn a_file_without_front_matter_or_its_end_is_refused() {
        for (text, expected) in [
            ("", "the first line must be +++"),
            ("Just instructions.\n", "the first line must be +++"),
            ("\n+++\ntools = []\n+++\n", "the first line must be +++"),
            (
                "+++\ntools = []\nInstructions.\n",
                "no line +++ closes the front matter",
            ),
            ("+++\ntools = [\n+++\n", "invalid front matter"),
            ("+++\nname = 'x'\n+++\n", "unknown field `name`"),
        ] {
            let error = format!("{:#}", AgentFile::parse(text).err().unwrap());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }
}
