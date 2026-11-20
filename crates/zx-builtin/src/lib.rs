//! Everything of zx-agent that runs outside the kernel but ships with it: the built-in tools,
//! where they run, the built-in policies and hooks, the storage of session files, and the
//! MCP client. The `zx-agent` crate re-exports this crate as `builtin`.
//!
//! - [`tools`]: built-in tools
//! - [`executor`]: where built-in tools run commands and read or write files
//! - [`jsonl`]: sessions kept as JSONL files, one file per session
//! - [`output_store`]: long tool results saved to files, paged with `read_output`
//! - [`policy`]: the built-in command and rule policies
//! - [`continuation`]: the hook that continues an answer cut off at the output token limit
//! - [`redact`]: the hook that redacts known secrets from tool output
//! - `files`: bounded reads of regular files that never wait on a FIFO, for the local
//!   executor, the files that tools page, the redaction and zx-app's configuration files;
//!   hidden, not part of the API, though `tools::paging` re-exports its `Links`
//! - `calendar`: dates of the Gregorian calendar, for the times that file names and prompts
//!   show
//! - [`mcp`]: the tools of external MCP servers, as an extension; the feature `mcp`, on by
//!   default
//! - `test_support`, compiled for tests on Unix only: the FIFOs that tests put where a file
//!   is expected

// The modules name the model layer and the shared helpers by these crate-root paths.
use zx_ai as ai;
use zx_util::{sync, text};

mod calendar;
pub mod continuation;
pub mod executor;
#[doc(hidden)]
pub mod files;
pub mod jsonl;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod output_store;
pub mod policy;
pub mod redact;
#[cfg(all(test, unix))]
mod test_support;
pub mod tools;
