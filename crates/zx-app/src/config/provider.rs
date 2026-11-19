//! The `[provider]` table: which provider reaches the model, `api`, and its settings. zx-app
//! reads `api`, `apiKeyEnv` and `maxRetries` itself; every other key is the provider's. The
//! keys of the built-in providers are checked when the file is read, those of a provider a
//! host registers when its agent is built.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};
use zx_agent::ai::anthropic::{self, AnthropicConfig};
use zx_agent::ai::openai::OpenAiConfig;
use zx_agent::ai::retry::RetryPolicy;

use super::parsed;

/// The built-in provider of OpenAI-compatible servers, the one config.toml means unless
/// `provider.api` names another.
pub const OPENAI: &str = "openai";

/// The built-in provider of the Anthropic Messages API.
pub const ANTHROPIC: &str = "anthropic";

/// The `[provider]` table: how the model is reached.
#[derive(Clone, Debug)]
pub struct ProviderSettings {
    /// The provider, by the name it is registered under: [`OPENAI`], [`ANTHROPIC`], or one a
    /// host registers ([`Application::register_provider`](crate::Application::register_provider)).
    pub api: String,
    /// The provider's own keys, as config.toml writes them: everything in the table but
    /// `api`, `apiKeyEnv` and `maxRetries`.
    pub options: toml::Table,
    /// The variable holding the API key; `None` uses the provider's own.
    pub api_key_env: Option<String>,
    /// How transient provider errors are retried; the file sets only the count.
    pub retry: RetryPolicy,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            api: OPENAI.to_string(),
            options: toml::Table::new(),
            api_key_env: None,
            retry: RetryPolicy::default(),
        }
    }
}

impl ProviderSettings {
    /// The settings a `[provider]` table gives, over the defaults.
    pub(super) fn from_table(mut table: toml::Table) -> Result<Self> {
        let mut settings = Self::default();
        if let Some(api) = take_text(&mut table, "api")? {
            settings.api = api;
        }
        settings.api_key_env = take_text(&mut table, "apiKeyEnv")?;
        if let Some(retries) = table.remove("maxRetries") {
            settings.retry.max_retries = retries
                .try_into()
                .context("provider.maxRetries must be a count of retries")?;
        }
        settings.options = table;
        Ok(settings)
    }

    /// Check the values zx-app reads, and the keys of a built-in provider.
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(!self.api.trim().is_empty(), "provider.api must not be empty");
        ensure!(
            self.api_key_env.as_deref().is_none_or(|env| !env.trim().is_empty()),
            "provider.apiKeyEnv must not be empty"
        );
        match self.api.as_str() {
            OPENAI => openai_config(&self.options).map(drop),
            ANTHROPIC => anthropic_config(&self.options).map(drop),
            // A provider a host registers checks its own keys.
            _ => Ok(()),
        }
    }
}

/// The text of `key`, taken out of `table`.
fn take_text(table: &mut toml::Table, key: &str) -> Result<Option<String>> {
    match table.remove(key) {
        None => Ok(None),
        Some(toml::Value::String(text)) => Ok(Some(text)),
        Some(_) => anyhow::bail!("provider.{key} must be a string"),
    }
}

/// The keys of `[provider]` the built-in providers read. One type for both, since serde
/// refuses unknown keys only of a type that lists every one: the keys of the OpenAI-compatible
/// provider alone are refused for the other ([`BuiltInKeys::refuse_openai_keys`]).
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct BuiltInKeys {
    base_url: Option<String>,
    request_fields: Map<String, Value>,
    headers: BTreeMap<String, String>,
    idle_timeout_secs: Option<u64>,
    system_updates: Option<String>,
    reasoning_format: Option<String>,
    max_tokens_field: Option<String>,
    fill_missing_reasoning: bool,
    prompt_cache_key: bool,
}

