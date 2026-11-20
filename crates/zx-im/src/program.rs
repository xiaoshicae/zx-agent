//! The program `zx-im`: reads im.toml, connects the platforms it configures, and answers
//! their messages, one session per conversation, until a signal or a platform's failure ends
//! it. The server itself is [`Server`]; the agent comes from `zx-app`, as the command line's
//! do. [`Program`] is the program with what a host adds: platforms besides the built-in ones,
//! and its own parts of the application.
//!
//! - `args`: the command line
//! - `config_file`: im.toml
//! - `log`: the operator log on stdout

mod args;
mod config_file;
mod log;

use std::collections::HashSet;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use zx_agent::builtin::files::create_private_dir;
use zx_app::config::Builtin;
use zx_app::otel::{Telemetry, TraceHook};
use zx_app::shutdown::{self, spawn_signal_handler};
use zx_app::{Application, Approvals, ConfiguredAgent, NoModel, Overrides, SessionPlace};
use zx_util::registry::Registry;
use zx_util::stdio::eprint_line;

pub(crate) use self::args::Args;
use self::config_file::{ConfigFile, access, read_file, server_config};
use self::log::{Log, failure_log};
use crate::backlog::BacklogFiles;
use crate::platform::{self, BuiltChannel, PlatformFactory, PlatformInput};
use crate::server::{CONVERSATION, CONVERSATION_SECTION, StatusFiles};
use crate::{ImChannel, ImError, Server, ServerConfig};

/// Exit codes, so that a supervisor can react without parsing output: serving ended as
/// asked, a platform failed while serving, or the configuration or the credentials were
/// refused.
const EXIT_COMPLETED: u8 = 0;
const EXIT_ERROR: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;
/// The traces' `service.name` when the environment names no service: the program's name.
const SERVICE_NAME: &str = "zx-im";
/// Under the session directory: the statuses of runs in progress (`StatusFiles`), and the
/// messages received and not handled yet (`BacklogFiles`). The leading dot keeps them apart
/// from the directories of the sessions, whose names start with `--`.
const RUNNING_DIR: &str = ".running";
const BACKLOG_DIR: &str = ".backlog";

/// What a host gives the application of the program ([`Program::customize`]).
type Customization = Box<dyn FnOnce(Application) -> Application + Send>;

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
                eprint_line(&format!("zx-im: {error:#}"));
                ExitCode::from(EXIT_USAGE)
            }
        }
    }
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
    let BuiltChannels { channels, hidden_env } = build_channels(&file, &registered, &path)?;
    let names: HashSet<&str> = channels.iter().map(|channel| channel.platform()).collect();
    let (conversations, approvers) = access(&file.access, &names)?;
    let (code, end) =
        shutdown::with_telemetry(SERVICE_NAME, async move |telemetry: Option<&Telemetry>| -> Result<u8> {
            if telemetry.is_some() {
                eprint_line(shutdown::EXPORT_NOTICE);
            }
            let dir = file.sessions.dir;
            let (statuses, backlog) = open_stores(&dir)?;
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
                statuses: Some(Arc::new(statuses)),
                backlog: Arc::new(backlog),
                ..server_config(&file.limits, file.replies, conversations, approvers)
            };
            Ok(serve(config, &agent, channels, &dir).await)
        })
        .await?;
    for notice in end.notices() {
        eprint_line(&notice);
    }
    code
}

