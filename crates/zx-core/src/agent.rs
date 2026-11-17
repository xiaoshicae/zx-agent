//! The agent: the role every session plays, and the sessions that are open.
//!
//! An [`Agent`] is the one object a host makes. It keeps what every session shares
//! ([`AgentConfig`]), asks the host's [`SessionSetup`] for what belongs to one session, and
//! creates, opens, finds and closes the sessions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::ai::retry::RetryPolicy;
use crate::ai::{ModelMeta, Provider, Reasoning, SystemPrompt};
use crate::compaction::{self, Summarizer};
use crate::extensions::{Expanded, Extension};
use crate::hooks::HookChain;
use crate::policy::{ApprovalMode, PolicyChain};
use crate::runner::LoopConfig;
pub use crate::runner::RunLimits;
use crate::session::{Session, SessionOptions};
use crate::sync::lock;
use crate::tools::{Tool, Toolbox};
use crate::transcript::{MemoryStorage, SessionHeader, SessionList, SessionQuery, SessionStorage, Transcript};

/// The role an agent plays in every session: the model, the tools and what surrounds them,
/// the bounds of a run, and where sessions are kept.
///
/// [`AgentConfig::new`] takes the provider and the model and leaves the rest at its default;
/// set other fields with struct update syntax (`AgentConfig { tools,
/// ..AgentConfig::new(provider, model) }`) or afterwards.
pub struct AgentConfig {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    /// Changes are recorded as system messages before the next request. Empty by default.
    pub system_prompt: SystemPrompt,
    /// The tools every session offers. No tool by default.
    pub tools: Vec<Arc<dyn Tool>>,
    pub max_tokens: Option<u32>,
    /// Sent with every task request; `None` leaves the server's default. Summary requests
    /// send none.
    pub reasoning: Option<Reasoning>,
    /// The hooks of every session; the sessions share them.
    pub hooks: HookChain,
    /// The policies that decide whether a tool call may run; none by default, which allows
    /// every call.
    pub policies: PolicyChain,
    /// Who answers when a policy asks about a call; nobody by default, which denies it.
    pub approvals: ApprovalMode,
    /// What they add is taken from them once, when the agent is made; every session has
    /// it, after the session's own parts.
    pub extensions: Vec<Arc<dyn Extension>>,
    /// What is known about models, by ID: the prices give each response a cost, the context
    /// window can set the token trigger of automatic compaction. Empty by default: costs and
    /// windows stay unknown.
    pub models: HashMap<String, ModelMeta>,
    /// Whether every task request carries its session's ID as the cache key
    /// ([`crate::ai::Request::cache_key`]), so that a provider keeps one cache for each
    /// conversation. Off by default.
    pub session_cache_key: bool,
    /// The bounds of each run; none by default.
    pub limits: RunLimits,
    /// How often and how patiently a failed response is tried again.
    pub retry: RetryPolicy,
    /// When and how the old part of a conversation is summarized.
    pub compaction: compaction::Config,
    /// Makes the summaries. None by default: the kernel asks the model, and the request can
    /// be rebuilt from the session.
    pub summarizer: Option<Arc<dyn Summarizer>>,
    /// Where sessions are kept; in memory by default, which forgets them with the process.
    pub storage: Arc<dyn SessionStorage>,
    /// Makes what belongs to one session. None by default: every session then has what the
    /// fields above give it, and nothing of its own.
    pub session_setup: Option<Arc<dyn SessionSetup>>,
}

impl AgentConfig {
    /// An agent that asks `model` of `provider`, with no system prompt, no tool, no hook and
    /// no human to approve calls, whose sessions are kept in memory.
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            system_prompt: SystemPrompt::default(),
            tools: Vec::new(),
            max_tokens: None,
            reasoning: None,
            hooks: HookChain::new(),
            policies: PolicyChain::new(),
            approvals: ApprovalMode::Deny,
            extensions: Vec::new(),
            models: HashMap::new(),
            session_cache_key: false,
            limits: RunLimits::default(),
            retry: RetryPolicy::default(),
            compaction: compaction::Config::default(),
            summarizer: None,
            storage: Arc::new(MemoryStorage::default()),
            session_setup: None,
        }
    }
}

