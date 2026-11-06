//! What a conversation runs on, shown at startup and by `/status`, and `/help`.

use std::path::{Path, PathBuf};

use zx_agent::ai::Message;
use zx_agent::session::Entry;
use zx_agent::tools::BASH;

use super::output::{self, Output};
use super::term::Tone;
use crate::cli::PreparedSession;
use crate::render::{local_time, visible};

/// How the last activity of a resumed session is shown.
const ACTIVITY_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// What a conversation runs on, shown at startup and by `/status`.
pub(super) struct About {
    pub(super) profile: String,
    pub(super) tools: Vec<String>,
    pub(super) auto_approve: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) session: Option<PathBuf>,
    /// Set when the session file already held a conversation.
    pub(super) resumed: Option<Resumed>,
}

pub(super) struct Resumed {
    /// User and assistant messages in the model's context.
    messages: usize,
    /// When the last record was written.
    last_activity_ms: Option<u64>,
}

impl About {
    /// What `prepared` runs on, taken from it before it becomes a session.
    pub(super) fn of(prepared: &mut PreparedSession) -> Self {
        Self {
            profile: std::mem::take(&mut prepared.profile_name),
            tools: prepared
                .config
                .spec
                .tools
                .iter()
                .map(|tool| tool.name().to_string())
                .collect(),
            auto_approve: std::mem::take(&mut prepared.auto_approve_commands),
            cwd: prepared.config.spec.cwd.clone(),
            session: prepared.store.path().map(Path::to_path_buf),
            resumed: (!prepared.store.entries().is_empty()).then(|| Resumed {
                messages: prepared
                    .store
                    .messages()
                    .iter()
                    .filter(|message| matches!(message, Message::User { .. } | Message::Assistant(_)))
                    .count(),
                last_activity_ms: prepared.store.entries().last().map(Entry::timestamp),
            }),
        }
    }
}

fn session_text(about: &About) -> String {
    about
        .session
        .as_deref()
        .map_or_else(|| "memory".to_string(), |path| path.display().to_string())
}

/// `model= · profile= · session= · cwd=` after `prefix`, for the banner and `/status`.
fn about_line(prefix: &str, model: &str, about: &About) -> String {
    format!(
        "{prefix} model={} · profile={} · session={} · cwd={}",
        visible(model),
        visible(&about.profile),
        visible(&session_text(about)),
        visible(&about.cwd.display().to_string())
    )
}

/// How much of an earlier conversation the session file held, when it held one.
pub(super) fn resumed_line(output: &Output, about: &About) {
    let Some(resumed) = &about.resumed else {
        return;
    };
    let when = resumed
        .last_activity_ms
        .and_then(|ms| local_time(ms, ACTIVITY_TIME_FORMAT))
        .map(|time| format!("; last activity {time}"))
        .unwrap_or_default();
    output.line(
        Tone::Dim,
        &format!(
            "[chat] resumed {} message(s) from the session file{when}",
            resumed.messages
        ),
    );
}

/// What the conversation runs on, once at startup.
pub(super) fn banner(output: &Output, model: &str, about: &About) {
    output.line(Tone::Dim, &about_line("[chat]", model, about));
    resumed_line(output, about);
    let approvals = if about.tools.iter().any(|tool| tool == BASH) {
        let asks = if about.auto_approve.is_empty() {
            "every bash command asks first".to_string()
        } else {
            format!("bash asks first, except {}", visible(&about.auto_approve.join(", ")))
        };
        format!(" · {asks} · commands run with this process's permissions")
    } else {
        String::new()
    };
    output.line(
        Tone::Dim,
        &format!(
            "[chat] tools={}{approvals} · /help for commands",
            visible(&about.tools.join(","))
        ),
    );
}

pub(super) fn status(output: &Output, model: &str, about: &About) {
    output.line(Tone::Dim, &about_line("[status]", model, about));
    let totals = output.totals();
    output.line(
        Tone::Dim,
        &format!(
            "[status] this conversation: {} turn(s) · {} request(s) · {} in · {} out · {} cached{}",
            totals.turns,
            totals.requests,
            totals.input_tokens,
            totals.output_tokens,
            totals.cached_tokens,
            totals.cost_usd.map(output::cost_text).unwrap_or_default()
        ),
    );
}

pub(super) fn help(output: &Output, commands: &[(String, String)]) {
    for line in [
        "/new                  start a fresh conversation; approvals granted so far are dropped",
        "/model [id]           show or change the model while idle",
        "/reasoning [level]    show or change the reasoning level while idle: off, minimal,",
        "                      low, medium, high, xhigh, or default to leave it to the server",
        "/compact              summarize old context while retaining raw history",
        "/continue             continue without new input, such as after a failed answer",
        "/status               show the model, session file and what this conversation has used",
        "/sessions             list the sessions in the session directory, most recent first",
        "/resume <ID>          switch to the session whose ID starts with <ID>",
        "/fork                 continue in a copy of this session; the original stays as it is",
        "/tree                 show the session tree: every message on every branch, the current one marked",
        "/branch <ID>          continue after the entry with that ID, or before your own message with that ID",
        "/queue [mode]         show or change how input typed during a task is taken: all, or one-at-a-time",
        "/quit                 exit chat; Ctrl+D while idle does the same",
        "Ctrl+C                cancel the current task or approval; keep the conversation",
        "                      twice within 2 seconds while idle: exit",
        "While a task runs, a typed line is steering: the model sees it before its next request.",
        "Lines pasted together are sent as one message; a slash command is always one line.",
        "Approvals: 1 or y allow once · 2 allow identical calls in this conversation · 3 or n deny · 4 discuss",
    ] {
        output.line(Tone::Plain, line);
    }
    for (name, description) in commands {
        output.line(
            Tone::Plain,
            &format!("/{:<21} {} (extension command)", visible(name), visible(description)),
        );
    }
}
