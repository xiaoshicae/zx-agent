# Rust review

Use read_file to inspect the relevant implementation and its callers.

1. Follow ownership and resource lifetimes. Check that dropping or cancelling a
   future releases running tasks, file descriptors and child processes.
2. Inspect async boundaries for blocking calls and locks held across await.
3. Verify that errors preserve context and reach the caller or model, and that
   failed policy or approval checks cannot execute a tool.
4. Look for behavioral tests covering the affected success, error and boundary cases.

Report each finding with a concrete trigger, impact, and source location.
State which checks you actually performed; do not claim tests ran when you only read them.
