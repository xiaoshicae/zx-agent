//! Request configuration changes and per-dispatch projection checkpoints.

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::{Message, Reasoning, Request, ToolDef, now_ms};

use super::{Entry, SessionStore, summary_input, summary_messages};

/// Non-secret request fields. Authentication and transport credentials are not recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestConfig {
    pub provider: String,
    pub model: String,
    pub tools: Vec<ToolDef>,
    pub max_tokens: Option<u32>,
    /// The request's reasoning level, absent unless set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// The request's prompt cache key, absent unless set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    pub provider_options: Value,
}

/// The configuration requests use from here on. Recorded only when it differs from the
/// latest one on the branch, in any field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestConfigEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub config: RequestConfig,
}

/// How to reconstruct the messages at dispatch, without copying the whole transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum RequestSource {
    /// The session projection at this request entry's parent, not at the current leaf.
    Projection,
    /// Original records referenced by a compaction, with request timestamps.
    Summary {
        compaction_id: String,
        system_timestamp: u64,
        user_timestamp: u64,
    },
}

/// A model request as it is about to be sent: its configuration, and how to rebuild its
/// messages.
///
/// Written before Provider::stream. A missing response means dispatch was unresolved;
/// the record alone cannot prove that a remote server accepted the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestStartedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub config_id: String,
    pub source: RequestSource,
}

impl SessionStore {
    /// Persist configuration changes and a dispatch checkpoint before a model call; any
    /// failure prevents dispatch. The checkpoint records how to rebuild the messages, not the
    /// messages: the caller sends the ones `source` names, as of the current leaf.
    pub async fn record_request(
        &mut self,
        request: &Request,
        provider: &str,
        provider_options: Value,
        source: RequestSource,
    ) -> Result<String> {
        let config = RequestConfig {
            provider: provider.into(),
            model: request.model.clone(),
            tools: request.tools.clone(),
            max_tokens: request.max_tokens,
            reasoning: request.reasoning,
            cache_key: request.cache_key.clone(),
            provider_options,
        };
        let existing = self.ancestors(self.leaf.as_deref()).find_map(|entry| match entry {
            Entry::RequestConfig(entry) => Some(entry),
            _ => None,
        });
        let unchanged = existing
            .filter(|entry| entry.config == config)
            .map(|entry| entry.id.clone());
        let config_id = match unchanged {
            Some(id) => id,
            None => {
                self.append(Entry::RequestConfig(RequestConfigEntry {
                    id: self.new_id(),
                    parent_id: self.leaf.clone(),
                    timestamp: now_ms(),
                    config,
                }))
                .await?
            }
        };
        self.append(Entry::RequestStarted(RequestStartedEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            config_id,
            source,
        }))
        .await
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
            reasoning: config.reasoning,
            cache_key: config.cache_key.clone(),
        };
        Ok((config, request))
    }

    fn request_config(&self, id: &str) -> Result<&RequestConfig> {
        match self.get(id) {
            Some(Entry::RequestConfig(entry)) => Ok(&entry.config),
            _ => bail!("request config {id:?} not found"),
        }
    }

    /// The rule a request configuration follows: it names a provider and a model.
    pub(super) fn validate_request_config(entry: &RequestConfigEntry) -> Result<()> {
        ensure!(
            !entry.config.model.trim().is_empty() && !entry.config.provider.trim().is_empty(),
            "empty request model or provider"
        );
        Ok(())
    }

    /// The rules a request checkpoint follows: its configuration is on its branch, and so is
    /// the compaction whose summary it requests, if it requests one.
    pub(super) fn validate_request_start(&self, started: &RequestStartedEntry) -> Result<()> {
        let parent = started.parent_id.as_deref();
        ensure!(
            self.ancestors(parent).any(|entry| entry.id() == started.config_id),
            "request config is not on this branch"
        );
        self.request_config(&started.config_id)?;
        // Check only what can fail: a projection can always be built, and building one for
        // every request record would clone the branch's messages once per record.
        if let RequestSource::Summary { compaction_id, .. } = &started.source {
            Self::compaction_start_on(&self.branch_path_from(parent), compaction_id)?;
        }
        Ok(())
    }

    fn request_messages(&self, source: &RequestSource, parent: Option<&str>) -> Result<Vec<Message>> {
        match source {
            RequestSource::Projection => Ok(self
                .projection_at(parent)
                .into_iter()
                .map(|item| item.message)
                .collect()),
            RequestSource::Summary {
                compaction_id,
                system_timestamp,
                user_timestamp,
            } => {
                let (_, started) = Self::compaction_start_on(&self.branch_path_from(parent), compaction_id)?;
                let source = self.compaction_source(started.parent_id.as_deref(), &started.first_kept_entry_id)?;
                Ok(summary_messages(
                    &started.request.instructions,
                    summary_input(&source)?,
                    *system_timestamp,
                    *user_timestamp,
                ))
            }
        }
    }
}
