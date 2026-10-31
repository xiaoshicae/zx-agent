//! The system prompt as named sections, recorded in the conversation (pi: `SystemMessage` in
//! packages/ai, `buildSystemPromptSections` and `diffSystemPromptSections` in coding-agent).
//!
//! The first system message of a conversation sets the prompt; later ones carry only the
//! sections that changed. Because every version stays in the transcript, the session file
//! shows which prompt each request used, and with inline updates the earlier requests stay a
//! prefix of later ones, so the model sees when the prompt changed and the provider's prompt
//! cache keeps working.

use serde::{Deserialize, Serialize};

use super::types::Message;

/// Name of the section rendered first and without a tag (pi: `preamble`).
pub const PREAMBLE: &str = "preamble";

/// A system prompt: the preamble, then other sections, each wrapped in a tag of its name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemPrompt {
    /// `(name, content)` in render order; names are unique.
    sections: Vec<(String, String)>,
}

impl SystemPrompt {
    /// Set section `name`, replacing one with the same name in place. Blank content is
    /// left out, so optional sections can be passed unconditionally.
    pub fn with_section(mut self, name: &str, content: &str) -> Self {
        let content = content.trim();
        if !content.is_empty() {
            self.set(name, content);
        }
        self
    }

    /// Whether the prompt has no sections.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// The full prompt text.
    pub fn render(&self) -> String {
        let parts: Vec<String> = self
            .sections
            .iter()
            .map(|(name, content)| render_section(name, content))
            .collect();
        parts.join("\n\n")
    }

    /// The prompt that the system messages in `messages` set, applied in order.
    pub fn from_transcript(messages: &[Message]) -> Self {
        let mut prompt = Self::default();
        for message in messages {
            if let Message::System(system) = message {
                prompt.apply(&system.sections);
            }
        }
        prompt
    }

    /// The changes that turn `self` into `next`: new or changed sections in `next`'s order,
    /// then removed ones (pi: `diffSystemPromptSections`).
    pub fn diff(&self, next: &SystemPrompt) -> Vec<SectionChange> {
        let changed = next
            .sections
            .iter()
            .filter(|(name, content)| self.get(name) != Some(content.as_str()))
            .map(|(name, content)| SectionChange {
                name: name.clone(),
                content: Some(content.clone()),
            });
        let removed = self
            .sections
            .iter()
            .filter(|(name, _)| next.get(name).is_none())
            .map(|(name, _)| SectionChange {
                name: name.clone(),
                content: None,
            });
        changed.chain(removed).collect()
    }

    /// Apply `changes` in order. A changed section keeps its place; a new one goes last.
    pub fn apply(&mut self, changes: &[SectionChange]) {
        for change in changes {
            match &change.content {
                Some(content) => self.set(&change.name, content),
                None => self.sections.retain(|(name, _)| *name != change.name),
            }
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, content)| content.as_str())
    }

    fn set(&mut self, name: &str, content: &str) {
        match self.sections.iter_mut().find(|(existing, _)| existing == name) {
            Some((_, existing)) => *existing = content.to_string(),
            None => self.sections.push((name.to_string(), content.to_string())),
        }
    }
}

/// A change to one section (pi: one entry of `SystemMessage.sections`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionChange {
    pub name: String,
    /// The new content; `None` removes the section.
    pub content: Option<String>,
}

/// A system prompt change recorded in the conversation (pi: `SystemMessage`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemMessage {
    /// The sections this message sets or removes; the first system message sets them all.
    pub sections: Vec<SectionChange>,
    pub timestamp: u64,
}

impl SystemMessage {
    /// The text that tells a model holding an earlier prompt what changed
    /// (pi: `renderSystemMessageUpdate`).
    pub fn render_update(&self) -> String {
        let parts: Vec<String> = self
            .sections
            .iter()
            .map(|change| match &change.content {
                Some(content) => format!(
                    "Updated system prompt section \"{}\":\n\n{}",
                    change.name,
                    render_section(&change.name, content)
                ),
                None => format!("Removed system prompt section \"{}\".", change.name),
            })
            .collect();
        parts.join("\n\n")
    }
}

fn render_section(name: &str, content: &str) -> String {
    if name == PREAMBLE {
        content.to_string()
    } else {
        format!("<{name}>\n{content}\n</{name}>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(environment: &str, policy: &str) -> SystemPrompt {
        SystemPrompt::default()
            .with_section(PREAMBLE, "Do the task.")
            .with_section("environment", environment)
            .with_section("command_policy", policy)
    }

    #[test]
    fn renders_the_preamble_then_tagged_sections() {
        assert_eq!(
            prompt("OS: linux", "").render(),
            "Do the task.\n\n<environment>\nOS: linux\n</environment>"
        );
    }

    #[test]
    fn diff_lists_changed_and_removed_sections_only() {
        let changes = prompt("t1", "grep").diff(&prompt("t2", ""));
        let summary: Vec<(&str, Option<&str>)> = changes
            .iter()
            .map(|change| (change.name.as_str(), change.content.as_deref()))
            .collect();
        assert_eq!(summary, [("environment", Some("t2")), ("command_policy", None)]);
    }

    #[test]
    fn applying_a_diff_reproduces_the_target() {
        let (old, new) = (prompt("t1", "grep"), prompt("t2", "kubectl"));
        let mut replayed = old.clone();
        replayed.apply(&old.diff(&new));
        assert_eq!(replayed, new);
    }

    #[test]
    fn updates_name_each_changed_section() {
        let update = SystemMessage {
            sections: vec![
                SectionChange {
                    name: "environment".into(),
                    content: Some("t2".into()),
                },
                SectionChange {
                    name: "command_policy".into(),
                    content: None,
                },
            ],
            timestamp: 0,
        };
        assert_eq!(
            update.render_update(),
            "Updated system prompt section \"environment\":\n\n<environment>\nt2\n</environment>\n\n\
             Removed system prompt section \"command_policy\"."
        );
    }
}