impl BuiltInKeys {
    /// The keys of `options`, checked: an address that is not empty, a positive idle timeout,
    /// request fields and headers with names, and no header named `key_header`, which comes
    /// from the API key.
    fn read(options: &toml::Table, key_header: &str) -> Result<Self> {
        let keys: Self = toml::Value::Table(options.clone())
            .try_into()
            .context("invalid [provider] settings")?;
        ensure!(
            keys.base_url.as_deref().is_none_or(|url| !url.trim().is_empty()),
            "provider.baseUrl must not be empty"
        );
        ensure!(
            keys.idle_timeout_secs != Some(0),
            "provider.idleTimeoutSecs must be greater than zero"
        );
        ensure!(
            keys.request_fields.keys().all(|field| !field.trim().is_empty()),
            "provider.requestFields: a field has no name"
        );
        for name in keys.headers.keys() {
            ensure!(!name.trim().is_empty(), "provider.headers: a header has no name");
            ensure!(
                !name.trim().eq_ignore_ascii_case(key_header),
                "provider.headers: {key_header} comes from the API key (provider.apiKeyEnv); it cannot be set as a header"
            );
        }
        Ok(keys)
    }

    /// Refuse the keys only the OpenAI-compatible provider reads, for the provider `api`.
    fn refuse_openai_keys(&self, api: &str) -> Result<()> {
        for (key, set) in [
            ("reasoningFormat", self.reasoning_format.is_some()),
            ("maxTokensField", self.max_tokens_field.is_some()),
            ("fillMissingReasoning", self.fill_missing_reasoning),
            ("promptCacheKey", self.prompt_cache_key),
        ] {
            ensure!(
                !set,
                "provider.{key} is a setting of the {OPENAI} provider, not of {api}"
            );
        }
        Ok(())
    }
}

/// The settings of the OpenAI-compatible provider that `options` give, over its defaults,
/// checked; without the API key, which the assembly reads.
pub(crate) fn openai_config(options: &toml::Table) -> Result<OpenAiConfig> {
    let keys = BuiltInKeys::read(options, "Authorization")?;
    let mut config = OpenAiConfig::default();
    if let Some(url) = keys.base_url {
        config.base_url = url;
    }
    if let Some(updates) = parsed("provider.systemUpdates", keys.system_updates)? {
        config.body.system_updates = updates;
    }
    if let Some(format) = parsed("provider.reasoningFormat", keys.reasoning_format)? {
        config.body.reasoning_format = format;
    }
    if let Some(field) = parsed("provider.maxTokensField", keys.max_tokens_field)? {
        config.body.max_tokens_field = field;
    }
    config.body.fill_missing_reasoning = keys.fill_missing_reasoning;
    config.body.prompt_cache_key = keys.prompt_cache_key;
    config.body.extra_fields = keys.request_fields;
    config.headers = keys.headers.into_iter().collect();
    if let Some(secs) = keys.idle_timeout_secs {
        config.idle_timeout = Duration::from_secs(secs);
    }
    Ok(config)
}

