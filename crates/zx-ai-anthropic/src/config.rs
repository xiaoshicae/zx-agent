//! The settings of the provider and their checks: where to connect, how to authenticate, and
//! how requests are written.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zx_ai::ApiKey;
use zx_ai_http::DEFAULT_MAX_EVENT_BYTES;
use zx_ai_http::settings::{check_base_url, check_headers, check_limits};

/// The API version a request names unless a header of the settings names another: the newest,
/// whose changes from the one before touch nothing this provider sends.
pub const DEFAULT_API_VERSION: &str = "2026-10-21";

/// The header that names the API version.
pub(crate) const VERSION_HEADER: &str = "anthropic-version";

/// The header that carries the API key.
pub(crate) const KEY_HEADER: &str = "x-api-key";

/// Default maximum raw SSE response bytes per model request, thinking included.
pub(crate) const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Request body fields that the provider sets itself, so that [`BodyOptions::extra_fields`]
/// cannot replace them.
pub(crate) const RESERVED_FIELDS: [&str; 6] = ["model", "messages", "system", "tools", "max_tokens", "stream"];

/// Connection settings for the Anthropic Messages API, or a server that offers the same
/// protocol. Debug output redacts the API key.
#[derive(Clone)]
pub struct AnthropicConfig {
    /// Base URL without `/v1/messages`, e.g. `https://api.anthropic.com`: an http or https
    /// address that ends with its path, without a query or a fragment.
    pub base_url: String,
    /// The key sent as `x-api-key`, fixed or asked for before each request.
    pub api_key: ApiKey,
    /// Headers added to every request, such as `anthropic-beta`. One named
    /// `anthropic-version` replaces [`DEFAULT_API_VERSION`]; `x-api-key` is refused, since the
    /// key goes through `api_key`. Neither the names nor the values are recorded in the
    /// session.
    pub headers: Vec<(String, String)>,
    /// Name recorded on assistant messages.
    pub provider_name: String,
    /// Longest wait for response headers and between two stream chunks.
    pub idle_timeout: Duration,
    /// Honor HTTP(S)_PROXY environment variables.
    pub use_system_proxy: bool,
    /// Maximum raw streaming response bytes per request.
    pub max_response_bytes: usize,
    /// Maximum bytes in a single SSE event, including framing.
    pub max_sse_event_bytes: usize,
    /// How request bodies are written: the one part of these settings that the session
    /// records with every request.
    pub body: BodyOptions,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: ApiKey::None,
            headers: Vec::new(),
            provider_name: "anthropic".to_string(),
            idle_timeout: Duration::from_secs(120),
            use_system_proxy: true,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_sse_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            body: BodyOptions::default(),
        }
    }
}

impl AnthropicConfig {
    /// Check the settings. Errors: an address requests cannot go to, a byte budget or a
    /// timeout of zero, an extra field the provider sets itself, and a header the client
    /// cannot send.
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        check_base_url(&self.base_url)?;
        check_limits(self.idle_timeout, self.max_response_bytes, self.max_sse_event_bytes)?;
        self.body.check()?;
        check_headers(&self.headers, KEY_HEADER)
    }

    /// Whether the headers name the API version, in place of the default.
    pub(crate) fn names_version(&self) -> bool {
        self.headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(VERSION_HEADER))
    }
}

impl fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug output can reach logs and panic messages; the key must not.
        f.debug_struct("AnthropicConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key)
            .field(
                "headers",
                &self.headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            )
            .field("provider_name", &self.provider_name)
            .field("idle_timeout", &self.idle_timeout)
            .field("use_system_proxy", &self.use_system_proxy)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("max_sse_event_bytes", &self.max_sse_event_bytes)
            .field("body", &self.body)
            .finish()
    }
}

/// How the body of a request is written: every setting that changes it, and nothing secret.
/// What [`Provider::request_options`](zx_ai::Provider::request_options) gives the session to
/// record with each request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BodyOptions {
    /// How system prompt changes after the first one reach the model.
    pub system_updates: SystemUpdates,
    /// Fields added as they are to every request body, such as `metadata` or an
    /// `output_config` the request's reasoning level does not set. The fields the provider
    /// sets itself are refused when the provider is built.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra_fields: Map<String, Value>,
}

impl BodyOptions {
    /// Refuse extra fields that would replace what the provider sets itself.
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        if let Some(field) = self
            .extra_fields
            .keys()
            .find(|field| RESERVED_FIELDS.contains(&field.as_str()))
        {
            anyhow::bail!("request field {field:?} cannot be added: the provider sets it itself");
        }
        Ok(())
    }
}

/// How system prompt changes after the first one are sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SystemUpdates {
    /// Fold every version into the top-level `system` of the request, which every model
    /// takes. A change rewrites the beginning of the conversation: the cache reuses less, and
    /// a model that binds thinking to the conversation refuses the earlier thinking, which
    /// fails the request unless its `thinking.block_binding` lets the API drop that thinking.
    #[default]
    Collapse,
    /// Send each change as a system message in the conversation, right before the next answer
    /// of the model, so that earlier messages stay a prefix. Only models that accept
    /// mid-conversation system messages take it.
    Inline,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn settings_the_client_cannot_use_are_refused() {
        let refused = |config: AnthropicConfig| config.check().unwrap_err().to_string();
        assert!(
            refused(AnthropicConfig {
                base_url: "ftp://example.com".into(),
                ..AnthropicConfig::default()
            })
            .contains("http or https")
        );
        assert!(
            refused(AnthropicConfig {
                base_url: "https://example.com/?key=secret".into(),
                ..AnthropicConfig::default()
            })
            .contains("without a query")
        );
        assert!(
            refused(AnthropicConfig {
                idle_timeout: Duration::ZERO,
                ..AnthropicConfig::default()
            })
            .contains("idle_timeout")
        );
        assert!(
            refused(AnthropicConfig {
                headers: vec![("X-Api-Key".into(), "k".into())],
                ..AnthropicConfig::default()
            })
            .contains("comes from the API key")
        );
        let mut extra_fields = Map::new();
        extra_fields.insert("max_tokens".into(), json!(1));
        assert!(
            refused(AnthropicConfig {
                body: BodyOptions {
                    extra_fields,
                    ..BodyOptions::default()
                },
                ..AnthropicConfig::default()
            })
            .contains("\"max_tokens\"")
        );
        AnthropicConfig::default().check().unwrap();
    }

    #[test]
    fn a_header_names_the_api_version_and_debug_output_hides_the_key() {
        let config = AnthropicConfig {
            api_key: ApiKey::fixed("sk-ant-secret"),
            headers: vec![("Anthropic-Version".into(), "2023-06-01".into())],
            ..AnthropicConfig::default()
        };
        assert!(config.names_version());
        assert!(!AnthropicConfig::default().names_version());
        assert!(!format!("{config:?}").contains("sk-ant-secret"));
    }
}
