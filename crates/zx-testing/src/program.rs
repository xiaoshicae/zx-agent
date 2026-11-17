//! A program a test runs, such as the compiled `zx-agent`: the command that starts it apart
//! from the developer's environment, the zx-agent home a test gives it, and what it leaves
//! behind. Cargo gives the path of a program only to the tests of its own package
//! (`env!("CARGO_BIN_EXE_<name>")`), so the tests pass it in; the tests of every program of
//! the workspace start theirs alike.

use std::path::{Path, PathBuf};

/// `program`, a binary built for the test, started in `cwd` with the zx-agent home `home`,
/// and killed when the test drops it. The caller adds the arguments, the model's address and
/// the key.
///
/// It runs without the variables a developer may have set for zx-agent, and finds its
/// configuration only in `home`, so that no `config.toml`, agent or key of the developer's
/// reaches a test.
pub fn command(program: &Path, cwd: &Path, home: Home<'_>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .current_dir(cwd)
        .kill_on_drop(true)
        .env_remove("ZX_MODEL")
        .env_remove("ZX_BASE_URL")
        // The default variable of the API key: a developer's key must not reach a fake model.
        .env_remove("OPENAI_API_KEY");
    match home {
        Home::Agent(dir) => command.env("ZX_AGENT_HOME", dir),
        Home::User(dir) => command.env("HOME", dir).env_remove("ZX_AGENT_HOME"),
    };
    command
}

/// Where a test's program finds its zx-agent home.
#[derive(Clone, Copy)]
pub enum Home<'a> {
    /// `ZX_AGENT_HOME` names this directory, such as an [`AgentHome`].
    Agent(&'a Path),
    /// `HOME` is this directory and `ZX_AGENT_HOME` is unset: the zx-agent home is its
    /// `.zx-agent`, where chat and acp keep their sessions by default.
    User(&'a Path),
}

/// The config.toml of a test against a fake model: `top` before the `[provider]` table, the
/// API key in the variable `key_env`, no retries, and `rest` after these provider keys (more
/// of them, or further tables). The model's address goes in `ZX_BASE_URL`.
pub fn fake_model_config(key_env: &str, top: &str, rest: &str) -> String {
    format!("{top}\n[provider]\napiKeyEnv = \"{key_env}\"\nmaxRetries = 0\n{rest}\n")
}

/// A zx-agent home directory of its own, for `ZX_AGENT_HOME`: a `config.toml` and the
/// user's agents, which replace the built-in ones. Removed on drop.
pub struct AgentHome {
    dir: tempfile::TempDir,
}

impl AgentHome {
    /// A home whose `config.toml` holds `config`.
    pub fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), config);
        Self { dir }
    }

    /// A home without `config.toml` whose `agents/<builtin>.md` extends that built-in agent
    /// with `front`; see [`write_agent`].
    pub fn agent(builtin: &str, front: &str) -> Self {
        Self::new("").with_agent(builtin, front)
    }

    /// This home, with `agents/<builtin>.md` as [`write_agent`] writes it.
    pub fn with_agent(self, builtin: &str, front: &str) -> Self {
        write_agent(self.path(), builtin, front);
        self
    }

    /// The directory, as `ZX_AGENT_HOME` takes it.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Write `config` as the `config.toml` of the zx-agent home `home`, such as `<HOME>/.zx-agent`
/// of a test that sets `HOME`.
pub fn write_config(home: &Path, config: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("config.toml"), config).unwrap();
}

/// Write `agents/<builtin>.md` in the zx-agent home `home`: an agent that extends the built-in
/// agent of that name with `front`, the rest of its front matter, and keeps its instructions.
/// Settings the command line does not take, such as permissions, context files or summaries,
/// reach the program this way.
pub fn write_agent(home: &Path, builtin: &str, front: &str) {
    write_agent_file(
        home,
        builtin,
        &agent_text(&format!("extends = \"{builtin}\"\n{front}"), ""),
    );
}

/// Write `text` as `agents/<name>.md` in the zx-agent home `home`, and return its path.
pub fn write_agent_file(home: &Path, name: &str, text: &str) -> PathBuf {
    let agents = home.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let path = agents.join(format!("{name}.md"));
    std::fs::write(&path, text).unwrap();
    path
}

/// The text of an agent file with the front matter `front` and the instructions `body`.
pub fn agent_text(front: &str, body: &str) -> String {
    format!("+++\n{front}\n+++\n{body}")
}

/// Where chat and acp keep the sessions of `cwd` under `home` by default, spelled out as pi
/// spells it.
pub fn session_dir(home: &Path, cwd: &Path) -> PathBuf {
    let encoded = cwd.display().to_string().trim_start_matches('/').replace('/', "-");
    home.join(".zx-agent/sessions").join(format!("--{encoded}--"))
}

/// Write a session file in `dir`: the session `id`, started in `cwd`, with one prompt, last
/// written at `at_ms`. A test gets a session of a known age this way, whatever the clock
/// says, and the program reads it as one of its own.
pub fn seed_session(dir: &Path, id: &str, cwd: &Path, at_ms: u64, prompt: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let header = serde_json::json!({"type": "session", "version": zx_agent::transcript::SESSION_VERSION, "id": id,
        "timestamp": at_ms - 1, "cwd": cwd.display().to_string(), "model": "mock"});
    let message = serde_json::json!({"type": "message", "id": format!("{id}-1"), "parentId": null,
        "timestamp": at_ms, "message": {"role": "user", "content": prompt, "timestamp": at_ms}});
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, format!("{header}\n{message}\n")).unwrap();
    path
}

/// The session files in `dir`, by name; none when the directory does not exist.
pub fn session_files(dir: &Path) -> Vec<PathBuf> {
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

/// What a finished program wrote to stdout, decoded lossily.
pub fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// What a finished program wrote to stderr, decoded lossily.
pub fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Send `name` (as `kill` spells it, such as `TERM`) to `child`.
#[cfg(unix)]
pub async fn signal(child: &tokio::process::Child, name: &str) {
    assert!(
        tokio::process::Command::new("kill")
            .args([name, &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
}
