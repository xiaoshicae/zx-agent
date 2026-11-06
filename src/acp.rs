//! The ACP agent: an editor or a terminal client drives sessions over stdio with the Agent
//! Client Protocol (JSON-RPC, one message per line). The client renders everything, so this
//! module only maps between the protocol and `AgentSession`: prompts become runs, run events
//! become session updates, approvals become permission requests.
//!
//! Sessions are kept as files, one per session in the directory chat uses for the same
//! working directory, so that a client can list them and load one again in a later process:
//! `session/load` replays the conversation to the client before it goes on, `session/resume`
//! goes on without the replay.
//!
//! Stdout is the protocol channel: nothing else in this process may print there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, CompactionId, CompactionStatus, CompactionUpdate, Content, ContentBlock,
    ContentChunk, Diff, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, Notice,
    NoticeSeverity, PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionId, SessionInfo,
    SessionListCapabilities, SessionNotification, SessionResumeCapabilities, SessionUpdate, StopReason, TextContent,
    ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use agent_client_protocol::{Agent, Client, Error, ErrorCode, Stdio};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use zx_agent::AgentSession;
use zx_agent::agent::{AgentEvent, ApprovalResult, Event, EventSink, RunOutcome, ToolVerdict};
use zx_agent::agent_session::Subscription;
use zx_agent::ai::{ContentBlock as MessageBlock, Delta, Message, UserMessageKind};
use zx_agent::interaction::{Approval, ApprovalRequest, Interaction};
use zx_agent::otel::Telemetry;
use zx_agent::profile::Builtin;
use zx_agent::runner::RunReport;
use zx_agent::session::{Entry, ProjectedMessage, SessionInfo as StoredSession, SessionStore};
use zx_agent::sync::lock;
use zx_agent::text::clip;
use zx_agent::tools::{BASH, EDIT_FILE, FIND, GREP, LS, READ_FILE, WRITE_FILE};

use crate::cli::{self, AcpArgs, SessionOptions};
use crate::process::EXIT_COMPLETED;
use crate::render::describe_call;
use crate::sessions::{self, SessionFile};

/// Characters of a tool call's title; the client shows the raw input in full.
const TITLE_CHARS: usize = 80;
/// The option IDs offered with every permission request, in the order they are shown.
const ALLOW_ONCE: &str = "allow-once";
const ALLOW_SESSION: &str = "allow-session";
const REJECT_ONCE: &str = "reject-once";

/// What every session shares: the arguments, the instructions and MCP connections, and the
/// trace export.
struct Settings {
    args: AcpArgs,
    shared: cli::Shared,
    traces: Option<Arc<dyn EventSink>>,
}

/// One ACP session: a conversation the client created with `session/new` or loaded with
/// `session/load`.
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

