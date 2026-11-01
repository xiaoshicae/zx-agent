//! `zx-agent serve`: answers IM messages with one session per thread until stopped.
//! Design: docs/serve-im-review.md; the server itself is `zx_agent::im::Server`.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::NullSink;
use zx_agent::ai::now_ms;
use zx_agent::im::feishu::{FeishuChannel, FeishuConfig};
use zx_agent::im::slack::{SlackChannel, SlackConfig};
use zx_agent::im::{
    ConversationId, ErrorReport, ImChannel, ImError, Server, ServerConfig, ServerEvent, ServerSink, SessionFactory,
    SessionKey, UserId,
};
use zx_agent::interaction::Interaction;
use zx_agent::profile::Builtin;

use crate::cli::{self, Budget, ServeArgs, SessionFile, SessionOptions};
use crate::{EXIT_COMPLETED, EXIT_ERROR, EXIT_USAGE, spawn_signal_handler, write_line};

/// How long an approval card waits for a click; docs/serve-im-review.md, section 3.6.
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 600;
const DEFAULT_APP_TOKEN_ENV: &str = "SLACK_APP_TOKEN";
const DEFAULT_BOT_TOKEN_ENV: &str = "SLACK_BOT_TOKEN";
const DEFAULT_APP_ID_ENV: &str = "FEISHU_APP_ID";
const DEFAULT_APP_SECRET_ENV: &str = "FEISHU_APP_SECRET";

/// The serve configuration file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ServeFile {
    slack: Option<SlackSection>,
    feishu: Option<FeishuSection>,
    access: AccessSection,
    #[serde(default)]
    limits: LimitsSection,
    sessions: SessionsSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SlackSection {
    /// Variable holding the app-level token (`xapp-`), used for Socket Mode.
    #[serde(default = "default_app_token_env")]
    app_token_env: String,
    /// Variable holding the bot token (`xoxb-`), used to post.
    #[serde(default = "default_bot_token_env")]
    bot_token_env: String,
    /// Web API base URL, for a proxy or a test server.
    api_base: Option<String>,
}

fn default_app_token_env() -> String {
    DEFAULT_APP_TOKEN_ENV.into()
}

