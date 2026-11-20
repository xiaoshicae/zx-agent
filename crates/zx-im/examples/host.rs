//! A host's own zx-im: the program with the built-in chat platforms, whose agents are also
//! given a prompt section of the host's. A platform of the host's own is added the same way,
//! with `Program::register_platform`; see docs/LIBRARY.md.
//!
//! Run: cargo run -p zx-im --example host -- --config im.toml

use std::process::ExitCode;

/// What every conversation's agent is told besides its agent file.
const HOUSE_RULES: &str = "Answer in the language of the question.";

#[tokio::main]
async fn main() -> ExitCode {
    zx_im::Program::new()
        .customize(|app| app.add_prompt_section("house_rules", HOUSE_RULES))
        .run()
        .await
}
