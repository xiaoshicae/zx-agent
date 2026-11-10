# 配置：config.toml 与 agent

zx-agent 的配置分两部分：

- **`config.toml`：环境**，即在哪里、用什么运行——模型与 provider、命令在本机还是容器中执行、MCP 服务器、模型的元数据、长工具结果的存放位置。
- **agent：角色**，即做什么、可以做什么——说明（系统提示词的开头）、工具、权限、预算、项目上下文、skills 与摘要设置。

两者都放在 zx-agent 主目录里，都可以没有：没有 `config.toml` 时用内置默认值，没有 agent 文件时用内置的 agent。

## zx-agent 主目录

主目录是 `~/.zx-agent`（`$HOME/.zx-agent`）；设置环境变量 `ZX_AGENT_HOME` 时改用它，值必须是绝对路径，相对路径以退出码 2 报错。主目录里有：

| 路径 | 内容 |
|---|---|
| `config.toml` | 环境配置，见下文 |
| `agents/assistant.md`、`agents/unattended.md` | 替换同名内置 agent 的文件，见下文“agent” |
| `sessions/--<工作目录>--/` | chat 与 acp 的默认会话目录（`/` 换成 `-`），`zx-agent sessions` 列出的也是它 |

不用 `~/.agents/`：这个目录由多个工具共用（Codex 读取 `$HOME/.agents/skills`），放一个通用的 `config.toml` 会与别的工具冲突。

## config.toml

文件可选，最多 64 KiB，必须是普通 UTF-8 文件；键用 camelCase，未知的键以退出码 2 报错，错误信息带文件路径。示例：

```toml
model = "my-model"
reasoning = "high"                 # off、minimal、low、medium、high、xhigh；chat 的 /reasoning 可以改
maxTokens = 4096
toolTimeoutSecs = 120
approvalTimeoutSecs = 600          # chat、acp 与 im 的审批等待
secretFiles = ["/var/run/secrets/kubernetes.io/serviceaccount/token"]

[provider]
baseUrl = "https://llm-gateway.example.com/v1"
apiKeyEnv = "LLM_API_KEY"          # 存放 API key 的环境变量名，不是 key 本身
systemUpdates = "collapse"         # 或 inline
reasoningFormat = "thinking"       # effort、thinking 或 reasoning-object
fillMissingReasoning = true
promptCacheKey = false
requestFields = { temperature = 0.2 }   # 不能与 reasoning 写入的字段（此处为 thinking、reasoning_effort）重名
headers = { X-Route = "eu" }
idleTimeoutSecs = 120
maxRetries = 3

[container]
image = "debian:bookworm-slim"

[toolOutput]
thresholdBytes = 8192

[[models]]
id = "my-model"
contextWindow = 128000
cost = { input = 0.27, output = 1.10, cacheRead = 0.07 }

[[mcp]]
name = "prom"
url = "https://prom-mcp.monitoring.svc/mcp"
tools = ["query", "query_range"]
```

顶层的键（`model`、`reasoning`、`execution` 等）要写在第一个 `[表]` 之前。

