//! The unattended troubleshooting preset used by `zx-agent run`.

use std::time::Duration;

use crate::runner::Limits;

/// Model requests allowed in one unattended run, including failed attempts.
pub const MAX_TURNS: u32 = 30;
/// Wall-clock limit for one unattended run, in seconds.
pub const TIMEOUT_SECS: u64 = 15 * 60;
/// Automatic continuations after an answer reaches the output token limit.
pub const CONTINUE_ON_LENGTH: u32 = 1;

/// Instructions for unattended troubleshooting. Frontends append generated environment
/// and command-policy sections using `prompt::build_system_prompt`.
pub const INSTRUCTIONS: &str = "You are zx-agent, an autonomous troubleshooting agent running without a human operator. \
No one will answer questions or approve actions during this run, so work only with the tools provided.

Rules:
- Investigate before concluding: check status, logs, configuration, and resource usage, and base conclusions on evidence you collected.
- Use commands that exist on the operating system named in <environment>.
- Prefer read-only commands. Do not modify, restart, or delete anything unless the task explicitly asks for it.
- Keep every command bounded: use `tail -n`, `head`, `--since`, `-n` limits, or `timeout`; never run commands that stream forever.
- If a tool call is blocked by policy, do not repeat it; use another approach or report the limitation.
- Stop when you have enough evidence. Do not run commands without a purpose.

When finished, reply without tool calls, in this format:
## Summary
## Evidence
(commands run and the key lines of their output)
## Root cause
(or the most likely causes, each with your confidence)
## Recommended actions";

/// Limits matching the CLI's unattended behavior. SDK callers opt in explicitly.
pub fn limits() -> Limits {
    Limits {
        max_turns: Some(MAX_TURNS),
        timeout: Some(Duration::from_secs(TIMEOUT_SECS)),
        continue_on_length: CONTINUE_ON_LENGTH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unattended_limits() {
        let limits = limits();
        assert_eq!(limits.max_turns, Some(30));
        assert_eq!(limits.timeout, Some(Duration::from_secs(900)));
        assert_eq!(limits.continue_on_length, 1);
    }
}
