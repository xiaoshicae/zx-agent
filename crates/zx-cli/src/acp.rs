//! The ACP agent: an editor or a terminal client drives sessions over stdio with the Agent
//! Client Protocol (JSON-RPC, one message per line). The client renders everything, so this
//! module only maps between the protocol and `Session`; it answers the requests:
//!
//! - `sessions`: the sessions a client creates, loads, resumes and lists
//! - `turn`: a prompt's turn: the run it starts, read to its end
//! - `updates`: run events as session updates, and the replay of a loaded session
//! - `permissions`: approvals as permission requests
//! - `commands`: the agent's slash commands, which a prompt that starts with one runs
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
mod permissions;
mod sessions;
mod turn;
mod updates;

use std::sync::Arc;

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities, SessionId, SessionListCapabilities,
    SessionResumeCapabilities, ToolKind,
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

use crate::application::connect;
use crate::cli::{self, AcpArgs};
use crate::process::{EXIT_COMPLETED, cancel_on_signals, with_telemetry};

use commands::Command;
use turn::{Running, Turn, prompt_text};

struct Host {
    /// The agent of every session, which also holds the sessions the client has open: the
    /// client's approval is asked for its calls. With it, where the sessions are kept, which
    /// is the session files chat keeps too, or memory with `--no-session`.
    configured: ConfiguredAgent,
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
        opened(responder, cx, result, NewSessionResponse::new)
    }

    /// `session/load`: the replay is sent before the response, as the protocol has it.
    async fn load_session(
        &self,
        request: &LoadSessionRequest,
        responder: Responder<LoadSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = sessions::load(&self.configured, cx, request).await;
        opened(responder, cx, result, |_| LoadSessionResponse::new())
    }

    /// `session/resume`: the session goes on from its file, with nothing replayed.
    async fn resume_session(
        &self,
        request: &ResumeSessionRequest,
        responder: Responder<ResumeSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = sessions::resume(&self.configured, request).await;
        opened(responder, cx, result, |_| ResumeSessionResponse::new())
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
    /// without text is refused, as `run` refuses one. A prompt
    /// that is one of the agent's commands runs the command instead; `/compact` is a turn of
    /// its own, taken and cancelled as a prompt's is.
    ///
    /// The session is taken for the turn here, in the dispatch loop: a `session/cancel` that
    /// follows the prompt at once then finds the turn, though it has not run yet. A session
    /// that is busy or closed answers the prompt at once.
    fn prompt(
        &self,
        request: &PromptRequest,
        responder: Responder<PromptResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let Some(session) = self.session(&request.session_id) else {
            // Never opened, closed by the client, or closed by itself when its record could
            // no longer be written.
            let why = if self.keeps_files() {
                "it is not open; session/load or session/resume opens it"
            } else {
                "it is not open, and nothing is kept of a closed session (--no-session)"
            };
            return responder.respond_with_error(sessions::unknown_session(&request.session_id.0, why));
        };
        let text = prompt_text(&request.prompt);
        // The kernel takes any input; an empty one would reach the model as a message.
        if text.trim().is_empty() {
            return responder.respond_with_error(Error::new(ErrorCode::INVALID_PARAMS, "the prompt is empty"));
        }
        let session_id = request.session_id.clone();
        let started = match commands::parse(&text) {
            Some(Command::Name(name)) => {
                return cx.spawn({
                    let cx = cx.clone();
                    async move {
                        let answer = commands::name(&session, name, &cx, &session_id).await;
                        responder.respond_with_result(answer.map_err(protocol_error))
                    }
                });
            }
            Some(Command::Usage(usage)) => {
                return responder.respond_with_error(Error::new(ErrorCode::INVALID_PARAMS, usage));
            }
            Some(Command::Compact) => session.compact().map(Running::Compaction),
            None => session.send(text).map(Running::Task),
        };
        let running = match started {
            Ok(running) => running,
            Err(error) => return responder.respond_with_error(protocol_error(error.into())),
        };
        let turn = Turn::new(session, cx.clone(), session_id, self.stop.clone());
        // None once the stop has waited for the turns: this one then ends with the process.
        let in_progress = self.turns.upgrade();
        cx.spawn(async move {
            let answer = turn.answer(running).await;
            let answered = responder.respond_with_result(answer.map_err(protocol_error));
            drop(in_progress);
            answered
        })
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
    let app = connect(cli::overrides(args.base, &args.budget), Builtin::Assistant)
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
    let stop = CancellationToken::new();
    cancel_on_signals(stop.clone());
    let (turns, mut turns_ended) = mpsc::channel::<()>(1);
    let connection = client_connection(Arc::new(Host {
        configured,
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
            async move |request: PromptRequest, responder, cx| on_prompt.prompt(&request, responder, &cx),
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

/// Answer a request that opened a session, then offer the session's commands: the client
/// knows the session from the answer on.
fn opened<T: JsonRpcResponse>(
    responder: Responder<T, V1>,
    cx: &ConnectionTo<Client>,
    result: Result<SessionId>,
    response: impl FnOnce(SessionId) -> T,
) -> Result<(), Error> {
    match result {
        Ok(id) => {
            responder.respond(response(id.clone()))?;
            commands::offer(cx, &id);
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

/// What the agent offers: closing sessions, which frees them also in memory, and loading,
/// listing and resuming sessions when it keeps their files.
fn agent_capabilities(kept: bool) -> AgentCapabilities {
    let mut sessions = SessionCapabilities::new().close(SessionCloseCapabilities::new());
    if kept {
        sessions = sessions
            .list(SessionListCapabilities::new())
            .resume(SessionResumeCapabilities::new());
    }
    AgentCapabilities::new()
        .load_session(kept)
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
