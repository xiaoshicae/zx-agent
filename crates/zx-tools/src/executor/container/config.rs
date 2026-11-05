//! How a container is started and described: its settings, the network it joins, what it
//! reports about itself and the prompt section that tells the model where commands run.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::prompt::{pretty_name, run_started};

/// The user commands run as when the agent itself runs as root, or off Unix: nobody.
pub(super) const NOBODY: &str = "65534:65534";

/// Whether the container may change a part of its file system. In configuration files,
/// `readWrite` or `readOnly`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    #[default]
    ReadWrite,
    ReadOnly,
}

/// How to start the container. Paths are absolute host paths, mounted at the same paths.
#[derive(Clone, Debug)]
pub struct ContainerConfig {
    /// Image to run. It must already be present locally; nothing is pulled.
    pub image: String,
    /// Docker client program; looked up on `PATH` unless it is a path.
    pub docker: PathBuf,
    /// Mounted at the same path; commands start here. Everything in it is readable inside the
    /// container, and with [`Access::ReadWrite`] changeable, so keep files the container must
    /// not read or change, such as the session file, elsewhere.
    pub workspace: PathBuf,
    /// Whether commands may change the workspace, which holds the host's own files.
    pub workspace_access: Access,
    /// Whether commands may change the container's own files, such as installed packages.
    /// Changes are discarded with the container. With [`Access::ReadOnly`] only a `/tmp` in
    /// memory is writable.
    pub root_access: Access,
    /// Docker network the container joins.
    pub network: Network,
    /// Shell that commands run with; the image must provide it.
    pub shell: String,
    /// `uid:gid` that commands run as. Must not be root: the keep-alive processes run as root,
    /// and cleanup stops every process of this user.
    pub user: String,
    /// Memory limit in Docker's syntax, such as `1g`.
    pub memory: String,
    /// Maximum number of processes in the container.
    pub pids_limit: u32,
    /// Value of the `zx-agent.session` label, for example the session ID.
    pub label: String,
}

impl ContainerConfig {
    /// Settings for `image` with the defaults: a writable workspace and root, the `bridge`
    /// network, the agent's own `uid:gid` (so that files written in the workspace belong to
    /// the agent's user; nobody, `65534:65534`, when the agent runs as root or off Unix), 1 GiB
    /// of memory, 256 processes, bash, and the `docker` client found on `PATH`.
    pub fn new(image: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            docker: PathBuf::from("docker"),
            workspace: workspace.into(),
            workspace_access: Access::ReadWrite,
            root_access: Access::ReadWrite,
            network: Network::Bridge,
            shell: "bash".into(),
            user: default_user(),
            memory: "1g".into(),
            pids_limit: 256,
            label: String::new(),
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        ensure!(!self.image.trim().is_empty(), "container image is empty");
        ensure!(!self.shell.trim().is_empty(), "container shell is empty");
        ensure!(!self.user.trim().is_empty(), "container user is empty");
        // The start-up probe also checks the effective UID, which catches names mapped to 0.
        let uid = self.user.split(':').next().unwrap_or_default().trim();
        ensure!(
            uid != "root" && uid.parse::<u32>() != Ok(0),
            "container user {:?} is root; commands must run as an unprivileged user",
            self.user
        );
        ensure!(!self.memory.trim().is_empty(), "container memory limit is empty");
        ensure!(self.pids_limit > 0, "container process limit must be greater than zero");
        ensure!(
            self.workspace.is_absolute(),
            "container mount {} is not absolute",
            self.workspace.display()
        );
        // --mount separates its fields with commas.
        let text = self.workspace.to_str().context("container mount paths must be UTF-8")?;
        ensure!(
            !text.contains([',', '"', '\n']),
            "container mount {text:?} contains a comma, quote or newline"
        );
        Ok(())
    }
}

/// The Docker network a container joins. `host` and `container:<name>` are never among them:
/// they would share a network namespace outside the sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Network {
    /// Docker's default bridge: the container can reach the network.
    Bridge,
    /// No network.
    None,
    /// A user-defined network, by name.
    Named(String),
}

impl FromStr for Network {
    type Err = anyhow::Error;

    /// `bridge`, `none` (both case-insensitive) or the name of a user-defined network;
    /// surrounding whitespace is ignored.
    fn from_str(text: &str) -> Result<Self> {
        let name = text.trim();
        ensure!(!name.is_empty(), "container network is empty");
        let lower = name.to_ascii_lowercase();
        ensure!(
            lower != "host" && !lower.starts_with("container:"),
            "container network {text:?} would share a network namespace outside the sandbox; use bridge, none or a network of your own"
        );
        Ok(match lower.as_str() {
            "bridge" => Self::Bridge,
            "none" => Self::None,
            _ => Self::Named(name.to_string()),
        })
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bridge => f.write_str("bridge"),
            Self::None => f.write_str("none"),
            Self::Named(name) => f.write_str(name),
        }
    }
}

/// Whether `network` names a network a container may join; configuration layers call this to
/// report a mistake before anything starts.
pub fn check_network(network: &str) -> Result<()> {
    network.parse::<Network>().map(|_| ())
}

/// What the container reports about itself at start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Platform {
    /// Contents of `/etc/os-release`; empty when the image has none.
    pub os_release: String,
    /// `uname -r`: the kernel the container shares with its host.
    pub kernel: String,
    /// `uname -m`.
    pub arch: String,
}

/// The agent's own `uid:gid`, so that files written in the workspace belong to its user; nobody
/// when the agent runs as root, since commands must not.
#[cfg(unix)]
pub(super) fn default_user() -> String {
    // SAFETY: getuid and getgid take no arguments, always succeed and touch no memory.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if uid == 0 {
        NOBODY.into()
    } else {
        format!("{uid}:{gid}")
    }
}

#[cfg(not(unix))]
pub(super) fn default_user() -> String {
    NOBODY.into()
}

pub(super) fn parse_platform(output: &str) -> Platform {
    let (head, os_release) = output.split_once("--- os-release\n").unwrap_or((output, ""));
    let field = |key: &str| {
        head.lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    Platform {
        os_release: os_release.to_string(),
        kernel: field("kernel="),
        arch: field("arch="),
    }
}

/// The `environment` section of a prompt when commands run in the container: its system, the
/// workspace (`config.workspace`, where commands start), and what commands may change and
/// reach, as `config` sets it up.
pub fn environment_section(config: &ContainerConfig, platform: &Platform) -> String {
    let name = pretty_name(&platform.os_release).unwrap_or_else(|| "Linux".to_string());
    let workspace = match config.workspace_access {
        Access::ReadWrite => "writable; changes are made to the host's files",
        Access::ReadOnly => "read-only",
    };
    let network = match &config.network {
        Network::None => "without network access".to_string(),
        network => format!("with network access (Docker network {network})"),
    };
    let files = match config.root_access {
        Access::ReadWrite => "the container's own files are writable and are discarded with the container",
        Access::ReadOnly => "outside the working directory only /tmp is writable",
    };
    format!(
        "OS: {name}, Linux kernel {} ({}), in a container from image {}\nRun started: {}\n\
         Working directory: {} ({workspace})\n\
         Isolation: commands run as user {} {network}; {files}.",
        platform.kernel,
        platform.arch,
        config.image,
        run_started(),
        config.workspace.display(),
        config.user,
    )
}
