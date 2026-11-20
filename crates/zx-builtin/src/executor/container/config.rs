//! How a container is started and described: its settings and their checks, with what the
//! start found when the command user cannot use the workspace, the network it joins, what it
//! reports about itself and the prompt section that tells the model where commands run.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::executor::environment::{OPENED_LABEL, OS_LABEL, WORKING_DIRECTORY_LABEL, current_time, pretty_name};

/// The user commands run as when the agent itself runs as root, or off Unix: nobody.
pub(super) const NOBODY: &str = "65534:65534";
/// The prefix of the line of the start-up probe's output that holds `uname -m`.
pub(super) const ARCH_FIELD: &str = "arch=";
/// The prefix of the line of the start-up probe's output that holds `uname -r`.
pub(super) const KERNEL_FIELD: &str = "kernel=";
/// The line of the start-up probe's output after which the image's `/etc/os-release` follows.
pub(super) const OS_RELEASE_HEADER: &str = "--- os-release";

/// Whether the container may change a part of its file system. In configuration files,
/// `readWrite` or `readOnly`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    #[default]
    ReadWrite,
    ReadOnly,
}

/// How to start a container: what every container of a host has alike. The workspace, which
/// differs from one container to the next, is given to
/// [`ContainerExecutor::start`](super::ContainerExecutor::start).
#[derive(Clone, Debug)]
pub struct ContainerConfig {
    /// Image to run. It must already be present locally; nothing is pulled.
    pub image: String,
    /// Docker client program; looked up on `PATH` unless it is a path.
    pub docker: PathBuf,
    /// Whether commands may change the workspace, which holds the host's own files.
    pub workspace_access: Access,
    /// Whether commands may change the container's own files, such as installed packages.
    /// Changes are discarded with the container. With [`Access::ReadOnly`] only a `/tmp` in
    /// memory is writable.
    pub root_access: Access,
    /// Docker network the container joins.
    pub network: Network,
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
    /// The memory limit by default, in Docker's syntax.
    pub const DEFAULT_MEMORY: &'static str = "1g";
    /// The limit on processes by default.
    pub const DEFAULT_PIDS_LIMIT: u32 = 256;

    /// Settings for `image` with the defaults: a writable workspace and root, the `bridge`
    /// network, the agent's own `uid:gid` (so that files written in the workspace belong to
    /// the agent's user; nobody, `65534:65534`, when the agent runs as root or off Unix),
    /// [`Self::DEFAULT_MEMORY`] of memory, [`Self::DEFAULT_PIDS_LIMIT`] processes, no label,
    /// and the `docker` client found on `PATH`.
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            docker: PathBuf::from("docker"),
            workspace_access: Access::ReadWrite,
            root_access: Access::ReadWrite,
            network: Network::Bridge,
            user: default_user(),
            memory: Self::DEFAULT_MEMORY.into(),
            pids_limit: Self::DEFAULT_PIDS_LIMIT,
            label: String::new(),
        }
    }

    /// Check the settings, as [`ContainerExecutor::start`](super::ContainerExecutor::start)
    /// does before Docker runs: an image, a user and a memory limit that are not empty, a user
    /// that is not root, room for a process, and a network of the container's own. The start
    /// also checks the effective UID inside the container, which catches names mapped to 0.
    pub fn validate(&self) -> Result<(), InvalidContainerConfig> {
        if self.image.trim().is_empty() {
            return Err(InvalidContainerConfig::EmptyImage);
        }
        if self.user.trim().is_empty() {
            return Err(InvalidContainerConfig::EmptyUser);
        }
        // Splitting yields at least one piece, so the default is never used.
        let uid = self.user.split(':').next().unwrap_or_default().trim();
        if uid == "root" || uid.parse::<u32>() == Ok(0) {
            return Err(InvalidContainerConfig::RootUser(self.user.clone()));
        }
        if self.memory.trim().is_empty() {
            return Err(InvalidContainerConfig::EmptyMemory);
        }
        if self.pids_limit == 0 {
            return Err(InvalidContainerConfig::ZeroPidsLimit);
        }
        self.network.check()
    }
}

