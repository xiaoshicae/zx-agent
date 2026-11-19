//! The program `zx-im`: reads im.toml, connects the platforms it configures, and answers
//! their messages, one session per conversation, until a signal or a platform's failure ends
//! it. The server itself is [`Server`]; the agent comes from `zx-app`, as the command line's
//! do. [`Program`] is the program with what a host adds: platforms besides the built-in ones,
//! and its own parts of the application.
//!
//! - `args`: the command line

mod args;

use std::collections::HashSet;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use zx_agent::ai::now_ms;
use zx_app::config::{Builtin, read_text};
use zx_app::otel::{Telemetry, TraceHook};
use zx_app::shutdown::{self, spawn_signal_handler};
use zx_app::{Application, Approvals, ConfiguredAgent, NoModel, Overrides, SessionPlace};
use zx_util::registry::Registry;

use crate::backlog::BacklogFiles;
use crate::platform::{self, BuiltChannel, PlatformFactory, PlatformInput};
use crate::server::{CONVERSATION, CONVERSATION_SECTION, StatusFiles};
use crate::{
    CardLabels, ConversationId, ErrorReport, ImChannel, ImError, Replies, Server, ServerConfig, ServerEvent,
    ServerSink, SessionKey, UserId,
};

pub(crate) use args::Args;

/// Exit codes, so that a supervisor can react without parsing output: serving ended as
/// asked, a platform failed while serving, or the configuration or the credentials were
/// refused.
const EXIT_COMPLETED: u8 = 0;
const EXIT_ERROR: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;
/// The traces' `service.name` when the environment names no service: the program's name.
const SERVICE_NAME: &str = "zx-im";
/// Under the session directory: the statuses of runs in progress (`StatusFiles`), and the
/// messages received and not handled yet (`BacklogFiles`). The dot keeps them apart from the
/// platform directories, whose encoded names never contain one.
const RUNNING_DIR: &str = ".running";
const BACKLOG_DIR: &str = ".backlog";

type Conversations = Vec<(String, ConversationId)>;
type Approvers = Vec<(String, UserId)>;
/// What a host gives the application of the program ([`Program::customize`]).
type Customization = Box<dyn FnOnce(Application) -> Application + Send>;

/// The sections of im.toml that are the server's; each of the others configures the platform
/// registered under its name.
const SERVER_SECTIONS: [&str; 5] = ["access", "limits", "sessions", "replies", "cardLabels"];

/// The program `zx-im` with what a host adds: platforms besides Slack and Feishu, and its own
/// parts of the application the agent is built with. A program of the host's own runs it from
/// its `main`, as `zx-im` does through [`main`](crate::main).
pub struct Program {
    platforms: Registry<dyn PlatformFactory>,
    customizations: Vec<Customization>,
}

impl Default for Program {
    fn default() -> Self {
        Self::new()
    }
}

impl Program {
    /// The program with the built-in platforms: Slack, and Feishu where the build has it.
    pub fn new() -> Self {
        Self {
            platforms: platform::built_in(),
            customizations: Vec::new(),
        }
    }

    /// Register `factory` under `name`, the im.toml section that configures it; its channel
    /// goes by that name. A name that another platform goes by already fails the start.
    pub fn register_platform(mut self, name: &str, factory: Arc<dyn PlatformFactory>) -> Self {
        self.platforms.register(name, factory);
        self
    }

    /// Let `customize` give the application that every conversation's agent is built with
    /// the host's own parts, with the verbs of [`Application`]: tools, hooks, policies,
    /// extensions and prompt sections, the variables to hide from commands, and the providers
    /// and executors config.toml chooses by name. The program's own parts come after them: the
    /// platforms' credentials, hidden beside the host's variables, the trace export, which is
    /// the program's, and the prompt section on how answers reach people. Each call adds one
    /// more, applied in order.
    pub fn customize(mut self, customize: impl FnOnce(Application) -> Application + Send + 'static) -> Self {
        self.customizations.push(Box::new(customize));
        self
    }

    /// Run with the command line the process was started with, and return its exit code. A
    /// configuration problem found before serving started is written to stderr and exits
    /// with the code of a usage error.
    pub async fn run(self) -> ExitCode {
        match run(Args::parse(), self).await {
            Ok(code) => ExitCode::from(code),
            Err(error) => {
                eprintln!("zx-im: {error:#}");
                ExitCode::from(EXIT_USAGE)
            }
        }
    }
}

