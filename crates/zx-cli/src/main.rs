//! The `zx-agent` command. All of it is in the library, `zx_cli`.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    zx_cli::main().await
}
