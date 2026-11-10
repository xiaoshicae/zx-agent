+++
# Agent for conversations in the resident Pod: chat (kubectl exec) and serve. The
# kustomization puts it in the ConfigMap zx-agent-agents, mounted at /data/.zx-agent/agents,
# where it replaces the built-in assistant. It adds the read-only kubectl commands to the
# assistant's commands that run without asking, and limits their options. zx-agent itself
# knows nothing about kubectl. The instructions are the assistant's: the body below the front
# matter is empty.
extends = "assistant"
# The assistant's tools without edit_file and write_file: the Pod is for looking at
# clusters, not for changing files.
tools = ["bash", "read_file", "read_skill", "read_output"]

[rules]
askTools = ["bash"]
# Replaces the assistant's list, so its commands are repeated here.
autoApprove = [
  "kubectl get",
  "kubectl describe",
  "kubectl logs",
  "kubectl top",
  "kubectl events",
  "kubectl explain",
  "kubectl api-resources",
  "kubectl version",
  "kubectl config get-contexts",
  "kubectl config current-context",
  "grep",
  "head",
  "tail",
  "wc",
  "jq",
]
# autoApproveExcept is inherited from the assistant: its "secret" makes any command that
# names Secrets ask.

# The kubectl options that a command run without asking may pass: they choose what to read
# and how to show it. Every other option asks, among them those that send the request
# elsewhere (--server), use other credentials (--kubeconfig, --token, --user, --as), skip
# certificate checks (--insecure-skip-tls-verify), write files (--profile-output,
# --cache-dir), read any API path (--raw) or read manifests (--filename). kubectl v1.36.1
# sent the kubeconfig token to an HTTPS address given with --server. Checked against
# kubectl v1.36.1.
[rules.autoApproveOptions.kubectl]
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
# -n, -o, -l, -L and -c take a value; -A, -p and -R stand alone.
shortWithValue = ["n", "o", "l", "L", "c"]
shortFlags = ["A", "p", "R"]
+++