/// Makes what belongs to one session rather than to the agent: tools bound to the session's
/// working directory or to something started for it, such as a container, the hooks that go
/// with them, and the prompt sections that describe them.
///
/// The agent asks once for each session it creates or opens, and the session keeps what it
/// gets until it is closed or dropped. A host whose sessions differ in nothing needs no setup:
/// it puts its tools into [`AgentConfig::tools`].
#[async_trait]
pub trait SessionSetup: Send + Sync {
    /// The parts of the session with `header`: its ID, its labels, and `cwd`, the directory
    /// it works in. An error refuses the session, and a new one is then not created in the
    /// storage. The agent waits as long as this takes; dropping the call that creates or
    /// opens the session drops this one.
    async fn setup(&self, header: &SessionHeader) -> Result<SessionParts>;

    /// The names of the prompt sections that [`setup`](Self::setup) gives, which no extension
    /// may take: a session leaves a blank section out, and an extension that took its name
    /// there would clash with it where it is not blank. None by default.
    fn section_names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// What belongs to one session, added to what the agent gives every session.
#[derive(Default)]
pub struct SessionParts {
    /// Offered after the agent's tools. Their names must differ from every other tool's.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Run after the agent's hooks of the same stage.
    pub hooks: HookChain,
    /// `(name, content)` sections placed after the agent's and before the extensions'. One
    /// named like a section the agent's prompt has replaces it in place; blank content is
    /// left out.
    pub prompt_sections: Vec<(String, String)>,
}

/// The entry to the kernel: the role every session plays, and the sessions that are open.
/// A handle: its clones are the same agent.
///
/// A session works in the directory it was created in, which its header records, also when
/// it is opened again later. It stays open, with its place in the storage and everything set
/// up for it, until [`close_session`](Self::close_session) or until the agent and every
/// handle of the session are dropped.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use zx_ai::Provider;
/// use zx_core::{Agent, AgentConfig, SessionOptions};
///
/// # async fn example(provider: Arc<dyn Provider>) -> anyhow::Result<()> {
/// let agent = Agent::new(AgentConfig::new(provider, "model-id"))?;
/// let session = agent.create_session(SessionOptions::new(".")).await?;
/// let mut run = session.send("Inspect the logs.")?;
/// while let Some(event) = run.next().await {
///     // Show the event; answer an `approval_requested` with `session.resolve_approval`.
/// }
/// let report = run.wait().await;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Agent {
    shared: Arc<Shared>,
}

struct Shared {
    config: AgentConfig,
    /// What the extensions add to every session.
    extensions: Expanded,
    /// The sessions that are open, by ID. A cell is empty while its session is being opened,
    /// so that calls that open the same session at the same time get the same one.
    sessions: Mutex<HashMap<String, Opening>>,
}

/// The place of a session in the table: empty until the session is open.
type Opening = Arc<OnceCell<Session>>;

/// Take the place of the session `id` out of the table when that session is closed, as one
/// is that closed itself because its record could no longer be written. What comes back is
/// dropped by the caller without the table's lock: it holds host objects.
fn forget_closed(sessions: &mut HashMap<String, Opening>, id: &str) -> Option<Opening> {
    let closed = sessions.get(id)?.get().is_some_and(Session::is_closed);
    if closed { sessions.remove(id) } else { None }
}

/// Takes a session's place out of the table again when the call that asked for it ends
/// while the place is empty and no other call waits for it. A call that still waits tries to
/// open the session itself, and takes the place out when it is the last.
struct ForgetUnopened<'a> {
    sessions: &'a Mutex<HashMap<String, Opening>>,
    id: &'a str,
    opening: &'a Opening,
}

impl Drop for ForgetUnopened<'_> {
    fn drop(&mut self) {
        let mut sessions = lock(self.sessions);
        // The table's share and this call's: a call gets a place only under the table's lock,
        // so nobody else has this one, and nobody can fill it now.
        let unused = !self.opening.initialized() && Arc::strong_count(self.opening) == 2;
        if unused
            && sessions
                .get(self.id)
                .is_some_and(|place| Arc::ptr_eq(place, self.opening))
        {
            sessions.remove(self.id);
        }
    }
}

