//! The session's name (pi: `SessionInfoEntry`, `appendSessionInfo`, `getSessionName`): a
//! `session_info` record names the session, and the latest one counts, whichever branch it
//! was given on. Listings show the name instead of the first prompt; the model never sees it.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ai::now_ms;

use super::{Entry, SessionStore};

/// What a session is called: one line, without surrounding whitespace, never empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionName(String);

impl SessionName {
    /// `text` as a name: each run of line breaks and other control characters becomes one
    /// space, and the whitespace around the result goes. `None` when nothing is left.
    pub fn new(text: &str) -> Option<Self> {
        let line = text
            .split(breaks_line)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let name = line.trim();
        (!name.is_empty()).then(|| Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Control characters, line breaks among them, and the Unicode line and paragraph
/// separators, which are not control characters but end a line all the same.
fn breaks_line(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

impl TryFrom<String> for SessionName {
    type Error = &'static str;

    // A stored name must be one that `new` could have made, so a loaded session holds no
    // name that could not have been given.
    fn try_from(text: String) -> Result<Self, Self::Error> {
        match Self::new(&text) {
            Some(name) if name.0 == text => Ok(name),
            Some(_) | None => Err("a session name is one line without surrounding whitespace, and not empty"),
        }
    }
}

impl From<SessionName> for String {
    fn from(name: SessionName) -> Self {
        name.0
    }
}

/// A `session_info` record: a name given to the session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub name: SessionName,
}

impl SessionStore {
    /// Name the session. Like every record, this one is a child of the leaf, so a copied
    /// branch takes it along; it never reaches the model. Returns the entry's ID.
    pub async fn append_session_name(&mut self, name: SessionName) -> Result<String> {
        let entry = Entry::SessionInfo(SessionInfoEntry {
            id: self.new_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            name,
        });
        self.append(entry).await
    }

    /// The session's name: the latest one recorded, on any branch; `None` until one is given.
    pub fn session_name(&self) -> Option<&SessionName> {
        self.entries.iter().rev().find_map(|entry| {
            if let Entry::SessionInfo(info) = entry {
                Some(&info.name)
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::ai::Message;
    use crate::session::SESSION_VERSION;
    use crate::session::test_support::{assistant, header, texts};

    fn name(text: &str) -> SessionName {
        SessionName::new(text).unwrap()
    }

    #[test]
    fn a_name_is_one_line_without_surrounding_whitespace() {
        assert_eq!(name("  fix  the build \n").as_str(), "fix  the build");
        assert_eq!(name("one\r\ntwo\tthree").as_str(), "one two three");
        assert_eq!(name("排查\u{2028}nginx 502").as_str(), "排查 nginx 502");
        assert_eq!(name("\u{1b}[31mred").as_str(), "[31mred");
        for nothing in ["", " ", "\n\t\r", "\u{2029}"] {
            assert_eq!(SessionName::new(nothing), None, "{nothing:?}");
        }
    }

    #[tokio::test]
    async fn the_latest_name_counts_on_any_branch_and_the_model_never_sees_it() {
        let mut store = SessionStore::in_memory(header());
        assert_eq!(store.session_name(), None);
        let question = store.append_message(Message::user("q")).await.unwrap();
        let answer = store.append_message(assistant("a")).await.unwrap();
        let first = store.append_session_name(name("first")).await.unwrap();
        assert_eq!(store.get(&first).and_then(Entry::parent_id), Some(answer.as_str()));
        // Named again on another branch, then back on the first one.
        store.branch(&question).unwrap();
        store.append_session_name(name("second")).await.unwrap();
        store.branch(&first).unwrap();

        assert_eq!(store.session_name().map(SessionName::as_str), Some("second"));
        assert_eq!(texts(&store), ["q", "a"]);
    }

    #[tokio::test]
    async fn a_name_is_written_as_a_session_info_record_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = SessionStore::create(&path, header()).unwrap();
        let question = store.append_message(Message::user("q")).await.unwrap();
        let id = store.append_session_name(name("排查 nginx 502")).await.unwrap();
        drop(store);

        let text = std::fs::read_to_string(&path).unwrap();
        let line = text.lines().last().unwrap();
        let timestamp = serde_json::from_str::<Value>(line).unwrap()["timestamp"].clone();
        assert!(timestamp.is_u64(), "{line}");
        assert_eq!(
            line,
            format!(
                r#"{{"type":"session_info","id":"{id}","parentId":"{question}","timestamp":{timestamp},"name":"排查 nginx 502"}}"#
            )
        );
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.session_name().map(SessionName::as_str), Some("排查 nginx 502"));
    }

    #[test]
    fn opening_refuses_a_name_that_could_not_have_been_given() {
        let dir = tempfile::tempdir().unwrap();
        for stored in [" padded", "two\nlines", ""] {
            let path = dir.path().join("s.jsonl");
            let header = json!({"type": "session", "version": SESSION_VERSION, "id": "h", "timestamp": 1,
                "cwd": "/", "model": "m"});
            let record = json!({"type": "session_info", "id": "n1", "parentId": null, "timestamp": 2,
                "name": stored});
            std::fs::write(&path, format!("{header}\n{record}\n")).unwrap();

            let error = SessionStore::open(&path)
                .err()
                .unwrap_or_else(|| panic!("{stored:?} was accepted"));
            assert!(
                format!("{error:#}").contains("a session name is one line"),
                "{stored:?}: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn a_copied_branch_keeps_the_name_given_on_it() {
        let mut source = SessionStore::in_memory(header());
        let question = source.append_message(Message::user("q")).await.unwrap();
        source.append_session_name(name("on this branch")).await.unwrap();
        let leaf = source.append_message(assistant("a")).await.unwrap();
        source.branch(&question).unwrap();
        let other = source.append_message(assistant("elsewhere")).await.unwrap();

        let mut copy = SessionStore::in_memory(header());
        copy.copy_branch(&source, &leaf).await.unwrap();
        let mut other_copy = SessionStore::in_memory(header());
        other_copy.copy_branch(&source, &other).await.unwrap();

        assert_eq!(copy.session_name().map(SessionName::as_str), Some("on this branch"));
        assert_eq!(other_copy.session_name(), None);
    }
}
