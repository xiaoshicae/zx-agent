//! The settings of the provider and their checks: where to connect, how to authenticate, and
//! how requests are written.

use std::fmt;
use std::time::Duration;

use serde::de::IntoDeserializer;
use serde::de::value::{Error as NameError, StrDeserializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zx_ai::{ApiKey, Proxy};
use zx_ai_http::DEFAULT_MAX_EVENT_BYTES;
use zx_ai_http::settings::{check_base_url, check_headers, check_limits, check_provider_name};

/// Default maximum raw SSE response bytes per model request, including reasoning.
pub(crate) const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Request body fields that the provider sets itself, so that
/// [`BodyOptions::extra_fields`] cannot replace them. The output token limit is the
/// provider's under both of its names ([`MaxTokensField`]): a request carries one of them.
pub(crate) const RESERVED_FIELDS: [&str; 7] = [
    "model",
    "messages",
    "stream",
    "stream_options",
    "tools",
    "max_tokens",
    "max_completion_tokens",
];

/// Connection settings for an OpenAI-compatible server. Debug output redacts the API key.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// Base URL without `/chat/completions`, e.g. `https://api.openai.com/v1`: an http or
    /// https address that ends with its path, without a query or a fragment.
    pub base_url: String,
    /// The bearer token, fixed or asked for before each request. Local servers often need
    /// none.
    pub api_key: ApiKey,
    /// Headers added to every request, such as a gateway's routing header. `Authorization`
    /// is refused: the key goes through `api_key`. Neither the names nor the values are
    /// recorded in the session.
    pub headers: Vec<(String, String)>,
    /// Name recorded on assistant messages and with every request; not blank.
    pub provider_name: String,
    /// Longest wait for response headers and between two stream chunks.
    pub idle_timeout: Duration,
    /// How requests reach the network: the environment's proxies by default.
    pub proxy: Proxy,
    /// Maximum raw streaming response bytes per request.
    pub max_response_bytes: usize,
    /// Maximum bytes in a single SSE event, including framing.
    pub max_sse_event_bytes: usize,
    /// How request bodies are written: the one part of these settings that the session
    /// records with every request.
    pub body: BodyOptions,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: ApiKey::None,
            headers: Vec::new(),
            provider_name: "openai-compatible".to_string(),
            idle_timeout: Duration::from_secs(120),
            proxy: Proxy::default(),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_sse_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            body: BodyOptions::default(),
        }
    }
}

impl OpenAiConfig {
    /// Check the settings. Errors: a blank provider name, an address requests cannot go to,
    /// a byte budget or a timeout of zero, an extra field the provider sets itself, and a
    /// header the client cannot send.
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        check_provider_name(&self.provider_name)?;
        check_base_url(&self.base_url)?;
        check_limits(self.idle_timeout, self.max_response_bytes, self.max_sse_event_bytes)?;
        self.body.check()?;
        check_headers(&self.headers, "Authorization")
    }
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug output can reach logs and panic messages; the key must not.
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key)
            .field(
                "headers",
                &self.headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            )
            .field("provider_name", &self.provider_name)
            .field("idle_timeout", &self.idle_timeout)
            .field("proxy", &self.proxy)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("max_sse_event_bytes", &self.max_sse_event_bytes)
            .field("body", &self.body)
            .finish()
    }
}

/// How the body of a request is written: every setting that changes it, and nothing secret.
///
/// This one value is what [`Provider::request_options`](zx_ai::Provider::request_options)
/// gives the session to record with each request and what [`replay_body`](crate::replay_body)
/// rebuilds a body from, so a setting added here is recorded and replayed with the others.
/// The record leaves out a setting at its default, except `system_updates`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BodyOptions {
    /// How system prompt changes after the first one reach the model.
    pub system_updates: SystemUpdates,
    /// Fields added as they are to every request body, for settings that some servers offer,
    /// such as `thinking` or `reasoning_effort`. The fields the provider sets itself are
    /// refused when the provider is built; those a request's reasoning level decides are left
    /// out of a request that carries a level ([`ReasoningFormat`]).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra_fields: Map<String, Value>,
    /// Once one assistant message of a request carries `reasoning_content`, give the others
    /// an empty one, for servers that reject a history in which only some answers carry
    /// reasoning. Off unless set: other servers have no use for the field.
    #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
    pub fill_missing_reasoning: bool,
    /// How a request's [`Reasoning`](zx_ai::Reasoning) level is written.
    #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
    pub reasoning_format: ReasoningFormat,
    /// Send the request's [`Request::cache_key`](zx_ai::Request::cache_key) as
    /// `prompt_cache_key`, for servers that take one (OpenAI does). Off by default: other
    /// servers reject unknown fields.
    #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
    pub prompt_cache_key: bool,
    /// The field that carries a request's output token limit.
    #[serde(default, skip_serializing_if = "zx_util::default::is_default")]
    pub max_tokens_field: MaxTokensField,
}