fn default_bot_token_env() -> String {
    DEFAULT_BOT_TOKEN_ENV.into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FeishuSection {
    /// Variable holding the App ID of the self-built app.
    #[serde(default = "default_app_id_env")]
    app_id_env: String,
    /// Variable holding the App Secret.
    #[serde(default = "default_app_secret_env")]
    app_secret_env: String,
    /// Open API base URL, for a proxy or a test server.
    api_base: Option<String>,
    /// Tenant token URL, for a proxy or a test server.
    token_url: Option<String>,
}

fn default_app_id_env() -> String {
    DEFAULT_APP_ID_ENV.into()
}

fn default_app_secret_env() -> String {
    DEFAULT_APP_SECRET_ENV.into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AccessSection {
    /// Chats the bot answers in, as `platform:id`.
    conversations: Vec<String>,
    /// Users who may approve, as `platform:id`.
    #[serde(default)]
    approvers: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LimitsSection {
    max_running: Option<NonZeroUsize>,
    approval_timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionsSection {
    dir: PathBuf,
}

/// Serve until SIGINT or SIGTERM, or until a platform fails. An `Err` is a configuration
/// problem found before serving started.
pub(crate) async fn run(args: ServeArgs) -> Result<u8> {
    anyhow::ensure!(
        args.request.session.is_none(),
        "--session does not apply to serve: each thread has its own session file under [sessions] dir"
    );
    // Startup reads; nothing runs concurrently yet.
    let text =
        std::fs::read_to_string(&args.config).with_context(|| format!("cannot read {}", args.config.display()))?;
    let file: ServeFile =
        toml::from_str(&text).with_context(|| format!("invalid serve configuration {}", args.config.display()))?;

    let mut channels: Vec<Arc<dyn ImChannel>> = Vec::new();
    let mut hidden_env = Vec::new();
    if let Some(slack) = &file.slack {
        let mut config = SlackConfig::new(
            secret(&slack.app_token_env, "the Slack app-level token")?,
            secret(&slack.bot_token_env, "the Slack bot token")?,
        );
        if let Some(base) = &slack.api_base {
            config.api_base.clone_from(base);
        }
        config.on_error = Some(failure_log("slack"));
        channels.push(Arc::new(SlackChannel::new(config).map_err(anyhow::Error::msg)?));
        hidden_env.extend([slack.app_token_env.clone(), slack.bot_token_env.clone()]);
    }
    if let Some(feishu) = &file.feishu {
        let mut config = FeishuConfig::new(
            secret(&feishu.app_id_env, "the Feishu App ID")?,
            secret(&feishu.app_secret_env, "the Feishu App Secret")?,
        );
        if let Some(base) = &feishu.api_base {
            config.api_base.clone_from(base);
        }
        if let Some(url) = &feishu.token_url {
            config.token_url.clone_from(url);
        }
        config.on_error = Some(failure_log("feishu"));
        channels.push(Arc::new(FeishuChannel::new(config).map_err(anyhow::Error::msg)?));
        // The App ID is not a secret; the App Secret is.
        hidden_env.push(feishu.app_secret_env.clone());
    }
    anyhow::ensure!(
        !channels.is_empty(),
        "{}: no IM platform is configured; add a [slack] or [feishu] section",
        args.config.display()
    );
    let platforms: HashSet<&str> = channels.iter().map(|channel| channel.platform()).collect();
    let conversations = entries(&file.access.conversations, &platforms, "access.conversations")?
        .into_iter()
        .map(|(platform, id)| (platform, ConversationId(id)))
        .collect();
    let approvers = entries(&file.access.approvers, &platforms, "access.approvers")?
        .into_iter()
        .map(|(platform, id)| (platform, UserId(id)))
        .collect();
    let dir = file.sessions.dir;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create the session directory {}", dir.display()))?;

    let instructions = cli::read_instructions(&args.base)?;
    let factory = Sessions {
        args,
        instructions,
        dir,
        approval_timeout: Duration::from_secs(
            file.limits
                .approval_timeout_secs
                .unwrap_or(DEFAULT_APPROVAL_TIMEOUT_SECS),
        ),
        hidden_env,
    };
    // A wrong model, profile or tool setting would otherwise surface only at the first
    // message, in the chat; check it once with a session that is never stored.
    factory
        .prepare(SessionFile::Memory)
        .await
        .context("cannot prepare a session with these settings")?;

    let mut config = ServerConfig {
        conversations,
        approvers,
        ..ServerConfig::default()
    };
    if let Some(running) = file.limits.max_running {
        config.max_running = running;
    }
    if let Some(secs) = file.limits.idle_secs {
        config.idle = Duration::from_secs(secs);
    }
    eprintln!(
        "[serve] platforms={}; sessions in {}",
        channels
            .iter()
            .map(|channel| channel.platform())
            .collect::<Vec<_>>()
            .join(","),
        factory.dir.display()
    );
    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());
    match Server::new(config, Arc::new(factory), Arc::new(Log))
        .run(channels, &cancel)
        .await
    {
        Ok(()) => Ok(EXIT_COMPLETED),
        Err(error @ ImError::Config(_)) => {
            eprintln!("zx-agent: {error}");
            Ok(EXIT_USAGE)
        }
        Err(error @ ImError::Transport(_)) => {
            eprintln!("zx-agent: {error}");
            Ok(EXIT_ERROR)
        }
    }
}

/// Reports a platform's failed connection attempts as `failed` lines, like the server's own.
fn failure_log(platform: &'static str) -> ErrorReport {
    Arc::new(move |error: &ImError| {
        write_line(&json!({"type": "failed", "platform": platform, "error": error.to_string(), "timeMs": now_ms()}));
    })
}

/// The value of a credential variable, which must be set and non-empty.
fn secret(name: &str, what: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => anyhow::bail!("{name} must hold {what}"),
    }
}

/// Split `platform:id` entries, each naming a configured platform.
fn entries(entries: &[String], platforms: &HashSet<&str>, field: &str) -> Result<Vec<(String, String)>> {
    entries
        .iter()
        .map(|entry| {
            let (platform, id) = entry
                .split_once(':')
                .filter(|(platform, id)| !platform.is_empty() && !id.is_empty())
                .with_context(|| format!("{field}: {entry:?} is not of the form platform:id"))?;
            anyhow::ensure!(
                platforms.contains(platform),
                "{field}: {entry:?} names {platform:?}, which is not configured"
            );
            Ok((platform.to_string(), id.to_string()))
        })
        .collect()
}

/// Opens each thread's session like chat does, with approvals in the thread.
struct Sessions {
    args: ServeArgs,
    instructions: Option<String>,
    dir: PathBuf,
    approval_timeout: Duration,
    /// Variables holding IM credentials: commands do not inherit them, and their values are
    /// redacted.
    hidden_env: Vec<String>,
}

impl Sessions {
    async fn prepare(&self, file: SessionFile<'_>) -> Result<cli::PreparedSession> {
        let mut prepared = cli::prepare(SessionOptions {
            base: &self.args.base,
            transport: &self.args.transport,
            request: &self.args.request,
            model: &self.args.base.model,
            file,
            default_profile: Builtin::Assistant,
            budget: Budget {
                max_turns: self.args.max_turns,
                timeout_secs: self.args.timeout_secs,
                continue_on_length: None,
            },
            instructions: self.instructions.as_deref(),
            hidden_env: &self.hidden_env,
        })
        .await?;
        prepared.require_bash_approval("shell commands require approval in the chat");
        prepared.config.interaction_timeout = self.approval_timeout;
        Ok(prepared)
    }

    async fn open_session(&self, key: &SessionKey, interaction: Arc<dyn Interaction>) -> Result<AgentSession> {
        let path = key.file_in(&self.dir);
        if let Some(parent) = path.parent() {
            // A short directory creation before the session file, like prepare's own startup reads.
            std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut prepared = self.prepare(SessionFile::Open(&path)).await?;
        prepared.config.interaction = interaction;
        // The session file is the record of each conversation; the operator log holds
        // approvals and failures (Log).
        prepared.into_session(Arc::new(NullSink))
    }
}

#[async_trait]
impl SessionFactory for Sessions {
    async fn open(&self, key: &SessionKey, interaction: Arc<dyn Interaction>) -> Result<AgentSession, String> {
        self.open_session(key, interaction)
            .await
            .map_err(|error| format!("{error:#}"))
    }
}

/// Writes the server's events as JSON lines on stdout, for the operator's log collection.
struct Log;

impl ServerSink for Log {
    fn emit(&self, event: &ServerEvent) {
        let line = match event {
            ServerEvent::Approval(record) => json!({
                "type": "approval",
                "platform": record.key.platform,
                "conversation": record.key.conversation.0,
                "thread": record.key.thread.0,
                "toolCallId": record.tool_call_id,
                "tool": record.tool,
                "approval": record.approval.as_ref().map(|id| id.0.as_str()),
                "user": record.user.as_ref().map(|user| user.0.as_str()),
                "result": record.result,
                "endedMs": record.ended_ms,
            }),
            ServerEvent::Released { key } => json!({
                "type": "released",
                "platform": key.platform,
                "conversation": key.conversation.0,
                "thread": key.thread.0,
                "timeMs": now_ms(),
            }),
            ServerEvent::Failed { key, error } => {
                let mut line = json!({"type": "failed", "error": error, "timeMs": now_ms()});
                if let (Some(key), Value::Object(fields)) = (key, &mut line) {
                    fields.insert("platform".into(), json!(key.platform));
                    fields.insert("conversation".into(), json!(key.conversation.0));
                    fields.insert("thread".into(), json!(key.thread.0));
                }
                line
            }
        };
        write_line(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platforms() -> HashSet<&'static str> {
        HashSet::from(["slack"])
    }

    #[test]
    fn entries_split_at_the_first_colon() {
        let parsed = entries(&["slack:C1".into(), "slack:a:b".into()], &platforms(), "f").unwrap();
        assert_eq!(parsed, [("slack".into(), "C1".into()), ("slack".into(), "a:b".into())]);
    }

    #[test]
    fn entries_without_a_platform_or_an_id_are_refused() {
        for entry in ["C1", ":C1", "slack:"] {
            let error = entries(&[entry.into()], &platforms(), "access.conversations").unwrap_err();
            assert!(error.to_string().contains("platform:id"), "{entry}: {error}");
        }
    }

    #[test]
    fn entries_for_an_unconfigured_platform_are_refused() {
        let error = entries(&["teams:C1".into()], &platforms(), "access.approvers").unwrap_err();
        assert!(error.to_string().contains("not configured"), "{error}");
    }

    #[test]
    fn a_feishu_section_defaults_its_variables() {
        let file: ServeFile =
            toml::from_str("[feishu]\n[access]\nconversations = []\n[sessions]\ndir = \"d\"\n").unwrap();
        let feishu = file.feishu.unwrap();
        assert_eq!(
            (feishu.app_id_env.as_str(), feishu.app_secret_env.as_str()),
            (DEFAULT_APP_ID_ENV, DEFAULT_APP_SECRET_ENV)
        );
    }

    #[test]
    fn a_configuration_needs_access_and_a_session_directory() {
        assert!(toml::from_str::<ServeFile>("[slack]\n").is_err());
        let file: ServeFile =
            toml::from_str("[slack]\n[access]\nconversations = []\n[sessions]\ndir = \"/data/sessions\"\n").unwrap();
        let slack = file.slack.unwrap();
        assert_eq!(
            (slack.app_token_env.as_str(), slack.bot_token_env.as_str()),
            (DEFAULT_APP_TOKEN_ENV, DEFAULT_BOT_TOKEN_ENV)
        );
    }

    #[test]
    fn unknown_keys_and_a_zero_run_limit_are_refused() {
        let base = "[access]\nconversations = []\n[sessions]\ndir = \"d\"\n";
        assert!(toml::from_str::<ServeFile>(&format!("{base}[teams]\n")).is_err());
        assert!(toml::from_str::<ServeFile>(&format!("{base}[limits]\nmaxRunning = 0\n")).is_err());
    }
}
