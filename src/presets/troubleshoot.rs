//! The unattended troubleshooting preset: the defaults of the `troubleshoot` profile, which
//! `zx-agent run` uses.

use std::time::Duration;

use crate::agent::HookChain;
use crate::extensions::ContinueOnLength;
use crate::runner::Limits;

/// Model requests allowed in one unattended run, including failed attempts.
pub const MAX_TURNS: u32 = 30;
/// Wall-clock limit for one unattended run, in seconds.
pub const TIMEOUT_SECS: u64 = 15 * 60;
/// Automatic continuations after an answer reaches the output token limit.
pub const CONTINUE_ON_LENGTH: u32 = 1;

/// Instructions for unattended troubleshooting. Frontends append generated environment
/// and command-policy sections using `prompt::build_system_prompt`.
pub const INSTRUCTIONS: &str = include_str!("../../profiles/troubleshoot.md");

/// Limits matching the CLI's unattended behavior. SDK callers opt in explicitly.
pub fn limits() -> Limits {
    Limits {
        max_turns: Some(MAX_TURNS),
        timeout: Some(Duration::from_secs(TIMEOUT_SECS)),
    }
}

/// Extension policy matching the CLI default; neutral SDK configuration has no hooks.
pub fn hooks() -> HookChain {
    HookChain::new().with(ContinueOnLength::new(CONTINUE_ON_LENGTH))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unattended_limits() {
        let limits = limits();
        assert_eq!(limits.max_turns, Some(30));
        assert_eq!(limits.timeout, Some(Duration::from_secs(900)));
    }
}
