//! Model layer: message types, the streaming event protocol, and providers (pi: packages/ai).

pub mod faux;
pub mod openai;
pub mod provider;
pub mod retry;
pub mod sse;
pub mod system_prompt;
pub mod types;

pub use provider::{EventReceiver, Provider};
pub use system_prompt::{PREAMBLE, SectionChange, SystemMessage, SystemPrompt};
pub use types::*;
