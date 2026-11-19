//! Helpers shared by the integration tests of the workspace's packages, as a library: a
//! package names it in `[dev-dependencies]` and its tests use it as `zx_testing::…`. A
//! crate's own unit tests do not use it: they would see a second copy of the crate under
//! test. This is the index; how to use each helper is in its own documentation.
//!
//! The modules whose helpers are re-exported here, under the names the tests use. A module
//! uses only modules listed before it, named in brackets where it uses any:
//! - `records`: messages a test writes ([`finished_answer`], [`tool_call_block`],
//!   [`tool_call_answer`], [`answer_reporting`], [`summary_json`]) and what it reads back:
//!   [`summarize`], [`replayed_requests`], [`json_lines`], [`read_lines`]
//! - `stand_ins`: in place of the real thing, [`EchoTool`], [`NotifyingProvider`], which says
//!   when a request starts, and [`hook_context`], to call a hook directly
//! - `sessions`: where a test's session comes from ([`Start`], [`Seed`], [`seeded_session`]),
//!   with the records only the kernel writes, such as a crash leaves them
//!   ([`Seed::append_record`], [`Seed::record_tool_call_started`]), and where it is kept:
//!   [`LogStorage`] with [`RefusingLog`], and session files
//!   ([`create_session_file`], [`open_session_file`], [`read_session_file`], [`header`],
//!   [`header_in`])
//! - `runner` (sessions): driving a session: [`RunnerSetup`] with its defaults ([`config`],
//!   [`test_limits`], [`fast_retry`]) gives a [`Driven`], which reads each run to its end,
//!   collects its events ([`Collect`]) and has an [`Approver`] answer what it [`Asked`];
//!   shorthands [`run_prompt`], [`run_prompt_with`], [`run_approved`]; and [`refusal`],
//!   [`take_queued_inputs`]
//!
//! The modules a test names by path, since names repeat among them:
//! - [`mock_server`]: a local fake model service ([`MockServer`](mock_server::MockServer)):
//!   SSE, HTTP errors, a stream that stalls after some data or ends when told, and waiting
//!   for a number of requests
//! - [`fake_mcp`]: an MCP server written from the specification, without rmcp
//! - [`fake_slack`], [`fake_feishu`]: the fake chat platforms
//! - [`http`]: reading and answering one HTTP request, and a listener that never answers
//! - [`program`]: a program a test runs, such as the compiled `zx-agent` or `zx-im`: the
//!   command that starts it apart from the developer's environment, the zx-agent home a
//!   test gives it, and what it leaves behind; its [`stdout`], [`stderr`] and [`signal`] are
//!   re-exported here
//! - [`summary_eval`] (sessions, runner): the summary evaluation, shared by the paid
//!   evaluation and the regular tests
//!
//! Elsewhere: the scripted model `FauxProvider` (`crates/zx-ai/src/faux.rs`; how the tests
//! turn it on is in `docs/TESTS.md`), `MemoryStorage` for a session store without files, what
//! only the program's tests can have (`crates/zx-cli/tests/common/mod.rs`: the compiled
//! `zx-agent` and a pseudo-terminal), running a chat channel
//! (`crates/zx-im/tests/common/im.rs`), and the unit tests' own `test_support` modules in
//! `zx-core`'s `transcript` and `zx-builtin`'s `jsonl`.

pub mod fake_feishu;
pub mod fake_mcp;
pub mod fake_slack;
pub mod http;
pub mod mock_server;
pub mod program;
pub mod summary_eval;

mod records;
mod runner;
mod sessions;
mod stand_ins;

#[cfg(unix)]
pub use program::signal;
pub use program::{stderr, stdout};
pub use records::{
    answer_reporting, finished_answer, json_lines, read_lines, replayed_requests, summarize, summary_json,
    tool_call_answer, tool_call_block,
};
pub use runner::{
    Approver, Asked, Collect, Driven, RunnerSetup, config, fast_retry, refusal, run_approved, run_prompt,
    run_prompt_with, take_queued_inputs, test_limits,
};
pub use sessions::{
    LogStorage, RefusingLog, Seed, Start, create_session_file, header, header_in, open_session_file, read_session_file,
    seeded_session,
};
pub use stand_ins::{EchoTool, NotifyingProvider, hook_context};