/// A setting of a [`ContainerConfig`] that cannot be used. `Display` names the field as the
/// struct does; a host that reads the settings from a file of its own matches the variant to
/// name its own key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidContainerConfig {
    /// `image` is empty.
    EmptyImage,
    /// `user` is empty.
    EmptyUser,
    /// `user`, given here, is root, by name or by UID 0.
    RootUser(String),
    /// `memory` is empty.
    EmptyMemory,
    /// `pids_limit` is zero.
    ZeroPidsLimit,
    /// `network` names no network.
    EmptyNetwork,
    /// `network`, given here, would share a network namespace outside the sandbox: `host` or
    /// `container:<name>`.
    SharedNetwork(String),
}

impl fmt::Display for InvalidContainerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyImage => f.write_str("image is empty"),
            Self::EmptyUser => f.write_str("user is empty"),
            Self::RootUser(user) => write!(f, "user {user:?} is root; commands must run as an unprivileged user"),
            Self::EmptyMemory => f.write_str("memory is empty"),
            Self::ZeroPidsLimit => f.write_str("pids_limit must be greater than zero"),
            Self::EmptyNetwork => f.write_str("network is empty"),
            Self::SharedNetwork(network) => write!(
                f,
                "network {network:?} would share a network namespace outside the sandbox; use bridge, none or a network of your own"
            ),
        }
    }
}

impl std::error::Error for InvalidContainerConfig {}

/// The command user of a container cannot use its workspace as it is mounted, which
/// [`ContainerExecutor::start`](super::ContainerExecutor::start) found. `Display` names the
/// settings that change it as [`ContainerConfig`] does; a host that reads them from a file of
/// its own finds this error by downcasting and names its own keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnusableWorkspace {
    /// `user` cannot read the workspace.
    Unreadable { user: String, workspace: PathBuf },
    /// `user` cannot write the workspace, which is mounted writable.
    Unwritable { user: String, workspace: PathBuf },
}

impl fmt::Display for UnusableWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { user, workspace } => write!(
                f,
                "user {user} cannot read the workspace {}; make it readable by others, or set `user` to one who can read it",
                workspace.display()
            ),
            Self::Unwritable { user, workspace } => write!(
                f,
                "user {user} cannot write the workspace {}; set `user` to its owner, or mount it read-only (`workspace_access: Access::ReadOnly`)",
                workspace.display()
            ),
        }
    }
}

impl std::error::Error for UnusableWorkspace {}

/// Check that `workspace` can be mounted at its own path: absolute, and UTF-8 without the
/// characters that `--mount` would read as the end of a field.
pub(super) fn check_workspace(workspace: &Path) -> Result<()> {
    ensure!(
        workspace.is_absolute(),
        "workspace {} is not absolute",
        workspace.display()
    );
    let text = workspace.to_str().context("the workspace path must be UTF-8")?;
    ensure!(
        !text.contains([',', '"', '\n']),
        "workspace {text:?} contains a comma, quote or newline"
    );
    Ok(())
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

impl Network {
    /// Refuse a named network that is empty or shares a namespace outside the sandbox.
    fn check(&self) -> Result<(), InvalidContainerConfig> {
        let Self::Named(name) = self else {
            return Ok(());
        };
        let lower = name.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return Err(InvalidContainerConfig::EmptyNetwork);
        }
        if lower == "host" || lower.starts_with("container:") {
            return Err(InvalidContainerConfig::SharedNetwork(name.clone()));
        }
        Ok(())
    }
}

impl FromStr for Network {
    type Err = InvalidContainerConfig;

    /// `bridge`, `none` (both case-insensitive) or the name of a user-defined network;
    /// surrounding whitespace is ignored.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let name = text.trim();
        let network = match name.to_ascii_lowercase().as_str() {
            "bridge" => Self::Bridge,
            "none" => Self::None,
            _ => Self::Named(name.to_string()),
        };
        network.check()?;
        Ok(network)
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

/// What the container reports about itself at start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Platform {
    /// Contents of `/etc/os-release`; empty when the image has none.
    pub(super) os_release: String,
    /// `uname -r`: the kernel the container shares with its host.
    pub(super) kernel: String,
    /// `uname -m`.
    pub(super) arch: String,
}

/// The agent's own `uid:gid`, so that files written in the workspace belong to its user; nobody
/// when the agent runs as root, since commands must not.
#[cfg(unix)]
fn default_user() -> String {
    // SAFETY: getuid and getgid take no arguments, always succeed and touch no memory.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if uid == 0 {
        NOBODY.into()
    } else {
        format!("{uid}:{gid}")
    }
}

