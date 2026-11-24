//! How full the model's context is, as `usage_update`: after each answer of a turn, when a
//! turn ends, and when a session opens or changes its model. Clients show it against the
//! context window, so nothing is sent for a model whose window config.toml does not give.
//! The cost is left out: the protocol wants its currency, and config.toml's prices name none.

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{SessionId, SessionUpdate, UsageUpdate};
use zx_agent::Session;
use zx_agent::ai::AssistantMessage;
use zx_agent::compaction;
use zx_util::stdio::eprint_line;

use super::models::Models;
use super::updates;

/// After `answer`, the context it was sent and what it wrote, measured by its usage: the
/// session cannot be read while its run goes on. An answer that reported no usage, as a
/// failed one, measures nothing.
pub(super) fn after_answer(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    answer: &AssistantMessage,
    models: &Models,
) {
    if !answer.reports_usage() {
        return;
    }
    if let Some(size) = models.window(&answer.model) {
        send(cx, session_id, answer.usage.input_and_output(), size);
    }
}

/// The context of `session`'s next request, which the client `cx` knows as `session_id`:
/// what the last answer measured, and an estimate of what came after it. Read while the
/// session is idle; a session that is running, or that closed itself, says nothing now, and
/// its next answer or turn sends the usage.
pub(super) fn of_session(cx: &ConnectionTo<Client>, session_id: &SessionId, session: &Session, models: &Models) {
    // Busy or closed: see above.
    let Ok(model) = session.model() else {
        return;
    };
    let Some(size) = models.window(&model) else {
        return;
    };
    match session.with_transcript(compaction::context_tokens) {
        Ok(Ok(used)) => send(cx, session_id, used, size),
        // Busy or closed since the model was read.
        Err(_) => {}
        // Only a message that cannot be serialized fails the estimate; the gauge then stays
        // as it was, and the cause goes to stderr.
        Ok(Err(error)) => eprint_line(&format!(
            "[acp] cannot estimate the context of session {}: {error:#}",
            session_id.0
        )),
    }
}

fn send(cx: &ConnectionTo<Client>, session_id: &SessionId, used: u64, size: u64) {
    updates::send(cx, session_id, SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)));
}
