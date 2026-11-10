+++
# The treatment arm of the sample behaviour evals (`tests/common/evals.rs`): the control plus
# the project notes in `hints.md`, next to this file. The two arms differ in nothing else.
extends = "unattended"
tools = ["read_file", "edit_file", "write_file"]

[limits]
maxModelRequests = 6
timeoutSecs = 120

[context]
discover = false
files = ["hints.md"]
+++