| 键 | 语义 |
|---|---|
| `model` | 模型 ID；`--model` 与 `ZX_MODEL` 优先 |
| `reasoning` | 推理强度：`off`、`minimal`、`low`、`medium`、`high` 或 `xhigh`。不设时请求不带推理设置，由服务决定；chat 的 `/reasoning` 可以改。会话的请求审计记录它 |
| `maxTokens` | 每次回复的输出 token 上限；推理模型的推理也算在内，太小可能让回答为空。不设时请求不带上限 |
| `approvalTimeoutSecs` | 审批请求等待答复的时间，秒，至少 1，默认 600：chat 的菜单选择、acp 客户端的答复与 im 的审批卡片共用这一个值；超时视为拒绝，任务不带这次调用继续。原来的 `[chat]`、`[acp]` 表与 IM 配置文件的 `limits.approvalTimeoutSecs` 已去掉，写了报未知键 |
| `toolTimeoutSecs` | bash 命令的默认超时，也是 MCP 调用的默认超时，秒，至少 1，默认 120 |
| `secretFiles` | 存放密钥的文件（如为 agent 挂载的 token），或这类文件的目录（以 `.` 开头的文件跳过），其内容在工具输出中替换为 `[REDACTED]`；相对路径相对 `config.toml` 所在目录。启动时每个路径都必须存在，每个文件都必须是至多 64 KiB 的普通文件；每次脱敏前重新读取。环境变量 `ZX_SECRET_FILES`（逗号分隔）中的文件另外加上，不替换这里的 |
| `provider.baseUrl` | OpenAI 兼容服务的地址（不含 `/chat/completions`），默认 `https://api.openai.com/v1`；`ZX_BASE_URL` 优先 |
| `provider.apiKeyEnv` | 存放 API key 的环境变量名，默认 `OPENAI_API_KEY`；key 只从环境变量读取，不出现在命令行与进程列表中 |
| `provider.systemUpdates` | 续接会话时系统提示词的变化怎样发给模型。`collapse`（默认）：所有版本合进开头的 system 消息，任何服务都能接受。`inline`：每次变化发在它发生的位置，保留提示词缓存，也让模型看到提示词何时变了，但服务必须接受对话中间的 system 消息 |
| `provider.reasoningFormat` | `reasoning` 写进请求的方式：`effort`（默认；OpenAI 的 `reasoning_effort`，`off` 不发送）、`thinking`（`thinking` 对象，开或关，加上 `reasoning_effort`）或 `reasoning-object`（`reasoning.effort`）。以服务的文档为准。请求审计记录它 |
| `provider.fillMissingReasoning` | `true` 时，只要此前有一个回答带 `reasoning_content`，其余回答发送时补一个空的，用于拒绝“只有部分回答带推理”的思考模式服务。请求审计记录这一选择 |
| `provider.promptCacheKey` | `true` 时把会话 ID 作为请求的 `prompt_cache_key` 发送，让同一对话的请求共用服务的提示词缓存（OpenAI 接受这个字段；拒绝未知字段的服务不能打开）。请求审计记录这个 key |
| `provider.requestFields` | 加到每个请求体中的字段，表：字段名 → 值（TOML 的值按 JSON 发送），用于某些服务提供的设置，如 ⚑ D:5/CHINA FLAG: DeepSeek 的 `thinking = { type = "disabled" }`。zx-agent 自己设置的字段（如 `model`、`max_tokens`）被拒绝；与 `reasoning` 写入的字段冲突时也拒绝。请求审计记录它们 |
| `provider.headers` | 每个请求都发送的请求头，表：名字 → 值，如网关的路由头。`Authorization` 被拒绝：key 来自 `apiKeyEnv`。不记录在会话中 |
| `provider.idleTimeoutSecs` | 等待响应头或下一段流的最长时间，秒，至少 1，默认 120 |
| `provider.maxRetries` | 临时性错误的重试次数，默认 3 |
| `execution` | `local`（默认）或 `container`；其他值报错。`container` 时 bash、read_file、edit_file 与 write_file 在 Docker 容器中执行，见 README 的“容器执行” |
| `container.image` | 容器镜像，`execution = "container"` 时必填，须已在本地（`docker pull`） |
| `container.user` | 容器内运行命令的 `uid:gid`，默认为运行 zx-agent 的用户（以 root 运行时为 `65534:65534`）；不能是 root（UID 0 或 `root`），启动时也会检查命令的实际 UID |
| `container.memory` | Docker 写法的内存上限，默认 `1g` |
| `container.pidsLimit` | 容器内进程数上限，大于零，默认 256 |
| `container.network` | 容器加入的 Docker 网络，默认 `bridge`；`none` 为无网络；`host` 与 `container:<名字>` 会与沙箱外共用网络命名空间，报配置错误 |
| `container.workspace` | `readWrite`（默认）或 `readOnly`：命令能否修改工作目录 |
| `container.root` | `readWrite`（默认）或 `readOnly`：命令能否修改容器自身的文件；`readOnly` 时只有内存中的 `/tmp` 可写 |
| `toolOutput.thresholdBytes` | 超过此大小的工具结果落盘；0 关闭，否则为 256–maxFileBytes，默认 8192 |
| `toolOutput.maxFileBytes` | 读写文件上限，1–1048576，默认 1 MiB |
| `toolOutput.directory` | 显式路径相对 `config.toml` 所在目录；省略时为工具 cwd 下的 `.zx-agent/outputs` |
| `[[models]]` | 模型的元数据，每项 `id`（必填）、`contextWindow`（token 数，大于零）、`cost`（每百万 token 的单价，币种自定，所有单价用同一种：`input`、`output`、`cacheRead`、`cacheWrite`，都不小于零；`input` 指既不读也不写缓存的提示词 token）。没有内置的模型表。有单价时每次回复的 `usage.totalCost` 与 chat 的 `[turn]`、`/status` 显示成本；`--cost-budget` 与 `limits.costBudget` 需要所用模型的单价 |
| `[[mcp]]` | 外部 MCP 服务器，可写多段，见下文“MCP 服务器” |

