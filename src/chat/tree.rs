//! `/tree` and `/branch`: the session tree as lines, and the entry a branch switch names.
//!
//! The tree lists the entries a person can place: messages and summaries. The records of
//! requests, starts, edits and upgrades between them stay out, as pi's `/tree` leaves out its
//! bookkeeping entries. The branch the conversation is on reads straight down; a branch left
//! behind is indented under it and says which shown entry it follows.

use std::collections::{HashMap, HashSet};

use zx_agent::ai::{AssistantMessage, Message, StopReason, UserMessageKind};
use zx_agent::session::{Entry, SessionStore};

use super::visible;
use crate::sessions::preview_text;

/// Spaces per level of a branch left behind.
const INDENT: &str = "    ";

/// Where `/branch` moves the conversation.
pub(super) enum Target {
    /// After this entry, described by `line`.
    After { id: String, line: String },
    /// Before this user message, whose text the person may send again, changed (pi: choosing
    /// a user message in `/tree` moves before it and returns its text to the editor).
    Before {
        parent: Option<String>,
        id: String,
        prompt: String,
    },
}

/// The kind column and preview of an entry the tree shows; `None` for the records it
/// leaves out.
fn describe(entry: &Entry) -> Option<(&'static str, String)> {
    match entry {
        Entry::Message(entry) => Some(match &entry.message {
            Message::System(system) => ("system", preview_text(&system.render_update())),
            Message::User { content, kind, .. } => (
                "user",
                match kind {
                    UserMessageKind::ContinueAnswer => format!("(continue) {}", preview_text(content)),
                    UserMessageKind::Prompt | UserMessageKind::Summary | UserMessageKind::Custom => {
                        preview_text(content)
                    }
                },
            ),
            Message::Assistant(answer) => ("assistant", describe_answer(answer)),
            Message::ToolResult(result) => (
                "tool",
                format!(
                    "{}{}: {}",
                    visible(&result.tool_name),
                    if result.is_error { " (error)" } else { "" },
                    preview_text(&result.content)
                ),
            ),
        }),
        Entry::Compaction(compaction) => Some((
            "compaction",
            format!("summary with {} fact(s)", compaction.summary.facts.len()),
        )),
        Entry::CustomMessage(entry) => Some((
            "custom",
            format!(
                "[{}] {}",
                visible(&entry.message.custom_type),
                preview_text(&entry.message.content)
            ),
        )),
        Entry::ContextEdit(_)
        | Entry::FormatUpgrade(_)
        | Entry::CompactionStarted(_)
        | Entry::CompactionFailed(_)
        | Entry::RequestConfig(_)
        | Entry::RequestStarted(_)
        | Entry::ToolCallStarted(_)
        | Entry::Custom(_) => None,
    }
}

fn describe_answer(answer: &AssistantMessage) -> String {
    let calls: Vec<String> = answer
        .tool_calls()
        .into_iter()
        .map(|call| visible(&call.name))
        .collect();
    let text = preview_text(&answer.text());
    match answer.stop_reason {
        StopReason::Error => format!(
            "(failed: {})",
            preview_text(answer.error_message.as_deref().unwrap_or("unknown error"))
        ),
        StopReason::Aborted => "(cancelled)".to_string(),
        StopReason::Stop | StopReason::Length | StopReason::ToolUse => {
            if calls.is_empty() {
                text
            } else if text.is_empty() {
                format!("calls {}", calls.join(", "))
            } else {
                format!("calls {}: {text}", calls.join(", "))
            }
        }
    }
}

fn local_time(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|utc| utc.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
        // Out of range only for a corrupt timestamp, which the tree still lists.
        .unwrap_or_else(|| "-".to_string())
}

/// The nearest shown entry above `id`, by its ID; `None` at the start of the session.
fn shown_ancestor<'a>(store: &'a SessionStore, mut id: Option<&'a str>) -> Option<&'a str> {
    while let Some(entry) = id.and_then(|id| store.get(id)) {
        if describe(entry).is_some() {
            return Some(entry.id());
        }
        id = entry.parent_id();
    }
    None
}

/// An entry to visit, at the indentation of its branch.
struct Visit {
    index: usize,
    depth: usize,
    /// The first entry of a branch left behind, announced before it.
    starts_branch: bool,
}

