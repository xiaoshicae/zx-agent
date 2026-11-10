//! The settings of the provider and their checks: where to connect, how to authenticate, and
//! how requests are written.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::sse::DEFAULT_MAX_EVENT_BYTES;

/// Default maximum raw SSE response bytes per model request, including reasoning.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Request body fields that the provider sets itself, so that
/// [`OpenAiConfig::extra_fields`] cannot replace them.
pub(crate) const RESERVED_FIELDS: [&str; 6] = ["model", "messages", "stream", "stream_options", "tools", "max_tokens"];

/// Connection settings for an OpenAI-compatible server. Debug output redacts the API key.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// Base URL without `/chat/completions`, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// The bearer token, fixed or asked for before each request. Local servers often need
    /// none.
    pub api_key: ApiKey,
    /// Headers added to every request, such as a gateway's routing header. `Authorization`
    /// is refused: the key goes through `api_key`. Neither the names nor the values are
    /// recorded in the session.
    pub headers: Vec<(String, String)>,
    /// Send the request's [`Request::cache_key`](zx_ai::Request::cache_key) as
    /// `prompt_cache_key`, for servers that take one (OpenAI does). Off by default: other
    /// servers reject unknown fields. Part of
    /// [`Provider::request_options`](zx_ai::Provider::request_options).
    pub prompt_cache_key: bool,
    /// Name recorded on assistant messages.
    pub provider_name: String,
    /// Longest wait for response headers and between two stream chunks.
    pub idle_timeout: Duration,
    /// Honor HTTP(S)_PROXY environment variables.
    pub use_system_proxy: bool,
    /// How system prompt changes after the first one reach the model.
    pub system_updates: SystemUpdates,
    /// Maximum raw streaming response bytes per request.
    pub max_response_bytes: usize,
    /// Maximum bytes in a single SSE event, including framing.
    pub max_sse_event_bytes: usize,
    /// Fields added as they are to every request body, for settings that some servers offer,
    /// such as `thinking` or `reasoning_effort`. The fields the provider sets itself are
    /// refused when the provider is built. They are part of
    /// [`Provider::request_options`](zx_ai::Provider::request_options), so the session's
    /// request audit records them.
    pub extra_fields: Map<String, Value>,
    /// Once one assistant message of a request carries `reasoning_content`, give the others
    /// an empty one, for servers that reject a history in which only some answers carry
    /// reasoning. Off unless set: other servers have no use for the field. Part of
    /// [`Provider::request_options`](zx_ai::Provider::request_options).
    pub fill_missing_reasoning: bool,
    /// How a request's [`Reasoning`](zx_ai::Reasoning) level is written in the body. Part of
    /// [`Provider::request_options`](zx_ai::Provider::request_options) when it is not the
    /// default.
    pub reasoning_format: ReasoningFormat,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: ApiKey::None,
            headers: Vec::new(),
            prompt_cache_key: false,
            provider_name: "openai-compatible".to_string(),
            idle_timeout: Duration::from_secs(120),
            use_system_proxy: true,
            system_updates: SystemUpdates::default(),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_sse_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            extra_fields: Map::new(),
            fill_missing_reasoning: false,
            reasoning_format: ReasoningFormat::default(),
        }
    }
}

impl OpenAiConfig {
    /// Check the settings. Errors: a byte budget or a timeout of zero, an extra field the
    /// provider sets itself, and a header the client cannot send.
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_response_bytes > 0,
            "max_response_bytes must be greater than zero"
        );
        anyhow::ensure!(
            self.max_sse_event_bytes > 0,
            "max_sse_event_bytes must be greater than zero"
        );
        // Zero would time out every request at once rather than disable the limit.
        anyhow::ensure!(!self.idle_timeout.is_zero(), "idle_timeout must be greater than zero");
        check_extra_fields(&self.extra_fields, self.prompt_cache_key)?;
        check_headers(&self.headers)
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
            .field("prompt_cache_key", &self.prompt_cache_key)
            .field("provider_name", &self.provider_name)
            .field("idle_timeout", &self.idle_timeout)
            .field("use_system_proxy", &self.use_system_proxy)
            .field("system_updates", &self.system_updates)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("max_sse_event_bytes", &self.max_sse_event_bytes)
            .field("extra_fields", &self.extra_fields.keys().collect::<Vec<_>>())
            .field("fill_missing_reasoning", &self.fill_missing_reasoning)
            .field("reasoning_format", &self.reasoning_format)
            .finish()
    }
}