/// The IM service configuration file: the server's sections, and a section for each platform.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ConfigFile {
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
    /// The section of each platform, by its name: what is left of the file but the server's.
    #[serde(skip)]
    platforms: Vec<(String, toml::Table)>,
}

impl ConfigFile {
    /// The file `text` holds: its server's sections, checked, and the others, which only the
    /// platforms they name can check.
    fn parse(text: &str) -> Result<Self> {
        let mut server = toml::Table::new();
        let mut platforms = Vec::new();
        for (key, value) in toml::from_str::<toml::Table>(text)? {
            if SERVER_SECTIONS.contains(&key.as_str()) {
                server.insert(key, value);
                continue;
            }
            let toml::Value::Table(section) = value else {
                bail!("unknown key `{key}`: a platform is configured by a section, such as [slack]");
            };
            platforms.push((key, section));
        }
        let mut file: Self = toml::Value::Table(server).try_into()?;
        file.platforms = platforms;
        Ok(file)
    }
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
    max_conversations: Option<NonZeroUsize>,
    max_pending_messages: Option<NonZeroUsize>,
    max_pending_bytes: Option<NonZeroUsize>,
    idle_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionsSection {
    dir: PathBuf,
}

/// Serve until SIGINT or SIGTERM, or until a platform fails, and return the exit code. An
/// `Err` is a configuration problem found before serving started.
///
/// Traces are exported when the environment names an OTLP endpoint ([`shutdown`]); what could
/// not be exported is said on stderr once serving ended, and never changes the exit code.
async fn run(args: Args, program: Program) -> Result<u8> {
    let Program {
        platforms: registered,
        customizations,
    } = program;
    let (path, overrides) = args.into_parts();
    let file = read_file(&path)?;
    let Platforms { channels, hidden_env } = platforms(&file, &registered, &path)?;
    let platforms: HashSet<&str> = channels.iter().map(|channel| channel.platform()).collect();
    let (conversations, approvers) = access(&file.access, &platforms)?;
    let (code, end) =
        shutdown::with_telemetry(SERVICE_NAME, async move |telemetry: Option<&Telemetry>| -> Result<u8> {
            if telemetry.is_some() {
                eprintln!("[otel] exporting traces over OTLP/HTTP");
            }
            let dir = file.sessions.dir;
            let running = create_session_dirs(&dir)?;
            let backlog_dir = dir.join(BACKLOG_DIR);
            let backlog = BacklogFiles::open(&backlog_dir)
                .with_context(|| format!("cannot create the backlog {}", backlog_dir.display()))?;
            // Once for the process: every conversation's session shares the connections.
            let app = assemble(
                connect(overrides).await?,
                customizations,
                &hidden_env,
                telemetry.map(Telemetry::hook),
            );
            let agent = conversations_agent(&app, &dir).await?;
            let config = ServerConfig {
                cwd: agent.new_session_cwd(None)?,
                backlog: Arc::new(backlog),
                ..server_config(&file.limits, file.replies, running, conversations, approvers)
            };
            Ok(serve(config, &agent, channels, &dir).await)
        })
        .await?;
    if end.dropped > 0 {
        eprintln!("[otel] {} event(s) were not exported: the queue was full", end.dropped);
    }
    if let Some(error) = end.error {
        eprintln!("[otel] {error:#}");
    }
    code
}

/// What the process shares, read and connected once, with its startup notices written to
/// stderr. Without a model, the error names the program's ways to give one.
async fn connect(overrides: Overrides) -> Result<Application> {
    let app = Application::from_environment(overrides, Builtin::Assistant)
        .await
        .map_err(with_model_hint)?;
    for notice in app.notices() {
        eprintln!("{notice}");
    }
    Ok(app)
}

/// `app` with the host's parts, given by `customizations` in order, then the program's own: the
/// platforms' `credentials`, hidden beside the variables the host hides, the `traces` export,
/// and the prompt section on how answers reach people.
fn assemble(
    app: Application,
    customizations: Vec<Customization>,
    credentials: &[String],
    traces: Option<TraceHook>,
) -> Application {
    let app = customizations.into_iter().fold(app, |app, customize| customize(app));
    credentials
        .iter()
        .fold(app, |app, name| app.add_hidden_env(name))
        .set_traces(traces)
        .add_prompt_section(CONVERSATION_SECTION, CONVERSATION)
}

/// `error`, or, when no model was given, the ways to give one: `--model`, over `ZX_MODEL`,
/// which the application reads, over config.toml.
fn with_model_hint(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<NoModel>() {
        Some(no_model) => anyhow::anyhow!(
            "no model: pass --model, set ZX_MODEL, or set model in {}",
            no_model.config_file
        ),
        None => error,
    }
}

/// The agent of every conversation, whose sessions are kept under `dir`, once a session with
/// these settings could be prepared: a wrong model, agent or tool setting would otherwise
/// surface only at the first message, in the chat. That session is never stored.
async fn conversations_agent(app: &Application, dir: &Path) -> Result<ConfiguredAgent> {
    build_agent(app, &SessionPlace::memory())?
        .create(None)
        .await
        .context("cannot prepare a session with these settings")?;
    // A conversation's sessions are those under `dir` that carry its labels.
    build_agent(app, &SessionPlace::directory(dir))
}

/// The agent of every conversation, whose sessions are kept in `place`, as chat's are, with
/// approvals in the conversation: the server answers each request with an approver's click
/// on the card it posts.
fn build_agent(app: &Application, place: &SessionPlace) -> Result<ConfiguredAgent> {
    let approvals = Approvals::Person("shell commands require approval in the chat");
    app.build(app.model(), place, approvals)
}

/// Serve `channels` as `config` says, with the sessions of `agent`, kept under `dir`, until a
/// signal or a platform's failure; the exit code says which.
async fn serve(config: ServerConfig, agent: &ConfiguredAgent, channels: Vec<Arc<dyn ImChannel>>, dir: &Path) -> u8 {
    let names: Vec<&str> = channels.iter().map(|channel| channel.platform()).collect();
    eprintln!("[im] platforms={}; sessions in {}", names.join(","), dir.display());
    let cancel = CancellationToken::new();
    // Without the listeners serving goes on, each run still bounded by its own limits.
    if let Err(error) = spawn_signal_handler(cancel.clone()) {
        eprintln!("zx-im: cannot listen for shutdown signals: {error}");
    }
    // The session file is the record of each conversation; the operator log holds approvals
    // and failures (Log), and traces, when exported, the timeline.
    let served = Server::new(config, agent.agent().clone(), Arc::new(Log))
        .run(channels, &cancel)
        .await;
    serve_exit_code(served)
}

/// The configured IM platforms: a channel for each, and the environment variables that hold
/// their secrets, which commands must not see.
#[derive(Default)]
struct Platforms {
    channels: Vec<Arc<dyn ImChannel>>,
    hidden_env: Vec<String>,
}

impl Platforms {
    /// Add a platform's channel and the variables holding its secrets.
    fn add(&mut self, built: BuiltChannel) {
        self.channels.push(built.channel);
        self.hidden_env.extend(built.hidden_env);
    }
}

/// The platforms `file` configures, each built by the factory `registered` under its
/// section's name. An error when it configures none, when no factory or two are registered
/// under a name, and when a channel goes by another name than its section's.
fn platforms(file: &ConfigFile, registered: &Registry<dyn PlatformFactory>, path: &Path) -> Result<Platforms> {
    registered.check_names().map_err(anyhow::Error::msg)?;
    let mut platforms = Platforms::default();
    for (name, section) in &file.platforms {
        let factory = registered
            .get(name)
            .map_err(|error| anyhow::anyhow!("[{name}]: {error}"))?;
        let built = factory
            .channel(PlatformInput {
                section,
                labels: &file.card_labels,
                on_error: failure_log(name),
            })
            .with_context(|| format!("[{name}]"))?;
        let platform = built.channel.platform();
        ensure!(
            platform == name,
            "[{name}]: its channel goes by the platform {platform:?}; a channel goes by the name its platform is registered under"
        );
        platforms.add(built);
    }
    ensure!(
        !platforms.channels.is_empty(),
        "{}: no IM platform is configured; add the section of one, such as [slack]",
        path.display()
    );
    Ok(platforms)
}

/// Create the session directory `dir` and the directory of the runs in progress under it.
/// Returns the latter. A startup write; nothing runs concurrently yet.
fn create_session_dirs(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create the session directory {}", dir.display()))?;
    let running = dir.join(RUNNING_DIR);
    std::fs::create_dir_all(&running).with_context(|| format!("cannot create {}", running.display()))?;
    Ok(running)
}

/// The IM service configuration file at `path`, read as config.toml is: a regular UTF-8 file
/// within a size limit. A startup read; nothing runs concurrently yet.
fn read_file(path: &Path) -> Result<ConfigFile> {
    let text = read_text(path)?;
    ConfigFile::parse(&text).with_context(|| format!("invalid IM service configuration {}", path.display()))
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
    if let Some(limit) = limits.max_conversations {
        config.max_conversations = limit;
    }
    if let Some(limit) = limits.max_pending_messages {
        config.max_pending_messages = limit;
    }
    if let Some(limit) = limits.max_pending_bytes {
        config.max_pending_bytes = limit;
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
            eprintln!("zx-im: {error:#}");
            EXIT_USAGE
        }
        Err(error @ ImError::Transport(_)) => {
            eprintln!("zx-im: {error:#}");
            EXIT_ERROR
        }
    }
}

