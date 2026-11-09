//! The ACP agent: an editor or a terminal client drives sessions over stdio with the Agent
//! Client Protocol (JSON-RPC, one message per line). The client renders everything, so this
//! module only maps between the protocol and `AgentSession`: prompts become runs, run events
//! become session updates (`updates`), approvals become permission requests (`permissions`).
//!
//! Sessions are kept as files, one per session in the directory chat uses for the same
//! working directory, so that a client can list them and load one again in a later process:
//! `session/load` replays the conversation to the client before it goes on, `session/resume`
//! goes on without the replay.
//!
//! Stdout is the protocol channel: nothing else in this process may print there.

mod permissions;
mod updates;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
    SessionCapabilities, SessionId, SessionInfo, SessionListCapabilities, SessionResumeCapabilities, StopReason,
    ToolKind,
};
use agent_client_protocol::{Agent, Client, Error, ErrorCode, Responder, Stdio, V1};
use anyhow::{Context, Result};

use zx_agent::AgentSession;
use zx_agent::agent::{EventSink, RunOutcome};
use zx_agent::agent_session::Subscription;
use zx_agent::otel::Telemetry;
use zx_agent::profile::Builtin;
use zx_agent::runner::RunReport;
use zx_agent::session::SessionInfo as StoredSession;
use zx_agent::sync::lock;
use zx_agent::tools::{BASH, EDIT_FILE, FIND, GREP, LS, READ_FILE, WRITE_FILE};

use crate::cli::{self, AcpArgs, ModeSettings, PreparedSession, SessionOptions};
use crate::process::{EXIT_COMPLETED, with_telemetry};
use crate::sessions::{self, SessionFile};

use permissions::Permissions;
use updates::Updates;

/// One ACP session: a conversation the client created with `session/new`, loaded with
/// `session/load` or resumed with `session/resume`.
struct Live {
    session: AgentSession,
    /// The trace export of this session, while it lasts.
    _traced: Option<Subscription>,
}

/// Where a session the client names is.
enum Located {
    Live(Arc<Live>),
    File(PathBuf),
}

/// A session prepared on its file, with its updates, before it runs: `session/load` replays
/// the file to the client in between.
struct Prepared {
    session: PreparedSession,
    updates: Arc<Updates>,
    id: SessionId,
}

struct Host {
    settings: ModeSettings<AcpArgs>,
    /// Sessions by ID. Handlers run one at a time on the dispatch loop but prompts run in
    /// spawned tasks, so the map is shared; locks are short and never held across an await.
    sessions: Mutex<HashMap<String, Arc<Live>>>,
}

impl Host {
    fn session(&self, id: &SessionId) -> Option<Arc<Live>> {
        lock(&self.sessions).get(&*id.0).cloned()
    }

    /// Where the sessions of `cwd` are kept: `--session-dir`, or chat's default directory
    /// for that working directory; `None` with `--no-session`.
    fn session_dir(&self, cwd: &Path) -> Result<Option<PathBuf>> {
        let args = &self.settings.args;
        if args.no_session {
            return Ok(None);
        }
        match &args.session_dir {
            Some(dir) => Ok(Some(dir.clone())),
            None => sessions::default_dir(&cli::resolve_cwd(Some(cwd))?).map(Some),
        }
    }

    /// Create a session for `cwd`, the client's project directory, which overrides `--cwd`.
    /// Its file goes to the session directory, where `session/load` finds it again.
    async fn create(&self, cx: &ConnectionTo<Client>, cwd: &Path) -> Result<SessionId> {
        let dir = self.session_dir(cwd)?;
        let file = match &dir {
            Some(dir) => SessionFile::NewIn(dir),
            None => SessionFile::Memory,
        };
        let prepared = self.prepare_session(cx, cwd, file).await?;
        self.start(prepared)
    }

    /// The session `id` for `cwd`: one this process holds, or the file it is kept in.
    async fn locate(&self, id: &SessionId, cwd: &Path) -> Result<Located> {
        if let Some(live) = self.session(id) {
            return Ok(Located::Live(live));
        }
        let id = &*id.0;
        let dir = self
            .session_dir(cwd)?
            .ok_or_else(|| unknown_session(id, "the agent keeps no session files (--no-session)"))?;
        let list = sessions::list(&dir).await?;
        match list.sessions.iter().find(|session| session.id == id) {
            Some(session) => Ok(Located::File(session.path.clone())),
            None => Err(unknown_session(id, &format!("no such session in {}", dir.display())).into()),
        }
    }