impl Agent {
    /// An agent that plays the role `config` describes. Fails for what no session could run
    /// with: tools that share a name, declare another name or an invalid parameter schema,
    /// extensions that [`Extension`] refuses, compaction budgets that cannot work, and a
    /// summarizer without a name.
    pub fn new(config: AgentConfig) -> Result<Self> {
        config.compaction.validate()?;
        if let Some(summarizer) = &config.summarizer {
            anyhow::ensure!(!summarizer.name().trim().is_empty(), "the summarizer has no name");
        }
        let reserved = config
            .session_setup
            .as_ref()
            .map(|setup| setup.section_names())
            .unwrap_or_default();
        let extensions = Expanded::of(&config.extensions, &reserved).map_err(anyhow::Error::msg)?;
        let agent = Self {
            shared: Arc::new(Shared {
                config,
                extensions,
                sessions: Mutex::default(),
            }),
        };
        // What every session shares is assembled once and dropped, so that it is refused
        // here rather than with the first session.
        let probe = SessionHeader::new(Path::new("."), &agent.shared.config.model);
        agent.assemble(&probe, SessionParts::default())?;
        Ok(agent)
    }

    /// Create a session and open it. The setup comes first, so a session whose setup fails
    /// leaves nothing in the storage. Fails when the working directory is empty, or relative
    /// and the process's current directory cannot be read.
    pub async fn create_session(&self, options: SessionOptions) -> Result<Session> {
        let config = &self.shared.config;
        // Recorded absolute: the session works there whenever it is opened again, from
        // whatever directory.
        let cwd = std::path::absolute(&options.cwd)
            .with_context(|| format!("cannot make the working directory {} absolute", options.cwd.display()))?;
        let header = SessionHeader {
            labels: options.labels,
            ..SessionHeader::new(&cwd, &config.model)
        };
        let parts = self.set_up(&header).await?;
        let assembled = self.assemble(&header, parts)?;
        let log = config
            .storage
            .create(&header)
            .await
            .context("cannot create the session")?;
        self.register(self.start(assembled, Transcript::with_log(header, log))?)
    }

    /// The session `id`: the open one when it is open, else the one in the storage, opened
    /// and set up again; a session that closed itself counts as not open. Fails when the
    /// storage has no such session or another writer holds it, and when its setup fails.
    pub async fn open_session(&self, id: &str) -> Result<Session> {
        let (opening, closed) = {
            let mut sessions = lock(&self.shared.sessions);
            let closed = forget_closed(&mut sessions, id);
            (sessions.entry(id.to_string()).or_default().clone(), closed)
        };
        drop(closed);
        // Whether opening fails or this call is dropped while it waits.
        let _forget = ForgetUnopened {
            sessions: &self.shared.sessions,
            id,
            opening: &opening,
        };
        opening.get_or_try_init(|| self.open(id)).await.cloned()
    }

    /// The session `id` when it is open. Reads nothing: a session that is only in the
    /// storage is not found.
    pub fn session(&self, id: &str) -> Option<Session> {
        let session = lock(&self.shared.sessions).get(id)?.get().cloned()?;
        // One that closed itself is in the table until it is asked for again.
        (!session.is_closed()).then_some(session)
    }

    /// Close the session `id`: cancel its run, if it has one, and let go of its place in the
    /// storage and of everything set up for it, at once when idle, else when the cancelled
    /// run has ended. Handles of the session then return
    /// [`SessionError::Closed`](crate::session::SessionError::Closed); the session stays in
    /// the storage, to be opened again. Returns false when no such session is open.
    pub fn close_session(&self, id: &str) -> bool {
        let closed = {
            let mut sessions = lock(&self.shared.sessions);
            // A cell that is still empty belongs to the call that is opening the session.
            if !sessions.get(id).is_some_and(|opening| opening.initialized()) {
                return false;
            }
            sessions.remove(id)
        };
        // Without the table's lock: closing lets go of host objects.
        closed.as_deref().and_then(OnceCell::get).is_some_and(Session::close)
    }

