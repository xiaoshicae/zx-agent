//! The program `zx-im`: reads im.toml, connects the platforms it configures, and answers
//! their messages, one session per conversation, until a signal or a platform's failure ends
//! it. The server itself is [`Server`]; the agent comes from `zx-app`, as the command line's
//! do. [`Program`] is the program with what a host adds: platforms besides the built-in ones,
//! and its own parts of the application.
//!
//! - `args`: the command line
//! - `config_file`: im.toml
//! - `log`: the operator log on stdout
//! - `reload`: on SIGHUP, the lists of im.toml read again (Unix only)
//! - `scheduler`: the schedules of im.toml, each asking the server for its conversation
//! - `send_file`: the tool of each session that posts a file in its conversation
//! - `stores`: where the sessions and the program's stores are kept, a directory or Postgres

mod args;
mod config_file;
mod log;
#[cfg(unix)]
mod reload;
mod scheduler;
mod send_file;
mod stores;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use zx_agent::Agent;
use zx_agent::ai::Proxy;
use zx_app::config::{Builtin, Config, Credentials};
use zx_app::otel::{Telemetry, TraceHook};
use zx_app::shutdown::{self, spawn_signal_handler};
use zx_app::{Application, Approvals, ConfiguredAgent, NoModel, Overrides, SessionPlace};
use zx_util::registry::Registry;
use zx_util::stdio::eprint_line;

pub(crate) use self::args::Args;
use self::config_file::{ConfigFile, OwnChat, ServerSections, read_file, server_config};
use self::log::{Log, failure_log};
use self::send_file::SendFiles;
use self::stores::{LeadLost, Leadership, Stores};
use crate::platform::{self, BuiltChannel, PlatformFactory, PlatformInput};
use crate::server::{CONVERSATION, CONVERSATION_SECTION};
use crate::{ChatConfig, DailyLimit, ImChannel, ImError, Server, ServerConfig, Trigger, Triggers};

/// Exit codes, so that a supervisor can react without parsing output: serving ended as
/// asked, a platform failed while serving, or the configuration or the credentials were
/// refused.
const EXIT_COMPLETED: u8 = 0;
const EXIT_ERROR: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;
/// The traces' `service.name` when the environment names no service: the program's name.
const SERVICE_NAME: &str = "zx-im";
/// The agent of the conversations in every chat without one of its own.
const SERVER_AGENT: Builtin = Builtin::Assistant;

/// What a host gives the application of the program ([`Program::customize`]).
type Customization = Box<dyn FnOnce(Application) -> Application + Send>;

/// A host's own source of conversations without a message ([`Program::on_triggers`]).
type TriggerSource = Box<dyn FnOnce(Triggers) + Send>;

/// What starts conversations without a message: the schedules of im.toml, each trigger with
/// its interval, and the host's sources.
struct Unprompted {
    schedules: Vec<(Duration, Trigger)>,
    sources: Vec<TriggerSource>,
}

