//! Request configuration changes and per-dispatch projection checkpoints.

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::{Message, Reasoning, Request, ToolDef, now_ms};

use super::{CompactionStartedEntry, Entry, SummarizedBy, Transcript, summary_input, summary_messages};

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

/// Which request a checkpoint records, as the `kind` of `request_start` events names it, and
/// how to rebuild its messages without copying the whole transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum RequestSource {
    /// A task request: the session projection at this request entry's parent, not at the
    /// current leaf.
    Task,
    /// A summary request: the original records a compaction read, with the request's
    /// timestamps.
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

impl Transcript {
    /// Persist configuration changes and a dispatch checkpoint before a model call; any
    /// failure prevents dispatch. The checkpoint records how to rebuild the messages, not the
    /// messages: the caller sends the ones `source` names, as of the current leaf.
    pub(crate) async fn record_request(
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
        let unchanged = self
            .request_config_at(self.leaf.as_deref())
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

    /// The request configuration in force at the entry `id`: the nearest one on its branch,
    /// which is the one every request recorded there names.
    fn request_config_at(&self, id: Option<&str>) -> Option<&RequestConfigEntry> {
        let position = self.request_configs[*self.index.get(id?)?]?;
        match &self.entries[position] {
            Entry::RequestConfig(entry) => Some(entry),
            _ => None,
        }
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

    /// The rules a request checkpoint follows: its configuration is the one in force on its
    /// branch, as [`Transcript::record_request`] records it, and the compaction whose summary
    /// it requests, if it requests one, which the kernel's own request makes, is on its branch.
    pub(super) fn validate_request_start(&self, started: &RequestStartedEntry) -> Result<()> {
        let parent = started.parent_id.as_deref();
        ensure!(
            self.request_config_at(parent)
                .is_some_and(|config| config.id == started.config_id),
            "request config {} is not the one in force on this branch",
            started.config_id
        );
        // Check only what can fail: a projection can always be built, and building one for
        // every request record would clone the branch's messages once per record.
        if let RequestSource::Summary { compaction_id, .. } = &started.source {
            let (_, compaction) = Self::compaction_start_on(&self.branch_path_from(parent), compaction_id)?;
            Self::summary_instructions(compaction)?;
        }
        Ok(())
    }

    /// The instructions of the request that makes the summary of `compaction`. An error when
    /// a summarizer of the host makes it: the session then holds no request for it.
    fn summary_instructions(compaction: &CompactionStartedEntry) -> Result<&str> {
        match &compaction.by {
            SummarizedBy::Request(request) => Ok(&request.instructions),
            SummarizedBy::Summarizer(name) => bail!(
                "compaction {} is summarized by {name:?}, not by a request",
                compaction.id
            ),
        }
    }

    fn request_messages(&self, source: &RequestSource, parent: Option<&str>) -> Result<Vec<Message>> {
        match source {
            RequestSource::Task => Ok(self
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
                    Self::summary_instructions(started)?,
                    summary_input(&source)?,
                    *system_timestamp,
                    *user_timestamp,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::transcript::test_support::{header, loaded, records_of};

    /// A request for `model` and nothing else.
    fn request_for(model: &str) -> Request {
        Request {
            model: model.into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            reasoning: None,
            cache_key: None,
        }
    }

    /// The IDs of the request configurations `transcript` recorded, oldest first.
    fn config_ids(transcript: &Transcript) -> Vec<String> {
        transcript
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                Entry::RequestConfig(entry) => Some(entry.id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Why the records of `transcript` do not load once a request checkpoint after `parent` names
    /// the configuration `config`.
    fn refusal_of_a_request(transcript: &Transcript, parent: &str, config: &str) -> String {
        let mut records = records_of(transcript);
        records.push(
            json!({"type":"request_started", "id":"invalid", "parentId":parent,
            "timestamp":0, "configId":config, "source":{"kind":"task"}})
            .to_string(),
        );
        let error = Transcript::snapshot(loaded(transcript.header().clone(), records))
            .err()
            .expect("the request must be refused");
        format!("{error:#}")
    }

    #[tokio::test]
    async fn a_task_request_is_recorded_as_its_kind() {
        let mut transcript = Transcript::in_memory(header());

        let id = transcript
            .record_request(&request_for("first"), "p", json!({}), RequestSource::Task)
            .await
            .unwrap();

        let line = serde_json::to_value(transcript.get(&id).unwrap()).unwrap();
        assert_eq!(line["source"], json!({"kind": "task"}));
    }

    #[tokio::test]
    async fn request_configuration_indexes_keep_branches_separate_when_replaying() {
        let mut transcript = Transcript::in_memory(header());
        let first = transcript
            .record_request(&request_for("first"), "p", json!({}), RequestSource::Task)
            .await
            .unwrap();
        transcript
            .record_request(&request_for("other"), "p", json!({}), RequestSource::Task)
            .await
            .unwrap();
        let foreign = config_ids(&transcript).pop().unwrap();
        transcript.branch(&first).unwrap();
        let last = transcript
            .record_request(&request_for("first"), "p", json!({}), RequestSource::Task)
            .await
            .unwrap();
        assert_eq!(config_ids(&transcript).len(), 2, "the branch reuses its configuration");

        let error = refusal_of_a_request(&transcript, &last, &foreign);

        assert!(error.contains("is not the one in force on this branch"), "{error}");
    }

    #[tokio::test]
    async fn a_request_names_the_configuration_in_force_not_an_earlier_one_of_its_branch() {
        let mut transcript = Transcript::in_memory(header());
        transcript
            .record_request(&request_for("first"), "p", json!({}), RequestSource::Task)
            .await
            .unwrap();
        let last = transcript
            .record_request(&request_for("other"), "p", json!({}), RequestSource::Task)
            .await
            .unwrap();
        let configs = config_ids(&transcript);
        assert_eq!(configs.len(), 2);

        let error = refusal_of_a_request(&transcript, &last, &configs[0]);

        assert!(error.contains("is not the one in force on this branch"), "{error}");
    }
}