struct Host {
    settings: Settings,
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
        self.open(cx, cwd, file, false).await
    }

    /// The session `id` for `cwd`: one this process holds, or the file it is kept in.
    async fn locate(&self, id: &SessionId, cwd: &Path) -> Result<Located, Error> {
        if let Some(live) = self.session(id) {
            return Ok(Located::Live(live));
        }
        let id = &*id.0;
        let dir = self
            .session_dir(cwd)
            .map_err(|error| failure(format!("{error:#}")))?
            .ok_or_else(|| unknown_session(id, "the agent keeps no session files (--no-session)"))?;
        let list = sessions::list(dir.clone())
            .await
            .map_err(|error| failure(format!("{error:#}")))?;
        match list.sessions.iter().find(|session| session.id == id) {
            Some(session) => Ok(Located::File(session.path.clone())),
            None => Err(unknown_session(id, &format!("no such session in {}", dir.display()))),
        }
    }

    /// Load the session `request` names from the session directory of its `cwd`, replaying
    /// the conversation to the client first. A session this process holds already is
    /// replayed as it is, without opening its file again.
    async fn load(&self, cx: &ConnectionTo<Client>, request: &LoadSessionRequest) -> Result<SessionId, Error> {
        match self.locate(&request.session_id, &request.cwd).await? {
            Located::Live(live) => {
                let updates = Updates::new(cx.clone(), request.session_id.clone(), request.cwd.clone());
                live.session
                    .inspect(|store| updates.replay(store))
                    .map_err(|error| failure(format!("cannot replay session {}: {error}", request.session_id.0)))?;
                Ok(request.session_id.clone())
            }
            Located::File(path) => self
                .open(cx, &request.cwd, SessionFile::Open(&path), true)
                .await
                .map_err(|error| failure(format!("{error:#}"))),
        }
    }

    /// Continue the session `request` names without replaying it (`session/resume`): the
    /// client keeps what it shows, and the conversation goes on from the file.
    async fn resume(&self, cx: &ConnectionTo<Client>, request: &ResumeSessionRequest) -> Result<SessionId, Error> {
        match self.locate(&request.session_id, &request.cwd).await? {
            Located::Live(_) => Ok(request.session_id.clone()),
            Located::File(path) => self
                .open(cx, &request.cwd, SessionFile::Open(&path), false)
                .await
                .map_err(|error| failure(format!("{error:#}"))),
        }
    }

    /// The sessions kept for `request.cwd`, or in the directory of the agent's own working
    /// directory, most recently active first. A listing is one page, so a cursor is never
    /// one of ours.
    async fn list(&self, request: &ListSessionsRequest) -> Result<Vec<SessionInfo>, Error> {
        if let Some(cursor) = &request.cursor {
            return Err(Error::new(
                ErrorCode::INVALID_PARAMS,
                format!("unknown cursor {cursor:?}: a listing is one page"),
            ));
        }
        let cwd = request.cwd.as_deref().or(self.settings.args.base.cwd.as_deref());
        let cwd = cli::resolve_cwd(cwd).map_err(|error| failure(format!("{error:#}")))?;
        let Some(dir) = self.session_dir(&cwd).map_err(|error| failure(format!("{error:#}")))? else {
            return Ok(Vec::new());
        };
        let list = sessions::list(dir)
            .await
            .map_err(|error| failure(format!("{error:#}")))?;
        // With `--session-dir`, one directory holds every working directory's sessions.
        let cwd = cwd.display().to_string();
        Ok(list
            .sessions
            .iter()
            .filter(|session| request.cwd.is_none() || session.cwd == cwd)
            .map(session_info)
            .collect())
    }

    /// Prepare a session on `file` for `cwd`; with `replay`, the client first sees the
    /// conversation the file holds.
    async fn open(
        &self,
        cx: &ConnectionTo<Client>,
        cwd: &Path,
        file: SessionFile<'_>,
        replay: bool,
    ) -> Result<SessionId> {
        let args = &self.settings.args;
        let mut base = args.base.clone();
        base.cwd = Some(cwd.to_path_buf());
        let mut prepared = cli::prepare(SessionOptions {
            base: &base,
            transport: &args.transport,
            request: &args.request,
            budget: &args.budget,
            model: &args.base.model,
            file,
            default_profile: Builtin::Assistant,
            continue_on_length: None,
            shared: &self.settings.shared,
            hidden_env: &[],
        })
        .await?;
        let id = SessionId::new(prepared.store.header().id.as_str());
        prepared.require_bash_approval("the client must approve shell commands");
        prepared.config.interaction = Arc::new(Permissions {
            cx: cx.clone(),
            session_id: id.clone(),
        });
        prepared.config.interaction_timeout = Duration::from_secs(args.approval_timeout_secs);
        let cwd = prepared.config.spec.cwd.clone();
        let updates = Arc::new(Updates::new(cx.clone(), id.clone(), cwd));
        if replay {
            updates.replay(&prepared.store);
        }
        let session = prepared.into_session(updates)?;
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
}

pub(crate) async fn run(args: AcpArgs) -> Result<u8> {
    let telemetry = cli::start_telemetry()?;
    let traces = telemetry.as_ref().map(Telemetry::sink);
    let result = serve(args, traces).await;
    cli::finish_telemetry(telemetry).await;
    result
}

