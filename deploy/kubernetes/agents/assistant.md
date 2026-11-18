+++
# Replaces the built-in assistant for chat and zx-im in the resident Pod. zx-agent knows
# nothing about kubectl, so this file adds its read-only commands to those that run without
# asking, and limits their options. The body below the front matter is empty: the
# instructions are the assistant's.
extends = "assistant"
# Without edit_file and write_file, since the Pod is for looking at clusters, not for changing
# files, and without grep, find and ls, which would search without asking: searches go through
# bash and its checks (docs/SECURITY.md, "工具").
tools = ["bash", "read_file", "read_skill", "read_output"]

[permissions]
ask = ["bash"]
# Replaces the assistant's list, so its commands are repeated here.
allow = [
  "bash(kubectl get)",
  "bash(kubectl describe)",
  "bash(kubectl logs)",
  "bash(kubectl top)",
  "bash(kubectl events)",
  "bash(kubectl explain)",
  "bash(kubectl api-resources)",
  "bash(kubectl version)",
  "bash(kubectl config get-contexts)",
  "bash(kubectl config current-context)",
  "bash(grep)",
  "bash(head)",
  "bash(tail)",
  "bash(wc)",
  "bash(jq)",
]
# allowExcept is inherited from the assistant: its "secret" makes any command that
# names Secrets ask.

# The kubectl options that a command run without asking may pass: they choose what to read
# and how to show it. Every other option asks, among them those that send the request
# elsewhere (--server), use other credentials (--kubeconfig, --token, --user, --as), skip
# certificate checks (--insecure-skip-tls-verify), write files (--profile-output,
# --cache-dir), read any API path (--raw) or read manifests (--filename). Checked against
# kubectl v1.36.1, which sent the kubeconfig token to an HTTPS address given with --server.
[permissions.allowOptions.kubectl]
long = [
  "namespace",
  "context",
  "request-timeout",
  "all-namespaces",
  "output",
  "selector",
  "field-selector",
  "label-columns",
  "show-labels",
  "show-kind",
  "no-headers",
  "sort-by",
  "chunk-size",
  "ignore-not-found",
  "show-managed-fields",
  "subresource",
  "show-events",
  "for",
  "types",
  "container",
  "previous",
  "since",
  "since-time",
  "tail",
  "timestamps",
  "all-containers",
  "all-pods",
  "prefix",
  "limit-bytes",
  "max-log-requests",
  "ignore-errors",
  "pod-running-timeout",
  "containers",
  "sum",
  "use-protocol-buffers",
  "show-capacity",
  "show-swap",
  "recursive",
  "api-version",
  "api-group",
  "cached",
  "categories",
  "namespaced",
  "verbs",
  "client",
]
shortWithValue = ["n", "o", "l", "L", "c"]
shortFlags = ["A", "p", "R"]
+++