/// The tree as lines for the terminal: a header, the shown entries, and the position.
pub(super) fn render(store: &SessionStore) -> Vec<String> {
    let entries = store.entries();
    let on_path: HashSet<&str> = store.branch_path().iter().map(|entry| entry.id()).collect();
    let leaf = store.leaf_id();
    // Children in append order, under `None` for a root; an entry whose parent is missing is
    // a root as well, as in pi.
    let mut children: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let parent = entry.parent_id().filter(|parent| store.get(parent).is_some());
        children.entry(parent).or_default().push(index);
    }
    let branches = entries
        .iter()
        .filter(|entry| !children.contains_key(&Some(entry.id())))
        .count();
    let mut lines = vec![format!(
        "[tree] {} entries on {branches} branch(es); * marks the current branch, > the current position",
        entries.len()
    )];
    let mut shown = 0;
    let mut stack: Vec<Visit> = Vec::new();
    // Depth first; at every fork the branch that goes on is listed first (pi: the branch
    // holding the leaf comes first) and the others are indented under it.
    let push = |parent: Option<&str>, depth: usize, stack: &mut Vec<Visit>| {
        let Some(ids) = children.get(&parent) else {
            return;
        };
        let mut ordered: Vec<usize> = ids.clone();
        if let Some(position) = ordered.iter().position(|&index| on_path.contains(entries[index].id())) {
            let preferred = ordered.remove(position);
            ordered.insert(0, preferred);
        }
        // Pushed last to first, so the preferred child is visited first.
        for (rank, index) in ordered.into_iter().enumerate().rev() {
            let side = rank > 0;
            stack.push(Visit {
                index,
                depth: if side { depth + 1 } else { depth },
                starts_branch: side,
            });
        }
    };
    push(None, 0, &mut stack);
    while let Some(visit) = stack.pop() {
        let entry = &entries[visit.index];
        let indent = INDENT.repeat(visit.depth);
        if visit.starts_branch {
            let after = match shown_ancestor(store, entry.parent_id()) {
                Some(id) => format!("after {}", visible(id)),
                None => "from the start".to_string(),
            };
            lines.push(format!("{indent}(branch {after})"));
        }
        if let Some((kind, preview)) = describe(entry) {
            shown += 1;
            let mark = if leaf == Some(entry.id()) {
                '>'
            } else if on_path.contains(entry.id()) {
                '*'
            } else {
                ' '
            };
            lines.push(format!(
                "{indent}{mark} {:<8}  {}  {kind:<10} {preview}",
                visible(entry.id()),
                local_time(entry.timestamp())
            ));
        }
        push(Some(entry.id()), visit.depth, &mut stack);
    }
    let position = match leaf {
        Some(id) => format!("after {}", visible(id)),
        None => "before the first entry".to_string(),
    };
    lines.push(format!(
        "[tree] {shown} shown of {} entries; the next message goes {position}; /branch <ID> moves",
        entries.len()
    ));
    lines
}

