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
//! Stdout is the protocol channel: nothing else in this process may print there.

mod commands;
mod permissions;
mod updates;

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

use zx_agent::Session;
use zx_agent::builtin::tools::{BASH, EDIT_FILE, FIND, GREP, LS, READ_FILE, WRITE_FILE};
use zx_agent::config::Builtin;
use zx_agent::events::EventKind;
use zx_agent::otel::{Telemetry, TraceHook};
use zx_agent::runner::{RunOutcome, RunReport};
use zx_agent::transcript::{SessionInfo as StoredSession, SessionQuery};

use crate::cli::{self, AcpArgs, Approvals, ModeAgent};
use crate::process::{EXIT_COMPLETED, with_telemetry};
use crate::session_files::{self, SessionChoice, SessionPlace};

use updates::Updates;

/// Where a session the client names is.
enum Located {
    /// A conversation this process has open: one the client created with `session/new`,
    /// loaded with `session/load` or resumed with `session/resume`.
    Open(Session),
    /// A session kept among the session files, which this process does not have open.
    Stored,
}

struct Host {
    /// `--cwd`, for a listing that names no directory.
    cwd: Option<PathBuf>,
    /// Where sessions are kept: the session files chat keeps too, or memory with
    /// `--no-session`.
    place: SessionPlace,
    /// The agent of every session, which also holds the sessions the client has open: the
    /// client's approval is asked for its calls.
    mode: ModeAgent,
}

impl Host {
    /// The session `id` when the client has it open.
    fn session(&self, id: &SessionId) -> Option<Session> {
        self.mode.agent.session(&id.0)
    }

    /// The sessions started in `cwd` that are kept as files, with their directory; `None`
    /// with `--no-session`, which keeps none.
    async fn kept(&self, cwd: &Path) -> Result<Option<(Vec<StoredSession>, PathBuf)>> {
        let Some(directory) = self.place.location(cwd) else {
            return Ok(None);
        };
        let query = SessionQuery {
            cwd: Some(cwd.to_path_buf()),
            ..SessionQuery::default()
        };
        Ok(Some((self.place.list(&query).await?.sessions, directory)))
    }

    /// Create a session for `cwd`, the client's project directory, which overrides `--cwd`.
    /// It is kept among the session files, where `session/load` finds it again.
    async fn create(&self, cwd: &Path) -> Result<SessionId> {
        let choice = SessionChoice::existing_or_new(None);
        let opened = self.mode.open(Some(cwd), choice).await?;
        Ok(SessionId::new(opened.session.id()))
    }

    /// Open the stored session `id`, which goes on in the directory it started in.
    async fn open(&self, id: &SessionId) -> Result<Session> {
        let opened = self.mode.open(None, SessionChoice::Existing(&id.0)).await?;
        Ok(opened.session)
    }

    /// The session `id` for `cwd`: one this process has open, or one kept among the sessions
    /// started in `cwd`.
    async fn locate(&self, id: &SessionId, cwd: &Path) -> Result<Located> {
        if let Some(session) = self.session(id) {
            return Ok(Located::Open(session));
        }
        let id = &*id.0;
        let (sessions, directory) = self
            .kept(&cli::resolve_cwd(Some(cwd))?)
            .await?
            .ok_or_else(|| unknown_session(id, "the agent keeps no session files (--no-session)"))?;
        if sessions.iter().any(|session| session.id == id) {
            Ok(Located::Stored)
        } else {
            Err(unknown_session(id, &format!("no such session in {}", directory.display())).into())
        }
    }

    /// Load the session `request` names from the session directory of its `cwd`, replaying
    /// the conversation to the client first.
    ///
    /// A session this process has open already is replayed as it is, without opening its
    /// file again.
    async fn load(&self, cx: &ConnectionTo<Client>, request: &LoadSessionRequest) -> Result<SessionId> {
        let id = &request.session_id;
        let session = match self.locate(id, &request.cwd).await? {
            Located::Open(session) => session,
            Located::Stored => self.open(id).await?,
        };
        let updates = Updates::new(cx.clone(), id.clone(), session.cwd().to_path_buf());
        session
            .with_transcript(|store| updates.replay(store))
            .with_context(|| format!("cannot replay session {}", id.0))?;
        Ok(id.clone())
    }

    /// Continue the session `request` names without replaying it (`session/resume`): the
    /// client keeps what it shows, and the conversation goes on from the file.
    async fn resume(&self, request: &ResumeSessionRequest) -> Result<SessionId> {
        if let Located::Stored = self.locate(&request.session_id, &request.cwd).await? {
            self.open(&request.session_id).await?;
        }
        Ok(request.session_id.clone())
    }

