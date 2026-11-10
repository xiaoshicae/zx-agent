//! `zx-agent im`: answers IM messages, one session per conversation, until stopped.
//! The server itself is `zx_agent::im::Server`.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use zx_agent::AgentSession;
use zx_agent::agent::Interaction;
use zx_agent::agent::NullSink;
use zx_agent::ai::now_ms;
use zx_agent::config::Builtin;
use zx_agent::im::feishu::{FeishuChannel, FeishuConfig};
use zx_agent::im::slack::{SlackChannel, SlackConfig};
use zx_agent::im::{
    CardLabels, ConversationId, ErrorReport, ImChannel, ImError, Opening, Replies, Server, ServerConfig, ServerEvent,
    ServerSink, SessionFactory, SessionKey, StatusFiles, UserId,
};
use zx_agent::otel::Telemetry;

use crate::cli::{self, ImArgs, env_value};
use crate::process::{EXIT_COMPLETED, EXIT_ERROR, EXIT_USAGE, spawn_signal_handler, with_telemetry, write_line};
use crate::session_files::SessionFile;

const DEFAULT_APP_TOKEN_ENV: &str = "SLACK_APP_TOKEN";
const DEFAULT_BOT_TOKEN_ENV: &str = "SLACK_BOT_TOKEN";
const DEFAULT_APP_ID_ENV: &str = "FEISHU_APP_ID";
const DEFAULT_APP_SECRET_ENV: &str = "FEISHU_APP_SECRET";
/// Under the session directory: the statuses of runs in progress (`StatusFiles`). The dot
/// keeps it apart from the platform directories, whose encoded names never contain one.
const RUNNING_DIR: &str = ".running";

type Conversations = Vec<(String, ConversationId)>;
type Approvers = Vec<(String, UserId)>;

/// The IM service configuration file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ConfigFile {
    slack: Option<SlackSection>,
    feishu: Option<FeishuSection>,
    access: AccessSection,
    #[serde(default)]
    limits: LimitsSection,
    sessions: SessionsSection,
    /// Texts the server posts besides answers; each field is optional, the defaults English.
    #[serde(default)]
    replies: Replies,
    /// Texts of approval cards on every platform; each field optional, the defaults English.
    #[serde(default)]
    card_labels: CardLabels,
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
    idle_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionsSection {
    dir: PathBuf,
}

/// Serve until SIGINT or SIGTERM, or until a platform fails. An `Err` is a configuration
/// problem found before serving started.
pub(crate) async fn run(args: ImArgs) -> Result<u8> {
    let file = read_file(&args.config)?;
    let Platforms { channels, hidden_env } = platforms(&file, &args.config)?;
    let platforms: HashSet<&str> = channels.iter().map(|channel| channel.platform()).collect();
    let (conversations, approvers) = access(&file.access, &platforms)?;
    with_telemetry(async move |telemetry| {
        let dir = file.sessions.dir;
        let running = create_session_dirs(&dir)?;

        // Once for the process: every thread's session shares the connections.
        let shared = cli::Shared::connect(args.base, args.budget, Builtin::Assistant)
            .await?
            .with_hidden_env(hidden_env)
            .with_traces(telemetry.map(Telemetry::sink));
        let factory = Sessions { shared, dir };
        // A wrong model, agent or tool setting would otherwise surface only at the first
        // message, in the chat; check it once with a session that is never stored.
        factory
            .prepare(SessionFile::Memory)
            .await
            .context("cannot prepare a session with these settings")?;

        let config = server_config(&file.limits, file.replies, running, conversations, approvers);
        eprintln!(
            "[im] platforms={}; sessions in {}",
            channels
                .iter()
                .map(|channel| channel.platform())
                .collect::<Vec<_>>()
                .join(","),
            factory.dir.display()
        );
        let cancel = CancellationToken::new();
        spawn_signal_handler(cancel.clone());
        let served = Server::new(config, Arc::new(factory), Arc::new(Log))
            .run(channels, &cancel)
            .await;
        Ok(serve_exit_code(served))
    })
    .await
}

/// The configured IM platforms: a channel for each, and the environment variables that hold
/// their secrets, which commands must not see.
struct Platforms {
    channels: Vec<Arc<dyn ImChannel>>,
    hidden_env: Vec<String>,
}