值无效时（如超时为 0、请求头写了 `Authorization`、字段名为空）以退出码 2 报错，错误信息带键名与文件路径。

### 优先级

只有两个键可以从别处覆盖：模型取 **`--model` > `ZX_MODEL` > `config.toml`**，服务地址取 **`ZX_BASE_URL` > `provider.baseUrl` > 内置默认值**。其余的键只写在 `config.toml` 中，没有对应的命令行参数（原来的 `--base-url`、`--api-key-env`、`--system-updates`、`--reasoning`、`--reasoning-format`、`--fill-missing-reasoning`、`--prompt-cache-key`、`--request-field`、`--header`、`--idle-timeout-secs`、`--max-retries`、`--max-tokens`、`--tool-timeout-secs`、`--secret-file`、`--approval-timeout-secs` 已去掉）。`--model` 不是必填；`--model`、`ZX_MODEL` 与 `config.toml` 都没有给出模型时，以退出码 2 报错：`no model: pass --model, set ZX_MODEL, or set model in <config.toml 的路径>`。

### 没有项目级配置文件

只读主目录里的 `config.toml`，不读工作目录或仓库里的配置。项目内容不能改变环境或权限：`baseUrl`、`apiKeyEnv` 与 MCP 的 `url`、`tokenEnv` 可以把密钥发到别处，`[[models]]` 的单价可以让花费预算失效，`execution` 可以让命令离开容器；把这些去掉之后剩下的键不值得单独支持。项目对 zx-agent 的贡献仍是 `AGENTS.md`（见“项目上下文”）。要为某个项目换一套配置，用另一个 `ZX_AGENT_HOME`。

`im` 的 `--config <im.toml>` 是 IM 服务的配置文件（平台、访问名单、会话目录），与 `config.toml` 无关，见 README 的“IM 服务”。

## agent

`run` 使用内置的 `unattended`，`chat`、`im` 与 `acp` 使用内置的 `assistant`。两个内置 agent 是仓库根目录的 `agents/unattended.md` 与 `agents/assistant.md`，构建时嵌入程序。主目录下 `agents/<名字>.md` 存在时替换同名的内置 agent：`agents/unattended.md` 改变 `run`，`agents/assistant.md` 改变 chat、im 与 acp。只读这两个文件名，`agents/` 中其他名字的文件不会被读取。没有选择 agent 的命令行参数；要换一组 agent，用另一个 `ZX_AGENT_HOME`。

内置 agent 不含任何特定环境的规则：例如 Kubernetes 中只读 kubectl 命令的放行与可带的选项，在部署目录的 `deploy/kubernetes/agents/assistant.md`；要求回答按排障报告的固定小节组织的 agent 也在那里（`unattended.md`，扩展内置的 `unattended`，只换说明），部署时挂载到 Pod 的 `/data/.zx-agent/agents/`。

### 文件格式

一个 agent 是一个 Markdown 文件，最多 64 KiB，UTF-8：第一行是 `+++`，到下一行 `+++` 为止是 TOML 写的 front matter，之后是说明。没有开头的 `+++`、没有结尾的 `+++`、TOML 无效或有未知的键，都以退出码 2 报错。

示例：只读的代码评审。完整文件在 [`examples/agents/`](../examples/agents/)；使用时把该目录复制到某个主目录的 `agents/` 下，并把 `review.md` 改名为 `assistant.md`（或者把 `ZX_AGENT_HOME` 指向这样准备的目录），然后运行 `zx-agent chat`。

```markdown
+++
extends = "assistant"
tools = ["read_file", "read_skill"]
continueOnLength = 0

[permissions]
deny = ["bash"]

[limits]
maxModelRequests = 12
timeoutSecs = 300

[context]
discover = true
files = ["review-context.md"]

[[skills]]
name = "rust-review"
description = "Review Rust ownership, async cancellation, errors and test coverage."
path = "skills/rust-review/SKILL.md"
+++
You review code without changing files. Reply in the user's language.
...
```