/// Reports what the channel of `platform` goes on after as `failed` lines, like the server's
/// own.
fn failure_log(platform: &str) -> ErrorReport {
    let platform = platform.to_string();
    Arc::new(move |error: &ImError| {
        write_line(&json!({"type": "failed", "platform": platform, "error": error.to_string(), "timeMs": now_ms()}));
    })
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

/// The thread of `key` in a log line: `null` for a direct chat.
fn thread(key: &SessionKey) -> Value {
    json!(key.thread.as_ref().map(|thread| thread.0.as_str()))
}

/// Print `line` and a newline on stdout, unlike `println!` without panicking when stdout is
/// closed.
fn write_line(line: &Value) {
    let mut out = std::io::stdout().lock();
    // A closed stdout (for example a log collector that exited) leaves nobody to report to;
    // serving goes on.
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
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
    use async_trait::async_trait;

    use super::*;
    use crate::{Destination, Inlet, MessageRef, Post, Replacement};

    /// The server's sections of a file that configures no platform.
    const SERVER: &str = "[access]\nconversations = []\n[sessions]\ndir = \"d\"\n";

    fn slack_only() -> HashSet<&'static str> {
        HashSet::from(["slack"])
    }

    /// A channel that only goes by its platform's name.
    struct Named(String);

    #[async_trait]
    impl ImChannel for Named {
        fn platform(&self) -> &str {
            &self.0
        }

        async fn run(&self, _inlet: Inlet, _cancel: &CancellationToken) -> Result<(), ImError> {
            Ok(())
        }

        async fn send(&self, _to: &Destination, _message: &Post) -> Result<MessageRef, ImError> {
            Err(ImError::Transport("nothing is sent".into()))
        }

        async fn update(&self, _posted: &MessageRef, _replacement: &Replacement) -> Result<(), ImError> {
            Ok(())
        }
    }

    /// A platform a host registers: its channel goes by `teams`, or by the `goesBy` of its
    /// section, and the variable `TEAMS_TOKEN` holds its credential.
    struct Teams;

    impl PlatformFactory for Teams {
        fn channel(&self, input: PlatformInput<'_>) -> anyhow::Result<BuiltChannel> {
            let name = input.section.get("goesBy").and_then(toml::Value::as_str);
            Ok(BuiltChannel {
                channel: Arc::new(Named(name.unwrap_or("teams").into())),
                hidden_env: vec!["TEAMS_TOKEN".into()],
            })
        }
    }

    /// The built-in platforms and `teams`.
    fn with_teams() -> Registry<dyn PlatformFactory> {
        let mut registry = platform::built_in();
        registry.register("teams", Arc::new(Teams));
        registry
    }

    /// The error of the platforms of the file `text`, with `registry`.
    fn platform_error(text: &str, registry: &Registry<dyn PlatformFactory>) -> String {
        let file = ConfigFile::parse(text).unwrap();
        format!("{:#}", platforms(&file, registry, Path::new("im.toml")).err().unwrap())
    }

    #[test]
    fn the_file_is_read_within_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("im.toml");
        let file = "[slack]\n[access]\nconversations = []\n[sessions]\ndir = \"/data/sessions\"\n";
        std::fs::write(&path, format!("{file}# {}", "x".repeat(64 * 1024))).unwrap();
        assert!(format!("{:#}", read_file(&path).err().unwrap()).contains("exceeds"));
    }

    #[test]
    fn the_deployment_example_is_a_valid_configuration() {
        // The im.toml that the Kubernetes deployment starts from, read as --config is read.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/kubernetes/im/im.example.toml");
        if let Err(error) = read_file(&path) {
            panic!("{error:#}");
        }
    }

    #[test]
    fn entries_split_at_the_first_colon() {
        let parsed = entries(&["slack:C1".into(), "slack:a:b".into()], &slack_only(), "f").unwrap();
        assert_eq!(parsed, [("slack".into(), "C1".into()), ("slack".into(), "a:b".into())]);
    }

    #[test]
    fn entries_without_a_platform_or_an_id_are_refused() {
        for entry in ["C1", ":C1", "slack:"] {
            let error = entries(&[entry.into()], &slack_only(), "access.conversations").unwrap_err();
            assert!(error.to_string().contains("platform:id"), "{entry}: {error}");
        }
    }

    #[test]
    fn entries_for_an_unconfigured_platform_are_refused() {
        let error = entries(&["teams:C1".into()], &slack_only(), "access.approvers").unwrap_err();
        assert!(error.to_string().contains("not configured"), "{error}");
    }

    #[test]
    fn request_limit_reply_can_be_configured() {
        let config = format!("{SERVER}[replies]\nmodelRequestLimit = \"limit\"\n");
        let file = ConfigFile::parse(&config).unwrap();
        assert_eq!(file.replies.model_request_limit, "limit");
        assert!(ConfigFile::parse(&config.replace("modelRequestLimit", "maxTurns")).is_err());
    }

    #[test]
    fn a_configuration_needs_access_and_a_session_directory() {
        assert!(ConfigFile::parse("[slack]\n").is_err());
        let file =
            ConfigFile::parse("[slack]\n[access]\nconversations = []\n[sessions]\ndir = \"/data/sessions\"\n").unwrap();
        assert_eq!(file.platforms, [("slack".to_string(), toml::Table::new())]);
        assert_eq!(file.sessions.dir, Path::new("/data/sessions"));
    }

    #[test]
    fn unknown_keys_and_a_zero_run_limit_are_refused() {
        // A key before the first section: after one, it would be that section's.
        let error = format!("{:#}", ConfigFile::parse(&format!("typo = 1\n{SERVER}")).err().unwrap());
        assert!(error.contains("unknown key `typo`"), "{error}");
        for name in [
            "maxRunning",
            "maxConversations",
            "maxPendingMessages",
            "maxPendingBytes",
        ] {
            assert!(ConfigFile::parse(&format!("{SERVER}[limits]\n{name} = 0\n")).is_err());
        }
        // The approval timeout is config.toml's, shared with chat and acp.
        assert!(ConfigFile::parse(&format!("{SERVER}[limits]\napprovalTimeoutSecs = 60\n")).is_err());
    }

    #[test]
    fn a_section_configures_the_platform_registered_under_its_name() {
        let file = ConfigFile::parse(&format!("[teams]\n{SERVER}")).unwrap();
        let built = platforms(&file, &with_teams(), Path::new("im.toml")).unwrap();
        let names: Vec<&str> = built.channels.iter().map(|channel| channel.platform()).collect();
        assert_eq!(names, ["teams"]);
        assert_eq!(built.hidden_env, ["TEAMS_TOKEN"]);
    }

    #[test]
    fn a_section_of_no_platform_and_a_channel_named_otherwise_are_refused() {
        assert_eq!(
            platform_error(&format!("[irc]\n{SERVER}"), &with_teams()),
            "[irc]: no IM platform is registered under the name \"irc\"; use one of slack, feishu, teams"
        );
        let error = platform_error(&format!("[teams]\ngoesBy = \"msteams\"\n{SERVER}"), &with_teams());
        assert!(
            error.starts_with("[teams]: its channel goes by the platform \"msteams\""),
            "{error}"
        );
        let mut twice = with_teams();
        twice.register("slack", Arc::new(Teams));
        assert_eq!(
            platform_error(&format!("[teams]\n{SERVER}"), &twice),
            "more than one IM platform is registered under the name \"slack\""
        );
    }

    #[cfg(not(feature = "feishu"))]
    #[test]
    fn a_build_without_feishu_refuses_its_section_by_name() {
        let error = platform_error(&format!("[feishu]\n{SERVER}"), &platform::built_in());
        assert!(error.contains("this build of zx-im has no such platform"), "{error}");
        assert!(error.contains("[feishu]"), "{error}");
    }

    #[test]
    fn a_missing_model_names_the_programs_ways_to_give_one() {
        let no_model = NoModel {
            config_file: "/home/me/.zx-agent/config.toml".into(),
        };
        assert_eq!(
            format!("{:#}", with_model_hint(no_model.into())),
            "no model: pass --model, set ZX_MODEL, or set model in /home/me/.zx-agent/config.toml"
        );
        let other = with_model_hint(anyhow::anyhow!("something else"));
        assert_eq!(format!("{other:#}"), "something else");
    }
}
