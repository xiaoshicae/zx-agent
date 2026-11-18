//! The `zx-im` program. All of it is in the library, `zx_im`.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    zx_im::main().await
}
