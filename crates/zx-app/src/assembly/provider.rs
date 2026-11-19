//! The provider of an agent: the factory registered under the name config.toml gives in
//! `provider.api` builds it from the `[provider]` table. `openai` and `anthropic` are
//! built in; a host registers others with
//! [`Application::register_provider`](super::Application::register_provider).

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value};

use super::settings::BASE_URL_ENV;
use crate::config::{ANTHROPIC, OPENAI, anthropic_config, openai_config};
use zx_agent::ai::anthropic::AnthropicMessages;
use zx_agent::ai::openai::{BodyOptions, OpenAiCompatible};
use zx_agent::ai::{Provider, Reasoning};
use zx_util::registry::Registry;

/// Builds the provider config.toml names in `provider.api`.
pub trait ProviderFactory: Send + Sync {
    /// The variable that holds the API key unless `provider.apiKeyEnv` names another.
    fn api_key_env(&self) -> &str;

    /// The provider `input` describes. Fails on settings it cannot use; the error names the
    /// keys as config.toml writes them.
    fn build(&self, input: ProviderInput<'_>) -> Result<BuiltProvider>;
}

/// What a factory builds a provider from.
pub struct ProviderInput<'a> {
    /// The provider's own keys of the `[provider]` table: all but `api`, `apiKeyEnv` and
    /// `maxRetries`.
    pub options: &'a toml::Table,
    /// The key the variable holds, when it holds one.
    pub api_key: Option<String>,
    /// The reasoning level of config.toml, for a factory whose settings can conflict with it.
    pub reasoning: Option<Reasoning>,
}

/// A provider a factory built.
pub struct BuiltProvider {
    pub provider: Arc<dyn Provider>,
    /// Whether requests carry the session's ID as the server's cache key
    /// ([`AgentConfig::session_cache_key`](zx_agent::AgentConfig::session_cache_key)).
    pub session_cache_key: bool,
}

/// The providers every application has, by name.
pub(super) fn built_in() -> Registry<dyn ProviderFactory> {
    let openai: Arc<dyn ProviderFactory> = Arc::new(OpenAi);
    let anthropic: Arc<dyn ProviderFactory> = Arc::new(Anthropic);
    Registry::new(
        "provider",
        vec![(OPENAI.to_string(), openai), (ANTHROPIC.to_string(), anthropic)],
    )
}

/// What a setting of the provider that `ZX_BASE_URL` may have replaced is said in.
fn invalid_settings() -> String {
    format!("invalid [provider] settings in config.toml, or {BASE_URL_ENV} in place of its baseUrl")
}

/// The provider of OpenAI-compatible servers.
struct OpenAi;

impl ProviderFactory for OpenAi {
    fn api_key_env(&self) -> &str {
        "OPENAI_API_KEY"
    }

    fn build(&self, input: ProviderInput<'_>) -> Result<BuiltProvider> {
        let mut config = openai_config(input.options)?;
        if input.reasoning.is_some() {
            check_reasoning_fields(&config.body)?;
        }
        config.api_key = input.api_key.into();
        let session_cache_key = config.body.prompt_cache_key;
        let provider = OpenAiCompatible::new(config).with_context(invalid_settings)?;
        Ok(BuiltProvider {
            provider: Arc::new(provider),
            session_cache_key,
        })
    }
}

/// Refuse request fields that the reasoning level sets, with the format the settings write
/// it in: which of the two would reach the server is not for the provider to choose.
fn check_reasoning_fields(body: &BodyOptions) -> Result<()> {
    if let Some(field) = body
        .reasoning_format
        .fields()
        .iter()
        .find(|field| body.extra_fields.contains_key(**field))
    {
        anyhow::bail!(
            "provider.requestFields.{field} conflicts with reasoning, which sets it with provider.reasoningFormat {}; use one of them",
            body.reasoning_format
        );
    }
    Ok(())
}

/// The provider of the Anthropic Messages API.
struct Anthropic;

impl ProviderFactory for Anthropic {
    fn api_key_env(&self) -> &str {
        "ANTHROPIC_API_KEY"
    }

    fn build(&self, input: ProviderInput<'_>) -> Result<BuiltProvider> {
        let mut config = anthropic_config(input.options)?;
        if input.reasoning.is_some() {
            check_thinking_fields(&config.body.extra_fields)?;
        }
        config.api_key = input.api_key.into();
        let provider = AnthropicMessages::new(config).with_context(invalid_settings)?;
        Ok(BuiltProvider {
            provider: Arc::new(provider),
            // Its cache is the prefix of a request, marked in the body.
            session_cache_key: false,
        })
    }
}

/// Refuse request fields that the reasoning level sets, `thinking.type`, `thinking.display`
/// and `output_config.effort`, as [`check_reasoning_fields`] does for the OpenAI-compatible
/// provider. The other settings of `thinking` and `output_config` go with the level's, such
/// as `thinking.block_binding`.
fn check_thinking_fields(fields: &Map<String, Value>) -> Result<()> {
    for (field, key) in [
        ("thinking", "type"),
        ("thinking", "display"),
        ("output_config", "effort"),
    ] {
        ensure!(
            fields.get(field).and_then(|value| value.get(key)).is_none(),
            "provider.requestFields.{field}.{key} conflicts with reasoning, which sets it; use one of them"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(text: &str) -> toml::Table {
        toml::from_str(text).unwrap()
    }

    fn built(api: &str, text: &str, reasoning: Option<Reasoning>) -> Result<BuiltProvider> {
        let registered = built_in();
        registered.get(api).map_err(anyhow::Error::msg)?.build(ProviderInput {
            options: &options(text),
            api_key: None,
            reasoning,
        })
    }

    #[test]
    fn the_built_in_providers_are_found_by_name() {
        let openai = built(OPENAI, "promptCacheKey = true", None).unwrap();
        assert_eq!(openai.provider.name(), "openai-compatible");
        assert!(openai.session_cache_key);
        let anthropic = built(ANTHROPIC, "", None).unwrap();
        assert_eq!(anthropic.provider.name(), "anthropic");
        assert!(!anthropic.session_cache_key);
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
            let error = format!("{:#}", built(api, text, Some(Reasoning::Low)).err().unwrap());
            assert!(error.contains(expected), "{api}, {text}: {error}");
        }
        // Without a reasoning level, the field is the only one that sets it; the other
        // settings of `output_config` are not the level's.
        built(OPENAI, "requestFields = { reasoning_effort = 'high' }", None).unwrap();
        built(ANTHROPIC, "requestFields = { thinking = { type = 'disabled' } }", None).unwrap();
        built(
            ANTHROPIC,
            "requestFields = { output_config = { format = { type = 'json_schema', schema = {} } } }",
            Some(Reasoning::Low),
        )
        .unwrap();
        // Nor are the other settings of `thinking`, such as what the API does with thinking
        // bound to another conversation.
        built(
            ANTHROPIC,
            "requestFields = { thinking = { block_binding = { prefix_mismatch_behavior = 'drop_block' } } }",
            Some(Reasoning::Low),
        )
        .unwrap();
    }

    #[test]
    fn an_address_requests_cannot_go_to_names_where_it_may_come_from() {
        let error = format!("{:#}", built(ANTHROPIC, "baseUrl = 'ftp://x'", None).err().unwrap());
        assert!(
            error.contains(BASE_URL_ENV) && error.contains("http or https"),
            "{error}"
        );
    }
}
