//! The ACP agent: an editor or a terminal client drives sessions over stdio with the Agent
//! Client Protocol (JSON-RPC, one message per line). The client renders everything, so this
//! module only maps between the protocol and `Session`; it answers the requests:
//!
//! - `sessions`: the sessions a client creates, loads, resumes and lists
//! - `turn`: a prompt's turn: the run it starts, read to its end
//! - `updates`: run events as session updates, and the replay of a loaded session
//! - `permissions`: approvals as permission requests
//! - `commands`: the agent's slash commands, which a prompt that starts with one runs
//! - `options`: the session's configuration options, the model and the reasoning level
//! - `usage`: how full the model's context is
//! - `models`: the models config.toml lists, which those two use
//!
//! Sessions are kept as files, one per session in the directory chat uses for the same
//! working directory, so that a client can list them and load one again in a later process:
//! `session/load` replays the conversation to the client before it goes on, `session/resume`
//! goes on without the replay. A session works in the directory it was created for, also
//! after it was loaded again. `session/close` closes a session: its turn, if it has one, is
//! cancelled, and its file is closed for another process to open, at once when it is idle,
//! else when the cancelled turn has ended. The file stays, to be loaded or resumed again.
//!
//! SIGINT and SIGTERM stop the agent: the turns in progress are cancelled and read to their
//! end, and the sessions close as the process leaves.
//!
//! Stdout is the protocol channel: nothing else in this process may print there.

mod commands;
mod models;
mod options;
mod permissions;
mod sessions;
mod turn;
mod updates;
mod usage;

use std::sync::Arc;

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities, SessionConfigOption,
    SessionId, SessionListCapabilities, SessionResumeCapabilities, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, ToolKind,
};
use agent_client_protocol::{Agent, Client, Error, ErrorCode, JsonRpcResponse, Responder, Stdio, V1};
use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zx_agent::Session;
use zx_agent::builtin::tools::{BASH, EDIT_FILE, FIND, GREP, LS, READ_FILE, WRITE_FILE};
use zx_app::config::Builtin;
use zx_app::otel::{Telemetry, TraceHook};
use zx_app::{Approvals, ConfiguredAgent, SessionPlace};

use self::commands::Command;
use self::models::Models;
use self::options::Change;
use self::turn::{Prompt, Running, Turn};
use crate::application::connect;
use crate::cli::{self, AcpArgs};
use crate::process::{EXIT_COMPLETED, cancel_on_signals, with_telemetry};

struct Host {
    /// The agent of every session, which also holds the sessions the client has open: the
    /// client's approval is asked for its calls. With it, where the sessions are kept, which
    /// is the session files chat keeps too, or memory with `--no-session`.
    configured: ConfiguredAgent,
    /// The models config.toml lists, shared with the turns, which run as tasks of their own.
    models: Arc<Models>,
    /// Cancelled when the process is told to stop: every turn then cancels its run.
    stop: CancellationToken,
    /// Each turn holds a sender of this channel while it runs, and nothing is ever sent:
    /// the stop drops the one sender that outlives the turns, and has waited for them once
    /// the channel is closed.
    turns: mpsc::WeakSender<()>,
}

impl Host {
    /// The session `id` when the client has it open.
    fn session(&self, id: &SessionId) -> Option<Session> {
        self.configured.agent().session(&id.0)
    }

    /// The error for a request about session `id` when the client does not have it open:
    /// never opened, closed by the client, or closed by itself when its record could no
    /// longer be written.
    fn not_open(&self, id: &SessionId) -> Error {
        let why = if self.keeps_files() {
            "it is not open; session/load or session/resume opens it"
        } else {
            "it is not open, and nothing is kept of a closed session (--no-session)"
        };
        sessions::unknown_session(&id.0, why)
    }

    /// Whether sessions are kept as files, where a later request or process finds them
    /// again: not with `--no-session`.
    fn keeps_files(&self) -> bool {
        self.configured.place().is_persistent()
    }

