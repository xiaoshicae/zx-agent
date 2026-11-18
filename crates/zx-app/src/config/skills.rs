//! An agent's skills: an explicit catalog whose metadata enters the prompt; the bodies are read
//! only by the tool that the assembly registers (`read_skill`).

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use super::MAX_FILE_BYTES;

/// Name of the tool that reads the body of a skill, which an agent enables like the built-in
/// tools.
pub(crate) const READ_SKILL: &str = "read_skill";

/// Metadata supplied by an agent. `path` is relative to the agent file until resolved.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Skill {
    /// Unique name accepted by read_skill, using lowercase letters, digits and hyphens.
    pub(crate) name: String,
    /// Brief selection guidance included in the initial catalog.
    pub(crate) description: String,
    /// Body file; no scripts or supporting files are executed automatically.
    pub(crate) path: PathBuf,
}

/// Check the registrations of `skills` without reading their bodies: at most 64, unique names
/// of lowercase letters, digits and hyphens, a description, and a regular file within
/// `MAX_FILE_BYTES` for each.
pub(crate) fn validate_skills(skills: &[Skill]) -> Result<()> {
    ensure!(skills.len() <= 64, "at most 64 skills may be registered");
    let mut names = HashSet::new();
    for skill in skills {
        ensure!(
            !skill.name.is_empty()
                && skill.name.len() <= 64
                && skill
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
            "invalid skill name {:?}; use 1-64 lowercase letters, digits or hyphens",
            skill.name
        );
        ensure!(names.insert(&skill.name), "duplicate skill {:?}", skill.name);
        ensure!(
            !skill.description.trim().is_empty() && skill.description.len() <= 1024,
            "skill {:?} needs a description up to 1024 bytes",
            skill.name
        );
        let meta =
            std::fs::metadata(&skill.path).with_context(|| format!("cannot inspect skill {}", skill.path.display()))?;
        ensure!(meta.is_file(), "skill {} is not a regular file", skill.path.display());
        ensure!(
            meta.len() <= MAX_FILE_BYTES,
            "skill {} exceeds {MAX_FILE_BYTES} bytes",
            skill.path.display()
        );
    }
    Ok(())
}
