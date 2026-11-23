//! The application layer of zx-agent: agents configured from `config.toml` and the agent
//! files, for every host that shares these settings, such as the programs zx-agent and zx-im
//! or a web service, and the shutdown every such program needs. Nothing here knows a command
//! line, a terminal or a protocol: what startup and shutdown have to say is returned to the
//! host, never printed, and no message names a program's options.
//!
//! - [`Application`]: what the agents of a process share, read and connected once; it builds
//!   a [`ConfiguredAgent`] that keeps its sessions in a [`SessionPlace`]
//! - [`config`]: `config.toml`, the environment: the model, its provider and where commands
//!   run; and the agents, as files: instructions, tools, permissions and limits
//! - [`otel`]: OpenTelemetry traces of runs, exported when an OTLP endpoint is configured
//! - [`shutdown`]: how a process ends: signals cancel the runs, which are read to their end,
//!   and the trace export sends what remains
//!
//! Inside, `assembly` makes agents from config.toml, the agent file and what a host says over
//! them ([`Overrides`]), with what each session gets for itself; [`Application`] is its entry.

mod assembly;
pub mod config;
pub mod otel;
pub mod shutdown;

pub use self::assembly::{
    Application, Approvals, BuiltProvider, ConfiguredAgent, ExecutorFactory, ExecutorSession, NoModel, Overrides,
    ProviderFactory, ProviderInput, SessionPlace, SessionTools, resolve_cwd,
};