    /// `initialize`: what the agent can do; sessions can be loaded, listed and resumed when
    /// they are kept as files.
    fn initialize(
        &self,
        request: &InitializeRequest,
        responder: Responder<InitializeResponse, V1>,
    ) -> Result<(), Error> {
        responder.respond(
            InitializeResponse::new(request.protocol_version)
                .agent_capabilities(agent_capabilities(self.keeps_files()))
                .agent_info(Implementation::new("zx-agent", env!("CARGO_PKG_VERSION"))),
        )
    }

    /// `session/new`: a session for the client's project directory.
    async fn new_session(
        &self,
        request: &NewSessionRequest,
        responder: Responder<NewSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = sessions::create(&self.configured, &request.cwd).await;
        opened(responder, cx, &self.models, result, |id, options| {
            NewSessionResponse::new(id).config_options(options)
        })
    }

    /// `session/load`: the replay is sent before the response, as the protocol has it.
    async fn load_session(
        &self,
        request: &LoadSessionRequest,
        responder: Responder<LoadSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = sessions::load(&self.configured, cx, request).await;
        opened(responder, cx, &self.models, result, |_, options| {
            LoadSessionResponse::new().config_options(options)
        })
    }

    /// `session/resume`: the session goes on from its file, with nothing replayed.
    async fn resume_session(
        &self,
        request: &ResumeSessionRequest,
        responder: Responder<ResumeSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = sessions::resume(&self.configured, request).await;
        opened(responder, cx, &self.models, result, |_, options| {
            ResumeSessionResponse::new().config_options(options)
        })
    }

    /// `session/list`: the sessions kept for a working directory.
    async fn list_sessions(
        &self,
        request: &ListSessionsRequest,
        responder: Responder<ListSessionsResponse, V1>,
    ) -> Result<(), Error> {
        let result = sessions::list(&self.configured, request).await;
        responder.respond_with_result(result.map(ListSessionsResponse::new).map_err(protocol_error))
    }

    /// `session/prompt`: the turn runs outside the dispatch loop, so cancellations and
    /// permission answers keep arriving; the response is sent when the turn ends. A prompt
    /// with neither text nor images is refused, as `run` refuses an empty one, and so is one
    /// with an image that cannot be fitted to the model APIs' limits. A prompt that is one of
    /// the agent's commands runs the command instead, which takes no images; `/compact` is a
    /// turn of its own, taken and cancelled as a prompt's is.
    ///
    /// The images are fitted and the session is taken for the turn here, in the dispatch
    /// loop: a `session/cancel` that follows the prompt at once then finds the turn, though it
    /// has not run yet, and the loop waits only while the images of a prompt are fitted. A
    /// session that is busy or closed answers the prompt at once.
    async fn prompt(
        &self,
        request: &PromptRequest,
        responder: Responder<PromptResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let Some(session) = self.session(&request.session_id) else {
            return responder.respond_with_error(self.not_open(&request.session_id));
        };
        let prompt = match Prompt::of(&request.prompt).await {
            Ok(prompt) => prompt,
            Err(why) => return responder.respond_with_error(Error::new(ErrorCode::INVALID_PARAMS, why)),
        };
        // The kernel takes any input; an empty one would reach the model as a message.
        if prompt.is_empty() {
            return responder.respond_with_error(Error::new(ErrorCode::INVALID_PARAMS, "the prompt is empty"));
        }
        let session_id = request.session_id.clone();
        let command = commands::parse(&prompt.text);
        if command.is_some() && !prompt.images.is_empty() {
            return responder.respond_with_error(Error::new(ErrorCode::INVALID_PARAMS, "a command takes no images"));
        }
        let started = match command {
            Some(Command::Name(name)) => {
                return cx.spawn({
                    let cx = cx.clone();
                    async move {
                        let answer = commands::name(&cx, &session_id, &session, name).await;
                        responder.respond_with_result(answer.map_err(protocol_error))
                    }
                });
            }
            Some(Command::Usage(usage)) => {
                return responder.respond_with_error(Error::new(ErrorCode::INVALID_PARAMS, usage));
            }
            Some(Command::Compact) => session.compact().map(Running::Compaction),
            None => session.send(prompt.into_input()).map(Running::Task),
        };
        let running = match started {
            Ok(running) => running,
            Err(error) => return responder.respond_with_error(protocol_error(error.into())),
        };
        let turn = Turn::new(
            session,
            cx.clone(),
            session_id,
            self.stop.clone(),
            Arc::clone(&self.models),
        );
        // None once the stop has waited for the turns: this one then ends with the process.
        let in_progress = self.turns.upgrade();
        cx.spawn(async move {
            let answer = turn.answer(running).await;
            let answered = responder.respond_with_result(answer.map_err(protocol_error));
            drop(in_progress);
            answered
        })
    }