| 键 | 语义 |
|---|---|
| `extends` | 可选的 `unattended` 或 `assistant`，只能是内置 agent，不支持文件链式继承。省略时从空的基线开始，见下文 |
| `tools` | 替换启用列表，支持 `bash`、`read_file`、`edit_file`、`write_file`、`grep`、`find`、`ls`、`read_skill`、`read_output`；`[]` 禁用全部工具。内置 assistant 启用全部 |
| `continueOnLength` | 输出达到 token 上限后的续写次数；每次消耗一次模型请求预算 |
| `[permissions]` | 权限，见下文“权限” |
| `limits.maxModelRequests` | 每轮模型请求数；`"unlimited"` 不限，`0` 不请求模型 |
| `limits.timeoutSecs` | 每轮总时间上限，`0` 关闭 |
| `limits.costBudget` | 每轮所有模型请求的花费上限，按 `config.toml` 的 `[[models]]` 中该模型的单价计算，单位与单价相同（按人民币写单价，预算就是人民币）；摘要请求与失败的尝试也计入，工具报告的花费不计入；每次请求之前检查，最后一次请求可能超出；`0` 关闭，负数或非有限值报错。模型没有单价时 CLI 拒绝启动。用完时 run 以退出码 8 结束 |
| `limits.tokenBudget` | 每轮所有模型请求的输入加输出 token 上限，以服务返回的用量为准，摘要请求与失败的尝试也计入；每次请求之前检查，最后一次请求可能超出；`0` 关闭。用完时 run 以退出码 7 结束 |
| `context.discover` | 是否自动发现项目 AGENTS.md；`assistant` 默认开启，`unattended` 默认关闭 |
| `context.files` | 显式上下文文件，相对 agent 文件所在目录 |
| `skills` | 显式登记 name、description、path 的目录，path 相对 agent 文件所在目录；正文按需读取 |
| `compaction.*` | 自动摘要，见下文“上下文管理” |

`context.files` 与 `skills.path` 相对 agent 文件所在目录解析（符号链接解析之后），与工作目录和 `--cwd` 无关。

### 说明

front matter 之后的文本就是说明，即系统提示词的开头；开头的空行不算。去掉空白后不为空时替换所扩展 agent 的说明，为空时沿用它。文件每次建会话时读取（`chat` 的 `/new` 读取当时的内容）。

### 继承与合并

- `extends` 只能写内置 agent。不写时，文件从空的基线开始，不随子命令变化：没有工具、没有权限、内核的默认上限（不限请求数与时间）、`continueOnLength = 0`、不自动发现项目上下文、没有 skills、内核的默认摘要设置（不自动摘要）、没有说明。这样的文件必须写说明，否则报错：`agent <路径> has no instructions: write them below the front matter or extend a built-in agent`。
- 只有一条合并规则：文件写了的键整个替换继承的值，列表也是；表（`permissions`、`limits`、`context`、`compaction`）逐键合并；`permissions.allowOptions` 按程序替换。没有累加的列表。
- `CommandPolicy` 内置的命令拒绝规则始终生效，任何 agent 都去不掉。

叠加顺序为 **内置 agent < agent 文件 < 显式命令行参数**；即使命令行的值恰好等于内置值，也按显式覆盖处理。命令行只覆盖 `--tools` 与预算（`--max-model-requests`、`--timeout-secs`、`--token-budget`、`--cost-budget`）；其余（权限、说明、上下文、摘要、续写）只写在 agent 中。

`run` 始终是无人值守前端，`chat` 始终保留 bash 的终端审批（设了 `permissions.approveAll` 的容器执行除外）；agent 不改变前端。例如 `agents/unattended.md` 写 `extends = "assistant"` 时，`run` 遇到要审批的调用仍然拒绝；`agents/assistant.md` 写 `extends = "unattended"` 时，chat 仍会询问 shell 审批。

## 权限

`[permissions]` 决定哪些调用要审批、哪些直接执行、哪些被拒绝。条目是工具名（内置工具名，或 `config.toml` 中配置的 MCP 工具名 `<服务器名>__<工具名>`），或 `bash(<文本>)`。

```toml
[permissions]
ask = ["bash", "edit_file", "write_file"]
allow = ["bash(kubectl get)", "bash(grep)", "prom__query"]
allowExcept = ["secret"]
deny = ["bash(rm -rf)", "write_file"]

[permissions.allowOptions.kubectl]
long = ["namespace", "output"]
shortWithValue = ["n", "o"]
shortFlags = ["A"]
```