/// The program `zx-im` with what a host adds: platforms besides Slack and Feishu, its own
/// parts of the application the agent is built with, and its own sources of conversations
/// without a message. A program of the host's own runs it from its `main`, as `zx-im` does
/// through [`main`](crate::main).
pub struct Program {
    platforms: Registry<dyn PlatformFactory>,
    customizations: Vec<Customization>,
    trigger_sources: Vec<TriggerSource>,
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
            trigger_sources: Vec::new(),
        }
    }

    /// Hand `source` the server's [`Triggers`] as serving starts: a source of the host's own
    /// asks through them for conversations without a message, besides the schedules of
    /// im.toml, such as one for each alert that an endpoint of the host's receives. Each call
    /// adds one more.
    pub fn on_triggers(mut self, source: impl FnOnce(Triggers) + Send + 'static) -> Self {
        self.trigger_sources.push(Box::new(source));
        self
    }

    /// Register `factory` under `name`, the im.toml section that configures it; its channel
    /// goes by that name. A name that another platform goes by already fails the start.
    pub fn register_platform(mut self, name: &str, factory: Arc<dyn PlatformFactory>) -> Self {
        self.platforms.register(name, factory);
        self
    }

    /// Let `customize` give the application that every conversation's agent is built with
    /// the host's own parts, with the verbs of [`Application`]: tools, hooks, policies and
    /// prompt sections, credentials, and the providers and executors config.toml chooses by
    /// name. The program's own parts come after them: the platforms' credentials, added
    /// beside the host's, the trace export, which is the program's, and the prompt section
    /// on how answers reach people. Each call adds one more, applied in order.
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
        trigger_sources,
    } = program;
    let (path, overrides) = args.into_parts();
    let file = read_file(&path)?;
    // The platforms and the trace export start before the application, which reads
    // config.toml again.
    let proxy = Config::from_environment()?.proxy;
    let BuiltChannels {
        channels,
        mut credentials,
    } = build_channels(&file, &registered, &path, &proxy)?;
    credentials.extend(file.credentials.clone());
    let names: HashSet<&str> = channels.iter().map(|channel| channel.platform()).collect();
    let ServerSections {
        conversations,
        approvers,
        chats,
        daily,
        schedules,
        sessions,
    } = file.server_sections(&names)?;
    let unprompted = Unprompted {
        schedules,
        sources: trigger_sources,
    };
    let (code, end) = shutdown::with_telemetry(
        SERVICE_NAME,
        &proxy,
        async move |telemetry: Option<&Telemetry>| -> Result<u8> {
            if telemetry.is_some() {
                eprint_line(shutdown::EXPORT_NOTICE);
            }
            let Stores {
                place,
                statuses,
                backlog,
                usage,
                shown,
                leadership,
            } = stores::open(&sessions).await?;
            // Once for the process: every conversation's session shares the connections.
            let app = assemble(
                connect(overrides).await?,
                customizations,
                credentials,
                telemetry.map(Telemetry::hook),
                &channels,
            );
            priced_for_daily_cost(&app, daily, &chats)?;
            // A conversation's sessions are those in the place that carry its labels.
            let (agent, chats) = conversations_agents(&app, &place, chats).await?;
            let config = ServerConfig {
                cwd: agent.new_session_cwd(None)?,
                statuses: Some(statuses),
                backlog,
                usage,
                chats,
                daily,
                ..server_config(&file.limits, file.replies, conversations, approvers)
            };
            let files = Files {
                im_toml: &path,
                sessions: &shown,
            };
            let serving = Serving {
                channels,
                unprompted,
                leadership,
            };
            serve(config, &agent, serving, files).await
        },
    )
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
    let app = Application::from_environment(overrides)
        .await
        .map_err(with_model_hint)?;
    for notice in app.notices() {
        eprint_line(notice);
    }
    Ok(app)
}