#[cfg(not(unix))]
fn default_user() -> String {
    NOBODY.into()
}

/// The platform the start-up probe printed in `output`.
pub(super) fn parse_platform(output: &str) -> Platform {
    let (head, os_release) = output
        .split_once(&format!("{OS_RELEASE_HEADER}\n"))
        .unwrap_or((output, ""));
    let field = |key: &str| {
        head.lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    Platform {
        os_release: os_release.to_string(),
        kernel: field(KERNEL_FIELD),
        arch: field(ARCH_FIELD),
    }
}

/// The `environment` section of a prompt when commands run in the container that `config`
/// started with `workspace` mounted: its system, the working directory `cwd`, and what
/// commands may change and reach. The workspace is named on its own line when `cwd` is
/// another directory.
pub(super) fn container_section(config: &ContainerConfig, workspace: &Path, platform: &Platform, cwd: &Path) -> String {
    let name = pretty_name(&platform.os_release).unwrap_or_else(|| "Linux".to_string());
    let access = match config.workspace_access {
        Access::ReadWrite => "writable; changes are made to the host's files",
        Access::ReadOnly => "read-only",
    };
    let (directories, mounted) = if cwd == workspace {
        (
            format!("{WORKING_DIRECTORY_LABEL}{} ({access})", cwd.display()),
            "the working directory",
        )
    } else {
        (
            format!(
                "{WORKING_DIRECTORY_LABEL}{}\nWorkspace: {} ({access})",
                cwd.display(),
                workspace.display()
            ),
            "the workspace",
        )
    };
    let network = match &config.network {
        Network::None => "without network access".to_string(),
        network => format!("with network access (Docker network {network})"),
    };
    let files = match config.root_access {
        Access::ReadWrite => "the container's own files are writable and are discarded with the container".to_string(),
        Access::ReadOnly => format!("outside {mounted} only /tmp is writable"),
    };
    format!(
        "{OS_LABEL}{name}, Linux kernel {} ({}), in a container from image {}\n{OPENED_LABEL}{}\n{directories}\n\
         Isolation: commands run as user {} {network}; {files}.",
        platform.kernel,
        platform.arch,
        config.image,
        current_time(),
        config.user,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ContainerConfig {
        ContainerConfig::new("debian:bookworm-slim")
    }

    #[cfg(unix)]
    #[test]
    fn commands_run_as_the_agents_own_user_unless_it_is_root() {
        use std::os::unix::fs::MetadataExt;
        // A new file belongs to the process's user. Its group may be the directory's, as on
        // macOS, so only the user is compared.
        let file = tempfile::NamedTempFile::new().unwrap();
        let uid = file.as_file().metadata().unwrap().uid();
        let user = ContainerConfig::new("image").user;
        if uid == 0 {
            assert_eq!(user, NOBODY);
        } else {
            assert_eq!(user.split(':').next(), Some(uid.to_string().as_str()), "{user}");
        }
    }

    #[test]
    fn invalid_settings_are_rejected_before_docker_runs() {
        let cases = [
            (ContainerConfig::new(" "), InvalidContainerConfig::EmptyImage),
            (
                ContainerConfig {
                    user: " ".into(),
                    ..config()
                },
                InvalidContainerConfig::EmptyUser,
            ),
            (
                ContainerConfig {
                    memory: String::new(),
                    ..config()
                },
                InvalidContainerConfig::EmptyMemory,
            ),
            (
                ContainerConfig {
                    pids_limit: 0,
                    ..config()
                },
                InvalidContainerConfig::ZeroPidsLimit,
            ),
            // A network built directly is held to the rule that parsing one follows.
            (
                ContainerConfig {
                    network: Network::Named("host".into()),
                    ..config()
                },
                InvalidContainerConfig::SharedNetwork("host".into()),
            ),
        ];
        for (config, expected) in cases {
            assert_eq!(config.validate(), Err(expected), "{config:?}");
        }
        assert_eq!(config().validate(), Ok(()));
    }

    #[test]
    fn workspaces_that_docker_cannot_mount_are_rejected_before_docker_runs() {
        for workspace in ["work", "/work,ro", "/work\"x"] {
            assert!(check_workspace(Path::new(workspace)).is_err(), "{workspace:?}");
        }
        check_workspace(Path::new("/work")).unwrap();
    }

    #[test]
    fn networks_shared_with_the_outside_are_refused() {
        for network in ["host", " HOST ", "container:other"] {
            let error = network.parse::<Network>().unwrap_err().to_string();
            assert!(error.contains("outside the sandbox"), "{network:?}: {error}");
        }
        let error = " ".parse::<Network>().unwrap_err().to_string();
        assert!(error.contains("is empty"), "{error}");
    }

    #[test]
    fn network_names_are_read_case_insensitively_and_written_back_as_docker_expects() {
        assert_eq!("bridge".parse::<Network>().unwrap(), Network::Bridge);
        assert_eq!(" None ".parse::<Network>().unwrap(), Network::None);
        assert_eq!("my-net".parse::<Network>().unwrap(), Network::Named("my-net".into()));
        assert_eq!(Network::None.to_string(), "none");
        assert_eq!(Network::Named("my-net".into()).to_string(), "my-net");
    }

    #[test]
    fn commands_must_not_run_as_root() {
        for user in ["0", "0:0", "00:1000", "root", "root:root", " 0 :0"] {
            let config = ContainerConfig {
                user: user.into(),
                ..config()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("is root"), "{user:?}: {error}");
        }
        for user in ["65534:65534", "1000", "nobody"] {
            let config = ContainerConfig {
                user: user.into(),
                ..config()
            };
            assert!(config.validate().is_ok(), "{user:?}");
        }
    }

    #[test]
    fn the_probe_output_describes_the_platform() {
        let platform = parse_platform(
            "arch=aarch64\nkernel=6.10.14-linuxkit\n--- os-release\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n",
        );
        assert_eq!(platform.arch, "aarch64");
        assert_eq!(platform.kernel, "6.10.14-linuxkit");
        assert_eq!(platform.os_release, "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n");
        assert_eq!(
            parse_platform("arch=x86_64\n"),
            Platform {
                arch: "x86_64".into(),
                ..Platform::default()
            }
        );
    }

    #[test]
    fn the_environment_section_describes_the_container_and_its_isolation() {
        let platform = Platform {
            os_release: "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n".into(),
            kernel: "6.10.14-linuxkit".into(),
            arch: "aarch64".into(),
        };
        let open = ContainerConfig {
            user: "1000:1000".into(),
            ..ContainerConfig::new("debian:bookworm-slim")
        };
        let app = Path::new("/srv/app");
        let section = container_section(&open, app, &platform, app);
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(
            lines[0],
            "OS: Debian GNU/Linux 12 (bookworm), Linux kernel 6.10.14-linuxkit (aarch64), in a container from image debian:bookworm-slim"
        );
        assert!(lines[1].starts_with("Session opened: "), "{section}");
        assert_eq!(
            lines[2],
            "Working directory: /srv/app (writable; changes are made to the host's files)"
        );
        assert_eq!(
            lines[3],
            "Isolation: commands run as user 1000:1000 with network access (Docker network bridge); \
             the container's own files are writable and are discarded with the container."
        );
        let unnamed = Platform::default();
        assert!(container_section(&open, app, &unnamed, app).starts_with("OS: Linux, Linux kernel"));
    }

    fn strict() -> ContainerConfig {
        ContainerConfig {
            user: "65534:65534".into(),
            workspace_access: Access::ReadOnly,
            root_access: Access::ReadOnly,
            network: Network::None,
            ..ContainerConfig::new("debian:bookworm-slim")
        }
    }

    #[test]
    fn a_strict_container_is_described_as_read_only_and_offline() {
        let app = Path::new("/srv/app");
        let section = container_section(&strict(), app, &Platform::default(), app);
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(lines[2], "Working directory: /srv/app (read-only)");
        assert_eq!(
            lines[3],
            "Isolation: commands run as user 65534:65534 without network access; outside the working directory only /tmp is writable."
        );
    }

    #[test]
    fn a_working_directory_other_than_the_workspace_is_told_apart_from_it() {
        let section = container_section(
            &strict(),
            Path::new("/srv/app"),
            &Platform::default(),
            Path::new("/srv/app/web"),
        );
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(
            lines[2..4],
            ["Working directory: /srv/app/web", "Workspace: /srv/app (read-only)"]
        );
        assert!(
            lines[4].ends_with("; outside the workspace only /tmp is writable."),
            "{section}"
        );
    }
}