    /// Load the session `request` names from the session directory of its `cwd`, replaying
    /// the conversation to the client first.
    ///
    /// A session this process holds already is replayed as it is, without opening its file
    /// again.
    async fn load(&self, cx: &ConnectionTo<Client>, request: &LoadSessionRequest) -> Result<SessionId> {
        match self.locate(&request.session_id, &request.cwd).await? {
            Located::Live(live) => {
                let updates = Updates::new(cx.clone(), request.session_id.clone(), request.cwd.clone());
                live.session
                    .inspect(|store| updates.replay(store))
                    .with_context(|| format!("cannot replay session {}", request.session_id.0))?;
                Ok(request.session_id.clone())
            }
            Located::File(path) => {
                let prepared = self.prepare_session(cx, &request.cwd, SessionFile::Open(&path)).await?;
                prepared.updates.replay(&prepared.session.store);
                self.start(prepared)
            }
        }
    }

    /// Continue the session `request` names without replaying it (`session/resume`): the
    /// client keeps what it shows, and the conversation goes on from the file.
    async fn resume(&self, cx: &ConnectionTo<Client>, request: &ResumeSessionRequest) -> Result<SessionId> {
        match self.locate(&request.session_id, &request.cwd).await? {
            Located::Live(_) => Ok(request.session_id.clone()),
            Located::File(path) => {
                let prepared = self.prepare_session(cx, &request.cwd, SessionFile::Open(&path)).await?;
                self.start(prepared)
            }
        }
    }

    /// The sessions kept for `request.cwd`, or in the directory of the agent's own working
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
        let cwd = request.cwd.as_deref().or(self.settings.args.base.cwd.as_deref());
        let cwd = cli::resolve_cwd(cwd)?;
        let Some(dir) = self.session_dir(&cwd)? else {
            return Ok(Vec::new());
        };
        let list = sessions::list(&dir).await?;
        // With `--session-dir`, one directory holds every working directory's sessions.
        let cwd = cwd.display().to_string();
        Ok(list
            .sessions
            .iter()
            .filter(|session| request.cwd.is_none() || session.cwd == cwd)
            .map(session_info)
            .collect())
    }

    /// Prepare a session on `file` for `cwd`, with the client as its approver and the
    /// receiver of its updates.
    async fn prepare_session(&self, cx: &ConnectionTo<Client>, cwd: &Path, file: SessionFile<'_>) -> Result<Prepared> {
        let args = &self.settings.args;
        let mut session = cli::prepare(SessionOptions {
            base: &args.base,
            transport: &args.transport,
            request: &args.request,
            budget: &args.budget,
            model: &args.base.model,
            cwd: Some(cwd),
            file,
            default_profile: Builtin::Assistant,
            shared: &self.settings.shared,
        })
        .await?;
        let id = SessionId::new(session.store.header().id.as_str());
        session.require_bash_approval("the client must approve shell commands");
        session.config.interaction = Arc::new(Permissions {
            cx: cx.clone(),
            session_id: id.clone(),
        });
        session.config.interaction_timeout = Duration::from_secs(args.approval_timeout_secs);
        let cwd = session.config.spec.cwd.clone();
        let updates = Arc::new(Updates::new(cx.clone(), id.clone(), cwd));
        Ok(Prepared { session, updates, id })
    }

    /// Start the prepared session and keep it by its ID.
    fn start(&self, prepared: Prepared) -> Result<SessionId> {
        let Prepared { session, updates, id } = prepared;
        let session = session.into_session(updates)?;
        let traced = self.settings.traces.clone().map(|sink| session.subscribe(sink));
        lock(&self.sessions).insert(
            id.0.to_string(),
            Arc::new(Live {
                session,
                _traced: traced,
            }),
        );
        Ok(id)
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
                .agent_capabilities(agent_capabilities(!self.settings.args.no_session))
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
        let result = self.create(cx, &request.cwd).await;
        responder.respond_with_result(result.map(NewSessionResponse::new).map_err(protocol_error))
    }

    /// `session/load`: the replay is sent before the response, as the protocol has it.
    async fn load_session(
        &self,
        request: &LoadSessionRequest,
        responder: Responder<LoadSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = self.load(cx, request).await;
        responder.respond_with_result(result.map(|_| LoadSessionResponse::new()).map_err(protocol_error))
    }

    /// `session/resume`: the session goes on from its file, with nothing replayed.
    async fn resume_session(
        &self,
        request: &ResumeSessionRequest,
        responder: Responder<ResumeSessionResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let result = self.resume(cx, request).await;
        responder.respond_with_result(result.map(|_| ResumeSessionResponse::new()).map_err(protocol_error))
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
    /// permission answers keep arriving; the response is sent when the turn ends.
    fn prompt(
        &self,
        request: &PromptRequest,
        responder: Responder<PromptResponse, V1>,
        cx: &ConnectionTo<Client>,
    ) -> Result<(), Error> {
        let Some(live) = self.session(&request.session_id) else {
            return responder.respond_with_error(unknown_session(&request.session_id.0, "no such session is open"));
        };
        let text = prompt_text(&request.prompt);
        cx.spawn(async move { responder.respond_with_result(answer_prompt(&live, text).await.map_err(protocol_error)) })
    }

    /// `session/cancel`: abort the session's turn, if it has one.
    fn cancel(&self, notification: &CancelNotification) {
        if let Some(live) = self.session(&notification.session_id) {
            live.session.abort();
        }
    }
}