    /// The sessions kept that were started in `request.cwd`, or in the agent's own working
    /// directory, most recently active first. A listing is one page, so a cursor is never
    /// one of ours.
    async fn list(&self, request: &ListSessionsRequest) -> Result<Vec<SessionInfo>> {
        if let Some(cursor) = &request.cursor {
            return Err(Error::new(
                ErrorCode::INVALID_PARAMS,
                format!("unknown cursor {cursor:?}: a listing is one page"),
            )
            .into());
        }
        let cwd = request.cwd.as_deref().or(self.cwd.as_deref());
        let cwd = cli::resolve_cwd(cwd)?;
        let kept = self.kept(&cwd).await?;
        Ok(kept
            .iter()
            .flat_map(|(sessions, _)| sessions)
            .map(session_info)
            .collect())
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
                .agent_capabilities(agent_capabilities(!matches!(self.place, SessionPlace::Memory(_))))
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
        // Session setup holds the dispatch loop, like any request handler; it is a few
        // startup reads and, with MCP servers, nothing new: they are connected once.
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
    /// that is one of the agent's commands runs the command instead.
    fn prompt(
        &self,
        request: &PromptRequest,
        responder: Responder<PromptResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let Some(session) = self.session(&request.session_id) else {
            return responder.respond_with_error(unknown_session(&request.session_id.0, "no such session is open"));
        };
        let text = prompt_text(&request.prompt);
        let (connection, session_id) = (cx.clone(), request.session_id.clone());
        if let Some(command) = commands::parse(&text) {
            return cx.spawn(async move {
                let answer = commands::run(&session, command, &connection, &session_id).await;
                responder.respond_with_result(answer.map_err(protocol_error))
            });
        }
        cx.spawn(async move {
            let turn = Turn {
                updates: Updates::new(connection.clone(), session_id.clone(), session.cwd().to_path_buf()),
                session,
                cx: connection,
                id: session_id,
            };
            responder.respond_with_result(turn.answer(text).await.map_err(protocol_error))
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
        self.mode.agent.close_session(&request.session_id.0);
        responder.respond(CloseSessionResponse::new())
    }
}

/// One turn of a session: the conversation, and the client it reports to and asks.
struct Turn {
    session: Session,
    updates: Updates,
    cx: ConnectionTo<Client>,
    id: SessionId,
}

pub(crate) async fn run(args: AcpArgs) -> Result<u8> {
    with_telemetry(async move |telemetry| serve(args, telemetry.map(Telemetry::hook)).await).await
}

async fn serve(args: AcpArgs, traces: Option<TraceHook>) -> Result<u8> {
    let shared = cli::Shared::connect(args.base, args.budget, Builtin::Assistant)
        .await?
        .with_traces(traces);
    let place = if args.no_session {
        SessionPlace::memory()
    } else {
        SessionPlace::home()?
    };
    // The client answers: each approval request becomes a `session/request_permission`.
    let approvals = Approvals::Person("the client must approve shell commands");
    let mode = ModeAgent::build(&shared, &shared.settings.model, &place, approvals)?;
    let host = Arc::new(Host {
        cwd: shared.base.cwd,
        place,
        mode,
    });
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
        .on_receive_request(
            async move |request: NewSessionRequest, responder, cx| on_new.new_session(&request, responder, &cx).await,
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                on_load.load_session(&request, responder, &cx).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, cx| {
                on_resume.resume_session(&request, responder, &cx).await
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, _cx| on_list.list_sessions(&request, responder).await,
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
        .context("the ACP connection failed")?;
    Ok(EXIT_COMPLETED)
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
    /// The response to `session/prompt` once the turn that `text` starts has ended: its stop
    /// reason, or the run's error. While the turn runs, its events go to the client as
    /// session updates, and a call that waits for approval becomes a permission request.
    async fn answer(&self, text: String) -> Result<PromptResponse> {
        let mut run = self.session.send(text)?;
        while let Some(event) = run.next().await {
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
        stop_reason(&run.wait().await).map(PromptResponse::new)
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
        .title(session.title().map(session_files::preview_text))
        .updated_at(updated_at)
}

/// The text of a prompt: its text blocks, one per line. The agent advertises no image, audio
/// or embedded-context capability, so clients send nothing else.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            // The protocol may add block kinds: the enum is non-exhaustive.
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a finished run answers `session/prompt`. RunLimits that end a turn have a stop reason;
/// a failed or timed-out run is a request error, with the run's message.
fn stop_reason(report: &RunReport) -> Result<StopReason> {
    match report.outcome {
        RunOutcome::Completed => Ok(StopReason::EndTurn),
        // The protocol has no reason for a cost limit; tokens are what a cost is paid for.
        RunOutcome::Truncated | RunOutcome::TokenBudget | RunOutcome::CostBudget => Ok(StopReason::MaxTokens),
        RunOutcome::ModelRequestLimit => Ok(StopReason::MaxTurnRequests),
        RunOutcome::Cancelled => Ok(StopReason::Cancelled),
        RunOutcome::Failed | RunOutcome::TimedOut => Err(anyhow::Error::msg(
            report
                .error
                .clone()
                .unwrap_or_else(|| format!("the run ended with {:?}", report.outcome)),
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
    use agent_client_protocol::v1::schema::TextContent;
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
        assert_eq!(
            answer(&report(RunOutcome::TokenBudget, None)),
            Ok(StopReason::MaxTokens)
        );
        assert_eq!(
            answer(&report(RunOutcome::ModelRequestLimit, None)),
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
    fn a_prompt_is_the_text_of_its_text_blocks() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("one")),
            ContentBlock::Text(TextContent::new("two")),
        ];
        assert_eq!(prompt_text(&blocks), "one\ntwo");
        assert_eq!(prompt_text(&[]), "");
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