/// The platforms `file` configures. An error when it configures none.
fn platforms(file: &ConfigFile, path: &Path) -> Result<Platforms> {
    let mut channels: Vec<Arc<dyn ImChannel>> = Vec::new();
    let mut hidden_env = Vec::new();
    if let Some(slack) = &file.slack {
        let (channel, secrets) = slack_channel(slack, &file.card_labels)?;
        channels.push(channel);
        hidden_env.extend(secrets);
    }
    if let Some(feishu) = &file.feishu {
        let (channel, secrets) = feishu_channel(feishu, &file.card_labels)?;
        channels.push(channel);
        hidden_env.extend(secrets);
    }
    anyhow::ensure!(
        !channels.is_empty(),
        "{}: no IM platform is configured; add a [slack] or [feishu] section",
        path.display()
    );
    Ok(Platforms { channels, hidden_env })
}

/// Create the session directory `dir` and the directory of the runs in progress under it.
/// Returns the latter. A startup write; nothing runs concurrently yet.
fn create_session_dirs(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create the session directory {}", dir.display()))?;
    let running = dir.join(RUNNING_DIR);
    std::fs::create_dir_all(&running).with_context(|| format!("cannot create {}", running.display()))?;
    Ok(running)
}

/// The IM service configuration file at `path`. A startup read; nothing runs concurrently yet.
fn read_file(path: &Path) -> Result<ConfigFile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid IM service configuration {}", path.display()))
}

/// The chats the bot answers in and the users who may approve, each on a configured platform.
fn access(section: &AccessSection, platforms: &HashSet<&str>) -> Result<(Conversations, Approvers)> {
    let conversations = entries(&section.conversations, platforms, "access.conversations")?
        .into_iter()
        .map(|(platform, id)| (platform, ConversationId(id)))
        .collect();
    let approvers = entries(&section.approvers, platforms, "access.approvers")?
        .into_iter()
        .map(|(platform, id)| (platform, UserId(id)))
        .collect();
    Ok((conversations, approvers))
}

/// The server's settings: the file's limits over the defaults, with the statuses of runs in
/// progress kept in `running`, so that a crash or a kill cannot leave threads saying the work
/// goes on.
fn server_config(
    limits: &LimitsSection,
    replies: Replies,
    running: PathBuf,
    conversations: Conversations,
    approvers: Approvers,
) -> ServerConfig {
    let mut config = ServerConfig {
        conversations,
        approvers,
        replies,
        statuses: Some(Arc::new(StatusFiles::new(running))),
        ..ServerConfig::default()
    };
    if let Some(running) = limits.max_running {
        config.max_running = running;
    }
    if let Some(secs) = limits.idle_secs {
        config.idle = Duration::from_secs(secs);
    }
    config
}

/// How serving ended, as an exit code; a failure is reported on stderr.
fn serve_exit_code(served: Result<(), ImError>) -> u8 {
    match served {
        Ok(()) => EXIT_COMPLETED,
        Err(error @ ImError::Config(_)) => {
            eprintln!("zx-agent: {error}");
            EXIT_USAGE
        }
        Err(error @ ImError::Transport(_)) => {
            eprintln!("zx-agent: {error}");
            EXIT_ERROR
        }
    }
}

/// The Slack channel of its section, and the variables holding its tokens.
fn slack_channel(section: &SlackSection, labels: &CardLabels) -> Result<(Arc<dyn ImChannel>, Vec<String>)> {
    let mut config = SlackConfig::new(
        secret(&section.app_token_env, "the Slack app-level token")?,
        secret(&section.bot_token_env, "the Slack bot token")?,
    );
    if let Some(base) = &section.api_base {
        config.api_base.clone_from(base);
    }
    config.labels.clone_from(labels);
    config.on_error = Some(failure_log("slack"));
    let channel = SlackChannel::new(config).map_err(anyhow::Error::msg)?;
    Ok((
        Arc::new(channel),
        vec![section.app_token_env.clone(), section.bot_token_env.clone()],
    ))
}

/// The Feishu channel of its section, and the variable holding its secret: the App ID is
/// not one.
fn feishu_channel(section: &FeishuSection, labels: &CardLabels) -> Result<(Arc<dyn ImChannel>, Vec<String>)> {
    let mut config = FeishuConfig::new(
        secret(&section.app_id_env, "the Feishu App ID")?,
        secret(&section.app_secret_env, "the Feishu App Secret")?,
    );
    if let Some(base) = &section.api_base {
        config.api_base.clone_from(base);
    }
    if let Some(url) = &section.token_url {
        config.token_url.clone_from(url);
    }
    config.labels.clone_from(labels);
    config.on_error = Some(failure_log("feishu"));
    let channel = FeishuChannel::new(config).map_err(anyhow::Error::msg)?;
    Ok((Arc::new(channel), vec![section.app_secret_env.clone()]))
}

