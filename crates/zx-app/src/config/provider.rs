//! The `[provider]` table: which provider reaches the model, `api`, and its settings. zx-app
//! reads `api`, `apiKey` and `maxRetries` itself; every other key is the provider's. The
//! keys of the built-in providers are checked when the file is read, with the request fields
//! that the reasoning level would conflict with; those of a provider a host registers are
//! checked when its agent is built.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};
use zx_agent::ai::Reasoning;
use zx_agent::ai::anthropic::{AnthropicConfig, ApiKeyHeader};
use zx_agent::ai::openai::OpenAiConfig;
use zx_agent::ai::retry::RetryPolicy;
use zx_util::env::variables;

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
    /// `api`, `apiKey` and `maxRetries`.
    pub options: toml::Table,
    /// The API key as written, its placeholders not replaced, such as `${OPENAI_API_KEY}`, or
    /// the key itself; `None` names the provider's own variable. A variable it names without
    /// a value means no key, as local model servers need none.
    pub api_key: Option<String>,
    /// How transient provider errors are retried; the file sets only the count.
    pub retry: RetryPolicy,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            api: OPENAI.to_string(),
            options: toml::Table::new(),
            api_key: None,
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
        settings.api_key = take_text(&mut table, "apiKey")?;
        if let Some(retries) = table.remove("maxRetries") {
            settings.retry.max_retries = retries
                .try_into()
                .context("provider.maxRetries must be a count of retries")?;
        }
        settings.options = table;
        Ok(settings)
    }

    /// Check the values zx-app reads, and the keys of a built-in provider, also against the
    /// `reasoning` level of config.toml.
    pub(super) fn validate(&self, reasoning: Option<Reasoning>) -> Result<()> {
        ensure!(!self.api.trim().is_empty(), "provider.api must not be empty");
        if let Some(key) = &self.api_key {
            ensure!(!key.trim().is_empty(), "provider.apiKey must not be empty");
            // Its variables are read when the provider is built; what it names is checked here.
            variables(key).map_err(|error| anyhow!("provider.apiKey: {error}"))?;
        }
        match self.api.as_str() {
            OPENAI => openai_config(&self.options, reasoning).map(drop),
            ANTHROPIC => anthropic_config(&self.options, reasoning).map(drop),
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
    api_key_header: Option<String>,
}

impl BuiltInKeys {
    /// The keys of `options`, checked: an address that is not empty, a positive idle timeout,
    /// and request fields and headers with names.
    fn read(options: &toml::Table) -> Result<Self> {
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
        }
        Ok(keys)
    }

    /// Refuse a header named `key_header`, which carries the API key.
    fn refuse_key_header(&self, key_header: &str) -> Result<()> {
        for name in self.headers.keys() {
            ensure!(
                !name.trim().eq_ignore_ascii_case(key_header),
                "provider.headers: {key_header} comes from the API key (provider.apiKey); it cannot be set as a header"
            );
        }
        Ok(())
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
/// checked, also against the `reasoning` level; without the API key, which the assembly reads.
pub(crate) fn openai_config(options: &toml::Table, reasoning: Option<Reasoning>) -> Result<OpenAiConfig> {
    let keys = BuiltInKeys::read(options)?;
    keys.refuse_key_header("Authorization")?;
    ensure!(
        keys.api_key_header.is_none(),
        "provider.apiKeyHeader is a setting of the {ANTHROPIC} provider, not of {OPENAI}"
    );
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
    if reasoning.is_some()
        && let Some(field) = config.body.reasoning_overlap()
    {
        anyhow::bail!(
            "provider.requestFields.{field} conflicts with reasoning, which sets it with provider.reasoningFormat {}; use one of them",
            config.body.reasoning_format
        );
    }
    Ok(config)
}

/// The settings of the Anthropic Messages API provider that `options` give, over its
/// defaults, checked, also against the `reasoning` level; without the API key, which the
/// assembly reads.
pub(crate) fn anthropic_config(options: &toml::Table, reasoning: Option<Reasoning>) -> Result<AnthropicConfig> {
    let keys = BuiltInKeys::read(options)?;
    keys.refuse_openai_keys(ANTHROPIC)?;
    let api_key_header: ApiKeyHeader =
        parsed("provider.apiKeyHeader", keys.api_key_header.clone())?.unwrap_or_default();
    keys.refuse_key_header(api_key_header.name())?;
    let mut config = AnthropicConfig {
        api_key_header,
        ..AnthropicConfig::default()
    };
    // The address, the request fields, the headers and the idle timeout are set as for the
    // OpenAI-compatible provider: the two configurations share no type to set them through.
    if let Some(url) = keys.base_url {
        config.base_url = url;
    }
    if let Some(updates) = parsed("provider.systemUpdates", keys.system_updates)? {
        config.body.system_updates = updates;
    }
    config.body.extra_fields = keys.request_fields;
    config.headers = keys.headers.into_iter().collect();
    if let Some(secs) = keys.idle_timeout_secs {
        config.idle_timeout = Duration::from_secs(secs);
    }
    if reasoning.is_some()
        && let Some(field) = config.body.reasoning_overlap()
    {
        anyhow::bail!("provider.requestFields.{field} conflicts with reasoning, which sets it; use one of them");
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::anthropic;
    use zx_agent::ai::openai::{MaxTokensField, ReasoningFormat, SystemUpdates};

    use super::*;

    fn table(text: &str) -> toml::Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_anthropic_key_may_go_as_a_bearer_token_and_no_header_may_carry_it_too() {
        let bearer = anthropic_config(&table("apiKeyHeader = 'authorization'"), None).unwrap();
        assert_eq!(bearer.api_key_header, ApiKeyHeader::Authorization);
        let error = format!(
            "{:#}",
            anthropic_config(
                &table("apiKeyHeader = 'authorization'\nheaders = { Authorization = 'Bearer x' }"),
                None
            )
            .unwrap_err()
        );
        assert!(
            error.contains("provider.headers: authorization comes from the API key"),
            "{error}"
        );
        // By default the key goes as x-api-key, and a gateway's own Authorization may be set.
        assert!(anthropic_config(&table("headers = { Authorization = 'Bearer x' }"), None).is_ok());
        let error = format!(
            "{:#}",
            openai_config(&table("apiKeyHeader = 'authorization'"), None).unwrap_err()
        );
        assert!(
            error.contains("provider.apiKeyHeader is a setting of the anthropic provider"),
            "{error}"
        );
    }

    #[test]
    fn the_common_keys_are_read_out_and_the_rest_left_to_the_provider() {
        let settings = ProviderSettings::from_table(table(
            "api = 'anthropic'\napiKey = '${KEY}'\nmaxRetries = 0\nbaseUrl = 'http://x'",
        ))
        .unwrap();
        assert_eq!(settings.api, ANTHROPIC);
        assert_eq!(settings.api_key.as_deref(), Some("${KEY}"));
        assert_eq!(settings.retry.max_retries, 0);
        assert_eq!(settings.options, table("baseUrl = 'http://x'"));
        let defaults = ProviderSettings::default();
        assert_eq!((defaults.api.as_str(), defaults.api_key), (OPENAI, None));
    }

    #[test]
    fn an_api_key_that_names_no_variable_as_it_should_is_refused() {
        let broken = ProviderSettings::from_table(table("apiKey = '${KEY'")).unwrap();
        let error = format!("{:#}", broken.validate(None).unwrap_err());
        assert!(error.starts_with("provider.apiKey: ${KEY is no placeholder"), "{error}");
        // A variable as the shell names it would be sent as the key itself.
        let unbraced = ProviderSettings::from_table(table("apiKey = '$OPENAI_API_KEY'")).unwrap();
        let error = format!("{:#}", unbraced.validate(None).unwrap_err());
        assert!(
            error.starts_with("provider.apiKey: $OPENAI_API_KEY is no placeholder: write ${OPENAI_API_KEY}"),
            "{error}"
        );
        // Written as it is, the key is taken as it is.
        let literal = ProviderSettings::from_table(table("apiKey = 'sk-1'")).unwrap();
        assert!(literal.validate(None).is_ok());
    }

    #[test]
    fn the_openai_keys_set_the_settings_of_its_provider() {
        let config = openai_config(
            &table(
                "baseUrl = 'http://localhost:8000/v1'\nsystemUpdates = 'inline'\nreasoningFormat = 'thinking'\n\
             maxTokensField = 'max_completion_tokens'\nfillMissingReasoning = true\npromptCacheKey = true\n\
             idleTimeoutSecs = 15\nrequestFields = { top_k = 5 }\nheaders = { X-Route = 'eu' }",
            ),
            None,
        )
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
        let config = anthropic_config(
            &table(
                "baseUrl = 'https://gateway.example/anthropic'\nsystemUpdates = 'inline'\nidleTimeoutSecs = 30\n\
             requestFields = { metadata = { user_id = 'u' } }\nheaders = { anthropic-beta = 'x' }",
            ),
            None,
        )
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
            anthropic_config(&table("reasoningFormat = 'thinking'"), None).unwrap_err()
        );
        assert!(
            error.contains("provider.reasoningFormat is a setting of the openai provider"),
            "{error}"
        );
        let error = format!("{:#}", anthropic_config(&table("typo = 1"), None).unwrap_err());
        assert!(error.contains("unknown field `typo`"), "{error}");
        let error = format!(
            "{:#}",
            anthropic_config(&table("headers = { X-Api-Key = 'k' }"), None).unwrap_err()
        );
        assert!(error.contains("x-api-key comes from the API key"), "{error}");
        let error = format!(
            "{:#}",
            anthropic_config(&table("systemUpdates = 'merge'"), None).unwrap_err()
        );
        assert!(error.contains("provider.systemUpdates: unknown variant"), "{error}");
        let error = format!(
            "{:#}",
            anthropic_config(&table("apiKeyHeader = 'bearer'"), None).unwrap_err()
        );
        assert!(error.contains("provider.apiKeyHeader: unknown variant"), "{error}");
        // A provider a host registers checks its own keys.
        let custom = ProviderSettings {
            api: "mine".into(),
            options: table("anything = 1"),
            ..ProviderSettings::default()
        };
        custom.validate(Some(Reasoning::Low)).unwrap();
    }

    /// The settings of `api` that `text` gives, checked against the reasoning level `Low`.
    fn with_reasoning(api: &str, text: &str) -> Result<()> {
        let options = table(text);
        match api {
            OPENAI => openai_config(&options, Some(Reasoning::Low)).map(drop),
            _ => anthropic_config(&options, Some(Reasoning::Low)).map(drop),
        }
    }

    #[test]
    fn request_fields_that_the_reasoning_level_sets_are_refused() {
        for (api, text, expected) in [
            (
                OPENAI,
                "requestFields = { reasoning_effort = 'high' }",
                "provider.requestFields.reasoning_effort conflicts with reasoning",
            ),
            (
                OPENAI,
                "reasoningFormat = 'thinking'\nrequestFields = { thinking = { type = 'enabled' } }",
                "provider.requestFields.thinking conflicts with reasoning",
            ),
            (
                ANTHROPIC,
                "requestFields = { thinking = { type = 'disabled' } }",
                "provider.requestFields.thinking.type conflicts with reasoning",
            ),
            (
                ANTHROPIC,
                "requestFields = { thinking = { display = 'omitted' } }",
                "provider.requestFields.thinking.display conflicts with reasoning",
            ),
            (
                ANTHROPIC,
                "requestFields = { output_config = { effort = 'max' } }",
                "provider.requestFields.output_config.effort conflicts with reasoning",
            ),
        ] {
            let error = format!("{:#}", with_reasoning(api, text).unwrap_err());
            assert!(error.contains(expected), "{api}, {text}: {error}");
        }
        // Without a reasoning level, the field is the only one that sets it; the other
        // settings of `output_config` are not the level's.
        openai_config(&table("requestFields = { reasoning_effort = 'high' }"), None).unwrap();
        anthropic_config(&table("requestFields = { thinking = { type = 'disabled' } }"), None).unwrap();
        with_reasoning(
            ANTHROPIC,
            "requestFields = { output_config = { format = { type = 'json_schema', schema = {} } } }",
        )
        .unwrap();
        // Nor are the other settings of `thinking`, such as what the API does with thinking
        // bound to another conversation.
        with_reasoning(
            ANTHROPIC,
            "requestFields = { thinking = { block_binding = { prefix_mismatch_behavior = 'drop_block' } } }",
        )
        .unwrap();
    }
}