    /// The headers of the sessions in the storage that `query` asks for, the most recently
    /// started first, read without their records: what a host that keeps an index of its
    /// sessions starts from. Opening one checks its records.
    pub async fn session_headers(&self, query: &SessionQuery) -> Result<Vec<SessionHeader>> {
        self.shared.config.storage.headers(query).await
    }

    /// The sessions in the storage that `query` asks for, most recently active first.
    pub async fn list_sessions(&self, query: &SessionQuery) -> Result<SessionList> {
        self.shared.config.storage.list(query).await
    }

    /// Open the session `id` from the storage and set it up.
    async fn open(&self, id: &str) -> Result<Session> {
        let (loaded, log) = self.shared.config.storage.open(id).await?;
        let transcript = tokio::task::spawn_blocking(move || Transcript::load(loaded, log))
            .await
            .context("session replay stopped")?
            .with_context(|| format!("cannot load session {id}"))?;
        let parts = self.set_up(transcript.header()).await?;
        let assembled = self.assemble(transcript.header(), parts)?;
        self.start(assembled, transcript)
    }

    /// What the host's setup makes for the session with `header`.
    async fn set_up(&self, header: &SessionHeader) -> Result<SessionParts> {
        match &self.shared.config.session_setup {
            Some(setup) => setup.setup(header).await.context("cannot set up the session"),
            None => Ok(SessionParts::default()),
        }
    }

    /// What the loops of the session with `header` run with, in the directory the header
    /// names: the agent's role, then the session's own `parts`, then the extensions.
    fn assemble(&self, header: &SessionHeader, parts: SessionParts) -> Result<LoopConfig> {
        let (config, extensions) = (&self.shared.config, &self.shared.extensions);
        let tools: Vec<Arc<dyn Tool>> = config
            .tools
            .iter()
            .cloned()
            .chain(parts.tools)
            .chain(extensions.tools.iter().cloned())
            .collect();
        let mut hooks = config.hooks.clone();
        hooks.extend(parts.hooks);
        hooks.extend(extensions.hooks.clone());
        let mut policies = config.policies.clone();
        policies.extend(extensions.policies.clone());
        let own = parts
            .prompt_sections
            .iter()
            .fold(config.system_prompt.clone(), |prompt, (name, content)| {
                prompt.with_section(name, content)
            });
        Ok(LoopConfig {
            system_prompt: extensions.sections_after(own).map_err(anyhow::Error::msg)?,
            // The tool set's own check covers names, declarations and panics.
            tools: Toolbox::new(tools).map_err(anyhow::Error::msg)?,
            cwd: PathBuf::from(&header.cwd),
            max_tokens: config.max_tokens,
            reasoning: config.reasoning,
            hooks,
            policies,
            approvals: config.approvals,
            commands: extensions.commands.clone(),
            models: config.models.clone(),
            cache_key: config.session_cache_key.then(|| header.id.clone()),
            summarizer: config.summarizer.clone(),
            ..LoopConfig::new(config.provider.clone(), config.model.clone())
        })
    }

    /// The session that records in `transcript` and runs with `assembled`.
    fn start(&self, assembled: LoopConfig, transcript: Transcript) -> Result<Session> {
        let config = &self.shared.config;
        Session::new(
            assembled,
            config.retry.clone(),
            config.limits.clone(),
            config.compaction.clone(),
            transcript,
        )
    }

    /// Put `session`, which this call made, into the table of open sessions. Fails when the
    /// table has a session with its ID, open or being opened; `session` is then dropped.
    fn register(&self, session: Session) -> Result<Session> {
        let closed;
        {
            let mut sessions = lock(&self.shared.sessions);
            closed = forget_closed(&mut sessions, session.id());
            anyhow::ensure!(
                !sessions.contains_key(session.id()),
                "session {} is already open",
                session.id()
            );
            let opening = Arc::new(OnceCell::new_with(Some(session.clone())));
            sessions.insert(session.id().to_string(), opening);
        }
        drop(closed);
        Ok(session)
    }
}

#[cfg(test)]
mod tests;
