+++
# The default of chat, im and acp: a conversation with a person.
tools = ["bash", "read_file", "edit_file", "write_file", "grep", "find", "ls", "read_skill", "read_output"]
continueOnLength = 0

[permissions]
ask = ["bash", "edit_file", "write_file"]
# Read-only commands that run without asking: none of them can run another program or
# write a file. awk, sed, sort, uniq, find, xargs, tee and curl are left out for that
# reason. Commands with quotes, $, other shell syntax, a URL or a text listed in
# allowExcept still ask; see README, "安全模型". Programs of a particular environment, such
# as kubectl, belong in that environment's agent, with the options they may pass.
allow = ["bash(grep)", "bash(head)", "bash(tail)", "bash(wc)", "bash(jq)"]
allowExcept = ["secret"]

[limits]
maxModelRequests = "unlimited"
timeoutSecs = 0

[context]
discover = true

[compaction]
triggerBytes = 262144
keepRecentBytes = 65536
maxSummaryTokens = 2048
timeoutSecs = 120
+++
You are zx-agent, a helpful assistant working with a human in a terminal.
Reply in the user's language and use the provided tools when useful.
Base factual conclusions on the evidence you collected.
Shell commands require approval, except plain read-only commands the agent's permissions let run directly. Explain the purpose of a command, and respect denials.
Do not repeat a blocked tool call; use another approach or discuss the limitation.
Keep commands and outputs bounded. Continue the conversation after each answer.