| 键 | 语义 |
|---|---|
| `ask` | 要审批的工具名；不能写 `bash(...)`。run 没有人审批而拒绝，chat 在终端询问，im 发审批卡片，acp 交给客户端 |
| `allow` | `bash(<前缀>)`：以该前缀开头的命令不经审批直接执行，只在 bash 需要审批时起作用（chat、im、acp，或 `ask` 中有 `bash`）。带引号、`$` 等 shell 语法、URL 或串联多个命令的仍要审批，拒绝规则始终优先，见 README 的“安全模型”。工具名：该 MCP 工具不经审批直接执行（MCP 工具默认都要审批）。写内置工具名报错（`<工具> runs without asking unless it is in permissions.ask`）；同一个工具同时出现在 `ask` 与 `allow` 也报错 |
| `allowExcept` | 字词：命令中出现其中之一（不分大小写，作为子串比较）时，`allow` 放行的命令仍要审批。内置 assistant 为 `["secret"]` |
| `allowOptions.<程序>` | 列出该程序（命令的第一个词，小写）的命令不经审批时可以带的选项：`long`（不带 `--` 的长选项名，`--name=value` 按名字比较）、`shortWithValue`（带值的单字母选项，值可紧跟，如 `-oyaml`）、`shortFlags`（不带值的单字母选项，须单独出现，如 `-A`）。其他选项一律审批；没有列出的程序不限选项。文件中列出的程序整组替换继承的同名项（内置 agent 都没有）。示例见 `deploy/kubernetes/agents/assistant.md` |
| `deny` | 工具名：拒绝该工具，启用它或批准调用都不能覆盖。`bash(<文本>)`：拒绝**任何位置**含有该文本的命令，不分大小写；内置的拒绝规则始终保留 |
| `approveAll` | `true` 时不询问任何审批：chat 与 im 的 bash、`ask` 中的工具与 MCP 工具都直接执行；拒绝规则与命令策略仍然生效。只能与 `config.toml` 的 `execution = "container"` 一起使用，且 `ask` 须为空（内置 assistant 的 `ask` 不为空，需写 `ask = []`），见 README 的“容器执行” |

注意两种 `bash(...)` 的比较方式不同：`deny` 的文本在命令中**任何位置**出现都算，`allow` 的文本必须是命令的**开头**（按词比较）。

只允许一部分命令时，写 `ask = ["bash"]` 加上 `allow = ["bash(kubectl get)", ...]`：`allow` 覆盖的命令直接执行，其余命令要审批，在 run 中因为没有人审批而被拒绝。没有命令白名单（原来的 `rules.allow`，SDK 的 `CommandPolicy` 也不再接受白名单）。

工具名不认识、条目为空、`bash()` 中没有文本、`bash(` 没有右括号时，以退出码 2 报错，错误信息带键名（如 `permissions.deny`）。

## MCP 服务器

`config.toml` 的 `[[mcp]]` 让模型调用外部 MCP 服务器的工具（只支持 Streamable HTTP）。MCP 服务器由外部服务提供，本项目只做客户端。

```toml
[[mcp]]
name = "prom"                               # 小写 ASCII 字母、数字与 -
url = "https://prom-mcp.monitoring.svc/mcp"
tokenEnv = "PROM_MCP_TOKEN"                 # 可选：以 Authorization: Bearer 发送
tools = ["query", "query_range"]            # 交给模型的工具，其余不暴露
timeoutSecs = 60                            # 可选：一次调用多久没有应答也没有进度通知就超时，默认同 toolTimeoutSecs
maxTimeoutSecs = 600                        # 可选：一次调用的总上限，不管进度；省略则只受 timeoutSecs、本轮超时与取消约束
```

| 配置 | 语义 |
|---|---|
| `name` | 服务器名，不能重复。模型看到的工具名是 `<name>__<工具名>`：工具名中 ASCII 字母、数字、`_`、`-` 以外的字符换成 `_`，全名不超过 64 个字符 |
| `url` | MCP 端点，`http://` 或 `https://` |
| `tokenEnv` | 存放 token 的环境变量；设置后必须有值。命令看不到这个变量，它的值在工具输出中替换为 `[REDACTED]` |
| `tools` | 交给模型的工具，按服务器给出的名字，至少一个。服务器没有列出的工具时启动失败 |
| `timeoutSecs` | 单次调用的上限，大于零 |

- MCP 工具默认每次调用都要审批：chat 在终端询问，im 发审批卡片，run 没有人审批而拒绝。要直接执行，在 agent 的 `permissions.allow` 中写模型看到的工具名，如 `allow = ["prom__query"]`（原来 `[[mcp]]` 的 `allowTools` 已去掉，写了报未知字段）。
- 每个进程启动时连接一次：run、chat 在组装会话前，im 在开始接收消息前，各线程共用连接。连不上、token 变量没有值、服务器缺少列出的工具时，以退出码 2 报错。
- 连接成功时，stderr 每个服务器一行，例如 `[mcp] prom: prom__query, prom__query_range (asks); protocol 2026-07-28; https://prom-mcp.monitoring.svc`。只写地址的协议、主机与端口；经 `http://` 发送 token 时另加 `the token is sent unencrypted`。
- 服务器给出的工具注释（annotations）只是提示，不据此放行；工具一律按有副作用处理。