async fn serve(args: AcpArgs, traces: Option<Arc<dyn EventSink>>) -> Result<u8> {
    anyhow::ensure!(
        args.request.session.is_none(),
        "acp keeps one file per session: give --session-dir, not --session"
    );
    let shared = cli::Shared::connect(&args.base, Builtin::Assistant, &args.transport).await?;
    let capabilities = agent_capabilities(!args.no_session);
    let host = Arc::new(Host {
        settings: Settings { args, shared, traces },
        sessions: Mutex::new(HashMap::new()),
    });
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
            async move |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(capabilities.clone())
                        .agent_info(Implementation::new("zx-agent", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, cx| {
                // Session setup holds the dispatch loop, like any request handler; it is a
                // few startup reads and, with MCP servers, nothing new: they are connected once.
                match on_new.create(&cx, &request.cwd).await {
                    Ok(id) => responder.respond(NewSessionResponse::new(id)),
                    Err(error) => responder.respond_with_error(failure(format!("{error:#}"))),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                // The replay is sent before the response, as the protocol has it.
                match on_load.load(&cx, &request).await {
                    Ok(_) => responder.respond(LoadSessionResponse::new()),
                    Err(error) => responder.respond_with_error(error),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, cx| match on_resume.resume(&cx, &request).await {
                Ok(_) => responder.respond(ResumeSessionResponse::new()),
                Err(error) => responder.respond_with_error(error),
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, _cx| match on_list.list(&request).await {
                Ok(sessions) => responder.respond(ListSessionsResponse::new(sessions)),
                Err(error) => responder.respond_with_error(error),
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                let Some(live) = on_prompt.session(&request.session_id) else {
                    return responder.respond_with_error(Error::new(
                        ErrorCode::INVALID_PARAMS,
                        format!("unknown session {}", request.session_id.0),
                    ));
                };
                let text = prompt_text(&request.prompt);
                // The turn runs outside the dispatch loop, so cancellations and permission
                // answers keep arriving; the response is sent when the turn ends.
                cx.spawn(async move {
                    let response = match live.session.prompt(text).await {
                        Ok(report) => stop_reason(&report).map(PromptResponse::new),
                        Err(error) => Err(error.to_string()),
                    };
                    match response {
                        Ok(response) => responder.respond(response),
                        Err(message) => responder.respond_with_error(failure(message)),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _cx| {
                if let Some(live) = on_cancel.session(&notification.session_id) {
                    live.session.abort();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
        .context("the ACP connection failed")?;
    Ok(EXIT_COMPLETED)
}

/// A request error whose message says what went wrong: clients show the message, while the
/// SDK's own internal-error helper keeps the text in `data`.
fn failure(message: String) -> Error {
    Error::new(ErrorCode::INTERNAL_ERROR, message)
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
fn stop_reason(report: &RunReport) -> Result<StopReason, String> {
    match report.outcome {
        RunOutcome::Completed => Ok(StopReason::EndTurn),
        RunOutcome::Truncated | RunOutcome::TokenBudget => Ok(StopReason::MaxTokens),
        RunOutcome::ModelRequestLimit => Ok(StopReason::MaxTurnRequests),
        RunOutcome::Aborted => Ok(StopReason::Cancelled),
        RunOutcome::Error | RunOutcome::Timeout => Err(report
            .error
            .clone()
            .unwrap_or_else(|| format!("the run ended with {:?}", report.outcome))),
    }
}

/// Approvals go to the client as `session/request_permission`; the client answers with one
/// of the offered options, or `cancelled` once it cancelled the turn.
struct Permissions {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
}

#[async_trait]
impl Interaction for Permissions {
    async fn confirm(&self, request: &ApprovalRequest) -> Result<Approval, String> {
        let call = &request.call;
        let fields = ToolCallUpdateFields::new()
            .title(title(&call.name, &call.arguments))
            .kind(tool_kind(&call.name))
            .status(ToolCallStatus::Pending)
            .raw_input(call.arguments.clone());
        let options = vec![
            PermissionOption::new(ALLOW_ONCE, "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                ALLOW_SESSION,
                "Allow identical calls in this session",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new(REJECT_ONCE, "Reject", PermissionOptionKind::RejectOnce),
        ];
        let permission = RequestPermissionRequest::new(
            self.session_id.clone(),
            ToolCallUpdate::new(ToolCallId::new(call.id.as_str()), fields),
            options,
        );
        // This runs inside the turn, a spawned task, so waiting here does not hold the
        // dispatch loop that delivers the answer.
        let response = self
            .cx
            .send_request(permission)
            .block_task()
            .await
            .map_err(|error| format!("the permission request failed: {error}"))?;
        Ok(match response.outcome {
            RequestPermissionOutcome::Cancelled => Approval::Deny("the client cancelled the turn".into()),
            RequestPermissionOutcome::Selected(selected) => match &*selected.option_id.0 {
                ALLOW_ONCE => Approval::AllowOnce,
                ALLOW_SESSION => Approval::AllowSession,
                _ => Approval::Deny("rejected by the user".into()),
            },
            // The protocol may add outcomes: the enum is non-exhaustive, and an unknown one
            // never authorizes a call.
            _ => Approval::Deny("the client answered with an unknown outcome".into()),
        })
    }
}

/// Run events as `session/update` notifications.
struct Updates {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    cwd: PathBuf,
    state: Mutex<UpdateState>,
}

#[derive(Default)]
struct UpdateState {
    /// The answer in progress streamed text, so its end need not repeat it.
    wrote_text: bool,
    /// Arguments of the tool calls in progress, by call ID, for their results.
    calls: HashMap<String, Value>,
}

impl Updates {
    fn new(cx: ConnectionTo<Client>, session_id: SessionId, cwd: PathBuf) -> Self {
        Self {
            cx,
            session_id,
            cwd,
            state: Mutex::new(UpdateState::default()),
        }
    }

    fn send(&self, update: SessionUpdate) {
        if let Err(error) = self
            .cx
            .send_notification(SessionNotification::new(self.session_id.clone(), update))
        {
            // The connection is going away; the run still ends and the response says how.
            eprintln!("[acp] cannot send a session update: {error}");
        }
    }

    fn text_chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
    }

    /// Replay the conversation `store` holds to a client that loads the session: what it
    /// would have seen as the turns ran, without the streaming. The person's prompts, the
    /// answers with their thoughts, and the tool calls with their results; an extension's
    /// message when it asked to be shown. Summaries and the model's own continuations are
    /// the model's, not the person's.
    fn replay(&self, store: &SessionStore) {
        let projection = store.projection();
        for (index, item) in projection.iter().enumerate() {
            match &item.message {
                Message::User {
                    content,
                    kind: UserMessageKind::Prompt,
                    ..
                } => self.send(SessionUpdate::UserMessageChunk(Self::text_chunk(content))),
                Message::User {
                    content,
                    kind: UserMessageKind::Custom,
                    ..
                } => {
                    if let Some(Entry::CustomMessage(entry)) = store.get(&item.entry_id)
                        && entry.message.display
                    {
                        self.send(SessionUpdate::UserMessageChunk(Self::text_chunk(content)));
                    }
                }
                Message::User {
                    kind: UserMessageKind::ContinueAnswer | UserMessageKind::Summary,
                    ..
                }
                | Message::ToolResult(_)
                | Message::System(_) => {}
                Message::Assistant(message) => {
                    for block in &message.content {
                        match block {
                            MessageBlock::Text { text } if !text.is_empty() => {
                                self.send(SessionUpdate::AgentMessageChunk(Self::text_chunk(text)));
                            }
                            MessageBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                                self.send(SessionUpdate::AgentThoughtChunk(Self::text_chunk(thinking)));
                            }
                            MessageBlock::Text { .. } | MessageBlock::Thinking { .. } => {}
                            MessageBlock::ToolCall {
                                id, name, arguments, ..
                            } => {
                                let call = self.recorded_call(id, name, arguments, &projection[index + 1..]);
                                self.send(SessionUpdate::ToolCall(call));
                            }
                        }
                    }
                }
            }
        }
    }

    /// A recorded call as one update: with its result when a later message holds one, else
    /// still pending, which the next run settles.
    fn recorded_call(&self, id: &str, name: &str, arguments: &Value, later: &[ProjectedMessage]) -> ToolCall {
        let call = ToolCall::new(ToolCallId::new(id), title(name, arguments))
            .kind(tool_kind(name))
            .raw_input(arguments.clone())
            .locations(self.locations(arguments));
        let result = later.iter().find_map(|item| match &item.message {
            Message::ToolResult(result) if result.tool_call_id == id => Some(result),
            _ => None,
        });
        match result {
            Some(result) => {
                let status = if result.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                };
                let raw_output = result
                    .details
                    .clone()
                    .unwrap_or_else(|| Value::String(result.content.clone()));
                call.status(status)
                    .content(self.result_content(name, Some(arguments), &result.content, result.details.as_ref()))
                    .raw_output(raw_output)
            }
            None => call.status(ToolCallStatus::Pending),
        }
    }

    fn tool_update(&self, id: &str, fields: ToolCallUpdateFields) {
        self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(id),
            fields,
        )));
    }

    /// Where a call works, for clients that show locations: the `path` argument, made
    /// absolute against the session's directory.
    fn locations(&self, args: &Value) -> Vec<ToolCallLocation> {
        args.get("path")
            .and_then(Value::as_str)
            .map(|path| vec![ToolCallLocation::new(self.absolute(path))])
            .unwrap_or_default()
    }

    fn absolute(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// What a result shows: a write as a diff of the new file, an edit as its patch, anything
    /// else as the text the model received.
    fn result_content(
        &self,
        name: &str,
        args: Option<&Value>,
        content: &str,
        details: Option<&Value>,
    ) -> Vec<ToolCallContent> {
        let mut shown = Vec::new();
        if name == WRITE_FILE
            && let Some(args) = args
            && let (Some(path), Some(text)) = (
                args.get("path").and_then(Value::as_str),
                args.get("content").and_then(Value::as_str),
            )
        {
            shown.push(ToolCallContent::Diff(Diff::new(self.absolute(path), text)));
        }
        let patch = details.and_then(|details| details.get("patch")).and_then(Value::as_str);
        let text = match patch {
            Some(patch) if name == EDIT_FILE => patch,
            Some(_) | None => content,
        };
        if !text.is_empty() {
            shown.push(ToolCallContent::Content(Content::new(ContentBlock::Text(
                TextContent::new(text),
            ))));
        }
        shown
    }
}

impl EventSink for Updates {
    fn emit(&self, event: &Event) {
        match &event.event {
            AgentEvent::MessageStart { role: "assistant" } => lock(&self.state).wrote_text = false,
            AgentEvent::MessageUpdate {
                delta: Delta::Text { text },
            } => {
                lock(&self.state).wrote_text = true;
                self.send(SessionUpdate::AgentMessageChunk(Self::text_chunk(text)));
            }
            AgentEvent::MessageUpdate {
                delta: Delta::Thinking { text },
            } => self.send(SessionUpdate::AgentThoughtChunk(Self::text_chunk(text))),
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
                ..
            } => {
                let text = message.text();
                let wrote_text = {
                    let mut state = lock(&self.state);
                    for block in &message.content {
                        if let MessageBlock::ToolCall { id, arguments, .. } = block {
                            state.calls.insert(id.clone(), arguments.clone());
                        }
                    }
                    state.wrote_text
                };
                if !wrote_text && !text.is_empty() {
                    self.send(SessionUpdate::AgentMessageChunk(Self::text_chunk(&text)));
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                lock(&self.state).calls.insert(tool_call_id.clone(), args.clone());
                self.send(SessionUpdate::ToolCall(
                    ToolCall::new(ToolCallId::new(tool_call_id.as_str()), title(tool_name, args))
                        .kind(tool_kind(tool_name))
                        .status(ToolCallStatus::Pending)
                        .raw_input(args.clone())
                        .locations(self.locations(args)),
                ));
            }
            // A call the policy lets through, or one the client approved, is running now.
            AgentEvent::ToolDecision {
                tool_call_id,
                verdict: ToolVerdict::Allow,
                ..
            }
            | AgentEvent::ApprovalResolved {
                tool_call_id,
                approval: ApprovalResult::AllowOnce | ApprovalResult::AllowSession | ApprovalResult::Remembered,
                ..
            } => self.tool_update(
                tool_call_id,
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            ),
            AgentEvent::MessageEnd {
                message: Message::ToolResult(result),
                ..
            } => {
                let args = lock(&self.state).calls.remove(&result.tool_call_id);
                let status = if result.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                };
                let content = self.result_content(
                    &result.tool_name,
                    args.as_ref(),
                    &result.content,
                    result.details.as_ref(),
                );
                let raw_output = result
                    .details
                    .clone()
                    .unwrap_or_else(|| Value::String(result.content.clone()));
                self.tool_update(
                    &result.tool_call_id,
                    ToolCallUpdateFields::new()
                        .status(status)
                        .content(content)
                        .raw_output(raw_output),
                );
            }
            AgentEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => self.send(notice(
                NoticeSeverity::Warning,
                format!("Retrying the model request ({attempt}/{max_retries}) in {delay_ms}ms"),
                error,
            )),
            AgentEvent::ContextOverflow { error } => self.send(notice(
                NoticeSeverity::Warning,
                "The context is too long for the model; summarizing before another attempt",
                error,
            )),
            AgentEvent::HookFailed { hook, phase, error } => {
                self.send(notice(
                    NoticeSeverity::Error,
                    format!("Hook {hook} failed in {phase}"),
                    error,
                ));
            }
            AgentEvent::CompactionStart { .. } => {
                self.send(compaction_update(&event.run_id, CompactionStatus::InProgress));
            }
            AgentEvent::CompactionEnd { .. } => {
                self.send(compaction_update(&event.run_id, CompactionStatus::Completed));
            }
            AgentEvent::CompactionFailed { error, .. } => self.send(SessionUpdate::CompactionUpdate(
                CompactionUpdate::new(CompactionId::new(event.run_id.as_str()), CompactionStatus::Failed)
                    .error(error.clone()),
            )),
            AgentEvent::RunStart { .. }
            | AgentEvent::RunEnd { .. }
            | AgentEvent::AgentStart { .. }
            | AgentEvent::TurnStart { .. }
            | AgentEvent::RequestStart { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::ToolExecutionUpdate { .. }
            | AgentEvent::ToolDecision { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::ToolExecutionEnd { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::AgentEnd { .. }
            | AgentEvent::ContextEdited { .. }
            | AgentEvent::CustomEntry { .. }
            | AgentEvent::ToolCallRecovered { .. } => {}
        }
    }
}

/// A notice to the client, described by the error or reason behind it.
fn notice(severity: NoticeSeverity, title: impl Into<String>, description: &str) -> SessionUpdate {
    SessionUpdate::Notice(Notice::new(severity, title).description(description.to_string()))
}

/// The state of the summary that runs as `run_id`.
fn compaction_update(run_id: &str, status: CompactionStatus) -> SessionUpdate {
    SessionUpdate::CompactionUpdate(CompactionUpdate::new(CompactionId::new(run_id), status))
}

/// A short title for a call, as the client lists it; the raw input carries everything.
fn title(name: &str, args: &Value) -> String {
    clip(&describe_call(name, args), TITLE_CHARS)
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
    use serde_json::json;
    use zx_agent::ai::Usage;

    use super::*;

    fn report(outcome: RunOutcome, error: Option<&str>) -> RunReport {
        RunReport {
            outcome,
            final_text: String::new(),
            model_requests: 1,
            retries: 0,
            usage: Usage::default(),
            error: error.map(str::to_string),
            session_file: None,
        }
    }

    #[test]
    fn outcomes_map_to_stop_reasons_and_failures_to_request_errors() {
        assert_eq!(
            stop_reason(&report(RunOutcome::Completed, None)),
            Ok(StopReason::EndTurn)
        );
        assert_eq!(
            stop_reason(&report(RunOutcome::Truncated, None)),
            Ok(StopReason::MaxTokens)
        );
        assert_eq!(
            stop_reason(&report(RunOutcome::TokenBudget, None)),
            Ok(StopReason::MaxTokens)
        );
        assert_eq!(
            stop_reason(&report(RunOutcome::ModelRequestLimit, None)),
            Ok(StopReason::MaxTurnRequests)
        );
        assert_eq!(
            stop_reason(&report(RunOutcome::Aborted, None)),
            Ok(StopReason::Cancelled)
        );
        assert_eq!(
            stop_reason(&report(RunOutcome::Error, Some("boom"))),
            Err("boom".to_string())
        );
        assert_eq!(
            stop_reason(&report(RunOutcome::Timeout, None)),
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
        assert_eq!(title("bash", &json!({"command": "ls -la"})), "bash: ls -la");
    }
}
