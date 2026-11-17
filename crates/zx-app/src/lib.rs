//! The application layer of zx-agent: agents configured from `config.toml` and the agent
//! files, for the command line and for any other host that shares its settings, such as a
//! web service. Nothing here knows a command line, a terminal or a protocol: startup notices
//! are returned to the host, never printed, and no message names a command-line option.
//!
//! - [`Application`]: what the agents of a process share, read and connected once; it builds
//!   a [`ConfiguredAgent`] that keeps its sessions in a [`SessionPlace`]
//! - [`config`]: `config.toml`, the environment: the model, its provider and where commands
//!   run; and the agents, as files: instructions, tools, permissions and limits
//! - [`otel`]: OpenTelemetry traces of runs, exported when an OTLP endpoint is configured
//!
//! Inside, `assembly` makes an agent from config.toml, the agent file and what one start says
//! over them ([`Overrides`]), with what each session gets for itself, and `application` is
//! the entry over it.

pub mod config;
pub mod otel;

mod application;
mod assembly;

pub use application::Application;
pub use assembly::{Approvals, ConfiguredAgent, NoModel, Overrides, PlaceKind, SessionPlace, resolve_cwd};

// How every variable named in the configuration is read, for the command line's IM entry,
// which reads the platform tokens the same way. Not part of the API.
#[doc(hidden)]
pub use assembly::env_value;