pub(crate) async fn run(args: AcpArgs) -> Result<u8> {
    with_telemetry(async move |telemetry| serve(args, telemetry.map(Telemetry::sink)).await).await
}

async fn serve(args: AcpArgs, traces: Option<Arc<dyn EventSink>>) -> Result<u8> {
    anyhow::ensure!(
        args.request.session.is_none(),
        "acp keeps one file per session: give --session-dir, not --session"
    );
    let shared = cli::Shared::connect(&args.base, Builtin::Assistant, &args.transport).await?;
    let host = Arc::new(Host {
        settings: ModeSettings { args, shared, traces },
        sessions: Mutex::new(HashMap::new()),
    });
    let on_initialize = Arc::clone(&host);
    let on_new = Arc::clone(&host);
    let on_load = Arc::clone(&host);
    let on_resume = Arc::clone(&host);
    let on_list = Arc::clone(&host);
    let on_prompt = Arc::clone(&host);
    let on_cancel = Arc::clone(&host);
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

/// The response to `session/prompt` once the turn ended: its stop reason, or the run's error.
async fn answer_prompt(live: &Live, text: String) -> Result<PromptResponse> {
    let report = live.session.prompt(text).await?;
    stop_reason(&report).map(PromptResponse::new)
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

/// What the agent offers: loading, listing and resuming sessions when it keeps their files.
fn agent_capabilities(kept: bool) -> AgentCapabilities {
    let capabilities = AgentCapabilities::new().load_session(kept);
    if !kept {
        return capabilities;
    }
    capabilities.session_capabilities(
        SessionCapabilities::new()
            .list(SessionListCapabilities::new())
            .resume(SessionResumeCapabilities::new()),
    )
}

/// A listed session as the client shows it: its first prompt as the title, its last
/// activity as the time.
fn session_info(session: &StoredSession) -> SessionInfo {
    // A time past the year 292 million is not one a session file records; the entry then
    // has no time rather than a wrong one.
    let updated_at = i64::try_from(session.last_activity_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    SessionInfo::new(SessionId::new(session.id.as_str()), PathBuf::from(&session.cwd))
        .title(session.first_prompt.as_deref().map(sessions::preview_text))
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

/// How a finished run answers `session/prompt`. Limits that end a turn have a stop reason;
/// a failed or timed-out run is a request error, with the run's message.
fn stop_reason(report: &RunReport) -> Result<StopReason> {
    match report.outcome {
        RunOutcome::Completed => Ok(StopReason::EndTurn),
        // The protocol has no reason for a cost limit; tokens are what a cost is paid for.
        RunOutcome::Truncated | RunOutcome::TokenBudget | RunOutcome::CostBudget => Ok(StopReason::MaxTokens),
        RunOutcome::ModelRequestLimit => Ok(StopReason::MaxTurnRequests),
        RunOutcome::Aborted => Ok(StopReason::Cancelled),
        RunOutcome::Error | RunOutcome::Timeout => Err(anyhow::Error::msg(
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
            session_file: None,
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
        assert_eq!(answer(&report(RunOutcome::Aborted, None)), Ok(StopReason::Cancelled));
        assert_eq!(
            answer(&report(RunOutcome::Error, Some("boom"))),
            Err("boom".to_string())
        );
        assert_eq!(
            answer(&report(RunOutcome::Timeout, None)),
            Err("the run ended with Timeout".to_string())
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
