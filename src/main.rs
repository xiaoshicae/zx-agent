//! The `zx-agent` command: run, chat, serve, sessions and acp.

use std::process::ExitCode;

use clap::Parser;

mod acp;
mod chat;
mod cli;
mod process;
mod render;
mod run;
mod serve;
mod sessions;

use cli::{Cli, Command};
use process::EXIT_USAGE;

#[tokio::main]
async fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    if let Err(error) = deny_same_user_inspection() {
        eprintln!("[security] cannot stop commands from reading this process's environment: {error}");
    }
    let result = match Cli::parse().command {
        Command::Run(args) => run::run(args).await,
        Command::Chat(args) => chat::run(args).await,
        Command::Serve(args) => serve::run(args).await,
        Command::Sessions(args) => sessions::run(args).await,
        Command::Acp(args) => acp::run(args).await,
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("zx-agent: {e:#}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Mark this process non-dumpable before any command runs. Commands run as the same user,
/// and ptrace access mode checks then refuse them this process's `/proc/<pid>/environ`,
/// which holds the API key, unless they have CAP_SYS_PTRACE (ptrace(2),
/// proc_pid_environ(5)). The same attribute turns off core dumps of this process
/// (PR_SET_DUMPABLE(2const)). See docs/p6-online-access-review.md, section 3.3.
#[cfg(target_os = "linux")]
fn deny_same_user_inspection() -> std::io::Result<()> {
    // prctl(2): int prctl(int op, ... /* unsigned long arg2, arg3, arg4, arg5 */).
    const SUID_DUMP_DISABLE: libc::c_ulong = 0;
    const UNUSED: libc::c_ulong = 0;
    // SAFETY: PR_SET_DUMPABLE reads only its integer arguments, passed with the type the
    // call expects; no pointer into this process is handed to the kernel.
    let status = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, SUID_DUMP_DISABLE, UNUSED, UNUSED, UNUSED) };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
