//! Everything of zx-agent that runs outside the kernel but ships with it: the built-in tools,
//! where they run, the built-in policies and hooks, and the MCP client. The `zx-agent` crate
//! re-exports these modules under the same names.
//!
//! - [`tools`]: built-in tools
//! - [`executor`]: where built-in tools run commands and read or write files
//! - [`output_store`]: long tool results saved to files, paged with `read_output`
//! - [`policy`]: the built-in command and rule policies, and the hook that applies a policy
//! - [`continuation`]: the hook that continues an answer cut off at the output token limit
//! - [`redact`]: the hook that redacts known secrets from tool output
//! - [`mcp`]: the tools of external MCP servers, as an extension

// The modules name the model layer, the shared helpers and the kernel by these crate-root paths.
use zx_ai::{self as ai, sync, text};
use zx_core::{agent, extensions, prompt};

pub mod continuation;
pub mod executor;
#[doc(hidden)]
pub mod files;
pub mod mcp;
pub mod output_store;
pub mod policy;
pub mod redact;
pub mod tools;