    /// `session/set_config_option`: change the model or the reasoning level of an idle
    /// session; the response has every option as it is then. A new model may have another
    /// context window, so the usage follows.
    fn set_config_option(
        &self,
        request: &SetSessionConfigOptionRequest,
        responder: Responder<SetSessionConfigOptionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let Some(session) = self.session(&request.session_id) else {
            return responder.respond_with_error(self.not_open(&request.session_id));
        };
        let change = Change {
            option: &request.config_id.0,
            value: &request.value,
        };
        match options::set(&session, self.configured.provider(), &self.models, &change) {
            Ok(options) => {
                responder.respond(SetSessionConfigOptionResponse::new(options))?;
                usage::of_session(cx, &request.session_id, &session, &self.models);
                Ok(())
            }
            Err(error) => responder.respond_with_error(protocol_error(error)),
        }
    }

    /// `session/cancel`: abort the session's turn, if it has one.
    fn cancel(&self, notification: &CancelNotification) {
        if let Some(session) = self.session(&notification.session_id) {
            session.cancel();
        }
    }

    /// `session/close`: abort the session's turn, if it has one, and close the session: its
    /// file is free at once when it is idle, else when the aborted turn has ended. A session
    /// this process does not have open has nothing to free, so closing one succeeds too.
    fn close_session(
        &self,
        request: &CloseSessionRequest,
        responder: Responder<CloseSessionResponse, V1>,
    ) -> Result<(), Error> {
        self.configured.agent().close_session(&request.session_id.0);
        responder.respond(CloseSessionResponse::new())
    }
}

pub(crate) async fn run(args: AcpArgs) -> Result<u8> {
    with_telemetry(async move |telemetry| serve(args, telemetry.map(Telemetry::hook)).await).await
}

/// Serve the client on stdio until it closes the connection, or until SIGINT or SIGTERM.
///
/// A signal cancels the turns in progress, and the connection is served until they have
/// ended, so that each session records how its turn ended; whether the client still reads
/// the answers is not waited for. The sessions close as the connection and the agent are
/// dropped on the way out, before what remains of the traces is exported.
async fn serve(args: AcpArgs, traces: Option<TraceHook>) -> Result<u8> {
    let app = connect(
        cli::overrides(args.base, &args.budget, &args.approvals),
        Builtin::Assistant,
    )
    .await?
    .set_traces(traces);
    let place = if args.no_session {
        SessionPlace::memory()
    } else {
        SessionPlace::home()?
    };
    // The client answers: each approval request becomes a `session/request_permission`.
    let approvals = Approvals::Person("the client must approve shell commands");
    let configured = app.build(app.model(), &place, approvals)?;
    let models = Arc::new(Models::new(app.models()));
    let stop = CancellationToken::new();
    cancel_on_signals(stop.clone());
    let (turns, mut turns_ended) = mpsc::channel::<()>(1);
    let connection = client_connection(Arc::new(Host {
        configured,
        models,
        stop: stop.clone(),
        turns: turns.downgrade(),
    }));
    tokio::pin!(connection);
    tokio::select! {
        served = &mut connection => served.context("the ACP connection failed")?,
        () = stop.cancelled() => {
            // Every turn cancels its run now (`Turn::answer`). Without this sender, the
            // channel closes when the last of them has ended.
            drop(turns);
            tokio::select! {
                served = &mut connection => served.context("the ACP connection failed")?,
                _ = turns_ended.recv() => {}
            }
        }
    }
    Ok(EXIT_COMPLETED)
}

