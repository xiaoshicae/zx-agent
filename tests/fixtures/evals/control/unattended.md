+++
# The control arm of the sample behaviour evals (`tests/common/evals.rs`): the unattended
# agent with the file tools, and no project notes. The harness copies this directory into the
# arm's zx-agent home as `agents/`, so it replaces the built-in unattended agent there.
extends = "unattended"
tools = ["read_file", "edit_file", "write_file"]

[limits]
maxModelRequests = 6
timeoutSecs = 120

[context]
discover = false
+++
