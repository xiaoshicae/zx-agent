//! Model layer: message types, the streaming event protocol, and providers (pi: packages/ai).
//!
//! The lowest crate of the zx-agent workspace; the `zx-agent` crate re-exports it as
//! `zx_agent::ai`. It also holds two helpers every layer uses, `sync` and `text`, which are
//! public only so that the other workspace crates can reach them.

pub mod faux;
pub mod openai;
pub mod provider;
pub mod retry;
pub mod sse;
#[doc(hidden)]
pub mod sync;
pub mod system_prompt;
#[doc(hidden)]
pub mod text;
pub mod types;

pub use provider::{EventReceiver, Provider};
pub use system_prompt::{PREAMBLE, SectionChange, SystemMessage, SystemPrompt};
pub use types::*;
