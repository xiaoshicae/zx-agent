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

/// The most skills an agent registers: each one's name and description go into every request.
const MAX_SKILLS: usize = 64;
/// The longest skill name, in bytes.
const MAX_SKILL_NAME_BYTES: usize = 64;
/// The longest skill description, in bytes.
const MAX_DESCRIPTION_BYTES: usize = 1024;

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

/// Check the registrations of `skills` without reading their bodies: at most [`MAX_SKILLS`],
/// unique names of lowercase letters, digits and hyphens, a description, and a regular file
/// within `MAX_FILE_BYTES` for each.
pub(crate) fn validate_skills(skills: &[Skill]) -> Result<()> {
    ensure!(
        skills.len() <= MAX_SKILLS,
        "at most {MAX_SKILLS} skills may be registered"
    );
    let mut names = HashSet::new();
    for skill in skills {
        ensure!(
            !skill.name.is_empty()
                && skill.name.len() <= MAX_SKILL_NAME_BYTES
                && skill
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
            "invalid skill name {:?}; use 1-{MAX_SKILL_NAME_BYTES} lowercase letters, digits or hyphens",
            skill.name
        );
        ensure!(names.insert(&skill.name), "duplicate skill {:?}", skill.name);
        ensure!(
            !skill.description.trim().is_empty() && skill.description.len() <= MAX_DESCRIPTION_BYTES,
            "skill {:?} needs a description up to {MAX_DESCRIPTION_BYTES} bytes",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(path: PathBuf) -> Skill {
        Skill {
            name: "review".into(),
            description: "Review code".into(),
            path,
        }
    }

    #[test]
    fn invalid_metadata_and_duplicate_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "body").unwrap();
        assert!(validate_skills(&[skill(path.clone()), skill(path.clone())]).is_err());
        let mut invalid = skill(path.clone());
        invalid.name = "../escape".into();
        assert!(validate_skills(&[invalid]).is_err());
        let mut invalid = skill(path);
        invalid.description.clear();
        assert!(validate_skills(&[invalid]).is_err());
    }
}
