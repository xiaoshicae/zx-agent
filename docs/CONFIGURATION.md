# 配置：config.toml、agent 与 im.toml

配置分两部分，都放在主目录里，都可以没有：`config.toml` 决定环境（模型、provider、执行环境、MCP 服务器、模型元数据、落盘位置），没有时用内置默认值；agent 决定角色（说明、工具、权限、预算、项目上下文、skills、摘要），没有时用内置的 agent。IM 服务另有自己的配置文件 `im.toml`，见文末。

## zx-agent 主目录

主目录是 `~/.zx-agent`；设置 `ZX_AGENT_HOME` 时改用它，值必须是绝对路径，否则以退出码 2 报错。

| 路径 | 内容 |
|---|---|
| `config.toml` | 环境配置 |
| `agents/assistant.md`、`agents/unattended.md` | 替换同名的内置 agent |
| `sessions/--<工作目录>--/` | chat 与 acp 的默认会话目录（目录名见 [REFERENCE.md](REFERENCE.md#chat)），`zx-agent sessions` 列出的也是它 |

只读主目录里的 `config.toml`，没有项目级配置文件：项目内容不能改变环境或权限。要为某个项目换一套配置，用另一个 `ZX_AGENT_HOME`。

## config.toml

最多 64 KiB 的普通 UTF-8 文件。键用 camelCase，顶层的键写在第一个 `[表]` 之前。未知的键与无效的值以退出码 2 报错，错误信息带文件路径（值无效时还带键名）。

```toml
model = "my-model"
reasoning = "high"

[provider]
baseUrl = "https://llm-gateway.example.com/v1"
apiKeyEnv = "LLM_API_KEY"          # 存放 API key 的环境变量名，不是 key 本身

[[models]]
id = "my-model"
contextWindow = 128000
cost = { input = 0.27, output = 1.10, cacheRead = 0.07 }   # 每百万 token 的单价
```

模型取 `--model` > `ZX_MODEL` > `model`，服务地址取 `ZX_BASE_URL` > `provider.baseUrl` > 默认值；其余键没有命令行参数。这两个变量与 `ZX_AGENT_HOME` 为空或不是 UTF-8 时视为没设。三处都没有模型时以退出码 2 报错。

| 键 | 语义 |
|---|---|
| `model` | 模型 ID |
| `reasoning` | 推理强度：`off`、`minimal`、`low`、`medium`、`high`、`xhigh`。不设时请求不带推理设置；chat 的 `/reasoning` 可以改 |
| `maxTokens` | 每次回复的输出 token 上限，不设时不限。推理计入输出的模型在上限太小时可能回答为空 |
| `approvalTimeoutSecs` | chat、acp 与 zx-im 等待审批的秒数，至少 1，默认 600；超时视为拒绝 |
| `toolTimeoutSecs` | bash 命令与 MCP 调用的默认超时，秒，至少 1，默认 120。模型可经 bash 的参数 `timeoutSecs` 为单条命令申请更长时间，上限为该值与 600 秒中较大的一个 |
| `secretFiles` | 存放密钥的文件或目录（目录中以 `.` 开头的文件跳过），其内容在工具输出中替换为 `[REDACTED]`；相对路径相对 `config.toml` 所在目录。启动时每个路径都必须存在，每个文件须为至多 64 KiB 的普通文件。`ZX_SECRET_FILES`（逗号分隔）中的文件另外加上。规则见 [SECURITY.md](SECURITY.md#密钥) |
| `provider.api` | 由哪个 provider 访问模型：`openai`（默认，OpenAI 兼容的 Chat Completions）、`anthropic`（Anthropic Messages API，见下文示例），或宿主经 `register_provider` 注册的名字（见 [LIBRARY.md](LIBRARY.md#与命令行共用配置)）；没有注册的名字以退出码 2 报错。下面标明“仅 `openai`”的键对 `anthropic` 是配置错误；宿主注册的 provider 自己检查 `api`、`apiKeyEnv`、`maxRetries` 以外的键 |
| `provider.baseUrl` | 服务的地址，请求的路径接在它后面：`openai` 不含 `/chat/completions`，默认 `https://api.openai.com/v1`；`anthropic` 不含 `/v1/messages`，默认 `https://api.anthropic.com`。须是 `http://` 或 `https://` 开头的地址，以路径结尾，不带查询串与片段；`ZX_BASE_URL` 也一样，不满足时以退出码 2 报错 |
| `provider.apiKeyEnv` | 存放 API key 的环境变量名，默认为 provider 自己的：`openai` 是 `OPENAI_API_KEY`，`anthropic` 是 `ANTHROPIC_API_KEY`；变量没有值时请求不带认证头 |
| `provider.systemUpdates` | 续接会话时系统提示词的变化怎样发送：`collapse`（默认）合进开头的系统提示词；`inline` 发在变化发生的位置，保留提示词缓存，但服务必须接受对话中间的 system 消息 |
| `provider.reasoningFormat` | 仅 `openai`。`reasoning` 的写法：`effort`（默认，`reasoning_effort`，`off` 不发送）、`thinking`（`thinking` 对象加 `reasoning_effort`）或 `reasoning-object`（`reasoning.effort`），以服务的文档为准 |
| `provider.maxTokensField` | 仅 `openai`。输出 token 上限（`maxTokens`，以及摘要请求的 `compaction.maxSummaryTokens`）写进请求体的字段名：`max_tokens`（默认）或 `max_completion_tokens`，以服务的文档为准。OpenAI 的 API 已弃用 `max_tokens`，其 o 系列模型不接受它；不设上限的请求两个字段都不带 |
| `provider.fillMissingReasoning` | 仅 `openai`。`true` 时，只要此前有一个回答带 `reasoning_content`，其余回答发送时补一个空的，用于拒绝“只有部分回答带推理”的思考模式服务 |
| `provider.promptCacheKey` | 仅 `openai`。`true` 时把会话 ID 作为 `prompt_cache_key` 发送；拒绝未知字段的服务不能打开 |
| `provider.requestFields` | 加到每个请求体中的字段（TOML 的值按 JSON 发送），如下文 ⚑ D:5/CHINA FLAG: DeepSeek 示例中的 `thinking`。被拒绝的字段：zx-agent 自己设置的字段（`openai` 为 `model`、`messages`、`stream`、`stream_options`、`tools`、`max_tokens`、`max_completion_tokens`，以及打开 `promptCacheKey` 时的 `prompt_cache_key`；`anthropic` 为 `model`、`messages`、`system`、`tools`、`max_tokens`、`stream`），以及设了 `reasoning` 时由它写入的字段（`openai` 按 `reasoningFormat`；`anthropic` 为 `thinking` 与 `output_config.effort`） |
| `provider.headers` | 每个请求都发送的请求头，如网关的路由头；携带 API key 的头被拒绝（`openai` 的 `Authorization`，`anthropic` 的 `x-api-key`） |
| `provider.idleTimeoutSecs` | 等待响应头或下一段流的最长时间，秒，至少 1，默认 120 |
| `provider.maxRetries` | 临时性错误的重试次数，默认 3 |
| `execution` | 命令在哪里执行：`local`（默认，本机）、`container`（见下文“容器执行”），或宿主经 `register_executor` 注册的名字（见 [LIBRARY.md](LIBRARY.md#与命令行共用配置)）；没有注册的名字以退出码 2 报错 |
| `[[models]]` | 模型元数据：`id`（必填，不能重复）、`contextWindow`（大于零）、`cost`（每百万 token 的单价：`input`、`output` 必填，`cacheRead`、`cacheWrite` 默认 0；都不小于零，币种自定但须一致；`input` 指不读也不写缓存的提示词 token）。没有内置的模型表。有单价时回复带 `usage.totalCost`；花费预算需要单价；窗口用于推导 `compaction.triggerTokens` |
| `[[mcp]]` | 外部 MCP 服务器，见下文“MCP 服务器” |

`reasoning`、`maxTokens` 与 provider 中决定请求体写法的键（`systemUpdates`、`reasoningFormat`、`maxTokensField`、`fillMissingReasoning`、`promptCacheKey`、`requestFields`）随请求记入会话；服务地址、`headers` 与 API key 不记录。宿主注册的 provider 记录什么由它自己决定。

### provider 示例：Anthropic

`api = "anthropic"` 经 [Messages API](https://platform.claude.com/docs/en/api/messages/create) 访问 Claude 模型：

```toml
model = "claude-opus-5-5"
reasoning = "high"

[provider]
api = "anthropic"                  # API key 默认取 ANTHROPIC_API_KEY
# 经网关时：
# baseUrl = "https://llm-gateway.example.com/anthropic"
```

- 请求带 `anthropic-version: 2026-10-21`；`headers` 中的 `anthropic-version` 替换它，`anthropic-beta` 等其他请求头原样发送。
- 不设 `maxTokens` 时每次请求的输出上限为 64000 token（这个 API 要求每次请求给出上限）；模型允许的输出上限更低时设 `maxTokens`。
- `reasoning` 写成自适应思考（`thinking` 的 `type = "adaptive"`、`display = "summarized"`）加 `output_config.effort`，`minimal` 与 `low` 都写成 `low`；`off` 写成 `thinking.type = "disabled"`，不接受它的模型（如 Claude Opus 5.5）返回 400，这时去掉 `reasoning`。`requestFields` 中 `thinking` 与 `output_config` 的其他键（如 `thinking.block_binding`）照常发送，`thinking.type`、`thinking.display` 与 `output_config.effort` 则与 `reasoning` 冲突。不设 `reasoning` 时请求不带思考设置，由模型的默认行为决定；只接受 `budget_tokens` 的旧模型把 `thinking` 写进 `requestFields`。
- 思考块连同签名记入会话，只回传给产生它的 provider 名与模型；换了模型之后，较早回答中的思考不再发送。
- 系统提示词、工具定义与对话末尾标为缓存断点（`cache_control`），以便下一次请求读取这一次写入的缓存；回答的输入 token 包括读写缓存的部分，与 `[[models]]` 的 `cacheRead`、`cacheWrite` 单价对应。`collapse` 下系统提示词有变化时，开头的缓存失效。
- 思考块的签名绑定产生它时的对话：系统提示词、工具与此前的消息。之后的请求开头与那时不同时（如 `collapse` 下系统提示词变了，或摘要之后原样保留的近期消息中的思考），服务按 `thinking.block_binding.prefix_mismatch_behavior` 处理：`"error"` 以 400 拒绝请求，`"drop_block"` 丢掉这些思考块后照常回答；不设时是否拒绝由模型与账号决定（Claude Fable 5.2 一律拒绝）。provider 不改写请求：被拒绝时这次请求失败，不重发。要让服务丢掉这些思考块，带上请求头并设这个字段（只设字段、不带请求头时服务以 400 拒绝；不设 `reasoning` 时 `thinking` 还要写 `type`，如 `"adaptive"`）；模型接受对话中间的 system 消息时，`systemUpdates = "inline"` 让系统提示词的变化不改动开头：

```toml
[provider]
api = "anthropic"
systemUpdates = "inline"
headers = { anthropic-beta = "thinking-binding-controls-2026-08-01" }
requestFields = { thinking = { block_binding = { prefix_mismatch_behavior = "drop_block" } } }
```

### provider 示例：⚑ D:5/CHINA FLAG: DeepSeek

⚠ COMPLIANCE REVIEW: [对话内容、命令输出与工具结果会发往该服务的 API，属于远程访问与数据外发] 使用前须经法务 / 出口合规审批（Subject to Legal/Export Compliance approval）。

⚑ D:5/CHINA FLAG: DeepSeek（官方文档页面没有写明公司所在地；据公开信息为中国公司，未核实）的 API 兼容 OpenAI 格式（[官方文档](https://api-docs.deepseek.com/)），用默认的 `openai` provider 即可：

```toml
# ⚑ D:5/CHINA FLAG: DeepSeek
model = "deepseek-flash"

[provider]
baseUrl = "https://api.deepseek.com"
apiKeyEnv = "DEEPSEEK_API_KEY"
# 关闭思考模式：
# requestFields = { thinking = { type = "disabled" } }
```

思考模式默认开启，`requestFields` 中的 `reasoning_effort` 调整强度。回答中的 `reasoning_content` 记入会话并回传给模型（官方文档：带工具的请求必须回传，否则返回 400）。历史中混有不带推理内容的回答（如中途换过模型）时打开 `provider.fillMissingReasoning`：pi #3668 记录过这种历史经 OpenRouter 调用时返回 400 “The `reasoning_content` in the thinking mode must be passed back to the API.”，本项目没有实测这种情况。

### 容器执行

`execution = "container"` 时 bash、read_file、edit_file、write_file 与搜索工具（grep、find、ls）在 Docker 容器中执行；行为与安全边界见 [SECURITY.md](SECURITY.md#容器执行)。

| 键 | 语义 |
|---|---|
| `container.image` | 必填，须已在本地，不会自动拉取 |
| `container.user` | 命令的 `uid:gid`，默认为运行 zx-agent 的用户（以 root 运行时为 `65534:65534`）；不能是 root |
| `container.memory` | Docker 写法的内存上限，默认 `1g` |
| `container.pidsLimit` | 进程数上限，大于零，默认 256 |
| `container.network` | Docker 网络：`bridge`（默认）、`none`（无网络）或自建网络的名字；`host` 与 `container:<名字>` 报配置错误 |
| `container.workspace` | `readWrite`（默认）或 `readOnly`：命令能否修改工作目录 |
| `container.root` | `readWrite`（默认）或 `readOnly`：命令能否修改容器自身的文件；`readOnly` 时只有内存中的 `/tmp` 可写 |

## agent

### 内置 agent

`run` 使用内置的 `unattended`，`chat`、`acp` 与 `zx-im` 使用内置的 `assistant`，构建时嵌入程序（全文在仓库的 `crates/zx-app/agents/`）。主目录下的同名文件替换它们；其他文件名不会被读取，也没有选择 agent 的命令行参数。Kubernetes 用的 agent 在仓库的 `deploy/kubernetes/agents/`。

| | `unattended` | `assistant` |
|---|---|---|
| 说明 | 没有人值守：只用工具收集证据，最后给出结果 | 和人对话，审批由人回答 |
| 工具 | `bash`、`read_file`、`read_skill` | `bash`、`read_file`、`edit_file`、`write_file`、`grep`、`find`、`ls`、`read_skill`、`read_output` |
| 权限 | 没有 `[permissions]`：bash 命令除内置的拒绝规则外都直接执行 | bash、`edit_file`、`write_file` 要审批；`grep`、`head`、`tail`、`wc`、`jq` 开头的命令直接执行（`allowExcept = ["secret"]`） |
| 上限 | 30 次模型请求、900 秒 | 不限 |
| 截断后续写（`continueOnLength`） | 1 次 | 不续写 |
| 项目 `AGENTS.md`（`context.discover`） | 不加载 | 加载 |
| 自动摘要（`compaction.triggerBytes`） | 关闭 | 上下文超过 256 KiB 时 |

`read_skill` 只在登记了 skill 时提供；`read_output` 见下文“工具结果落盘”。

### 文件格式

文件最多 64 KiB：第一行 `+++` 到下一行 `+++` 之间是 TOML front matter，之后是说明（系统提示词的开头）。格式错误或有未知的键时以退出码 2 报错。完整示例在仓库的 `examples/agents/`。

```markdown
+++
extends = "assistant"
tools = ["read_file", "read_skill"]

[permissions]
deny = ["bash"]

[limits]
maxModelRequests = 12

[[skills]]
name = "rust-review"
description = "Review Rust ownership, async cancellation, errors and test coverage."
path = "skills/rust-review/SKILL.md"
+++
You review code without changing files.
```

| 键 | 语义 |
|---|---|
| `extends` | `unattended` 或 `assistant`，只能是内置 agent。省略时从空的基线开始：没有工具、权限、上限、上下文、skills、自动摘要与说明，此时必须写说明 |
| `tools` | 替换启用列表：`bash`、`read_file`、`edit_file`、`write_file`、`grep`、`find`、`ls`、`read_skill`、`read_output`；`[]` 禁用全部 |
| `continueOnLength` | 输出达到 token 上限后自动续写的次数，每次续写占一次模型请求 |
| `[permissions]` | 见下文“权限” |
| `limits.maxModelRequests` | 每轮模型请求数，失败的尝试也计入；`"unlimited"` 不限，`0` 不请求模型 |
| `limits.timeoutSecs` | 每轮总时间上限，`0` 关闭 |
| `limits.tokenBudget` | 每轮输入加输出 token 上限，以服务返回的用量为准，摘要请求、失败的尝试与缓存命中的输入也计入；`0` 关闭 |
| `limits.costBudget` | 每轮花费上限，按 `[[models]]` 的单价计算，单位与单价相同；摘要请求与失败的尝试计入，工具报告的花费不计入；`0` 关闭。模型没有单价时以退出码 2 拒绝启动；切换模型或请求钩子选择了未配置单价的模型时，该次运行在发送请求前失败 |
| `context.discover` | 是否自动发现项目 `AGENTS.md`，见下文“项目上下文” |
| `context.files` | 显式上下文文件 |
| `[[skills]]` | 登记 `name`、`description`、`path`；正文由 `read_skill` 按需读取 |
| `compaction.*` | 见下文“上下文管理” |

两种预算都在每次请求之前检查，最后一次请求可能超出。上限、预算或续写用完时 run 的退出码见 [REFERENCE.md](REFERENCE.md#run-退出码)。`context.files` 与 `skills.path` 相对 agent 文件所在目录解析。

合并规则：文件写了的键整个替换继承的值，列表也是；表（`permissions`、`limits`、`context`、`compaction`）逐键合并，`permissions.allowOptions` 按程序替换。说明不为空时替换继承的说明。命令行的 `--tools` 与预算参数再覆盖文件。agent 不改变子命令的交互方式：`run` 始终无人值守，chat 始终在终端审批 bash（`approveAll` 除外）。

## 权限

条目是工具名（内置工具，或 MCP 工具 `<服务器名>__<工具名>`），或 `bash(<文本>)`。

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
| `ask` | 要审批的工具名，不能写 `bash(...)`。run 没有人审批而拒绝，chat 在终端询问，zx-im 发审批卡片，acp 交给客户端 |
| `allow` | `bash(<前缀>)`：以该前缀开头（按词比较，区分大小写，只合并空白）的命令不经审批直接执行，只在 bash 需要审批时起作用；带 shell 语法、URL 或串联多个命令的仍要审批，见 [SECURITY.md](SECURITY.md#自动放行)。MCP 工具名：该工具直接执行（MCP 工具默认都要审批）。写内置工具名，或同一工具同时在 `ask` 与 `allow` 中，报错 |
| `allowExcept` | 命令中出现其中的字词（不分大小写，子串比较）时，`allow` 放行的命令仍要审批 |
| `allowOptions.<程序>` | 该程序（一个小写的词）的命令不经审批时可以带的选项：`long`（不带 `--` 的长选项名）、`shortWithValue`（带值的单字母选项，如 `-oyaml`）、`shortFlags`（不带值、须单独出现，如 `-A`）。其他选项一律审批；没有列出的程序不限选项。选项按命令的第一个词查找，所以程序须是 `allow` 中某条 `bash(...)` 的第一个词：只经 `sudo` 或带路径放行的程序得不到约束，这样的配置以退出码 2 报错 |
| `deny` | 工具名：拒绝该工具，批准也不能覆盖。`bash(<文本>)`：拒绝任何位置含该文本的命令，不分大小写。内置的拒绝规则始终保留 |
| `approveAll` | `true` 时不询问任何审批，拒绝规则仍然生效。只能与隔离命令的执行环境一起使用：内置的只有 `execution = "container"`，宿主注册的执行环境自己声明是否隔离；`ask` 须为空（扩展 `assistant` 时写 `ask = []`） |

没有命令白名单：只允许一部分命令时，写 `ask = ["bash"]` 加上 `allow`，其余命令要审批，在 run 中被拒绝。条目无效（工具名不认识、为空、`bash()` 中没有文本或缺右括号）时以退出码 2 报错。

## MCP 服务器

只支持 Streamable HTTP，本项目只做客户端。

```toml
[[mcp]]
name = "prom"
url = "https://prom-mcp.monitoring.svc/mcp"
tokenEnv = "PROM_MCP_TOKEN"
tools = ["query", "query_range"]
```

| 键 | 语义 |
|---|---|
| `name` | 服务器名，小写 ASCII 字母、数字与 `-`，不能重复。模型看到的工具名是 `<name>__<工具名>`（其他字符换成 `_`，全名至多 64 个字符） |
| `url` | `http://` 或 `https://` 端点 |
| `tokenEnv` | 可选，存放 token 的环境变量，以 `Authorization: Bearer` 发送；设置后必须有值 |
| `tools` | 交给模型的工具，至少一个，其余不暴露；服务器缺少其中的工具时启动失败 |
| `timeoutSecs` | 可选，大于零：一次调用多久没有应答也没有进度通知就超时，默认同 `toolTimeoutSecs`；每条进度通知重新计时 |

- 只有列出的工具交给模型；agent 的 `permissions.allow` 列出的直接执行，其余每次都要审批（run 中因此拒绝）。服务器给出的 annotations 不影响放行。
- 每个进程启动时连接一次；连不上、token 变量没有值、服务器缺少列出的工具时以退出码 2 报错。成功时 stderr 每个服务器一行 `[mcp] <名字>: <工具>; protocol <版本>; <地址去掉路径的部分>`，经 `http://` 发送 token 时另加 `; the token is sent unencrypted`。
- 先用 `server/discover` 握手，旧服务器回退到 `initialize`。结果中的文本原样交给模型，图片、音频与二进制资源只写一行说明，最多 2000 行或 50 KiB；进度通知作为工具进度显示。

## 项目上下文

开启 `context.discover` 时，从最近的 Git 根目录到工具 cwd 依次加载各层 `AGENTS.md`（没有 Git 根目录时只检查 cwd）。自动发现的文件解析符号链接后必须位于 Git 根目录（或 cwd）内，否则在请求模型前以退出码 2 报错。每个文件最多 64 KiB，总量最多 256 KiB，超限报错，不截断。

## skills

skills 只来自 agent 的 `[[skills]]`，不扫描全局目录，不解析 SKILL.md 的 frontmatter，不执行其中的脚本。每个 skill 的名称、简介与正文文件的路径进入系统提示词；正文只在模型调用 `read_skill` 时读取，与其他工具结果一样经过策略、脱敏与审计。最多登记 64 个；`name` 为 1–64 个小写字母、数字或连字符，不能重复；`description` 为 1–1024 字节；正文至多 64 KiB，像 `read_output` 的结果一样分页读取，一页连同脚注至多 50 KiB，脚注为 `[Use read_skill with name="<名称>", offset=<偏移> to continue.]`。

## 上下文管理

自动摘要由 `compaction.triggerBytes` 或 `compaction.triggerTokens` 开启；手动 `/compact` 不需要这两个阈值。一次提问之内的工具调用超过阈值时同样摘要：较早的调用与结果进入摘要，这条提问原样保留在摘要之前。

| 键 | 语义 |
|---|---|
| `compaction.triggerBytes` | 自动摘要的字节阈值，0 关闭；非零须大于 `keepRecentBytes`，且超出部分不多于 2 MiB |
| `compaction.keepRecentBytes` | 保留的近期消息字节数，大于零，默认 65536；最新输入与全部 system 总是完整保留 |
| `compaction.triggerTokens` | 按 token 估算的阈值，0 关闭，任一阈值超过即摘要；须高于 `keepRecentBytes / 4`。省略时，若 `triggerBytes` 开启且已知上下文窗口，取“窗口减 `reserveTokens`”：窗口是会话当时所用模型的，每次请求前重新取，`/model` 换了模型就随之改变；差值不高于 `keepRecentBytes / 4` 的模型没有这个阈值 |
| `compaction.reserveTokens` | 推导 `triggerTokens` 时留出的 token 数，默认 16384 |
| `compaction.maxSummaryTokens` | 摘要输出 token 上限，1–16384，默认 2048；把推理计入输出的模型需要调高 |
| `compaction.timeoutSecs` | 摘要的总超时，秒，至少 1，默认 120 |

一次自动摘要占用一次模型请求预算。配置费用预算时，手动 `/compact` 也会在发送前校验模型单价。自动摘要得到了回答、但摘要不可用（如不是合法的 JSON）时，本会话暂停自动摘要，直到手动 `/compact` 成功、切换模型或开新会话；请求出错、连接中断或超时不暂停，下次需要时再试。开启自动摘要时，模型以上下文过长拒绝请求后，运行先摘要一次，再从压缩后的上下文继续（不算重试，不占 `provider.maxRetries`）。摘要每次重新读取边界之前的全部原始记录，累计超过 2 MiB 后无法再摘要，需要 `/new`。

## 工具结果落盘

agent 启用 `read_output` 时，超过阈值的工具结果在脱敏之后写入文件，会话与模型收到它的第一页，末尾的脚注给出输出 ID 与下一页的字节偏移（`[Use read_output with id="<输出 ID>", offset=<偏移> to continue.]`），其余由模型用 `read_output` 分页读取；一页至多 2000 行，放得下时是整行（超长的一行在字符边界切开，下一页接着读），连同脚注不超过阈值。不启用时结果保持内联。文件在结果目录中名为 `<输出 ID>.txt`。`read_file`、`read_skill` 与 `read_output` 自己分页，它们的结果不落盘。写入失败或 3 秒内没完成时结果保持内联。

| 键 | 语义 |
|---|---|
| `toolOutput.thresholdBytes` | 超过此大小的结果落盘；0 保持内联，否则为 256–`maxFileBytes`，默认 8192 |
| `toolOutput.maxFileBytes` | 单个文件上限，1–1048576，默认 1 MiB |
| `toolOutput.directory` | 结果目录，相对 `config.toml` 所在目录；省略时为每个会话工作目录下的 `.zx-agent/outputs` |

文件不自动删除；目录中写入忽略全部文件的 `.gitignore`，Unix 上新目录与文件的权限为 0700 与 0600。

## im.toml

`zx-im --config <im.toml>` 读取的 IM 服务配置，与 `config.toml` 一样是最多 64 KiB 的普通 UTF-8 文件，未知的键视为错误。`[access]`（含 `conversations`）、`[sessions]` 与至少一个平台的段必须写。`[access]`、`[limits]`、`[sessions]`、`[replies]`、`[cardLabels]` 以外的每个段配置以段名注册的平台：内置的有 `slack` 与 ⚑ D:5/CHINA FLAG: `feishu`，宿主自己的程序经 `zx_im::Program::register_platform` 加别的（见 [LIBRARY.md](LIBRARY.md#im-平台)）；没有注册的段名以退出码 2 报错。⚑ D:5/CHINA FLAG: 飞书平台在 `zx-im` 默认开启的 cargo feature `feishu` 后；不带它构建的 `zx-im` 遇到 `[feishu]` 段时报错，说明这个构建没有该平台。行为见 [REFERENCE.md](REFERENCE.md#im-服务)。

```toml
[slack]
# appTokenEnv = "SLACK_APP_TOKEN"     # 存放 token 的环境变量名，注释中的值为默认值
# botTokenEnv = "SLACK_BOT_TOKEN"
# apiBase = "https://slack.com/api"   # 经代理或测试服务器时改

# ⚑ D:5/CHINA FLAG: 飞书；两个平台可以同时配置。名单中写作 feishu:oc_…（群或私聊）与 feishu:ou_…（用户）
# [feishu]
# appIdEnv = "FEISHU_APP_ID"
# appSecretEnv = "FEISHU_APP_SECRET"
# apiBase = "https://open.feishu.cn"
# tokenUrl = "https://accounts.feishu.cn/oauth/v3/token"

[access]
conversations = ["slack:C0123456789"]   # 只在这些频道或私聊中回答，其他地方只回一句“未授权”
approvers = ["slack:U0123456789"]       # 只有这些用户能批准或拒绝；默认为空，审批只能超时

[limits]
maxRunning = 4               # 同时运行的会话数，默认 4，其余排队
maxConversations = 128       # 运行、排队和空闲的会话 worker 总数，默认 128
maxPendingMessages = 32      # 每个会话已取入但尚未处理的消息数，默认 32
maxPendingBytes = 262144     # 上述消息的 UTF-8 文本总字节数与单条入站消息上限，默认 256 KiB
idleSecs = 1800              # 会话空闲多少秒后释放（关闭会话文件），默认 1800

[sessions]
dir = "/data/sessions"       # 会话文件在 <dir>/--<工作目录>--/ 下；<dir>/.running 与 <dir>/.backlog 是服务自己的记录

# 可选：替换默认的英文文案，全部字段与中文译文见仓库的 deploy/kubernetes/im/im.example.toml
[replies]
working = "处理中…"
[cardLabels]
allowed = "已批准"
```

`[limits]` 中的四个上限须为正整数。达到上限时会发生什么见 [REFERENCE.md](REFERENCE.md#im-服务) 的“容量”。

---

CONFIGURATION.md · v0.28 · 2026年11月
