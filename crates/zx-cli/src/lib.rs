//! The command line of zx-agent: the program `zx-agent` (`src/main.rs`) with its
//! subcommands `run`, `chat`, `acp`, `im` and `sessions`, built on the `zx-agent` library.
//!
//! This library holds the parts of it that do not depend on one invocation, which the
//! program and its tests share:
//!
//! - [`config`]: `config.toml`, the environment: the model, its provider and where commands
//!   run; and the agents, as files: instructions, tools, permissions and limits
//! - [`otel`]: OpenTelemetry traces of runs, exported when an OTLP endpoint is configured

pub mod config;
pub mod otel;
