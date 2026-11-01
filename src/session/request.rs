//! Request configuration changes and per-dispatch projection checkpoints.

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::{Message, PREAMBLE, Request, SystemMessage, SystemPrompt, ToolDef, now_ms};

use super::{Entry, SessionStore};

/// Non-secret request fields. Authentication and transport credentials are not recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestConfig {
    pub provider: String,
    pub model: String,
    pub tools: Vec<ToolDef>,
    pub max_tokens: Option<u32>,
    pub provider_options: Value,
}

/// Append only when model, tools, token budget or provider conversion options change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestConfigEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub config: RequestConfig,
}

/// How to reconstruct the messages at dispatch, without copying the whole transcript.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum RequestSource {
    /// The session projection at this request entry's parent, not at the current leaf.
    Projection,
    /// Original records referenced by a compaction attempt, with request timestamps.
    Summary {
        attempt_id: String,
        system_timestamp: u64,
        user_timestamp: u64,
    },
}

/// Written before Provider::stream. A missing response means dispatch was unresolved;
/// the record alone cannot prove that a remote server accepted the request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestStartedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub config_id: String,
    pub source: RequestSource,
}

impl SessionStore {
    /// Persist configuration changes and a dispatch checkpoint before a model call.
    /// Checks messages against the audit projection; any failure prevents dispatch.
    pub fn record_request(
        &mut self,
        request: &Request,
        provider: &str,
        provider_options: Value,
        source: RequestSource,
    ) -> Result<String> {
        ensure!(
            request.messages == self.request_messages(&source, self.leaf.as_deref())?,
            "request messages differ from the persisted session projection"
        );
        let config = RequestConfig {
            provider: provider.into(),
            model: request.model.clone(),
            tools: request.tools.clone(),
            max_tokens: request.max_tokens,
            provider_options,
        };
        self.ensure_format(3)?;
        let existing = self.branch_path().into_iter().rev().find_map(|entry| match entry {
            Entry::RequestConfig(entry) => Some(entry),
            _ => None,
        });
        let config_id = match existing {
            Some(entry) if entry.config == config => entry.id.clone(),
            _ => self.append(Entry::RequestConfig(RequestConfigEntry {
                id: self.new_id(),
                parent_id: self.leaf.clone(),
                timestamp: now_ms(),
                config,
            }))?,
        };
        self.append(Entry::RequestStarted(RequestStartedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            config_id,
            source,
        }))
    }

    /// Recover the exact provider-neutral request at a historical dispatch checkpoint,
    /// including tool definitions and model/options before later edits or model switches.
    pub fn replay_request(&self, id: &str) -> Result<(RequestConfig, Request)> {
        let Some(Entry::RequestStarted(started)) = self.get(id) else {
            bail!("request entry {id:?} not found");
        };
        let config = self.request_config(&started.config_id)?.clone();
        let request = Request {
            model: config.model.clone(),
            messages: self.request_messages(&started.source, started.parent_id.as_deref())?,
            tools: config.tools.clone(),
            max_tokens: config.max_tokens,
        };
        Ok((config, request))
    }

    fn request_config(&self, id: &str) -> Result<&RequestConfig> {
        match self.get(id) {
            Some(Entry::RequestConfig(entry)) => Ok(&entry.config),
            _ => bail!("request config {id:?} not found"),
        }
    }

    pub(super) fn validate_request_entry(&self, entry: &Entry) -> Result<()> {
        ensure!(self.format_version >= 3, "request audit requires format v3");
        match entry {
            Entry::RequestConfig(config) => {
                ensure!(
                    !config.config.model.trim().is_empty() && !config.config.provider.trim().is_empty(),
                    "empty request model or provider"
                );
            }
            Entry::RequestStarted(started) => {
                let path = self.branch_path_from(started.parent_id.as_deref());
                ensure!(
                    path.iter().any(|entry| entry.id() == started.config_id),
                    "request config is not on this branch"
                );
                self.request_config(&started.config_id)?;
                self.request_messages(&started.source, started.parent_id.as_deref())?;
            }
            _ => bail!("not a request record"),
        }
        Ok(())
    }

    fn request_messages(&self, source: &RequestSource, parent: Option<&str>) -> Result<Vec<Message>> {
        match source {
            RequestSource::Projection => Ok(self
                .compacted_projection_at(parent)
                .into_iter()
                .map(|item| item.message)
                .collect()),
            RequestSource::Summary {
                attempt_id,
                system_timestamp,
                user_timestamp,
            } => {
                let path = self.branch_path_from(parent);
                ensure!(
                    path.iter().any(|entry| entry.id() == attempt_id),
                    "summary attempt is not on this request branch"
                );
                let Some(Entry::CompactionStarted(attempt)) = self.get(attempt_id) else {
                    bail!("summary attempt {attempt_id:?} not found");
                };
                let raw = Self::raw_projection_from(&self.branch_path_from(attempt.parent_id.as_deref()));
                let source: Vec<_> = raw
                    .into_iter()
                    .take_while(|item| item.entry_id != attempt.first_kept_entry_id)
                    .collect();
                let system = Message::System(SystemMessage {
                    sections: SystemPrompt::default()
                        .diff(&SystemPrompt::default().with_section(PREAMBLE, &attempt.request.instructions)),
                    timestamp: *system_timestamp,
                });
                let mut user = Message::user(serde_json::to_string(&source)?);
                if let Message::User { timestamp, .. } = &mut user {
                    *timestamp = *user_timestamp;
                }
                Ok(vec![system, user])
            }
        }
    }
}
