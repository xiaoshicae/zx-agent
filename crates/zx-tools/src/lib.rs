//! Built-in tools, the built-in tool policies and the MCP client of zx-agent. The `zx-agent`
//! crate re-exports these modules under the same names.
//!
//! - [`tools`]: built-in tools
//! - [`policy`]: the built-in command and rule policies, and the hook that applies a policy
//! - [`mcp`]: the tools of external MCP servers, as an extension

// The modules name the model layer and the kernel by these crate-root paths.
use zx_ai as ai;
use zx_core::{agent, executor, extensions};

pub mod mcp;
pub mod policy;
pub mod tools;