## 项目上下文

开启自动发现时，从最近 Git 根目录到工具 cwd，依次加载各层 `AGENTS.md`。Git worktree 的 `.git` 文件也被识别为边界；没有 Git 根目录时只检查 cwd。不会隐式读取用户主目录里的其他 AI 配置。

`chat` 默认使用的 `assistant` 开启自动发现；`run` 默认使用的 `unattended` 关闭，无人值守运行不读取仓库里的说明。`run` 需要项目说明时，在主目录的 `agents/unattended.md` 中设置 `[context] discover = true`，或用 `files` 显式列出。

自动发现的 `AGENTS.md` 可以是符号链接，但解析后的路径必须位于 Git 根目录内（没有 Git 根目录时为 cwd），否则在请求模型前以退出码 2 报错。这样仓库内容就不能让程序读取并发送仓库外的文件。agent 中的上下文文件由使用者显式指定，不受这条限制。

项目说明会进入系统提示词，可能引导模型的行为，但不能改变工具和规则。在不可信的仓库中运行时，建议使用 `[context] discover = false` 的 agent。

```markdown
+++
extends = "assistant"

[context]
discover = false              # 不读取仓库里的 AGENTS.md
files = ["extra-context.md"]  # 相对本文件所在目录
+++
```

`context.files` 相对 agent 文件所在目录解析，与 `--cwd` 无关；文件写了它就整个替换所扩展 agent 的列表（内置 agent 都没有）。相同 canonical 路径只加载一次。

上下文追加到生成的 `project_context` 提示词段落，沿用现有系统提示词持久化和更新机制。只读普通 UTF-8 文件，每个文件最多 64 KiB；上下文总量包含路径标题和分隔符，最多 256 KiB。超限报错，不静默截断指令；报错会提示可以用 `[context] discover = false` 跳过自动发现。

## 按需加载 skills

P4 使用 agent 中显式登记的目录，不自动扫描全局 skills，也不解析 SKILL.md 的 YAML frontmatter。已有 SKILL.md 可以直接作为正文，名称和简介以 agent 文件为准；skill 里的脚本不会自动执行。

启动只检查登记项及文件元数据，并向 `available_skills` 加入目录。目录包含每个 skill 正文文件的路径；该路径与 `<environment>` 中的工作目录一样，会随系统提示词发送给模型服务方。正文只有模型调用 `read_skill` 时才读取，作为普通工具结果先持久化、再发给模型：

```text
agent 的目录信息 → 系统提示词 → 模型选择 read_skill(name)
                                      ↓
                              参数校验 → 策略 / 审批
                                      ↓
                         按需读取正文 → 脱敏 → 审计 → 模型
```

每个目录最多 64 项，name 使用 1–64 个小写字母、数字或连字符，description 为 1–1024 字节。正文文件最多 64 KiB；单次工具输出沿用 2000 行、50 KiB 的截断限制。输出被分页时返回下一次调用的 UTF-8 字节 `offset`，可继续读取，包括一条超长行的剩余部分。

`read_skill` 仅接受已登记名称和可选 offset，不能通过参数指定任意路径。内置 agent 预留该工具；目录为空时不注册。`--tools` 或 agent 的 tools 列表未启用它时，工具和目录均不加入请求。skill 调用与其他工具使用同一条校验、钩子、审批、取消、脱敏和审计流水线。

