use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::{ProjectContext, read_text};

const MAX_CONTEXT_BYTES: usize = 256 * 1024;

pub(super) fn load(cwd: &Path, config: &ProjectContext) -> Result<String> {
    let mut files = if config.discover { discover(cwd)? } else { Vec::new() };
    files.extend_from_slice(&config.files);
    let mut seen = HashSet::new();
    let mut section = String::new();
    for path in files {
        let path = path
            .canonicalize()
            .with_context(|| format!("cannot locate context {}", path.display()))?;
        if !seen.insert(path.clone()) {
            continue;
        }
        let text = read_text(&path)?;
        let header = format!("\n# Project instructions from {:?}\n", path);
        ensure!(
            section
                .len()
                .saturating_add(header.len())
                .saturating_add(text.len())
                .saturating_add(1)
                <= MAX_CONTEXT_BYTES,
            "project context exceeds {MAX_CONTEXT_BYTES} bytes"
        );
        section.push_str(&header);
        section.push_str(&text);
        section.push('\n');
    }
    Ok(section)
}

/// AGENTS.md files from the nearest Git root down to `cwd`, parents first; only `cwd`
/// without a Git root. A discovered file may be a link, but it must resolve inside that
/// root: repository content must not make the agent read other files on this machine.
fn discover(cwd: &Path) -> Result<Vec<PathBuf>> {
    let mut ancestors = Vec::new();
    let mut root = None;
    for directory in cwd.ancestors() {
        ancestors.push(directory);
        match directory.join(".git").try_exists() {
            Ok(true) => {
                root = Some(directory);
                break;
            }
            Ok(false) => {}
            Err(error) => return Err(error).with_context(|| format!("cannot inspect {}", directory.display())),
        }
    }
    let root = match root {
        Some(root) => root,
        None => {
            ancestors.truncate(1);
            cwd
        }
    };
    // Compare resolved paths: the root itself may be reached through a link.
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", root.display()))?;
    let mut files = Vec::new();
    for directory in ancestors.into_iter().rev() {
        let path = directory.join("AGENTS.md");
        if path
            .try_exists()
            .with_context(|| format!("cannot inspect {}", path.display()))?
        {
            let resolved = path
                .canonicalize()
                .with_context(|| format!("cannot locate context {}", path.display()))?;
            ensure!(
                resolved.starts_with(&root),
                "{} resolves to {}, outside the project root {}",
                path.display(),
                resolved.display(),
                root.display()
            );
            files.push(resolved);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProjectContext {
        ProjectContext {
            discover: true,
            files: Vec::new(),
        }
    }

    #[test]
    fn discovery_stays_within_the_nearest_git_root_and_orders_parents_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let cwd = root.join("nested/leaf");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "private parent").unwrap();
        // A worktree's .git is a file, not a directory.
        std::fs::write(root.join(".git"), "gitdir: elsewhere").unwrap();
        std::fs::write(root.join("AGENTS.md"), "root marker").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "leaf marker").unwrap();
        let text = load(&cwd, &config()).unwrap();
        assert!(!text.contains("private parent"));
        assert!(text.find("root marker").unwrap() < text.find("leaf marker").unwrap());
    }

    #[test]
    fn outside_a_git_repo_only_the_cwd_is_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("nested");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "parent marker").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "cwd marker").unwrap();
        let text = load(&cwd, &config()).unwrap();
        assert!(!text.contains("parent marker"));
        assert!(text.contains("cwd marker"));
    }

    #[cfg(unix)]
    #[test]
    fn discovered_instructions_cannot_link_outside_the_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "OUTSIDE SECRET").unwrap();
        std::os::unix::fs::symlink("../secret.txt", root.join("AGENTS.md")).unwrap();
        let error = load(&root, &config()).unwrap_err();
        assert!(format!("{error:#}").contains("outside the project root"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn without_a_git_root_discovered_instructions_cannot_link_outside_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "OUTSIDE SECRET").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), cwd.join("AGENTS.md")).unwrap();
        assert!(load(&cwd, &config()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn discovered_instructions_may_link_inside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/agents.md"), "LINKED CONTEXT").unwrap();
        std::os::unix::fs::symlink("docs/agents.md", root.join("AGENTS.md")).unwrap();
        assert!(load(&root, &config()).unwrap().contains("LINKED CONTEXT"));
    }

    #[test]
    fn discovery_can_be_disabled_while_explicit_files_are_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "automatic").unwrap();
        let explicit = dir.path().join("extra.md");
        std::fs::write(&explicit, "explicit marker").unwrap();
        let config = ProjectContext {
            discover: false,
            files: vec![explicit.clone(), explicit],
        };
        let text = load(dir.path(), &config).unwrap();
        assert!(!text.contains("automatic"));
        assert_eq!(text.matches("explicit marker").count(), 1);
    }

    #[test]
    fn combined_context_has_a_budget_including_its_generated_headers() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<_> = (0..5)
            .map(|index| {
                let path = dir.path().join(format!("{index}.md"));
                std::fs::write(&path, "x".repeat(64 * 1024)).unwrap();
                path
            })
            .collect();
        let error = load(dir.path(), &ProjectContext { discover: false, files }).unwrap_err();
        assert!(error.to_string().contains("context exceeds"));
    }

    #[test]
    fn a_missing_explicit_file_reports_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.md");
        let error = load(
            dir.path(),
            &ProjectContext {
                discover: false,
                files: vec![path],
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing.md"));
    }

    #[test]
    fn the_exact_total_budget_is_accepted_but_its_final_separator_cannot_exceed_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for index in 0..4 {
            let path = dir.path().join(format!("{index}.md"));
            std::fs::write(
                &path,
                if index < 3 {
                    "x".repeat(64 * 1024)
                } else {
                    String::new()
                },
            )
            .unwrap();
            files.push(path);
        }
        let last = files.last().unwrap().clone();
        let config = ProjectContext { discover: false, files };
        let remaining = MAX_CONTEXT_BYTES - load(dir.path(), &config).unwrap().len();
        std::fs::write(&last, "x".repeat(remaining)).unwrap();
        assert_eq!(load(dir.path(), &config).unwrap().len(), MAX_CONTEXT_BYTES);
        std::fs::write(&last, "x".repeat(remaining + 1)).unwrap();
        assert!(
            load(dir.path(), &config).is_err(),
            "the final separator exceeded the context budget"
        );
    }
}
