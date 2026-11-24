//! The provider of an agent: the factory registered under the name config.toml gives in
//! `provider.api` builds it from the `[provider]` table. `openai` and `anthropic` are
//! built in; a host registers others with
//! [`Application::register_provider`](super::Application::register_provider).

use std::sync::Arc;

use anyhow::{Context, Result};
use zx_agent::ai::anthropic::AnthropicMessages;
use zx_agent::ai::openai::OpenAiCompatible;
use zx_agent::ai::{Provider, Proxy, Reasoning};
use zx_util::registry::Registry;

use super::settings::BASE_URL_ENV;
use crate::config::{ANTHROPIC, OPENAI, anthropic_config, openai_config};

/// Builds the provider config.toml names in `provider.api`.
pub trait ProviderFactory: Send + Sync {
    /// The variable that holds the API key when config.toml writes no `provider.apiKey`.
    fn api_key_env(&self) -> &str;

    /// The provider `input` describes. Fails on settings it cannot use; the error names the
    /// keys as config.toml writes them.
    fn build(&self, input: ProviderInput<'_>) -> Result<BuiltProvider>;
}

/// What a factory builds a provider from.
pub struct ProviderInput<'a> {
    /// The provider's own keys of the `[provider]` table, their placeholders replaced: all but
    /// `api`, `apiKey` and `maxRetries`.
    pub options: &'a toml::Table,
    /// The key the variable holds, when it holds one.
    pub api_key: Option<String>,
    /// The reasoning level of config.toml, for a factory whose settings can conflict with it.
    pub reasoning: Option<Reasoning>,
    /// How the provider's requests reach the network: through the proxies of config.toml's
    /// `[proxy]`, else as the environment says.
    pub proxy: &'a Proxy,
}

/// A provider a factory built.
pub struct BuiltProvider {
    /// The provider the agent's sessions ask.
    pub provider: Arc<dyn Provider>,
    /// Whether requests carry the session's ID as the server's cache key
    /// ([`AgentConfig::session_cache_key`](zx_agent::AgentConfig::session_cache_key)).
    pub session_cache_key: bool,
}

/// The variable that holds the API key of the OpenAI-compatible provider when config.toml
/// writes no `provider.apiKey`.
const DEFAULT_OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
/// The variable that holds the API key of the Anthropic provider when config.toml writes no
/// `provider.apiKey`.
const DEFAULT_ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

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
        DEFAULT_OPENAI_API_KEY_ENV
    }

    fn build(&self, input: ProviderInput<'_>) -> Result<BuiltProvider> {
        let mut config = openai_config(input.options, input.reasoning)?;
        config.api_key = input.api_key.into();
        config.proxy = input.proxy.clone();
        let session_cache_key = config.body.prompt_cache_key;
        let provider = OpenAiCompatible::new(config).with_context(invalid_settings)?;
        Ok(BuiltProvider {
            provider: Arc::new(provider),
            session_cache_key,
        })
    }
}

/// The provider of the Anthropic Messages API.
struct Anthropic;

impl ProviderFactory for Anthropic {
    fn api_key_env(&self) -> &str {
        DEFAULT_ANTHROPIC_API_KEY_ENV
    }

    fn build(&self, input: ProviderInput<'_>) -> Result<BuiltProvider> {
        let mut config = anthropic_config(input.options, input.reasoning)?;
        config.api_key = input.api_key.into();
        config.proxy = input.proxy.clone();
        let provider = AnthropicMessages::new(config).with_context(invalid_settings)?;
        Ok(BuiltProvider {
            provider: Arc::new(provider),
            // Its cache is the prefix of a request, marked in the body.
            session_cache_key: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use zx_agent::ai::Proxies;

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
            proxy: &Proxy::Direct,
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
    fn the_built_in_factories_check_the_settings_against_the_reasoning_level() {
        for (api, text) in [
            (OPENAI, "requestFields = { reasoning_effort = 'high' }"),
            (ANTHROPIC, "requestFields = { thinking = { type = 'disabled' } }"),
        ] {
            let error = format!("{:#}", built(api, text, Some(Reasoning::Low)).err().unwrap());
            assert!(error.contains("conflicts with reasoning"), "{api}: {error}");
            built(api, text, None).unwrap();
        }
    }

    #[test]
    fn the_built_in_factories_send_through_the_proxy_they_are_given() {
        // A proxy that the client refuses shows that it reached the provider.
        let proxy = Proxy::Through(Proxies {
            https: Some("socks5://proxy.internal:1080".into()),
            ..Proxies::default()
        });
        let registered = built_in();
        for api in [OPENAI, ANTHROPIC] {
            let built = registered.get(api).unwrap().build(ProviderInput {
                options: &options(""),
                api_key: None,
                reasoning: None,
                proxy: &proxy,
            });
            let error = format!("{:#}", built.err().unwrap());
            assert!(error.contains("not socks5://"), "{api}: {error}");
        }
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
