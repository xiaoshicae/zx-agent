//! Helpers shared by the tests of the program: the compiled `zx-agent`, the home directory a
//! test gives it, and what it leaves behind. The helpers of the library's tests
//! (`tests/common` at the repository root) are part of this module: the fake model service
//! and the other fake servers, scripted runs, and session files.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers; `expect` would fail in a binary that uses them all"
)]

#[path = "../../../../tests/common/mod.rs"]
mod library;

use std::path::{Path, PathBuf};

pub(crate) use library::*;

/// The compiled `zx-agent`, for the tests that drive the program.
///
/// It runs without the variables a developer may have set for zx-agent, and with a zx-agent
/// home directory that holds nothing, so that no `config.toml` or agent of the real home
/// reaches a test. A test that needs a home of its own starts the program with
/// [`zx_agent_in`].
pub(crate) fn zx_agent() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_zx-agent"));
    command
        .env("ZX_AGENT_HOME", EMPTY_HOME)
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        // The default variable of the API key: a developer's key must not reach a fake model.
        .env_remove("OPENAI_API_KEY");
    command
}

/// Where a test's program finds its zx-agent home.
#[derive(Clone, Copy)]
pub(crate) enum Home<'a> {
    /// `ZX_AGENT_HOME` names this directory, such as an [`AgentHome`].
    Agent(&'a Path),
    /// `HOME` is this directory and `ZX_AGENT_HOME` is unset: the zx-agent home is its
    /// `.zx-agent`, where chat and acp keep their sessions by default.
    User(&'a Path),
}

/// [`zx_agent`] started in `cwd` with the zx-agent home `home`, and killed when the test
/// drops it. The caller adds the arguments, the model's address and the key.
pub(crate) fn zx_agent_in(cwd: &Path, home: Home<'_>) -> tokio::process::Command {
    let mut command = zx_agent();
    command.current_dir(cwd).kill_on_drop(true);
    match home {
        Home::Agent(dir) => command.env("ZX_AGENT_HOME", dir),
        Home::User(dir) => command.env("HOME", dir).env_remove("ZX_AGENT_HOME"),
    };
    command
}

/// The config.toml of a test against a fake model: `top` before the `[provider]` table, the
/// API key in the variable `key_env`, no retries, and `rest` after these provider keys (more
/// of them, or further tables). The model's address goes in `ZX_BASE_URL`.
pub(crate) fn fake_model_config(key_env: &str, top: &str, rest: &str) -> String {
    format!("{top}\n[provider]\napiKeyEnv = \"{key_env}\"\nmaxRetries = 0\n{rest}\n")
}

/// The zx-agent home of [`zx_agent`]: nothing creates it, so it holds no configuration.
const EMPTY_HOME: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/zx-agent-empty-home");

/// A zx-agent home directory of its own, for `ZX_AGENT_HOME`: a `config.toml` and the
/// user's agents, which replace the built-in ones. Removed on drop.
pub(crate) struct AgentHome {
    dir: tempfile::TempDir,
}

impl AgentHome {
    /// A home whose `config.toml` holds `config`.
    pub(crate) fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), config);
        Self { dir }
    }

    /// A home without `config.toml` whose `agents/<builtin>.md` extends that built-in agent
    /// with `front`; see [`write_agent`].
    pub(crate) fn agent(builtin: &str, front: &str) -> Self {
        Self::new("").with_agent(builtin, front)
    }

    /// This home, with `agents/<builtin>.md` as [`write_agent`] writes it.
    pub(crate) fn with_agent(self, builtin: &str, front: &str) -> Self {
        write_agent(self.path(), builtin, front);
        self
    }

    /// The directory, as `ZX_AGENT_HOME` takes it.
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Write `config` as the `config.toml` of the zx-agent home `home`, such as `<HOME>/.zx-agent`
/// of a test that sets `HOME`.
pub(crate) fn write_config(home: &Path, config: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("config.toml"), config).unwrap();
}

/// Write `agents/<builtin>.md` in the zx-agent home `home`: an agent that extends the built-in
/// agent of that name with `front`, the rest of its front matter, and keeps its instructions.
/// Settings the command line does not take, such as permissions, context files or summaries,
/// reach the program this way.
pub(crate) fn write_agent(home: &Path, builtin: &str, front: &str) {
    write_agent_file(
        home,
        builtin,
        &agent_text(&format!("extends = \"{builtin}\"\n{front}"), ""),
    );
}

/// Write `text` as `agents/<name>.md` in the zx-agent home `home`, and return its path.
pub(crate) fn write_agent_file(home: &Path, name: &str, text: &str) -> PathBuf {
    let agents = home.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let path = agents.join(format!("{name}.md"));
    std::fs::write(&path, text).unwrap();
    path
}

/// The text of an agent file with the front matter `front` and the instructions `body`.
pub(crate) fn agent_text(front: &str, body: &str) -> String {
    format!("+++\n{front}\n+++\n{body}")
}

/// Where chat and acp keep the sessions of `cwd` under `home` by default, spelled out as pi
/// spells it.
pub(crate) fn session_dir(home: &Path, cwd: &Path) -> PathBuf {
    let encoded = cwd.display().to_string().trim_start_matches('/').replace('/', "-");
    home.join(".zx-agent/sessions").join(format!("--{encoded}--"))
}

/// The session files in `dir`, by name; none when the directory does not exist.
pub(crate) fn session_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read_dir
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect();
    files.sort();
    files
}

/// A pseudo-terminal: the master end for the test, and the slave end to give a child as
/// its terminal (stdin or stdout).
#[cfg(unix)]
pub(crate) fn pty() -> (tokio::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: both output pointers refer to initialized local integers, and null optional
    // name/termios/winsize pointers request the OS defaults. Success yields two fresh FDs.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: openpty succeeded; each descriptor has exactly one owner from here on.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    (tokio::fs::File::from_std(std::fs::File::from(master)), slave)
}
