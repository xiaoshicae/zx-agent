//! The ACP agent: an editor or a terminal client drives sessions over stdio with the Agent
//! Client Protocol (JSON-RPC, one message per line). The client renders everything, so this
//! module only maps between the protocol and `Session`: prompts become runs, run events
//! become session updates (`updates`), approvals become permission requests (`permissions`),
//! and a prompt that starts with one of the agent's slash commands runs it (`commands`).
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
mod updates;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities, SessionId, SessionInfo,
    SessionListCapabilities, SessionResumeCapabilities, StopReason, ToolKind,
};
use agent_client_protocol::{Agent, Client, Error, ErrorCode, JsonRpcResponse, Responder, Stdio, V1};
use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use zx_agent::builtin::tools::{BASH, EDIT_FILE, FIND, GREP, LS, READ_FILE, WRITE_FILE};
use zx_agent::compaction::{self, Outcome};
use zx_agent::events::{Budget, EventKind, RunOutcome, RunReport};
use zx_agent::transcript::SessionInfo as StoredSession;
use zx_agent::{Run, Session};
use zx_app::config::Builtin;
use zx_app::otel::{Telemetry, TraceHook};
use zx_app::shutdown::next_event;
use zx_app::{Approvals, ConfiguredAgent, SessionPlace, resolve_cwd};

use crate::cli::{self, AcpArgs};
use crate::listing;
use crate::process::{EXIT_COMPLETED, cancel_on_signals, with_telemetry};

use commands::Command;
use updates::Updates;