/// The entry `/branch <id>` names: a unique prefix of the ID of an entry the tree shows.
pub(super) fn resolve(store: &SessionStore, id: &str) -> Result<Target, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("give an entry ID from /tree".to_string());
    }
    let mut matches = store
        .entries()
        .iter()
        .filter(|entry| entry.id().starts_with(id) && describe(entry).is_some());
    let Some(entry) = matches.next() else {
        return Err(format!("no entry in /tree starts with {}", visible(id)));
    };
    if matches.next().is_some() {
        return Err(format!(
            "{} names more than one entry; /tree shows the full IDs",
            visible(id)
        ));
    }
    Ok(match entry {
        Entry::Message(message) if matches!(message.message, Message::User { .. }) => Target::Before {
            parent: message.parent_id.clone(),
            id: message.id.clone(),
            prompt: describe(entry).map(|(_, preview)| preview).unwrap_or_default(),
        },
        _ => {
            let (kind, preview) = describe(entry).unwrap_or_default();
            Target::After {
                id: entry.id().to_string(),
                line: format!("{kind} {preview}"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use zx_agent::ai::{ContentBlock, ToolResultMessage, Usage};
    use zx_agent::session::SessionHeader;

    fn answer(text: &str, calls: &[&str]) -> Message {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text: text.into() });
        }
        for call in calls {
            content.push(ContentBlock::ToolCall {
                id: format!("call-{call}"),
                name: (*call).into(),
                arguments: json!({}),
                arguments_error: None,
            });
        }
        Message::Assistant(AssistantMessage {
            content,
            provider: "faux".into(),
            model: "m".into(),
            usage: Usage::default(),
            stop_reason: if calls.is_empty() {
                StopReason::Stop
            } else {
                StopReason::ToolUse
            },
            error_message: None,
            provider_error: None,
            timestamp: 1,
            duration_ms: None,
            response_id: None,
            response_model: None,
        })
    }

    /// `first` → `one` → `second` → `two`, then a branch after `one` with `third`;
    /// returns the store and the IDs in that order.
    async fn forked() -> (SessionStore, Vec<String>) {
        let mut store = SessionStore::in_memory(SessionHeader::new(Path::new("."), "m"));
        let mut ids = Vec::new();
        for message in [
            Message::user("first"),
            answer("one", &[]),
            Message::user("second"),
            answer("two", &[]),
        ] {
            ids.push(store.append_message(message).await.unwrap());
        }
        store.branch(&ids[1]).unwrap();
        ids.push(store.append_message(Message::user("third")).await.unwrap());
        (store, ids)
    }

    fn shown_ids(lines: &[String]) -> Vec<(char, String, usize)> {
        lines
            .iter()
            .filter(|line| !line.starts_with("[tree]") && !line.trim_start().starts_with("(branch"))
            .map(|line| {
                let mut depth = 0;
                let mut rest = line.as_str();
                while let Some(stripped) = rest.strip_prefix(INDENT) {
                    depth += 1;
                    rest = stripped;
                }
                let mark = rest.chars().next().unwrap();
                let id = rest[2..].split(' ').next().unwrap().to_string();
                (mark, id, depth)
            })
            .collect()
    }

    #[tokio::test]
    async fn the_current_branch_reads_straight_down_and_the_other_is_indented_after_its_fork() {
        let (store, ids) = forked().await;
        let lines = render(&store);
        assert_eq!(
            shown_ids(&lines),
            [
                ('*', ids[0].clone(), 0),
                ('*', ids[1].clone(), 0),
                ('>', ids[4].clone(), 0),
                (' ', ids[2].clone(), 1),
                (' ', ids[3].clone(), 1),
            ]
        );
        assert_eq!(lines[4], format!("{INDENT}(branch after {})", ids[1]));
        assert!(
            lines[0].starts_with("[tree] 5 entries on 2 branch(es);"),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[lines.len() - 1],
            format!(
                "[tree] 5 shown of 5 entries; the next message goes after {}; /branch <ID> moves",
                ids[4]
            )
        );
        assert!(lines[1].ends_with("user       first"), "{}", lines[1]);
        assert!(lines[2].ends_with("assistant  one"), "{}", lines[2]);
    }

    #[tokio::test]
    async fn bookkeeping_records_are_counted_but_not_listed() {
        let mut store = SessionStore::in_memory(SessionHeader::new(Path::new("."), "m"));
        let first = store.append_message(Message::user("first")).await.unwrap();
        store.append_message(answer("one", &[])).await.unwrap();
        store.append_context_edit(&first, None).await.unwrap();
        let lines = render(&store);
        assert_eq!(shown_ids(&lines).len(), 2);
        assert!(lines[lines.len() - 1].starts_with("[tree] 2 shown of 3 entries;"));
        // The leaf is the edit, which is not listed: no entry carries the position mark.
        assert!(shown_ids(&lines).iter().all(|(mark, _, _)| *mark == '*'));
    }

    #[tokio::test]
    async fn previews_name_tool_calls_results_and_failures() {
        let mut store = SessionStore::in_memory(SessionHeader::new(Path::new("."), "m"));
        store.append_message(Message::user("go")).await.unwrap();
        store.append_message(answer("", &["bash", "read_file"])).await.unwrap();
        store
            .append_message(Message::ToolResult(ToolResultMessage {
                tool_call_id: "call-bash".into(),
                tool_name: "bash".into(),
                content: "no such file\nmore".into(),
                is_error: true,
                timestamp: 1,
                duration_ms: None,
                details: None,
            }))
            .await
            .unwrap();
        store
            .append_message(Message::Assistant(AssistantMessage::failure(
                "faux",
                "m",
                StopReason::Error,
                "overloaded",
            )))
            .await
            .unwrap();
        let lines = render(&store);
        assert!(lines[2].ends_with("assistant  calls bash, read_file"), "{}", lines[2]);
        assert!(
            lines[3].ends_with("tool       bash (error): no such file…"),
            "{}",
            lines[3]
        );
        assert!(lines[4].ends_with("assistant  (failed: overloaded)"), "{}", lines[4]);
    }

    #[tokio::test]
    async fn a_user_message_resolves_to_before_it_and_other_entries_to_after_them() {
        let (store, ids) = forked().await;
        match resolve(&store, &ids[2]).unwrap() {
            Target::Before { parent, id, prompt } => {
                assert_eq!(
                    (parent.as_deref(), id.as_str(), prompt.as_str()),
                    (Some(ids[1].as_str()), ids[2].as_str(), "second")
                );
            }
            Target::After { .. } => panic!("a user message moves before itself"),
        }
        match resolve(&store, &ids[3]).unwrap() {
            Target::After { id, line } => assert_eq!((id.as_str(), line.as_str()), (ids[3].as_str(), "assistant two")),
            Target::Before { .. } => panic!("an answer is continued after"),
        }
    }

    #[tokio::test]
    async fn an_id_must_name_exactly_one_shown_entry() {
        let (store, ids) = forked().await;
        assert_eq!(resolve(&store, "").err().unwrap(), "give an entry ID from /tree");
        assert_eq!(
            resolve(&store, "zzzz").err().unwrap(),
            "no entry in /tree starts with zzzz"
        );
        // Every ID starts with one of 16 characters: the first character of some ID is shared
        // whenever two IDs begin alike, which five random IDs may not; build a sure case.
        let shared: String = ids[0].chars().take(1).collect();
        let shared_count = ids.iter().filter(|id| id.starts_with(&shared)).count();
        if shared_count > 1 {
            assert!(resolve(&store, &shared).is_err());
        }
        assert!(resolve(&store, &ids[0]).is_ok());
    }
}
