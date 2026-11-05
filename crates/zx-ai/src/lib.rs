//! Model layer: message types, the streaming event protocol and the provider contract (pi:
//! packages/ai).
//!
//! The lowest crate of the zx-agent workspace; the `zx-agent` crate re-exports it as
//! `zx_agent::ai`. Providers live in crates of their own, such as `zx-ai-openai`, so that an
//! embedder of the kernel does not compile an HTTP client it does not use. This crate also
//! holds two helpers every layer uses, `sync` and `text`, which are public only so that the
//! other workspace crates can reach them.

pub mod faux;
pub mod provider;
pub mod retry;
#[doc(hidden)]
pub mod sync;
pub mod system_prompt;
#[doc(hidden)]
pub mod text;
pub mod types;

pub use provider::{EventReceiver, Provider};
pub use system_prompt::{PREAMBLE, SectionChange, SystemMessage, SystemPrompt};
pub use types::*;