/// Reports a platform's failed connection attempts as `failed` lines, like the server's own.
fn failure_log(platform: &'static str) -> ErrorReport {
    Arc::new(move |error: &ImError| {
        write_line(&json!({"type": "failed", "platform": platform, "error": error.to_string(), "timeMs": now_ms()}));
    })
}

/// The value of a credential variable, which must be set and non-empty.
fn secret(name: &str, what: &str) -> Result<String> {
    env_value(name).with_context(|| format!("{name} must hold {what}"))
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

/// Opens each conversation's session like chat does, with approvals in the conversation.
struct Sessions {
    /// The command line, MCP servers, secret files, IM credentials and trace export, shared
    /// by every thread's session.
    shared: cli::Shared,
    dir: PathBuf,
}

impl Sessions {
    async fn prepare(&self, file: SessionFile<'_>) -> Result<cli::PreparedSession> {
        let mut prepared = cli::prepare(&self.shared, &self.shared.settings.model, None, file).await?;
        prepared.require_bash_approval("shell commands require approval in the chat");
        prepared.config.interaction_timeout = self.shared.settings.approval_timeout;
        Ok(prepared)
    }

    async fn open_session(
        &self,
        key: &SessionKey,
        opening: Opening,
        interaction: Arc<dyn Interaction>,
    ) -> Result<AgentSession> {
        // A short listing and directory creation before the session file, like prepare's own
        // startup reads.
        let path = key
            .file_in(&self.dir, opening)
            .with_context(|| format!("cannot list {}", key.dir_in(&self.dir).display()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut prepared = self.prepare(SessionFile::Open(&path)).await?;
        prepared.config.interaction = interaction;
        // The session file is the record of each conversation; the operator log holds
        // approvals and failures (Log), and traces, when exported, the timeline.
        prepared.into_session(Arc::new(NullSink))
    }
}

#[async_trait]
impl SessionFactory for Sessions {
    async fn open(
        &self,
        key: &SessionKey,
        opening: Opening,
        interaction: Arc<dyn Interaction>,
    ) -> Result<AgentSession, String> {
        self.open_session(key, opening, interaction)
            .await
            .map_err(|error| format!("{error:#}"))
    }

    /// A listing of the conversation's directory: a few names.
    fn has_session(&self, key: &SessionKey) -> bool {
        key.has_file_in(&self.dir)
    }
}

/// The thread of `key` in a log line: `null` for a direct chat.
fn thread(key: &SessionKey) -> Value {
    json!(key.thread.as_ref().map(|thread| thread.0.as_str()))
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
                "thread": thread(&record.key),
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
                "thread": thread(key),
                "timeMs": now_ms(),
            }),
            ServerEvent::Failed { key, error } => {
                let mut line = json!({"type": "failed", "error": error, "timeMs": now_ms()});
                if let (Some(key), Value::Object(fields)) = (key, &mut line) {
                    fields.insert("platform".into(), json!(key.platform));
                    fields.insert("conversation".into(), json!(key.conversation.0));
                    fields.insert("thread".into(), thread(key));
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
        let file: ConfigFile =
            toml::from_str("[feishu]\n[access]\nconversations = []\n[sessions]\ndir = \"d\"\n").unwrap();
        let feishu = file.feishu.unwrap();
        assert_eq!(
            (feishu.app_id_env.as_str(), feishu.app_secret_env.as_str()),
            (DEFAULT_APP_ID_ENV, DEFAULT_APP_SECRET_ENV)
        );
    }

    #[test]
    fn request_limit_reply_can_be_configured() {
        let config =
            "[access]\nconversations = []\n[sessions]\ndir = \"d\"\n[replies]\nmodelRequestLimit = \"limit\"\n";
        let file: ConfigFile = toml::from_str(config).unwrap();
        assert_eq!(file.replies.model_request_limit, "limit");
        assert!(toml::from_str::<ConfigFile>(&config.replace("modelRequestLimit", "maxTurns")).is_err());
    }

    #[test]
    fn a_configuration_needs_access_and_a_session_directory() {
        assert!(toml::from_str::<ConfigFile>("[slack]\n").is_err());
        let file: ConfigFile =
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
        assert!(toml::from_str::<ConfigFile>(&format!("{base}[teams]\n")).is_err());
        assert!(toml::from_str::<ConfigFile>(&format!("{base}[limits]\nmaxRunning = 0\n")).is_err());
        // The approval timeout is config.toml's, shared with chat and acp.
        assert!(toml::from_str::<ConfigFile>(&format!("{base}[limits]\napprovalTimeoutSecs = 60\n")).is_err());
    }
}