/// The connection to the client on stdio, each request and notification handled by `host`,
/// until the client closes it.
async fn client_connection(host: Arc<Host>) -> Result<(), Error> {
    let on_initialize = Arc::clone(&host);
    let on_new = Arc::clone(&host);
    let on_load = Arc::clone(&host);
    let on_resume = Arc::clone(&host);
    let on_list = Arc::clone(&host);
    let on_prompt = Arc::clone(&host);
    let on_set_option = Arc::clone(&host);
    let on_cancel = Arc::clone(&host);
    let on_close = Arc::clone(&host);
    Agent
        .v1()
        .name("zx-agent")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| on_initialize.initialize(&request, responder),
            agent_client_protocol::on_receive_request!(),
        )
        // Opening a session sets it up, which can take long, as starting its container does;
        // listing reads every session of a directory. They run outside the dispatch loop, so
        // that the messages of the other sessions keep arriving meanwhile. A client uses a
        // session only once it has the answer.
        .on_receive_request(
            async move |request: NewSessionRequest, responder, cx| {
                let host = Arc::clone(&on_new);
                cx.spawn({
                    let cx = cx.clone();
                    async move { host.new_session(&request, responder, &cx).await }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                let host = Arc::clone(&on_load);
                cx.spawn({
                    let cx = cx.clone();
                    async move { host.load_session(&request, responder, &cx).await }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, cx| {
                let host = Arc::clone(&on_resume);
                cx.spawn({
                    let cx = cx.clone();
                    async move { host.resume_session(&request, responder, &cx).await }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, cx| {
                let host = Arc::clone(&on_list);
                cx.spawn(async move { host.list_sessions(&request, responder).await })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| on_prompt.prompt(&request, responder, &cx).await,
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest, responder, cx| {
                on_set_option.set_config_option(&request, responder, &cx)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest, responder, _cx| on_close.close_session(&request, responder),
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _cx| {
                on_cancel.cancel(&notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// Answer a request that opened a session, with its configuration options, then offer the
/// session's commands and say how full its context is: the client knows the session from
/// the answer on.
fn opened<T: JsonRpcResponse>(
    responder: Responder<T, V1>,
    cx: &ConnectionTo<Client>,
    models: &Models,
    result: Result<Session>,
    response: impl FnOnce(SessionId, Option<Vec<SessionConfigOption>>) -> T,
) -> Result<(), Error> {
    match result {
        Ok(session) => {
            let id = SessionId::new(session.id());
            // None while a turn runs, as when a client loads a session this process runs:
            // its options cannot be read then, and the response leaves them out.
            let options = options::of(&session, models).ok();
            responder.respond(response(id.clone(), options))?;
            commands::offer(cx, &id);
            usage::of_session(cx, &id, &session, models);
            Ok(())
        }
        Err(error) => responder.respond_with_error(protocol_error(error)),
    }
}

/// The error a failed request answers with. An error the protocol defines, such as an
/// unknown session, keeps its code; anything else is an internal error whose message is the
/// whole chain of causes. Clients show the message: the SDK's own conversion from `anyhow`
/// would send a generic one, with only the outermost cause in `data`.
fn protocol_error(error: anyhow::Error) -> Error {
    error
        .downcast::<Error>()
        .unwrap_or_else(|error| Error::new(ErrorCode::INTERNAL_ERROR, format!("{error:#}")))
}

/// What the agent offers: images in prompts, closing sessions, which frees them also in
/// memory, and loading, listing and resuming sessions when it keeps their files.
fn agent_capabilities(kept: bool) -> AgentCapabilities {
    let mut sessions = SessionCapabilities::new().close(SessionCloseCapabilities::new());
    if kept {
        sessions = sessions
            .list(SessionListCapabilities::new())
            .resume(SessionResumeCapabilities::new());
    }
    AgentCapabilities::new()
        .load_session(kept)
        .prompt_capabilities(PromptCapabilities::new().image(true))
        .session_capabilities(sessions)
}

/// The protocol's kinds for the built-in tools; everything else, MCP tools included, is
/// `Other`, which clients show without an icon.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        BASH => ToolKind::Execute,
        READ_FILE | LS => ToolKind::Read,
        EDIT_FILE | WRITE_FILE => ToolKind::Edit,
        GREP | FIND => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_tools_have_protocol_kinds() {
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("grep"), ToolKind::Search);
        assert_eq!(tool_kind("ls"), ToolKind::Read);
        assert_eq!(tool_kind("prom__query"), ToolKind::Other);
    }
}