impl BodyOptions {
    /// Refuse extra fields that would replace what the provider sets itself.
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        let fields = &self.extra_fields;
        if let Some(field) = fields.keys().find(|field| RESERVED_FIELDS.contains(&field.as_str())) {
            anyhow::bail!("request field {field:?} cannot be added: the provider sets it itself");
        }
        if self.prompt_cache_key && fields.contains_key("prompt_cache_key") {
            anyhow::bail!(
                "request field \"prompt_cache_key\" cannot be added: the provider sets it from the cache key"
            );
        }
        Ok(())
    }

    /// The first extra field that a request's reasoning level decides, in the format these
    /// options write it in, such as `reasoning_effort`: one that only a request without a
    /// level sends ([`ReasoningFormat`]). A host that sets a level refuses such a field, so
    /// that one setting says how hard the model thinks.
    pub fn reasoning_overlap(&self) -> Option<String> {
        self.reasoning_format
            .fields()
            .iter()
            .find(|field| self.extra_fields.contains_key(**field))
            .map(ToString::to_string)
    }
}

/// How a request's reasoning level is written into the body.
///
/// Servers differ, and the provider does not guess from a server's name: the formats are
/// named after the field they write, not after a service.
///
/// A level chosen for a request decides the fields of the format alone, at every level, `off`
/// included: a value that [`BodyOptions::extra_fields`] fixes for one of them is not sent
/// with that request. A request without a level sends the extra fields as they are, and the
/// format writes nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningFormat {
    /// `reasoning_effort: "<level>"`, the OpenAI field. `off` sends nothing, since the field
    /// has no such value.
    #[default]
    Effort,
    /// `thinking: {"type": "enabled"}` with `reasoning_effort`, or `{"type": "disabled"}` for
    /// `off`, for servers that switch thinking on and off with a `thinking` object.
    Thinking,
    /// `reasoning: {"effort": "<level>"}`, with `none` for `off`, for servers that take an
    /// object named `reasoning`.
    ReasoningObject,
}

impl ReasoningFormat {
    /// The request body fields this format writes: the ones a request's level decides.
    pub(crate) fn fields(self) -> &'static [&'static str] {
        match self {
            ReasoningFormat::Effort => &["reasoning_effort"],
            ReasoningFormat::Thinking => &["thinking", "reasoning_effort"],
            ReasoningFormat::ReasoningObject => &["reasoning"],
        }
    }
}

/// How system prompt changes after the first one are sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SystemUpdates {
    /// Fold versions into one leading system message, for servers that reject mid-conversation
    /// system messages. Prompt changes can reduce reuse of the previous request prefix.
    #[default]
    Collapse,
    /// Send changes at their original positions. With other request settings unchanged,
    /// earlier messages remain a prefix. The server must accept mid-conversation system messages.
    Inline,
}

/// The request body field that carries a request's output token limit.
///
/// The provider does not guess the field from a server's name. The OpenAI API deprecated
/// `max_tokens` for `max_completion_tokens`, and its o-series models do not take the older
/// field; which of the two another server takes is in its documentation. The values are
/// named after the field they write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// `max_tokens`, the original name of the limit.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`, the name that replaced it in the OpenAI API.
    MaxCompletionTokens,
}

/// `Display` and `FromStr` for settings whose values have names: the names serde gives the
/// variants, so that configuration, the record of a request and messages spell a value one
/// way. Parsing ignores case and surrounding whitespace; its error lists the names.
macro_rules! named_values {
    ($($setting:ty),+) => {$(
        impl fmt::Display for $setting {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.serialize(f)
            }
        }

        impl std::str::FromStr for $setting {
            type Err = String;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                let name = text.trim().to_ascii_lowercase();
                let name: StrDeserializer<'_, NameError> = name.as_str().into_deserializer();
                Self::deserialize(name).map_err(|error| error.to_string())
            }
        }
    )+};
}

