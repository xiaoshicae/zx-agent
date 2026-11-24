//! What a conversation runs on, shown at startup and by `/status`, and `/help`.

use std::path::PathBuf;

use anyhow::Result;
use zx_agent::Session;
use zx_agent::builtin::tools::BASH;
use zx_agent::transcript::SessionInfo;
use zx_app::ConfiguredAgent;

use super::output::{self, Output};
use super::term::Tone;
use super::{APPROVAL_MENU, QUIT_INTERRUPT_WINDOW};
use crate::render::{local_time, visible};

/// Which bash commands ask before they run.
pub(super) enum CommandApproval {
    /// All but those of these auto-approved prefixes.
    AllBut(Vec<String>),
    /// None: every call is approved, by the agent or by `--dangerously-skip-approvals`.
    Skipped,
}

/// What a conversation runs on, shown at startup and by `/status`.
pub(super) struct About {
    pub(super) agent: String,
    pub(super) tools: Vec<String>,
    pub(super) commands: CommandApproval,
    /// Whether commands run apart from this machine, in a sandbox such as the container.
    pub(super) isolated: bool,
    pub(super) cwd: PathBuf,
    pub(super) session: Option<PathBuf>,
    /// What a listing shows of the conversation the session file held already, when it held
    /// one.
    pub(super) resumed: Option<SessionInfo>,
}

impl About {
    /// What `session` of `agent` runs on, read before its first run; `file` is the session's
    /// file, `None` for a session in memory.
    pub(super) fn of(agent: &ConfiguredAgent, session: &Session, file: Option<PathBuf>) -> Result<Self> {
        Ok(Self {
            agent: agent.name().to_string(),
            tools: session.tool_names().to_vec(),
            commands: if agent.asks_before_commands() {
                CommandApproval::AllBut(agent.auto_approve_commands().to_vec())
            } else {
                CommandApproval::Skipped
            },
            isolated: agent.isolates_commands(),
            cwd: session.cwd().to_path_buf(),
            session: file,
            resumed: session.with_transcript(|transcript| {
                (!transcript.entries().is_empty()).then(|| SessionInfo::of(transcript))
            })?,
        })
    }
}

fn session_text(about: &About) -> String {
    about
        .session
        .as_deref()
        .map_or_else(|| "memory".to_string(), |path| path.display().to_string())
}

/// `model= · agent= · session= · cwd=` after `prefix`, for the banner and `/status`.
fn about_line(prefix: &str, model: &str, about: &About) -> String {
    format!(
        "{prefix} model={} · agent={} · session={} · cwd={}",
        visible(model),
        visible(&about.agent),
        visible(&session_text(about)),
        visible(&about.cwd.display().to_string())
    )
}

/// How much of an earlier conversation the session file held, when it held one.
fn resumed_line(about: &About) -> Option<String> {
    let resumed = about.resumed.as_ref()?;
    let when = local_time(resumed.last_activity_ms)
        .map(|time| format!("; last activity {time}"))
        .unwrap_or_default();
    Some(format!(
        "[chat] resumed {} message(s) from the session file{when}",
        resumed.messages
    ))
}

/// What the conversation runs on, once at startup.
pub(super) fn banner(output: &Output, model: &str, about: &About) {
    output.line(Tone::Dim, &about_line("[chat]", model, about));
    if let Some(line) = resumed_line(about) {
        output.line(Tone::Dim, &line);
    }
    output.line(
        Tone::Dim,
        &format!(
            "[chat] tools={}{} · /help for commands",
            visible(&about.tools.join(",")),
            commands_part(about)
        ),
    );
}

/// ` · <which bash commands ask> · <where commands run>` for the banner, when the conversation
/// has bash; else nothing.
fn commands_part(about: &About) -> String {
    if !about.tools.iter().any(|tool| tool == BASH) {
        return String::new();
    }
    let asks = match &about.commands {
        CommandApproval::AllBut(prefixes) if prefixes.is_empty() => "every bash command asks first".to_string(),
        CommandApproval::AllBut(prefixes) => format!("bash asks first, except {}", visible(&prefixes.join(", "))),
        CommandApproval::Skipped => "bash runs without asking".to_string(),
    };
    let place = if about.isolated {
        "commands run isolated from this machine"
    } else {
        "commands run with this process's permissions"
    };
    format!(" · {asks} · {place}")
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
            totals.total_cost.map(output::cost_part).unwrap_or_default()
        ),
    );
}

pub(super) fn help(output: &Output, commands: &[(String, String)]) {
    let quit = format!(
        "                      twice within {} seconds while idle: exit",
        QUIT_INTERRUPT_WINDOW.as_secs()
    );
    let approvals = format!("Approvals: {APPROVAL_MENU}");
    for line in [
        "/new                  start a fresh conversation; approvals granted so far are dropped",
        "/model [id]           show or change the model while idle",
        "/reasoning [level]    show or change the reasoning level while idle: off, minimal,",
        "                      low, medium, high, xhigh, or default to leave it to the server",
        "/compact              summarize old context while retaining raw history",
        "/continue             continue without new input, such as after a failed answer",
        "/status               show the model, session file and what this conversation has used",
        "/quit                 exit chat; Ctrl+D while idle does the same",
        "Ctrl+C                cancel the current task or approval; keep the conversation",
        &quit,
        "While a task runs, a typed line is steering: the model sees it before its next request.",
        "Lines pasted together are sent as one message; a slash command is always one line.",
        &approvals,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn about(tools: &[&str], commands: CommandApproval, isolated: bool) -> About {
        About {
            agent: "assistant".into(),
            tools: tools.iter().map(ToString::to_string).collect(),
            commands,
            isolated,
            cwd: PathBuf::from("/work"),
            session: None,
            resumed: None,
        }
    }

    #[test]
    fn the_banner_says_which_commands_ask_and_where_they_run() {
        let asking = CommandApproval::AllBut(vec!["grep".into(), "head".into()]);
        assert_eq!(
            commands_part(&about(&["bash"], asking, false)),
            " · bash asks first, except grep, head · commands run with this process's permissions"
        );
        assert_eq!(
            commands_part(&about(&["read_file", "bash"], CommandApproval::Skipped, true)),
            " · bash runs without asking · commands run isolated from this machine"
        );
        // Without bash, no command runs.
        assert_eq!(
            commands_part(&about(&["read_file"], CommandApproval::AllBut(Vec::new()), false)),
            ""
        );
    }
}
