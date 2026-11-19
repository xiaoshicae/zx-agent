//! The session's name (pi: `SessionInfoEntry`, `appendSessionInfo`, `getSessionName`): a
//! `session_info` record names the session, and the latest one counts, whichever branch it
//! was given on. Listings show the name instead of the first prompt; the model never sees it.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ai::now_ms;

use super::{Entry, Transcript};

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

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Control characters, line breaks among them, and the Unicode line and paragraph
/// separators, which are not control characters but end a line all the same.
fn breaks_line(character: char) -> bool {
    character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
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

impl Transcript {
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
    use crate::transcript::test_support::{assistant, header, loaded, texts};
    use crate::transcript::{MemoryStorage, SessionStorage};

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
        let mut transcript = Transcript::in_memory(header());
        assert_eq!(transcript.session_name(), None);
        let question = transcript.append_message(Message::user("q")).await.unwrap();
        let answer = transcript.append_message(assistant("a")).await.unwrap();
        let first = transcript.append_session_name(name("first")).await.unwrap();
        assert_eq!(transcript.get(&first).and_then(Entry::parent_id), Some(answer.as_str()));
        // Named again on another branch, then back on the first one.
        transcript.branch(&question).unwrap();
        transcript.append_session_name(name("second")).await.unwrap();
        transcript.branch(&first).unwrap();

        assert_eq!(transcript.session_name().map(SessionName::as_str), Some("second"));
        assert_eq!(texts(&transcript), ["q", "a"]);
    }

    #[tokio::test]
    async fn a_name_is_written_as_a_session_info_record_and_reopens() {
        let storage = MemoryStorage::default();
        let header = header();
        let mut transcript = Transcript::with_log(header.clone(), storage.create(&header).await.unwrap());
        let question = transcript.append_message(Message::user("q")).await.unwrap();
        let id = transcript.append_session_name(name("排查 nginx 502")).await.unwrap();
        drop(transcript);

        let records = storage.read(&header.id).await.unwrap().records;
        let line = records.last().unwrap().as_str();
        let timestamp = serde_json::from_str::<Value>(line).unwrap()["timestamp"].clone();
        assert!(timestamp.is_u64(), "{line}");
        assert_eq!(
            line,
            format!(
                r#"{{"type":"session_info","id":"{id}","parentId":"{question}","timestamp":{timestamp},"name":"排查 nginx 502"}}"#
            )
        );
        let (loaded, log) = storage.open(&header.id).await.unwrap();
        let reopened = Transcript::load(loaded, log).unwrap();
        assert_eq!(reopened.session_name().map(SessionName::as_str), Some("排查 nginx 502"));
    }

    #[tokio::test]
    async fn opening_refuses_a_name_that_could_not_have_been_given() {
        for stored in [" padded", "two\nlines", ""] {
            let record = json!({"type": "session_info", "id": "n1", "parentId": null, "timestamp": 2,
                "name": stored});

            let error = Transcript::snapshot(loaded(header(), vec![record.to_string()]))
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
        let mut source = Transcript::in_memory(header());
        let question = source.append_message(Message::user("q")).await.unwrap();
        source.append_session_name(name("on this branch")).await.unwrap();
        let leaf = source.append_message(assistant("a")).await.unwrap();
        source.branch(&question).unwrap();
        let other = source.append_message(assistant("elsewhere")).await.unwrap();

        let mut copy = Transcript::in_memory(header());
        copy.copy_branch(&source, &leaf).await.unwrap();
        let mut other_copy = Transcript::in_memory(header());
        other_copy.copy_branch(&source, &other).await.unwrap();

        assert_eq!(copy.session_name().map(SessionName::as_str), Some("on this branch"));
        assert_eq!(other_copy.session_name(), None);
    }
}
