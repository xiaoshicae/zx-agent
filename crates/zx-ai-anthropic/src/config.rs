//! The settings of the provider and their checks: where to connect, how to authenticate, and
//! how requests are written.

use std::fmt;
use std::time::Duration;

use serde::de::value::{Error as NameError, StrDeserializer};
use serde::de::{DeserializeOwned, IntoDeserializer};
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

/// The header the Anthropic API takes the API key in.
pub(crate) const KEY_HEADER: &str = "x-api-key";

/// The header that carries the API key as a bearer token.
const BEARER_HEADER: &str = "authorization";

/// Default maximum raw SSE response bytes per model request, thinking included.
pub(crate) const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Request body fields that the provider sets itself, so that [`BodyOptions::extra_fields`]
/// cannot replace them.
pub(crate) const RESERVED_FIELDS: [&str; 6] = ["model", "messages", "system", "tools", "max_tokens", "stream"];

/// What a request's reasoning level sets in the body, as an object and a key in it. The other
/// settings of `thinking` and `output_config` go with the level's, such as
/// `thinking.block_binding`.
pub(crate) const REASONING_FIELDS: [(&str, &str); 3] = [
    ("thinking", "type"),
    ("thinking", "display"),
    ("output_config", "effort"),
];

/// Connection settings for the Anthropic Messages API, or a server that offers the same
/// protocol. Debug output redacts the API key.
#[derive(Clone)]
pub struct AnthropicConfig {
    /// Base URL without `/v1/messages`, e.g. `https://api.anthropic.com`: an http or https
    /// address that ends with its path, without a query or a fragment.
    pub base_url: String,
    /// The key, fixed or asked for before each request, sent in the header
    /// `api_key_header` names.
    pub api_key: ApiKey,
    /// Which header carries the key: `x-api-key` unless a server that offers the same
    /// protocol takes it as a bearer token.
    pub api_key_header: ApiKeyHeader,
    /// Headers added to every request, such as `anthropic-beta`. One named
    /// `anthropic-version` replaces [`DEFAULT_API_VERSION`]; the one `api_key_header` names
    /// is refused, since the key goes through `api_key`. Neither the names nor the values
    /// are recorded in the session.
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
            api_key_header: ApiKeyHeader::default(),
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
        check_headers(&self.headers, self.api_key_header.name())
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
            .field("api_key_header", &self.api_key_header)
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

    /// The first extra field that a request's reasoning level sets too, as a dotted path such
    /// as `thinking.type`: `thinking.type`, `thinking.display` or `output_config.effort`.
    /// Which of the two reaches the server is not for the provider to choose, so a host that
    /// sets a level refuses such a field.
    pub fn reasoning_overlap(&self) -> Option<String> {
        REASONING_FIELDS
            .iter()
            .find(|(object, key)| {
                self.extra_fields
                    .get(*object)
                    .and_then(|value| value.get(key))
                    .is_some()
            })
            .map(|(object, key)| format!("{object}.{key}"))
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

impl std::str::FromStr for SystemUpdates {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        by_name(text)
    }
}

/// Which header carries the API key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKeyHeader {
    /// `x-api-key: <key>`, as the Anthropic API takes it.
    #[default]
    XApiKey,
    /// `Authorization: Bearer <key>`, as a gateway that offers the same protocol may take
    /// it, such as OpenRouter's.
    Authorization,
}

impl ApiKeyHeader {
    /// The header's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::XApiKey => KEY_HEADER,
            Self::Authorization => BEARER_HEADER,
        }
    }
}

impl std::str::FromStr for ApiKeyHeader {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        by_name(text)
    }
}

/// A setting parsed by the names serde gives its variants, as the named settings of the
/// OpenAI-compatible provider are: case and surrounding whitespace are ignored, and the error
/// lists the names.
fn by_name<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    let name = text.trim().to_ascii_lowercase();
    let name: StrDeserializer<'_, NameError> = name.as_str().into_deserializer();
    T::deserialize(name).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn system_updates_parse_from_the_names_they_are_recorded_by() {
        assert_eq!(" Inline ".parse(), Ok(SystemUpdates::Inline));
        assert_eq!("collapse".parse(), Ok(SystemUpdates::Collapse));
        let error = "merge".parse::<SystemUpdates>().unwrap_err();
        assert!(error.contains("`collapse`") && error.contains("`inline`"), "{error}");
    }

    #[test]
    fn the_key_goes_in_the_header_the_settings_name_and_no_other_header_sets_it() {
        assert_eq!(" X-API-Key ".parse(), Ok(ApiKeyHeader::XApiKey));
        assert_eq!("authorization".parse(), Ok(ApiKeyHeader::Authorization));
        let error = "bearer".parse::<ApiKeyHeader>().unwrap_err();
        assert!(
            error.contains("`x-api-key`") && error.contains("`authorization`"),
            "{error}"
        );

        let with = |api_key_header, name: &str| AnthropicConfig {
            api_key_header,
            headers: vec![(name.into(), "v".into())],
            ..AnthropicConfig::default()
        };
        assert!(with(ApiKeyHeader::Authorization, "Authorization").check().is_err());
        assert!(with(ApiKeyHeader::Authorization, "x-api-key").check().is_ok());
        assert!(with(ApiKeyHeader::XApiKey, "Authorization").check().is_ok());
    }

    #[test]
    fn the_extra_field_a_reasoning_level_also_sets_is_named() {
        let overlap = |fields: Value| {
            BodyOptions {
                extra_fields: fields.as_object().unwrap().clone(),
                ..BodyOptions::default()
            }
            .reasoning_overlap()
        };
        assert_eq!(
            overlap(json!({"thinking": {"type": "enabled"}})),
            Some("thinking.type".into())
        );
        assert_eq!(
            overlap(json!({"output_config": {"effort": "max"}})),
            Some("output_config.effort".into())
        );
        assert_eq!(
            overlap(json!({"thinking": {"block_binding": {}}, "metadata": {}})),
            None
        );
    }

    #[test]
    fn the_check_runs_the_shared_checks_of_the_settings() {
        for config in [
            AnthropicConfig {
                base_url: "ftp://example.com".into(),
                ..AnthropicConfig::default()
            },
            AnthropicConfig {
                idle_timeout: Duration::ZERO,
                ..AnthropicConfig::default()
            },
            AnthropicConfig {
                headers: vec![("X-Api-Key".into(), "k".into())],
                ..AnthropicConfig::default()
            },
        ] {
            assert!(config.check().is_err());
        }
        AnthropicConfig::default().check().unwrap();
    }

    #[test]
    fn fields_the_provider_sets_itself_cannot_be_added() {
        let mut extra_fields = Map::new();
        extra_fields.insert("max_tokens".into(), json!(1));
        let config = AnthropicConfig {
            body: BodyOptions {
                extra_fields,
                ..BodyOptions::default()
            },
            ..AnthropicConfig::default()
        };
        assert!(config.check().unwrap_err().to_string().contains("\"max_tokens\""));
    }

    #[test]
    fn a_header_names_the_api_version() {
        let config = AnthropicConfig {
            headers: vec![("Anthropic-Version".into(), "2023-06-01".into())],
            ..AnthropicConfig::default()
        };
        assert!(config.names_version());
        assert!(!AnthropicConfig::default().names_version());
    }

    #[test]
    fn debug_output_hides_the_key() {
        let config = AnthropicConfig {
            api_key: ApiKey::fixed("sk-ant-secret"),
            ..AnthropicConfig::default()
        };
        assert!(!format!("{config:?}").contains("sk-ant-secret"));
    }
}
