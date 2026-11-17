//! Model layer: message types, the streaming event protocol and the provider contract (pi:
//! packages/ai).
//!
//! The lowest crate of the zx-agent workspace; the `zx-agent` crate re-exports it as
//! `zx_agent::ai`. Providers live in crates of their own, such as `zx-ai-openai`, so that an
//! embedder of the kernel does not compile an HTTP client it does not use. The helpers every
//! layer shares, such as the lock and text bounds, are in `zx-util`, below this crate.
//!
//! The scripted provider of tests, `faux`, is compiled only for this crate's tests and with
//! the feature `testing`, which the tests of the crates built on this one turn on.

#[cfg(any(test, feature = "testing"))]
pub mod faux;
pub mod provider;
pub mod retry;
pub mod system_prompt;
pub mod types;

pub use provider::{EventReceiver, Provider};
pub use system_prompt::{PREAMBLE, SectionChange, SystemMessage, SystemPrompt};
pub use types::*;