struct Host {
    /// The agent of every session, which also holds the sessions the client has open: the
    /// client's approval is asked for its calls. With it, where the sessions are kept, which
    /// is the session files chat keeps too, or memory with `--no-session`.
    agent: ConfiguredAgent,
    /// `--cwd`, the directory of a listing that names none; the current one without it.
    cwd: Option<PathBuf>,
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
        self.agent.agent().session(&id.0)
    }

    /// Whether sessions are kept as files, where a later request or process finds them
    /// again: not with `--no-session`.
    fn keeps_files(&self) -> bool {
        self.agent.place().is_persistent()
    }

    /// Create a session for `cwd`, the client's project directory, whatever `--cwd` says.
    /// It is kept among the session files, where `session/load` finds it again.
    async fn create(&self, cwd: &Path) -> Result<SessionId> {
        let session = self.agent.create(Some(cwd)).await?;
        Ok(SessionId::new(session.id()))
    }

    /// The session `id` for `cwd`: the one this process has open, or the one kept among the
    /// session files, opened, when it was started in `cwd`.
    ///
    /// A kept session is asked for by its ID: its file must be among the sessions of `cwd`
    /// before it is opened, which sets it up. A listing of the directory would read every
    /// session in it.
    async fn locate(&self, id: &SessionId, cwd: &Path) -> Result<Session> {
        let open = self.session(id);
        let (id, place) = (&*id.0, &self.agent.place());
        let cwd = resolve_cwd(Some(cwd))?;
        let directory = place.location(&cwd);
        let unknown = || match &directory {
            Some(directory) => unknown_session(id, &format!("no such session in {}", directory.display())),
            None => unknown_session(id, &format!("no such session in {}", cwd.display())),
        };
        if let Some(session) = open {
            // Open in this process, it is still only a session of the directory it works in.
            return if session.cwd() == cwd {
                Ok(session)
            } else {
                Err(unknown().into())
            };
        }
        let directory = directory
            .clone()
            .ok_or_else(|| unknown_session(id, "the agent keeps no session files (--no-session)"))?;
        let kept_there = place
            .path_of(id)
            .await?
            .is_some_and(|path| path.parent() == Some(directory.as_path()));
        if !kept_there {
            return Err(unknown().into());
        }
        let session = crate::process::resume(&self.agent, id).await?;
        // Two directories can share a directory name of sessions (`/a-b` and `/a/b`).
        if session.cwd() != cwd {
            // Started in another directory: not a session of this one, and not left open,
            // where the next request would find it whatever directory it names.
            self.agent.agent().close_session(id);
            return Err(unknown().into());
        }
        Ok(session)
    }

    /// Load the session `request` names from the session directory of its `cwd`, replaying
    /// the conversation to the client first.
    ///
    /// A session this process has open already is replayed as it is, without opening its
    /// file again.
    async fn load(&self, cx: &ConnectionTo<Client>, request: &LoadSessionRequest) -> Result<SessionId> {
        let id = &request.session_id;
        let session = self.locate(id, &request.cwd).await?;
        let updates = Updates::new(cx.clone(), id.clone(), session.cwd().to_path_buf());
        session
            .with_transcript(|store| updates.replay(store))
            .with_context(|| format!("cannot replay session {}", id.0))?;
        Ok(id.clone())
    }

    /// Continue the session `request` names without replaying it (`session/resume`): the
    /// client keeps what it shows, and the conversation goes on from the file.
    async fn resume(&self, request: &ResumeSessionRequest) -> Result<SessionId> {
        self.locate(&request.session_id, &request.cwd).await?;
        Ok(request.session_id.clone())
    }

    /// The sessions kept that were started in `request.cwd`, or in `--cwd`'s directory when
    /// the request names none, most recently active first; none with `--no-session`, which
    /// keeps none. A listing is one page, so a cursor is never one of ours.
    async fn list(&self, request: &ListSessionsRequest) -> Result<Vec<SessionInfo>> {
        if let Some(cursor) = &request.cursor {
            return Err(Error::new(
                ErrorCode::INVALID_PARAMS,
                format!("unknown cursor {cursor:?}: a listing is one page"),
            )
            .into());
        }
        let cwd = resolve_cwd(request.cwd.as_deref().or(self.cwd.as_deref()))?;
        if !self.keeps_files() {
            return Ok(Vec::new());
        }
        let kept = self.agent.place().started_in(&cwd).await?;
        Ok(kept.sessions.iter().map(session_info).collect())
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
        let result = self.create(&request.cwd).await;
        opened(responder, cx, result, NewSessionResponse::new)
    }

    /// `session/load`: the replay is sent before the response, as the protocol has it.
    async fn load_session(
        &self,
        request: &LoadSessionRequest,
        responder: Responder<LoadSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = self.load(cx, request).await;
        opened(responder, cx, result, |_| LoadSessionResponse::new())
    }

    /// `session/resume`: the session goes on from its file, with nothing replayed.
    async fn resume_session(
        &self,
        request: &ResumeSessionRequest,
        responder: Responder<ResumeSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = self.resume(request).await;
        opened(responder, cx, result, |_| ResumeSessionResponse::new())
    }

    /// `session/list`: the sessions kept for a working directory.
    async fn list_sessions(
        &self,
        request: &ListSessionsRequest,
        responder: Responder<ListSessionsResponse, V1>,
    ) -> Result<(), Error> {
        let result = self.list(request).await;
        responder.respond_with_result(result.map(ListSessionsResponse::new).map_err(protocol_error))
    }

    /// `session/prompt`: the turn runs outside the dispatch loop, so cancellations and
    /// permission answers keep arriving; the response is sent when the turn ends. A prompt
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
            return responder.respond_with_error(unknown_session(&request.session_id.0, why));
        };
        let text = prompt_text(&request.prompt);
        let (connection, session_id) = (cx.clone(), request.session_id.clone());
        let started = match commands::parse(&text) {
            Some(Command::Name(name)) => {
                return cx.spawn(async move {
                    let answer = commands::name(&session, name, &connection, &session_id).await;
                    responder.respond_with_result(answer.map_err(protocol_error))
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
        let turn = Turn {
            updates: Updates::new(connection.clone(), session_id.clone(), session.cwd().to_path_buf()),
            session,
            cx: connection,
            id: session_id,
            stop: self.stop.clone(),
        };
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
        self.agent.agent().close_session(&request.session_id.0);
        responder.respond(CloseSessionResponse::new())
    }
}

/// What a turn runs: the task a prompt starts, or the compaction `/compact` starts.
enum Running {
    Task(Run),
    Compaction(Run<compaction::Report>),
}

/// One turn of a session: the conversation, and the client it reports to and asks.
struct Turn {
    session: Session,
    updates: Updates,
    cx: ConnectionTo<Client>,
    id: SessionId,
    /// Cancelled when the process is told to stop.
    stop: CancellationToken,
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
    let app = crate::process::connect(cli::overrides(args.base, &args.budget), Builtin::Assistant)
        .await?
        .with_traces(traces);
    let place = if args.no_session {
        SessionPlace::memory()
    } else {
        SessionPlace::home()?
    };
    // The client answers: each approval request becomes a `session/request_permission`.
    let approvals = Approvals::Person("the client must approve shell commands");
    let agent = app.build(app.model(), &place, approvals)?;
    let stop = CancellationToken::new();
    cancel_on_signals(stop.clone());
    let (turns, mut turns_ended) = mpsc::channel::<()>(1);
    let connection = connect(Arc::new(Host {
        agent,
        cwd: app.cwd().map(Path::to_path_buf),
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
async fn connect(host: Arc<Host>) -> Result<(), Error> {
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
                let (host, connection) = (Arc::clone(&on_new), cx.clone());
                cx.spawn(async move { host.new_session(&request, responder, &connection).await })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                let (host, connection) = (Arc::clone(&on_load), cx.clone());
                cx.spawn(async move { host.load_session(&request, responder, &connection).await })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, cx| {
                let (host, connection) = (Arc::clone(&on_resume), cx.clone());
                cx.spawn(async move { host.resume_session(&request, responder, &connection).await })
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

impl Turn {
    /// The response to `session/prompt` once `running`, the turn, has ended: its stop reason,
    /// or its error. A compaction that found nothing old enough to summarize says so, since
    /// nothing else would show that it ran.
    async fn answer(mut self, running: Running) -> Result<PromptResponse> {
        let reason = match running {
            Running::Task(mut run) => {
                self.read(&mut run).await?;
                stop_reason(&run.wait().await)?
            }
            Running::Compaction(mut run) => {
                self.read(&mut run).await?;
                let report = run.wait().await;
                if report.outcome == Outcome::Noop {
                    self.updates.nothing_to_compact();
                }
                compaction_stop(&report)?
            }
        };
        Ok(PromptResponse::new(reason))
    }

    /// Read `run` to its end: its events go to the client as session updates, and a call
    /// that waits for approval becomes a permission request. A process that is told to stop
    /// cancels the run, which is still read to its end.
    async fn read<T>(&mut self, run: &mut Run<T>) -> Result<()> {
        while let Some(event) = next_event(run, &self.session, &self.stop).await {
            self.updates.emit(&event);
            if let EventKind::ApprovalRequested {
                approval_id,
                tool_call_id,
                tool_name,
                args,
                ..
            } = event.event
            {
                let request = permissions::Request {
                    approval_id,
                    tool_call_id,
                    tool: tool_name,
                    arguments: args,
                };
                permissions::ask(&self.cx, &self.id, &self.session, request)?;
            }
        }
        Ok(())
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

/// The error for a session the agent does not have, with the reason.
fn unknown_session(id: &str, why: &str) -> Error {
    Error::new(ErrorCode::INVALID_PARAMS, format!("unknown session {id}: {why}"))
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

/// A listed session as the client shows it: its name, else its first prompt, as the title,
/// and its last activity as the time.
fn session_info(session: &StoredSession) -> SessionInfo {
    // A time past the year 292 million is not one a session file records; the entry then
    // has no time rather than a wrong one.
    let updated_at = i64::try_from(session.last_activity_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    SessionInfo::new(SessionId::new(session.id.as_str()), PathBuf::from(&session.cwd))
        .title(session.title().map(listing::preview_text))
        .updated_at(updated_at)
}

/// The text of a prompt: its text blocks, one per line, and a line naming each resource
/// link, which the protocol has every agent take. The agent advertises no image, audio or
/// embedded-context capability, so clients send nothing else.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(Cow::Borrowed(text.text.as_str())),
            ContentBlock::ResourceLink(link) => Some(Cow::Owned(format!("[resource {} ({})]", link.uri, link.name))),
            // The protocol may add block kinds: the enum is non-exhaustive.
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a finished run answers `session/prompt`. Limits that end a turn have a stop reason;
/// a failed or timed-out run is a request error, with the run's message.
fn stop_reason(report: &RunReport) -> Result<StopReason> {
    match report.outcome {
        RunOutcome::Completed => Ok(StopReason::EndTurn),
        // The protocol has no reason for a cost limit; tokens are what a cost is paid for.
        RunOutcome::Truncated
        | RunOutcome::BudgetExhausted {
            budget: Budget::Tokens | Budget::Cost,
            ..
        } => Ok(StopReason::MaxTokens),
        RunOutcome::BudgetExhausted {
            budget: Budget::ModelRequests,
            ..
        } => Ok(StopReason::MaxTurnRequests),
        RunOutcome::Cancelled => Ok(StopReason::Cancelled),
        RunOutcome::Failed | RunOutcome::TimedOut => Err(anyhow::Error::msg(
            report
                .error
                .clone()
                .unwrap_or_else(|| format!("the run ended with {:?}", report.outcome)),
        )),
    }
}

/// How a finished compaction answers the `/compact` that asked for it; a failed one is a
/// request error, with its message.
fn compaction_stop(report: &compaction::Report) -> Result<StopReason> {
    match report.outcome {
        Outcome::Completed | Outcome::Noop => Ok(StopReason::EndTurn),
        Outcome::Cancelled => Ok(StopReason::Cancelled),
        Outcome::Failed => Err(anyhow::Error::msg(
            report
                .error
                .clone()
                .unwrap_or_else(|| "the compaction failed".to_string()),
        )),
    }
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
    use agent_client_protocol::v1::schema::{ResourceLink, TextContent};
    use zx_agent::ai::Usage;

    use super::*;

    fn report(outcome: RunOutcome, error: Option<&str>) -> RunReport {
        RunReport {
            outcome,
            final_text: String::new(),
            model_requests: 1,
            retries: 0,
            usage: Usage::default(),
            tool_usage: std::collections::BTreeMap::new(),
            denied_calls: 0,
            error: error.map(str::to_string),
            session_id: "session".into(),
        }
    }

    /// The stop reason, or the message of the request error as the client gets it.
    fn answer(report: &RunReport) -> Result<StopReason, String> {
        stop_reason(report).map_err(|error| protocol_error(error).message)
    }

    #[test]
    fn outcomes_map_to_stop_reasons_and_failures_to_request_errors() {
        assert_eq!(answer(&report(RunOutcome::Completed, None)), Ok(StopReason::EndTurn));
        assert_eq!(answer(&report(RunOutcome::Truncated, None)), Ok(StopReason::MaxTokens));
        let exhausted = |budget, truncated| RunOutcome::BudgetExhausted { budget, truncated };
        assert_eq!(
            answer(&report(exhausted(Budget::Tokens, false), None)),
            Ok(StopReason::MaxTokens)
        );
        // Which budget stopped the turn decides, not whether its last answer was cut off.
        assert_eq!(
            answer(&report(exhausted(Budget::ModelRequests, true), None)),
            Ok(StopReason::MaxTurnRequests)
        );
        assert_eq!(answer(&report(RunOutcome::Cancelled, None)), Ok(StopReason::Cancelled));
        assert_eq!(
            answer(&report(RunOutcome::Failed, Some("boom"))),
            Err("boom".to_string())
        );
        assert_eq!(
            answer(&report(RunOutcome::TimedOut, None)),
            Err("the run ended with TimedOut".to_string())
        );
    }

    #[test]
    fn a_compaction_ends_its_turn_and_a_failed_one_is_a_request_error() {
        let ended = |outcome, error: Option<&str>| {
            let report = compaction::Report {
                outcome,
                requests: 0,
                usage: Usage::default(),
                input_bytes: 0,
                output_bytes: 0,
                entry_id: None,
                error: error.map(str::to_string),
            };
            compaction_stop(&report).map_err(|error| error.to_string())
        };
        assert_eq!(ended(Outcome::Completed, None), Ok(StopReason::EndTurn));
        assert_eq!(ended(Outcome::Noop, None), Ok(StopReason::EndTurn));
        assert_eq!(ended(Outcome::Cancelled, Some("cancelled")), Ok(StopReason::Cancelled));
        assert_eq!(
            ended(Outcome::Failed, Some("invalid summary JSON")),
            Err("invalid summary JSON".to_string())
        );
    }

    #[test]
    fn a_prompt_is_the_text_of_its_text_blocks() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("one")),
            ContentBlock::Text(TextContent::new("two")),
        ];
        assert_eq!(prompt_text(&blocks), "one\ntwo");
        assert_eq!(prompt_text(&[]), "");
    }

    #[test]
    fn a_resource_link_in_a_prompt_reaches_the_model_as_a_line_naming_it() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("review this")),
            ContentBlock::ResourceLink(ResourceLink::new("main.rs", "file:///work/src/main.rs")),
        ];
        assert_eq!(
            prompt_text(&blocks),
            "review this\n[resource file:///work/src/main.rs (main.rs)]"
        );
    }

    #[test]
    fn built_in_tools_have_protocol_kinds() {
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("grep"), ToolKind::Search);
        assert_eq!(tool_kind("ls"), ToolKind::Read);
        assert_eq!(tool_kind("prom__query"), ToolKind::Other);
    }
}