/// The settings of the Anthropic Messages API provider that `options` give, over its
/// defaults, checked; without the API key, which the assembly reads.
pub(crate) fn anthropic_config(options: &toml::Table) -> Result<AnthropicConfig> {
    let keys = BuiltInKeys::read(options, "x-api-key")?;
    keys.refuse_openai_keys(ANTHROPIC)?;
    let mut config = AnthropicConfig::default();
    if let Some(url) = keys.base_url {
        config.base_url = url;
    }
    if let Some(updates) = keys.system_updates {
        config.body.system_updates = match updates.trim().to_ascii_lowercase().as_str() {
            "collapse" => anthropic::SystemUpdates::Collapse,
            "inline" => anthropic::SystemUpdates::Inline,
            _ => anyhow::bail!("provider.systemUpdates: unknown value {updates:?}; use collapse or inline"),
        };
    }
    config.body.extra_fields = keys.request_fields;
    config.headers = keys.headers.into_iter().collect();
    if let Some(secs) = keys.idle_timeout_secs {
        config.idle_timeout = Duration::from_secs(secs);
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::openai::{MaxTokensField, ReasoningFormat, SystemUpdates};

    use super::*;

    fn table(text: &str) -> toml::Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_common_keys_are_read_out_and_the_rest_left_to_the_provider() {
        let settings = ProviderSettings::from_table(table(
            "api = 'anthropic'\napiKeyEnv = 'KEY'\nmaxRetries = 0\nbaseUrl = 'http://x'",
        ))
        .unwrap();
        assert_eq!(settings.api, ANTHROPIC);
        assert_eq!(settings.api_key_env.as_deref(), Some("KEY"));
        assert_eq!(settings.retry.max_retries, 0);
        assert_eq!(settings.options, table("baseUrl = 'http://x'"));
        let defaults = ProviderSettings::default();
        assert_eq!((defaults.api.as_str(), defaults.api_key_env), (OPENAI, None));
    }

    #[test]
    fn the_openai_keys_set_the_settings_of_its_provider() {
        let config = openai_config(&table(
            "baseUrl = 'http://localhost:8000/v1'\nsystemUpdates = 'inline'\nreasoningFormat = 'thinking'\n\
             maxTokensField = 'max_completion_tokens'\nfillMissingReasoning = true\npromptCacheKey = true\n\
             idleTimeoutSecs = 15\nrequestFields = { top_k = 5 }\nheaders = { X-Route = 'eu' }",
        ))
        .unwrap();
        assert_eq!(config.base_url, "http://localhost:8000/v1");
        assert_eq!(config.body.system_updates, SystemUpdates::Inline);
        assert_eq!(config.body.reasoning_format, ReasoningFormat::Thinking);
        assert_eq!(config.body.max_tokens_field, MaxTokensField::MaxCompletionTokens);
        assert!(config.body.fill_missing_reasoning && config.body.prompt_cache_key);
        assert_eq!(config.body.extra_fields.get("top_k"), Some(&serde_json::json!(5)));
        assert_eq!(config.headers, [("X-Route".to_string(), "eu".to_string())]);
        assert_eq!(config.idle_timeout, Duration::from_secs(15));
    }

    #[test]
    fn the_anthropic_keys_set_the_settings_of_its_provider() {
        let config = anthropic_config(&table(
            "baseUrl = 'https://gateway.example/anthropic'\nsystemUpdates = 'inline'\nidleTimeoutSecs = 30\n\
             requestFields = { metadata = { user_id = 'u' } }\nheaders = { anthropic-beta = 'x' }",
        ))
        .unwrap();
        assert_eq!(config.base_url, "https://gateway.example/anthropic");
        assert_eq!(config.body.system_updates, anthropic::SystemUpdates::Inline);
        assert_eq!(config.idle_timeout, Duration::from_secs(30));
        assert!(config.body.extra_fields.contains_key("metadata"));
        assert_eq!(config.headers, [("anthropic-beta".to_string(), "x".to_string())]);
    }

    #[test]
    fn a_key_the_provider_does_not_read_is_refused() {
        let error = format!(
            "{:#}",
            anthropic_config(&table("reasoningFormat = 'thinking'")).unwrap_err()
        );
        assert!(
            error.contains("provider.reasoningFormat is a setting of the openai provider"),
            "{error}"
        );
        let error = format!("{:#}", anthropic_config(&table("typo = 1")).unwrap_err());
        assert!(error.contains("unknown field `typo`"), "{error}");
        let error = format!(
            "{:#}",
            anthropic_config(&table("headers = { X-Api-Key = 'k' }")).unwrap_err()
        );
        assert!(error.contains("x-api-key comes from the API key"), "{error}");
        let error = format!("{:#}", anthropic_config(&table("systemUpdates = 'merge'")).unwrap_err());
        assert!(error.contains("provider.systemUpdates"), "{error}");
        // A provider a host registers checks its own keys.
        let custom = ProviderSettings {
            api: "mine".into(),
            options: table("anything = 1"),
            ..ProviderSettings::default()
        };
        custom.validate().unwrap();
    }
}