项目上下文与 skill 正文是提示词和工具数据，不能修改已组装的工具或规则。认证 key 的脱敏边界仍见 [README 安全模型](../README.md#安全模型)；上下文和说明属于输入，不在工具输出过滤范围内。

## 上下文管理

`assistant` 默认在序列化上下文超过 256 KiB 时自动摘要，`unattended` 默认关闭。触发值取自所用的 agent，与前端名称无硬编码关联。手动 `/compact` 和 SDK `AgentSession::compact()` 共用独占运行权；没有旧消息可压缩时返回 Noop，不调用模型。

```toml
[compaction]
triggerBytes = 262144      # 0 禁用自动摘要
triggerTokens = 0          # 省略时按模型窗口推导，0 不按 token 触发
reserveTokens = 16384
keepRecentBytes = 65536
maxSummaryTokens = 2048
timeoutSecs = 120
```

模型的上下文窗口与单价写在 `config.toml` 的 `[[models]]` 中：

```toml
[[models]]
id = "my-model"
contextWindow = 128000
cost = { input = 0.27, output = 1.10, cacheRead = 0.07 }   # 每百万 token 的单价，币种自定
```

| 键 | 语义 |
|---|---|
| `compaction.triggerBytes` | 自动摘要阈值，0 关闭；非零须大于 `keepRecentBytes`，且超出部分不能多于 2 MiB（一次摘要最多读取的原始记录） |
| `compaction.keepRecentBytes` | 近期消息的序列化字节预算，大于零；完整保留最新输入和全部 system，可因此超出保留预算 |
| `compaction.triggerTokens` | 按 token 估算的自动摘要阈值，0 关闭；与 `triggerBytes` 任一超过即摘要。估算 = 最近一次成功回复报告的输入加输出 token，加上其后消息的字节数除以 4。须高于 `keepRecentBytes` 折算的 token 数（字节除以 4），否则摘要永远压不到阈值以下。省略时，开启自动摘要（`triggerBytes` 非零）且模型的上下文窗口已知（`config.toml` 的 `[[models]]`）的话，取“窗口减 `reserveTokens`”，不满足上述条件时不设 |
| `compaction.reserveTokens` | 按窗口推导 `triggerTokens` 时留出的 token 数，默认 16384（pi 的 `reserveTokens`） |
| `compaction.maxSummaryTokens` | 摘要输出 token 上限，1–16384，默认 2048。有的服务把推理计入输出 token，推理可能用满上限而写不出摘要（见 `docs/ROADMAP.md` 的验证记录），使用这类模型时需要调高；服务支持时也可以经 `provider.requestFields` 关闭推理，但它同样作用于任务请求 |
| `compaction.timeoutSecs` | 摘要流总超时，大于零，默认 120 秒 |

`triggerBytes = 0`（且不设 `triggerTokens`，或设为 0）关闭自动摘要，不影响手动 `/compact`；`run` 要自动摘要时，在主目录的 `agents/unattended.md` 中设置非零的 `triggerBytes`。SDK 默认不自动压缩，原构造器不变；宿主可在 Runner 上调用 `with_compaction(Config)` 校验并启用。

自动步骤在输入和上下文钩子的改动持久化之后、任务模型请求之前执行。一次摘要占用一次 `--max-model-requests`，其 usage 与失败成本也计入本轮报告，并受整轮时间限制。摘要的工具列表为空；失败不重试成任务请求。付费的自动摘要请求失败后，本会话暂停自动摘要：之后需要摘要时直接报错并说明原因，不再自动发请求；手动 `/compact` 成功或切换模型后恢复，新会话也重新开始。手动摘要单独返回 `compaction_report`，不占下一轮 prompt 的预算。

开启自动摘要时，模型以上下文过长拒绝请求（HTTP 413、`context_length_exceeded` 等错误码，或 “maximum context length”“prompt is too long” 等通用措辞；限流与服务端错误不算）后，运行不直接失败：先用 `context_edit` 省略失败的回答（记录保留），再摘要一次，然后从压缩后的上下文开始新的尝试，不重复用户消息（pi：`AgentSession` 的 overflow 恢复）。这不算重试，不占 `provider.maxRetries`；摘要请求计入请求与 token 预算，预算用完时以原错误结束。每次运行只恢复一次，之后又有回答成功时才可以再次恢复；恢复后紧接着又过长，或摘要失败、没有可摘要的范围，都以原错误加原因结束。付费的摘要请求失败同样暂停自动摘要。没有开启自动摘要时（`run` 的内置 agent 默认关闭），行为不变，以原错误结束。事件为 `context_overflow`。

预算按内部消息 JSON 的字节数估算（不含工具结果的 `details`，它不发给模型），不是 tokenizer，也不包含工具定义或 HTTP 封装；不能据此保证适合每个模型的 token 窗口。保留最近约 64 KiB、完整的最新用户输入、全部 system 更新，边界不拆开工具调用与结果。保留范围会向前扩到最新输入，并保留它之后的全部消息；单个任务过长也可能超过预算。若保留部分无法缩到阈值内，明确停止，不截断指令或最新输入。摘要源最多 2 MiB（`MAX_SOURCE_BYTES`），摘要响应和结构化正文最多 64 KiB；每次摘要都重新读取边界之前的全部原始记录，历史累计超过 2 MiB 后无法再摘要，需要 `/new` 开新会话。

审计流程如下：

```text
原始投影 + entryId → compaction_started（先写入）→ 同一 provider/model 的摘要请求
                                                        ↓
                                              校验 JSON / 引用 / 大小 / 停止原因
                                                        ↓
原始消息始终保留                 compaction 或 compaction_failed（再追加）
                                                        ↓
                              全部 system + 成功摘要 + 保留范围 → 任务请求
```

会话格式只有当前一个版本（见 README 的“会话文件”）。开始记录保存源 ID、保留边界、摘要请求配置；成功记录保存摘要、用量和开始记录的引用。失败或进程退出后未完成的尝试不改变投影。保留边界按条目在分支上的先后计算：保留范围的第一条消息之后被 `context_edit` 省略，摘要仍替代它之前的全部记录。重压缩始终读取应用过 `context_edit` 的原始投影，不读取旧摘要；各分支独立。摘要源去掉工具结果的 `details`，请求审计还原时同样去掉。

摘要字段为 `facts: [{text, sourceEntryIds}]`、`inferences: [string]`、`plans: [string]`。每条事实至少引用一个本次源条目；无效引用、无效 JSON、工具调用、输出截断或超限均拒绝提交。引用存在只证明出处可定位，**不证明模型的陈述正确或摘要无信息损失**。system 和合成摘要不能直接通过 `context_edit` 改写；修正被摘要覆盖的原消息后，旧摘要失效，投影恢复到已应用修正的原始记录。钩子看到的合成摘要是 `kind` 为 `summary` 的 user 消息，变换计划若编辑、省略或追加这类消息，会在校验时被整份拒绝，不写入任何条目。

`compact` 与 prompt 重叠时返回 Busy，运行中也不能切模型。取消、超时或丢弃 future 会释放运行权，未完成记录保留供审计。TTY 手动压缩期间可暂存一个普通输入，成功或失败返回后执行；Ctrl+C 清除暂存输入并报告。管道脚本仍按操作顺序消费。

## 工具结果落盘

`chat` 使用的 `assistant` 默认启用 `read_output`；`run` 使用的 `unattended` 默认不启用，工具结果保持内联，与 P5 之前一致，也不会向可能只读的工作目录写文件。启用后，工具返回的文本超过 8 KiB 时，在最后一个 `after_tool` 钩子中脱敏后写入文件，会话和模型只收到 `{outputId, path, bytes, readWith}` 引用。SDK 默认不安装这一钩子；显式 `--tools` 未启用 `read_output` 时也不安装，保留工具本来的有界输出。

这几项写在 `config.toml` 中：

```toml
[toolOutput]
thresholdBytes = 8192      # 0 保持内联，仍可读取已有结果
maxFileBytes = 1048576
# directory = "results"   # 相对 config.toml 所在目录；省略则使用 cwd/.zx-agent/outputs
```

保存的是 `bash`、`read_file` 等工具**已经返回的有界文本**，不会恢复此前已截掉的 stdout、stderr 或文件内容。写入失败（包括超过单文件上限、目录越界）或 3 秒内没有完成时，保留已脱敏的有界结果内联，并附一行原因，不编造文件引用；写了一半的文件会被删除，超时后才完成的写入可能留下文件。这个期限低于钩子默认的 5 秒超时，所以磁盘变慢时结果仍然保留，而不是被钩子超时整条丢弃。落盘钩子位于脱敏之后，内联的结果与不落盘时会话本来记录的内容相同。

模型调用 `read_output({"id":"<outputId>","offset":0})` 读取，offset 为 UTF-8 字节位置。正文加续读提示不超过配置阈值；默认 8 KiB，关闭落盘后读取仍使用 8 KiB 页。严格校验 UUID 和参数，只在配置目录读取 UUID 文件。`read_output` 和 `read_skill` 在 CLI 中不再次落盘，避免递归引用。

新目录及 UUID 文件在 Unix 上分别使用 0700、0600；已有目录权限不改动，UUID 文件用 `create_new` 防止覆盖。文件不自动删除；取消可能留下未被日志引用的部分文件。续跑应使用同一结果目录，并一并保存 JSONL 和结果文件；每文件受上限约束，总磁盘占用需由宿主管理。

结果目录中会写入内容为 `*` 的 `.gitignore`（已有则不覆盖），避免结果被提交到用户的仓库；本仓库自己的 `.gitignore` 也忽略默认目录。

SDK 宿主注册同配置的 `OutputStore`、`ReadOutputTool` 和 `SpillOutputs`，写入钩子必须在 `RedactSecrets` 之后，并把分页工具名加入豁免列表。落盘不扩大已知密钥过滤范围，未知凭证和其他敏感数据仍需由宿主处理。

---

CONFIGURATION.md · v0.11 · 2026年11月