/// Where the bearer token of a request comes from.
#[derive(Clone, Default)]
pub enum ApiKey {
    /// No `Authorization` header.
    #[default]
    None,
    /// The same token for every request.
    Fixed(String),
    /// A token asked for before every request, for credentials that expire (pi:
    /// `getApiKey`). A failure fails the request with
    /// [`ProviderError::ApiKey`](zx_ai::ProviderError::ApiKey), which is not retried.
    Dynamic(Arc<dyn ApiKeySource>),
}

/// Supplies the bearer token for the next request; see [`ApiKey::Dynamic`].
pub trait ApiKeySource: Send + Sync {
    /// The token to send. Called before every request, on the request's task; it must not
    /// block, and should return promptly or fail.
    fn api_key(&self) -> Result<String, String>;
}

impl ApiKey {
    /// A fixed token; an empty one is no token.
    pub fn fixed(key: impl Into<String>) -> Self {
        let key = key.into();
        if key.is_empty() { Self::None } else { Self::Fixed(key) }
    }

    /// The token for the next request, or `None` to send no `Authorization` header.
    pub(crate) fn resolve(&self) -> Result<Option<String>, String> {
        match self {
            Self::None => Ok(None),
            Self::Fixed(key) => Ok(Some(key.clone())),
            Self::Dynamic(source) => source.api_key().map(|key| (!key.is_empty()).then_some(key)),
        }
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug output can reach logs and panic messages; the key must not.
        f.write_str(match self {
            Self::None => "none",
            Self::Fixed(_) => "[REDACTED]",
            Self::Dynamic(_) => "[dynamic]",
        })
    }
}

impl From<Option<String>> for ApiKey {
    fn from(key: Option<String>) -> Self {
        key.map_or(Self::None, Self::fixed)
    }
}

impl From<String> for ApiKey {
    fn from(key: String) -> Self {
        Self::fixed(key)
    }
}

impl From<&str> for ApiKey {
    fn from(key: &str) -> Self {
        Self::fixed(key)
    }
}

/// How a request's reasoning level is written into the body.
///
/// Servers differ, and the provider does not guess from a server's name: the formats are
/// named after the field they write, not after a service. Nothing is sent for a request
/// without a level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    /// The name used in request options and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningFormat::Effort => "effort",
            ReasoningFormat::Thinking => "thinking",
            ReasoningFormat::ReasoningObject => "reasoning-object",
        }
    }

    /// The request body fields this format writes, which extra fields must not also set.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            ReasoningFormat::Effort => &["reasoning_effort"],
            ReasoningFormat::Thinking => &["thinking", "reasoning_effort"],
            ReasoningFormat::ReasoningObject => &["reasoning"],
        }
    }
}

impl std::str::FromStr for ReasoningFormat {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            ReasoningFormat::Effort,
            ReasoningFormat::Thinking,
            ReasoningFormat::ReasoningObject,
        ]
        .into_iter()
        .find(|format| format.as_str() == text.trim().to_ascii_lowercase())
        .ok_or_else(|| format!("unknown reasoning format {text:?}; use effort, thinking or reasoning-object"))
    }
}

/// How system prompt changes after the first one are sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemUpdates {
    /// Fold versions into one leading system message, for servers that reject mid-conversation
    /// system messages. Prompt changes can reduce reuse of the previous request prefix.
    #[default]
    Collapse,
    /// Send changes at their original positions. With other request settings unchanged,
    /// earlier messages remain a prefix. The server must accept mid-conversation system messages.
    Inline,
}

impl SystemUpdates {
    /// The name used in request options.
    pub fn as_str(self) -> &'static str {
        match self {
            SystemUpdates::Collapse => "collapse",
            SystemUpdates::Inline => "inline",
        }
    }
}

impl std::str::FromStr for SystemUpdates {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [SystemUpdates::Collapse, SystemUpdates::Inline]
            .into_iter()
            .find(|mode| mode.as_str() == text)
            .ok_or_else(|| format!("unknown system update mode {text:?}; use collapse or inline"))
    }
}

/// Refuse extra fields that would replace what the provider sets itself.
pub(crate) fn check_extra_fields(fields: &Map<String, Value>, prompt_cache_key: bool) -> anyhow::Result<()> {
    if let Some(field) = fields.keys().find(|field| RESERVED_FIELDS.contains(&field.as_str())) {
        anyhow::bail!("request field {field:?} cannot be added: the provider sets it itself");
    }
    if prompt_cache_key && fields.contains_key("prompt_cache_key") {
        anyhow::bail!("request field \"prompt_cache_key\" cannot be added: the provider sets it from the cache key");
    }
    Ok(())
}