/// `app` with the host's parts, given by `customizations` in order, then the program's own: the
/// `credentials` of the platforms and of im.toml, hidden and redacted beside those of the host,
/// the `traces` export, the prompt section on how answers reach people, and `send_file` in each
/// session, which posts on its conversation's channel among `channels`.
fn assemble(
    app: Application,
    customizations: Vec<Customization>,
    credentials: Credentials,
    traces: Option<TraceHook>,
    channels: &[Arc<dyn ImChannel>],
) -> Application {
    customizations
        .into_iter()
        .fold(app, |app, customize| customize(app))
        .add_credentials(credentials)
        .set_traces(traces)
        .add_prompt_section(CONVERSATION_SECTION, CONVERSATION)
        .add_session_tools(Arc::new(SendFiles::new(channels)))
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

/// Refuse a daily cost, of all chats or of one, where the model has no prices: nothing could
/// count against it. Every agent asks the model of `app`.
fn priced_for_daily_cost(app: &Application, daily: DailyLimit, chats: &[OwnChat]) -> Result<()> {
    let costs = daily.cost.is_some() || chats.iter().any(|chat| chat.daily.cost.is_some());
    ensure!(
        !costs || app.has_prices(app.model()),
        "dailyCost needs the prices of the model {:?}: add them to [[models]] in config.toml",
        app.model()
    );
    Ok(())
}

/// The agent of the conversations in every chat without one of its own, and what `chats` have
/// of their own, each with the agent it names. Every agent keeps its sessions in `place`, and
/// is built once, after a session with its settings could be prepared.
async fn conversations_agents(
    app: &Application,
    place: &SessionPlace,
    chats: Vec<OwnChat>,
) -> Result<(ConfiguredAgent, Vec<ChatConfig>)> {
    let server = conversations_agent(app, place, SERVER_AGENT.name()).await?;
    let mut built = HashMap::from([(SERVER_AGENT.name().to_string(), server.agent().clone())]);
    let mut configs = Vec::with_capacity(chats.len());
    for OwnChat {
        platform,
        chat,
        agent,
        approvers,
        daily,
    } in chats
    {
        let agent = match agent {
            Some(name) => {
                let named = named_agent(app, place, &mut built, name).await;
                Some(named.with_context(|| format!("chats \"{platform}:{chat}\""))?)
            }
            None => None,
        };
        configs.push(ChatConfig {
            platform,
            chat,
            agent,
            approvers,
            daily,
        });
    }
    Ok((server, configs))
}

/// The agent `name` among those `built`, built and kept there the first time a chat names it.
async fn named_agent(
    app: &Application,
    place: &SessionPlace,
    built: &mut HashMap<String, Agent>,
    name: String,
) -> Result<Agent> {
    if let Some(agent) = built.get(&name) {
        return Ok(agent.clone());
    }
    let agent = conversations_agent(app, place, &name).await?.agent().clone();
    built.insert(name, agent.clone());
    Ok(agent)
}

/// The agent `name` of conversations, whose sessions are kept in `place`, once a session with
/// its settings could be prepared: a wrong model, agent or tool setting would otherwise
/// surface only at the first message, in the chat. That session is never stored.
async fn conversations_agent(app: &Application, place: &SessionPlace, name: &str) -> Result<ConfiguredAgent> {
    build_agent(app, name, &SessionPlace::memory())?
        .create(None)
        .await
        .context("cannot prepare a session with these settings")?;
    build_agent(app, name, place)
}

/// The agent `name` of conversations, whose sessions are kept in `place`, as chat's are, with
/// approvals in the conversation: the server resolves each request with an approver's click
/// on the card it posts.
fn build_agent(app: &Application, name: &str, place: &SessionPlace) -> Result<ConfiguredAgent> {
    let approvals = Approvals::Person("shell commands require approval in the chat");
    app.build(name, app.model(), place, approvals)
}

/// Where the program's files are: im.toml, which a SIGHUP reads again, and the sessions, as
/// the startup line says.
struct Files<'a> {
    im_toml: &'a Path,
    sessions: &'a str,
}

/// What the server serves besides its configuration and agent: the platforms' channels, the
/// conversations started without a message, and when this process may serve.
struct Serving {
    channels: Vec<Arc<dyn ImChannel>>,
    unprompted: Unprompted,
    leadership: Leadership,
}