/// What the process shares, read and connected once, with its startup notices written to
/// stderr. Without a model, the error names the program's ways to give one. zx-agent's
/// subcommands connect the same way: zx-app neither prints nor names the programs' options,
/// and the two programs share no code.
async fn connect(overrides: Overrides) -> Result<Application> {
    let app = Application::from_environment(overrides, Builtin::Assistant)
        .await
        .map_err(with_model_hint)?;
    for notice in app.notices() {
        eprint_line(notice);
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
/// approvals in the conversation: the server resolves each request with an approver's click
/// on the card it posts.
fn build_agent(app: &Application, place: &SessionPlace) -> Result<ConfiguredAgent> {
    let approvals = Approvals::Person("shell commands require approval in the chat");
    app.build(app.model(), place, approvals)
}

/// Serve `channels` as `config` says, with the sessions of `agent`, kept under `dir`, until a
/// signal or a platform's failure; the exit code says which.
async fn serve(config: ServerConfig, agent: &ConfiguredAgent, channels: Vec<Arc<dyn ImChannel>>, dir: &Path) -> u8 {
    let names: Vec<&str> = channels.iter().map(|channel| channel.platform()).collect();
    eprint_line(&format!(
        "[im] platforms={}; sessions in {}",
        names.join(","),
        dir.display()
    ));
    let cancel = CancellationToken::new();
    // Without the listeners serving goes on, each run still bounded by its own limits.
    if let Err(error) = spawn_signal_handler(cancel.clone()) {
        eprint_line(&format!("zx-im: cannot listen for shutdown signals: {error}"));
    }
    // The session file is the record of each conversation; the operator log holds approvals
    // and failures (Log), and traces, when exported, the timeline.
    let served = Server::new(config, agent.agent().clone(), Arc::new(Log))
        .run(channels, &cancel)
        .await;
    serve_exit_code(served)
}

/// The channels of the configured IM platforms, and the environment variables that hold
/// their secrets, which commands must not see.
#[derive(Default)]
struct BuiltChannels {
    channels: Vec<Arc<dyn ImChannel>>,
    hidden_env: Vec<String>,
}

impl BuiltChannels {
    /// Add a platform's channel and the variables holding its secrets.
    fn add(&mut self, built: BuiltChannel) {
        self.channels.push(built.channel);
        self.hidden_env.extend(built.hidden_env);
    }
}

/// The channels of the platforms `file` configures, each built by the factory `registered`
/// under its section's name. An error when it configures none, when no factory or two are
/// registered under a name, and when a channel goes by another name than its section's.
fn build_channels(file: &ConfigFile, registered: &Registry<dyn PlatformFactory>, path: &Path) -> Result<BuiltChannels> {
    registered.check_names().map_err(anyhow::Error::msg)?;
    let mut channels = BuiltChannels::default();
    for (name, section) in &file.platforms {
        let factory = registered
            .get(name)
            .map_err(|error| anyhow::anyhow!("[{name}]: {error}"))?;
        let built = factory
            .build(PlatformInput {
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
        channels.add(built);
    }
    ensure!(
        !channels.channels.is_empty(),
        "{}: no IM platform is configured; add the section of one, such as [slack]",
        path.display()
    );
    Ok(channels)
}

/// The stores under the session directory `dir`: the statuses of runs in progress, kept so
/// that a crash or a kill cannot leave threads saying the work goes on, and the backlog. The
/// directories are created readable only by this user, as the session files are. A startup
/// write; nothing runs concurrently yet.
fn open_stores(dir: &Path) -> Result<(StatusFiles, BacklogFiles)> {
    create_private_dir(dir).with_context(|| format!("cannot create the session directory {}", dir.display()))?;
    let running = dir.join(RUNNING_DIR);
    let statuses = StatusFiles::open(&running).with_context(|| format!("cannot create {}", running.display()))?;
    let backlog_dir = dir.join(BACKLOG_DIR);
    let backlog = BacklogFiles::open(&backlog_dir)
        .with_context(|| format!("cannot create the backlog {}", backlog_dir.display()))?;
    Ok((statuses, backlog))
}

/// How serving ended, as an exit code; a failure is reported on stderr.
fn serve_exit_code(served: Result<(), ImError>) -> u8 {
    let Err(error) = served else {
        return EXIT_COMPLETED;
    };
    eprint_line(&format!("zx-im: {error}"));
    match error {
        ImError::Config(_) => EXIT_USAGE,
        ImError::Transport(_) => EXIT_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use self::config_file::SERVER;
    use super::*;
    use crate::{Destination, Inlet, MessageRef, Post, Replacement};

    #[cfg(unix)]
    #[test]
    fn only_the_owner_may_list_the_session_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        open_stores(&sessions).unwrap();
        for path in [&sessions, &sessions.join(RUNNING_DIR), &sessions.join(BACKLOG_DIR)] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", path.display());
        }
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
        fn build(&self, input: PlatformInput<'_>) -> anyhow::Result<BuiltChannel> {
            let name = input.section.get("goesBy").and_then(toml::Value::as_str);
            Ok(BuiltChannel {
                channel: Arc::new(Named(name.unwrap_or("teams").into())),
                hidden_env: vec!["TEAMS_TOKEN".into()],
            })
        }
    }

    /// The built-in platforms and `teams`.
    fn with_teams() -> Registry<dyn PlatformFactory> {
        Program::new().register_platform("teams", Arc::new(Teams)).platforms
    }

    /// The error of the platforms of the file `text`, with `registry`.
    fn platform_error(text: &str, registry: &Registry<dyn PlatformFactory>) -> String {
        let file = ConfigFile::parse(text).unwrap();
        format!(
            "{:#}",
            build_channels(&file, registry, Path::new("im.toml")).err().unwrap()
        )
    }

    #[test]
    fn a_section_configures_the_platform_registered_under_its_name() {
        let file = ConfigFile::parse(&format!("[teams]\n{SERVER}")).unwrap();
        let built = build_channels(&file, &with_teams(), Path::new("im.toml")).unwrap();
        let names: Vec<&str> = built.channels.iter().map(|channel| channel.platform()).collect();
        assert_eq!(names, ["teams"]);
        assert_eq!(built.hidden_env, ["TEAMS_TOKEN"]);
    }

    #[test]
    fn a_section_of_no_platform_is_refused() {
        assert_eq!(
            platform_error(&format!("[irc]\n{SERVER}"), &with_teams()),
            "[irc]: no IM platform is registered under the name \"irc\"; use one of slack, feishu, teams"
        );
    }

    #[test]
    fn a_channel_named_otherwise_than_its_section_is_refused() {
        let error = platform_error(&format!("[teams]\ngoesBy = \"msteams\"\n{SERVER}"), &with_teams());
        assert!(
            error.starts_with("[teams]: its channel goes by the platform \"msteams\""),
            "{error}"
        );
    }

    #[test]
    fn a_name_registered_twice_is_refused() {
        let twice = Program::new().register_platform("slack", Arc::new(Teams)).platforms;
        assert_eq!(
            platform_error(&format!("[slack]\n{SERVER}"), &twice),
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