/// Refuse headers the client cannot send: malformed ones, and `Authorization`, which the
/// key sets.
fn check_headers(headers: &[(String, String)]) -> anyhow::Result<()> {
    for (name, value) in headers {
        anyhow::ensure!(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok(),
            "invalid request header name {name:?}"
        );
        anyhow::ensure!(
            reqwest::header::HeaderValue::from_str(value).is_ok(),
            "invalid value for request header {name:?}"
        );
        anyhow::ensure!(
            !name.eq_ignore_ascii_case("authorization"),
            "the Authorization header comes from the API key; it cannot be set as a request header"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zx_ai::Reasoning;

    use super::*;
    use crate::OpenAiCompatible;

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
    fn a_zero_idle_timeout_is_rejected() {
        let config = OpenAiConfig {
            idle_timeout: Duration::ZERO,
            ..OpenAiConfig::default()
        };
        let error = OpenAiCompatible::new(config).err().expect("a zero timeout is rejected");
        assert!(
            error.to_string().contains("timeout must be greater than zero"),
            "{error}"
        );
    }

    #[test]
    fn zero_stream_budgets_are_rejected() {
        for config in [
            OpenAiConfig {
                max_response_bytes: 0,
                ..OpenAiConfig::default()
            },
            OpenAiConfig {
                max_sse_event_bytes: 0,
                ..OpenAiConfig::default()
            },
        ] {
            let error = OpenAiCompatible::new(config).err().expect("a zero budget is rejected");
            assert!(error.to_string().contains("must be greater than zero"), "{error}");
        }
    }

    #[test]
    fn reasoning_levels_and_formats_parse_by_name() {
        for level in Reasoning::ALL {
            assert_eq!(level.as_str().parse::<Reasoning>(), Ok(level));
        }
        assert_eq!(" HIGH ".parse::<Reasoning>(), Ok(Reasoning::High));
        assert!(
            "max"
                .parse::<Reasoning>()
                .unwrap_err()
                .contains("off, minimal, low, medium, high, xhigh")
        );
        assert_eq!("Thinking".parse::<ReasoningFormat>(), Ok(ReasoningFormat::Thinking));
        assert_eq!(
            "reasoning-object".parse::<ReasoningFormat>(),
            Ok(ReasoningFormat::ReasoningObject)
        );
        assert!("other".parse::<ReasoningFormat>().is_err());
        assert_eq!(serde_json::to_value(Reasoning::Xhigh).unwrap(), "xhigh");
    }

    #[test]
    fn system_update_modes_parse_by_their_recorded_name() {
        for mode in [SystemUpdates::Collapse, SystemUpdates::Inline] {
            assert_eq!(mode.as_str().parse::<SystemUpdates>(), Ok(mode));
        }
        assert!("sideways".parse::<SystemUpdates>().is_err());
    }

    #[test]
    fn headers_are_checked_when_the_provider_is_built() {
        let build = |headers: Vec<(&str, &str)>| {
            OpenAiCompatible::new(OpenAiConfig {
                base_url: "https://example.invalid/v1".into(),
                headers: headers
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
                ..OpenAiConfig::default()
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
        };
        assert_eq!(build(vec![("X-Route", "blue"), ("x-tenant", "a b")]), Ok(()));
        assert!(
            build(vec![("Authorization", "Bearer x")])
                .unwrap_err()
                .contains("Authorization")
        );
        assert!(
            build(vec![("bad name", "x")])
                .unwrap_err()
                .contains("invalid request header name")
        );
        assert!(
            build(vec![("x-a", "line\nbreak")])
                .unwrap_err()
                .contains("invalid value")
        );
        let mut fields = Map::new();
        fields.insert("prompt_cache_key".into(), json!("mine"));
        let error = OpenAiCompatible::new(OpenAiConfig {
            prompt_cache_key: true,
            extra_fields: fields,
            ..OpenAiConfig::default()
        })
        .map(|_| ())
        .unwrap_err();
        assert!(error.to_string().contains("prompt_cache_key"), "{error}");
        assert!(matches!(ApiKey::fixed(""), ApiKey::None));
        assert!(matches!(ApiKey::from(Some("k".to_string())), ApiKey::Fixed(key) if key == "k"));
    }
}