named_values!(ReasoningFormat, SystemUpdates, MaxTokensField);

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::OpenAiCompatible;

    #[test]
    fn the_extra_field_a_reasoning_level_also_sets_is_named_by_the_format() {
        let options = |reasoning_format, field: &str| {
            let mut extra_fields = Map::new();
            extra_fields.insert(field.into(), json!(1));
            BodyOptions {
                reasoning_format,
                extra_fields,
                ..BodyOptions::default()
            }
        };
        assert_eq!(
            options(ReasoningFormat::Effort, "reasoning_effort").reasoning_overlap(),
            Some("reasoning_effort".into())
        );
        assert_eq!(
            options(ReasoningFormat::Thinking, "thinking").reasoning_overlap(),
            Some("thinking".into())
        );
        assert_eq!(options(ReasoningFormat::Effort, "thinking").reasoning_overlap(), None);
    }

    #[test]
    fn debug_output_never_contains_the_api_key() {
        let config = OpenAiConfig {
            api_key: ApiKey::fixed("sk-debug-secret"),
            ..OpenAiConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-debug-secret"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
    }

    #[test]
    fn building_the_provider_runs_the_shared_checks_of_its_settings() {
        for config in [
            OpenAiConfig {
                base_url: "ftp://example.com".into(),
                ..OpenAiConfig::default()
            },
            OpenAiConfig {
                idle_timeout: Duration::ZERO,
                ..OpenAiConfig::default()
            },
            OpenAiConfig {
                headers: vec![("bad name".into(), "x".into())],
                ..OpenAiConfig::default()
            },
            OpenAiConfig {
                provider_name: " ".into(),
                ..OpenAiConfig::default()
            },
        ] {
            assert!(OpenAiCompatible::new(config).is_err());
        }
    }

    #[test]
    fn the_authorization_header_comes_from_the_api_key() {
        let error = OpenAiCompatible::new(OpenAiConfig {
            headers: vec![("Authorization".into(), "Bearer x".into())],
            ..OpenAiConfig::default()
        })
        .err()
        .expect("the header is refused");
        assert!(error.to_string().contains("Authorization"), "{error}");
    }

    #[test]
    fn reasoning_formats_parse_by_name() {
        assert_eq!("Thinking".parse::<ReasoningFormat>(), Ok(ReasoningFormat::Thinking));
        assert_eq!(
            "reasoning-object".parse::<ReasoningFormat>(),
            Ok(ReasoningFormat::ReasoningObject)
        );
        assert!("other".parse::<ReasoningFormat>().is_err());
    }

    /// Check that each of `values` displays the name its record holds and parses from it.
    fn assert_named<T>(values: &[T])
    where
        T: Copy + PartialEq + fmt::Debug + fmt::Display + Serialize + std::str::FromStr<Err = String>,
    {
        for value in values {
            let name = value.to_string();
            assert_eq!(serde_json::to_value(value).unwrap(), json!(name));
            assert_eq!(name.parse::<T>().as_ref(), Ok(value));
        }
    }

    #[test]
    fn a_named_setting_parses_from_the_name_it_displays_and_records() {
        assert_named(&[SystemUpdates::Collapse, SystemUpdates::Inline]);
        assert_named(&[
            ReasoningFormat::Effort,
            ReasoningFormat::Thinking,
            ReasoningFormat::ReasoningObject,
        ]);
        assert_named(&[MaxTokensField::MaxTokens, MaxTokensField::MaxCompletionTokens]);
        assert_eq!(SystemUpdates::Inline.to_string(), "inline");
        assert_eq!(ReasoningFormat::ReasoningObject.to_string(), "reasoning-object");
        assert_eq!(MaxTokensField::MaxCompletionTokens.to_string(), "max_completion_tokens");
    }

    #[test]
    fn an_unknown_name_is_refused_with_the_names_there_are() {
        let error = "sideways".parse::<SystemUpdates>().unwrap_err();
        assert!(error.contains("`collapse`") && error.contains("`inline`"), "{error}");
        let error = "max_output_tokens".parse::<MaxTokensField>().unwrap_err();
        assert!(
            error.contains("`max_tokens`") && error.contains("`max_completion_tokens`"),
            "{error}"
        );
    }

    #[test]
    fn a_cache_key_field_cannot_be_added_when_the_provider_sets_the_cache_key() {
        let mut extra_fields = Map::new();
        extra_fields.insert("prompt_cache_key".into(), json!("mine"));
        let error = OpenAiCompatible::new(OpenAiConfig {
            body: BodyOptions {
                prompt_cache_key: true,
                extra_fields,
                ..BodyOptions::default()
            },
            ..OpenAiConfig::default()
        })
        .map(|_| ())
        .unwrap_err();
        assert!(error.to_string().contains("prompt_cache_key"), "{error}");
    }
}