/// Serve as `config` says, with the sessions of `agent`, kept where `files` says, what
/// `serving` gives, once this process's turn comes, until a signal, a platform's failure or
/// the loss of the lead; the exit code says which. The signals are heard while it waits for
/// its turn: SIGINT and SIGTERM end the wait, and SIGHUP reads the lists again. An `Err` when
/// the turn cannot be waited for, such as when the database cannot be reached.
async fn serve(config: ServerConfig, agent: &ConfiguredAgent, serving: Serving, files: Files<'_>) -> Result<u8> {
    let Serving {
        channels,
        unprompted,
        leadership,
    } = serving;
    let names: Vec<String> = channels.iter().map(|channel| channel.platform().to_string()).collect();
    let cancel = CancellationToken::new();
    // Without the listeners serving goes on, each run still bounded by its own limits.
    if let Err(error) = spawn_signal_handler(cancel.clone()) {
        eprint_line(&format!("zx-im: cannot listen for shutdown signals: {error}"));
    }
    // The session file is the record of each conversation; the operator log holds approvals,
    // triggers and failures (Log), and traces, when exported, the timeline. Made before the
    // turn comes, so that a SIGHUP reaches its lists meanwhile.
    let server = Server::new(config, agent.agent().clone(), Arc::new(Log));
    #[cfg(unix)]
    let reloading = reload::on_hangup(
        files.im_toml.to_path_buf(),
        names.clone(),
        server.access(),
        cancel.clone(),
    );
    let served = match leadership.take(&cancel).await {
        Ok(Some(lead_lost)) => {
            eprint_line(&format!(
                "[im] platforms={}; sessions in {}; schedules: {}",
                names.join(","),
                files.sessions,
                unprompted.schedules.len()
            ));
            Ok(serve_turn(server, channels, unprompted, &cancel, lead_lost).await)
        }
        // A signal came first.
        Ok(None) => Ok(EXIT_COMPLETED),
        Err(error) => Err(error),
    };
    #[cfg(unix)]
    if let Some(reloading) = reloading {
        reloading.abort();
    }
    served
}

/// Serve with `server` until a signal cancels `cancel`, a platform fails or `lead_lost`
/// resolves, which stops the server as a signal does; the exit code says which.
async fn serve_turn(
    server: Server,
    channels: Vec<Arc<dyn ImChannel>>,
    unprompted: Unprompted,
    cancel: &CancellationToken,
    mut lead_lost: LeadLost,
) -> u8 {
    for source in unprompted.sources {
        source(server.triggers());
    }
    let scheduling = tokio::spawn(scheduler::run(unprompted.schedules, server.triggers(), cancel.clone()));
    let running = server.run(channels, cancel);
    tokio::pin!(running);
    let (served, lost) = tokio::select! {
        served = &mut running => (served, None),
        why = &mut lead_lost => {
            cancel.cancel();
            ((&mut running).await, Some(why))
        }
    };
    // The server is gone, also after a platform's failure, which cancels nothing.
    scheduling.abort();
    let code = serve_exit_code(served);
    match lost {
        Some(why) => {
            eprint_line(&format!("zx-im: lost the lead: {why}"));
            EXIT_ERROR
        }
        None => code,
    }
}

/// The channels of the configured IM platforms, and their credentials, whose variables commands
/// must not see.
#[derive(Default)]
struct BuiltChannels {
    channels: Vec<Arc<dyn ImChannel>>,
    credentials: Credentials,
}

impl BuiltChannels {
    /// Add a platform's channel and its credentials.
    fn add(&mut self, built: BuiltChannel) {
        self.channels.push(built.channel);
        self.credentials.extend(built.credentials);
    }
}

/// The channels of the platforms `file` configures, each built by the factory `registered`
/// under its section's name, reaching its platform through `proxy`. An error when it
/// configures none, when no factory or two are registered under a name, and when a channel
/// goes by another name than its section's.
fn build_channels(
    file: &ConfigFile,
    registered: &Registry<dyn PlatformFactory>,
    path: &Path,
    proxy: &Proxy,
) -> Result<BuiltChannels> {
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
                proxy,
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
                credentials: Credentials {
                    variables: vec!["TEAMS_TOKEN".into()],
                    values: Vec::new(),
                },
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
            build_channels(&file, registry, Path::new("im.toml"), &Proxy::Direct)
                .err()
                .unwrap()
        )
    }

    #[test]
    fn a_section_configures_the_platform_registered_under_its_name() {
        let file = ConfigFile::parse(&format!("[teams]\n{SERVER}")).unwrap();
        let built = build_channels(&file, &with_teams(), Path::new("im.toml"), &Proxy::Direct).unwrap();
        let names: Vec<&str> = built.channels.iter().map(|channel| channel.platform()).collect();
        assert_eq!(names, ["teams"]);
        assert_eq!(built.credentials.variables, ["TEAMS_TOKEN"]);
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
