# zx-agent

一个用 Rust 编写的通用 agent 内核：同一个内核支撑人机对话、无人值守运行和被其他程序嵌入。场景之间的差异通过 agent（提示词、工具、权限、上限）和插件表达，不改内核。分层方式、循环语义和会话文件格式沿用 [pi](https://github.com/earendil-works/pi)（commit `a92283056`），不同之处见[与 pi 的差异](#与-pi-的差异)。

当前版本为 v0.1.0，已实现：

- 子命令：无人值守的 `run`，终端对话 `chat`（应急与脚本化入口），ACP agent `acp`（交互式界面由 ACP 客户端提供），IM 服务 `im`，列出会话的 `sessions`。
- OpenAI 兼容的流式 provider，带错误分类、自动重试与响应大小上限。
- 工具：`bash`、`read_file`、`edit_file`、`write_file`，只读的 `grep`、`find`、`ls`，按需读取 skill 的 `read_skill`，分页读取落盘结果的 `read_output`，以及 MCP 服务器的工具。
- 主目录中的 `config.toml`（环境）与 agent 文件（角色），项目 `AGENTS.md`；命令策略与 ask / allow / deny 权限；已知密钥脱敏；可选的容器执行。
- 只追加的会话记录（默认 JSONL 文件，可续跑），可选的自动与手动摘要，长工具结果落盘。
- 轮次、时间、token 与花费上限；JSONL 事件流与可选的 OpenTelemetry 追踪导出；扩展接口 `Extension`。

尚未实现：OpenAI 兼容以外的 provider（如 Anthropic），见[路线图](docs/ROADMAP.md)。

## 快速开始

需要 Rust 1.88 及以上与 C 编译器（aws-lc 在构建时编译）。HTTPS 使用系统的根证书：Linux 上没有安装 CA 证书（如 Debian 的 `ca-certificates`）时启动即报错 `No CA certificates were loaded from the system`，只连 `http://` 时也一样。

```bash
cargo build --release
export OPENAI_API_KEY=...
./target/release/zx-agent run --model <模型 ID> "检查本机磁盘与内存使用情况，找出资源占用最高的进程"
```

模型与服务地址可以写进 `~/.zx-agent/config.toml`，见[配置说明](docs/CONFIGURATION.md)：

```toml
model = "<模型 ID>"

[provider]
baseUrl = "https://api.openai.com/v1"
apiKeyEnv = "OPENAI_API_KEY"
```

本地或私有部署的 OpenAI 兼容服务（vLLM、Ollama、llama.cpp server 等）用 `ZX_BASE_URL` 指定地址；找不到 API key 时请求不带认证头：

```bash
ZX_BASE_URL=http://127.0.0.1:11434/v1 ./target/release/zx-agent run --model <模型名> "排查 nginx 返回 502 的原因"
```

### 终端对话

```bash
zx-agent chat                          # 新对话，存入会话目录
zx-agent chat --resume                 # 续接本工作目录最近一次的对话
zx-agent chat --resume <ID 前缀>       # 续接 ID 以它开头的唯一会话
zx-agent sessions                      # 列出本工作目录的会话（可加 --cwd）
zx-agent chat --session ./chat.jsonl   # 指定会话文件
```

不给 `--session` 时，每次对话存为 `~/.zx-agent/sessions/--<工作目录>--/<UTC 时间>_<会话 ID>.jsonl`（目录名为去掉开头 `/`、其余 `/` 换成 `-` 的工作目录），新建的目录权限 0700、文件 0600；`--no-session` 只保存在内存。`--resume` 找不到会话时新建并提示，`--resume <ID 前缀>` 找不到或不唯一时以退出码 2 结束。`sessions` 每行一个会话：ID 前 8 个字符、最近活动时间、消息数、会话名称（没有名称时为第一条提示词的首行）。名称由 ACP 客户端的 `/name` 或 SDK 的 `AgentSession::set_name` 设置，chat 没有起名命令。

输入与输出：

- 直接输入问题开始下一轮。TTY 中运行时输入的文本作为插话，在下一次模型请求前进入上下文；任务结束时没被读到的插话作为下一轮提问发送，Ctrl+C 取消任务时丢弃。一次粘贴的多行合为一条消息（以 `/` 开头的命令只占一行；非 Unix 平台不合并）。管道输入逐轮、逐行消费。
- 单行（以及合并后的一条消息）最多 1 MiB；超限或无效 UTF-8 的行整行丢弃并提示，对话继续。
- stdout 只有模型的回答，其余在 stderr：启动信息、每个工具调用一行 `[tool] bash: <命令> -> ok 12ms` 及结果的前 3 行（`edit_file` 显示补丁的前 20 行），每轮一行 `[turn] 3 requests · 30 in · 15 out · 4.4s`。默认只在交互终端上着色，见 `--color`。
- 上限取自 agent：内置的 assistant 每轮不限轮次与时间、不自动续写。`--output jsonl` 每轮输出事件与一个 `run_end`。
- 这是逐行 REPL，没有行编辑：方向键等控制序列会原样作为输入发送。

| 输入 | 行为 |
|---|---|
| `/help` | 查看命令，含扩展注册的命令 |
| `/model`、`/model <id>` | 查看模型，或空闲时切换同一 provider 的模型，保留历史 |
| `/reasoning`、`/reasoning <级别>` | 查看或空闲时修改推理强度（`off`、`minimal`、`low`、`medium`、`high`、`xhigh`；`default` 交给服务端） |
| `/new` | 开新会话，清空历史和授权，重新加载 agent 与项目说明；运行中先取消任务。旧会话文件保留 |
| `/compact` | 请模型压缩旧上下文，原始记录保留；没有可压缩范围时不发请求 |
| `/continue` | 不加新输入，让模型接着回答，规则同 `run --continue` |
| `/status` | 模型、agent、会话文件、工作目录，以及本会话累计的轮数、请求数与 token 用量 |
| `/<扩展命令> [参数]` | 扩展注册的命令，内置命令优先；运行中也立即执行 |
| `/quit` | 取消当前任务并退出 |
| Ctrl+C | 取消当前任务或审批，保留会话；空闲时 2 秒内连按两次退出 |
| EOF（Ctrl+D）、SIGTERM | EOF 结束会话，管道中的任务先完成；SIGTERM 取消并退出 |

需要审批的调用（内置 assistant 的 bash、`edit_file`、`write_file`）显示工具名与完整参数，等待选择；agent 的 `permissions.allow` 列出的只读命令（内置为 `grep`、`head`、`tail`、`wc`、`jq`）直接执行，规则见[安全模型](#安全模型)：

```text
[approval 1] bash: ls -la
  the agent requires tool approval
  1/y allow once · 2 allow identical calls in this conversation · 3/n deny · 4 discuss
[approval] Choose 1, 2, 3 or 4:
```

1 只允许这一次；2 本会话允许工具名与参数完全相同的调用；3 拒绝；4 不执行，把输入的话交给当前任务。空输入不授权，超时（默认 600 秒，`approvalTimeoutSecs`）不执行。

退出码：`/quit` 与 EOF 为 0，参数或配置错误、读取终端失败为 2，SIGTERM 为 5。单轮的模型或预算错误报告后继续对话。

### ACP 客户端

```bash
zx-agent acp
```

`acp` 经 stdin 与 stdout 收发 [Agent Client Protocol](https://agentclientprotocol.com/) v1 的消息，由 ACP 客户端（Zed、JetBrains、Neovim、Emacs 的插件，或 Wisp 等终端客户端，见[客户端目录](https://agentclientprotocol.com/overview/clients)）启动：在客户端里把 agent 命令配置为上面这一行。参数与 `chat` 相同（`acp --help`），没有 `--session`、`--output` 与 `--color`；stdout 只走协议，诊断在 stderr。

- 会话：`session/new` 建会话，`cwd` 由客户端给出；每个会话一个文件，放在 chat 为该工作目录使用的会话目录中（`--no-session` 时只在内存）。`session/load` 续接并把对话回放给客户端，`session/resume` 续接但不回放，`session/list` 列出会话（标题为会话名称，没有名称时为第一条提问）。`session/close` 取消会话正在进行的回合，并把会话移出内存：会话文件随之关闭（有回合在跑时，等它结束），别的进程就能打开它；文件保留，可以再 `session/load` 或 `session/resume`。关闭本进程没有持有的会话也算成功。`initialize` 总会声明 `sessionCapabilities.close`（`--no-session` 时也是，关闭同样释放内存）。客户端不发 `session/close` 时，会话留在内存中、文件保持锁定，直到进程退出。`session/fork` 未实现。客户端给的 `mcpServers` 被忽略，MCP 服务器只来自 `config.toml`。
- 命令：建会话、载入或续接成功后，应答之后以 `available_commands_update` 声明 `/name`。`/name <名称>` 给会话起名，不发给模型：以 `session_info_update` 通知新标题，回合以 `end_turn` 结束。名称只取一行（换行等控制字符换成空格、去掉首尾空白）；没有给名称时以 JSON-RPC 错误（`-32602`）应答，回合进行中也以错误应答。`/named` 等其他文本仍是普通提问。
- 回合：`session/prompt` 的回答结束时应答 `stopReason`：完成为 `end_turn`，截断或 token 预算用尽为 `max_tokens`，请求次数上限为 `max_turn_requests`，取消为 `cancelled`；出错或超时以 JSON-RPC 错误应答。`session/cancel` 取消当前回合。
- 更新：回答与推理流式发送，工具调用发 `tool_call` 及其状态，`write_file` 与 `edit_file` 附 diff。审批经 `session/request_permission`（允许一次、本会话允许相同调用、拒绝），等待时间同 chat。
- 只用手写的 JSON-RPC 客户端与 SDK 的示例客户端验证过，没有在 Zed 或 Wisp 上验证。

### 配置与 agent

配置都在主目录（`~/.zx-agent`，或 `ZX_AGENT_HOME`）中，都可以没有。`config.toml` 决定环境：模型、provider、执行环境、MCP 服务器、模型的单价与上下文窗口。agent 决定角色：说明、工具、权限、上限、项目上下文、skills 与摘要。`run` 使用内置的 `unattended`，`chat`、`im` 与 `acp` 使用内置的 `assistant`；主目录下的 `agents/unattended.md`、`agents/assistant.md` 替换它们。`chat` 默认加载 Git 根目录到工作目录的 `AGENTS.md`，`run` 默认不加载。每个键见 [CONFIGURATION.md](docs/CONFIGURATION.md)。

### 无人值守示例

可用于 cron、systemd timer、Kubernetes Job：

```bash
export ZX_AGENT_HOME=/etc/zx-agent-inspect
mkdir -p "$ZX_AGENT_HOME/agents"
cat > "$ZX_AGENT_HOME/agents/unattended.md" <<'EOF'
+++
extends = "unattended"

[permissions]
ask = ["bash"]
allow = ["bash(kubectl get)", "bash(kubectl describe)", "bash(kubectl logs)", "bash(grep)", "bash(tail)", "bash(head)"]
+++
EOF
zx-agent run \
  --model "$ZX_MODEL" --tools bash \
  --max-model-requests 20 --timeout-secs 600 \
  --session "/var/log/zx-agent/$(date +%Y%m%d-%H%M%S).jsonl" \
  --output jsonl \
  "命名空间 shop 中的 Pod 频繁重启，排查原因" > events.jsonl
echo "exit code: $?"
```

`allow` 之外的 bash 命令要审批，`run` 没有人审批，因此拒绝。在 Kubernetes 中常驻部署见 [deploy/kubernetes/README.md](deploy/kubernetes/README.md)。

### IM 服务

`zx-agent im` 作为常驻进程接入 IM：私聊整段是一个会话，频道里一个线程一个会话；写操作在 IM 里审批。支持 Slack（Socket Mode）与 ⚑ D:5/CHINA FLAG: 飞书（长连接），都只需出网。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：对话内容、命令输出与用户 ID 发往该平台] 该平台的使用 Subject to Legal/Export Compliance approval。部署到 Kubernetes 见 `deploy/kubernetes/README.md` 的“IM 服务”。

```bash
export SLACK_APP_TOKEN=xapp-...   # app-level token，用于 Socket Mode
export SLACK_BOT_TOKEN=xoxb-...   # bot token，用于发消息
# 使用 ⚑ D:5/CHINA FLAG: 飞书时改为 FEISHU_APP_ID 与 FEISHU_APP_SECRET（企业自建应用的凭据）
zx-agent im --config im.toml --model <model>
```

`im.toml`（未知的键视为错误）：

```toml
[slack]
# appTokenEnv = "SLACK_APP_TOKEN"   # 存放 token 的环境变量名，此为默认值
# botTokenEnv = "SLACK_BOT_TOKEN"

# ⚑ D:5/CHINA FLAG: 飞书；两个平台可以同时配置。名单中写作 feishu:oc_…（群或私聊）与 feishu:ou_…（用户）
# [feishu]
# appIdEnv = "FEISHU_APP_ID"
# appSecretEnv = "FEISHU_APP_SECRET"

[access]
conversations = ["slack:C0123456789"]   # 只在这些频道或私聊中回答，其他地方只回一句“未授权”
approvers = ["slack:U0123456789"]       # 只有这些用户的点击能批准写操作

[limits]
maxRunning = 4              # 同时运行的会话数，默认 4，其余排队
idleSecs = 1800             # 会话空闲多久后释放其文件锁，默认 1800

[sessions]
dir = "/data/sessions"      # 线程的会话在 <dir>/slack/<频道>/<线程>/，私聊的在 <dir>/slack/<私聊>/，文件为 1.jsonl、2.jsonl…

# 可选：替换默认的英文文案（字段名见 Replies 与 CardLabels）
[replies]
working = "处理中…"
[cardLabels]
allowed = "已批准"
```

| 项 | 说明 |
|---|---|
| Slack 应用 | 打开 Socket Mode 与 Interactivity；订阅 bot 事件 `app_mention` 与 `message.im`。bot token 需要读取 @ 消息的权限（`app_mentions:read`，以应用设置页显示的为准）、`im:history` 与 `chat:write`；app-level token 带 `connections:write`。可选：订阅 `message.channels`、`message.groups`（权限 `channels:history`、`groups:history`）后线程里不 @ 的回复也会送来，机器人也能读取它第一次被 @ 的线程；`users:read` 让模型看到发言人的名字；`reactions:write` 让插话得到 👀 回执。Marketplace 之外商业分发的应用读取线程受 Slack 的限频（每分钟 1 次、每次 15 条），自建的内部应用不受影响 |
| ⚑ D:5/CHINA FLAG: 飞书应用 | 企业自建应用；事件与回调都选长连接；订阅事件 `im.message.receive_v1` 与回调 `card.action.trigger`；开通发送消息与读取 @ 机器人消息（`im:message.group_at_msg:readonly`）的权限。可选：开通“获取群组中所有消息”（`im:message.group_msg`）后，话题里不 @ 的回复也会送来。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：读取群内全部消息] Subject to Legal/Export Compliance approval。读话题历史、名字与回执未实现（模型看到的是 open_id） |
| 会话 | 私聊直接发送，`/new` 开新会话。频道（⚑ D:5/CHINA FLAG: 飞书为群）里 @ 机器人，回答在这条消息的线程里；刚被回答的人接着说不必再 @；别人在线程里的话只记进会话，被 @ 时一并参考。在别人发起的线程里第一次被 @ 时，先读入线程已有的消息（至多 50 条）。群聊中交给模型的每条消息前加发言人的名字。没 @ 机器人的频道消息、机器人没参与过的线程一概不理会；会话释放（`idleSecs`）或重启之后，接话要再 @ 一次 |
| 运行中 | 要回答的消息作为插话进入运行；单独发送 `/cancel` 取消本会话的任务。开始时发“处理中”，结束时替换为回答 |
| 数据范围 | 只保存机器人参与过的线程与授权聊天中的消息，其余在入口丢弃。线程中任何人的话都会进入模型的上下文，写操作仍要审批 |
| 审批 | 使用 assistant agent：自动放行的只读命令直接执行，其余发审批卡片，只有 `approvers` 中的用户点击才生效；超时（`approvalTimeoutSecs`）与取消也显示在卡片上 |
| 凭据 | 两个平台的 token 与 secret 只从环境变量读取；命令的环境中删除这些变量，输出中出现其值时脱敏 |
| 日志 | stdout 每行一个 JSON：`approval`（聊天、线程、工具调用、审批人、结果、时间）、`released`、`failed`。对话内容在会话文件中 |
| 退出 | SIGINT 或 SIGTERM 时取消进行中的任务，状态改为 `stopped` 的文案；未处理的消息不再处理，所在线程收到提示。退出码：正常为 0，配置错误与被拒的凭据为 2，平台连接意外结束为 1 |
| 崩溃之后 | 未处理的消息丢失且没有提示。下次启动时，上个进程遗留的“处理中”改为 `interrupted` 的文案 |

其余参数与 `chat` 相同，`--max-model-requests` 等作用于每一次运行；没有 `--session`。

### MCP 工具

在 `config.toml` 中用 `[[mcp]]` 列出外部 MCP 服务器及其工具（只支持 Streamable HTTP，[配置项](docs/CONFIGURATION.md#mcp-服务器)），模型以 `<服务器名>__<工具名>` 调用。

- 只有列出的工具交给模型；agent 的 `permissions.allow` 列出的直接执行，其余每次都要审批（run 中因此拒绝）。服务器给出的 annotations 不影响放行。
- 每个进程启动时连接一次；连不上、token 变量没有值、服务器缺少列出的工具时以退出码 2 报错。成功时 stderr 每个服务器一行 `[mcp] <名字>: <工具>; protocol <版本>; <地址>`，经 `http://` 发送 token 时另加 `the token is sent unencrypted`。
- 先用 `server/discover` 握手，旧服务器回退到 `initialize`。结果中的文本原样交给模型，图片、音频与二进制资源只写一行说明，最多 2000 行或 50 KiB；进度通知作为工具进度显示。

### DeepSeek

⚑ D:5/CHINA FLAG: DeepSeek（官方文档页面没有写明公司所在地；据公开信息为中国公司，未核实）。

⚠ COMPLIANCE REVIEW: [对话内容、命令输出与工具结果会发往该服务的 API，属于远程访问与数据外发] 使用前须经法务 / 出口合规审批（Subject to Legal/Export Compliance approval）。

该服务的 API 兼容 OpenAI 格式（[官方文档](https://api-docs.deepseek.com/)），用现有的 provider 即可：

```toml
model = "deepseek-flash"

[provider]
baseUrl = "https://api.deepseek.com"
apiKeyEnv = "DEEPSEEK_API_KEY"
# 关闭思考模式：
# requestFields = { thinking = { type = "disabled" } }
```

思考模式默认开启，`requestFields` 中的 `reasoning_effort` 调整强度。回答中的 `reasoning_content` 记入会话并回传给模型（官方文档：带工具的请求必须回传，否则返回 400）。历史中混有不带推理内容的回答（如中途换过模型）时打开 `provider.fillMissingReasoning`：pi 记录过该服务对这种历史返回 “The `reasoning_content` in the thinking mode must be passed back to the API.”（pi #3668），本项目没有实测这种情况。

### run 退出码

| 退出码 | 含义 |
|---|---|
| 0 | 完成：模型给出了最终回答 |
| 1 | 失败：模型请求出错且重试无效，或会话文件写入失败 |
| 2 | 参数或配置错误 |
| 3 | 达到 `--max-model-requests` 上限 |
| 4 | 达到 `--timeout-secs` 时间上限 |
| 5 | 被 SIGINT 或 SIGTERM 中止 |
| 6 | 截断：最终回答达到输出 token 上限，自动续写（agent 的 `continueOnLength`）后仍未完成；已生成的部分照常输出 |
| 7 | 达到 `--token-budget` 预算：用量已达预算，模型还要继续 |
| 8 | 达到 `--cost-budget` 预算：花费已达预算，模型还要继续 |
| 9 | 只在给出 `--fail-on-denied` 时出现：模型给出了最终回答，但至少一次工具调用被策略或审批拒绝。不加这个参数时这种情况仍是 0，被拒的次数在报告的 `deniedCalls` 与文本模式最后一行的 `denied_calls=` 中 |

### 主要参数

标注 agent 的默认值取自所用的 agent，显式传入时覆盖。`chat` 与 `im` 中的上限与预算为每轮上限。

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--model` | `ZX_MODEL`，再取 `config.toml` 的 `model` | 模型 ID；都没有时以退出码 2 结束 |
| `--tools` | agent：`run` 为 `bash,read_file`，`chat` 为 `bash,read_file,edit_file,write_file,grep,find,ls,read_output`；有 skill 目录时加入 `read_skill` | 替换工具集合 |
| `--cwd` | 当前目录 | 工具的工作目录 |
| `--max-model-requests` | agent：`run` 为 30，`chat` 不限 | 模型请求次数上限，失败的尝试也计入 |
| `--timeout-secs` | agent：`run` 为 900，`chat` 为 0 | 时间上限，0 不限 |
| `--token-budget` | 不设 | 输入加输出 token 上限，`run` 用完时以 7 结束 |
| `--cost-budget` | 不设 | 花费上限，需要模型的单价，`run` 用完时以 8 结束 |
| `--fail-on-denied` | 关闭 | 仅 `run`：有工具调用被拒时以 9 代替 0 |
| `--session` | `run`：无；`chat`：会话目录中的新文件 | 仅 `run` 与 `chat`：会话文件，不存在则创建，存在则继续 |
| `--no-session` | 关闭 | 仅 `chat` 与 `acp`：不写会话文件 |
| `--resume [ID 前缀]` | 关闭 | 仅 `chat`，不能与 `--session` 同用 |
| `--continue` | 关闭 | 仅 `run`，须给出已存在的 `--session`，不能再给提示词：不加新输入，让模型接着回答（如崩溃之后）。会话以失败或被取消的回答结尾时先省略该回答；以完成的回答结尾且没有排队的消息时以 1 结束、不发请求 |
| `--output` | `text` | `text`：进度与诊断在 stderr，最终回答在 stdout；`jsonl`：每个事件一行 JSON，输出到 stdout |
| `--deltas` | 关闭 | 仅 `jsonl`：同时输出流式的 `message_update` |
| `--color` | `auto` | 仅 `chat`：`auto` 在交互终端上着色（设了 `NO_COLOR` 或 `TERM=dumb` 时不着色），`always`、`never` |

其余设置在 `config.toml` 与 agent 中，见 [CONFIGURATION.md](docs/CONFIGURATION.md)。流式响应的上限为每次请求 16 MiB、每个 SSE 事件 1 MiB，达到时以退出码 1 结束、不重试。重试依据 HTTP 状态与错误码，遵守服务端的 `Retry-After`（至多 60 秒）。服务端以 `stop`、`end`、`length`、`tool_calls`、`function_call` 以外的 `finish_reason`（如 `content_filter`）结束时按错误处理，错误码为 `finish_reason:<取值>`，不重试。

## 架构

一个 Cargo workspace，8 个 crate，依赖只能自上而下，由编译器保证：

```
zx-agent    src/             CLI（run、chat、im、acp）、config.toml 与 agent、OpenTelemetry 导出；
                             库以原有路径再导出下面各 crate
  ├─ zx-im-slack   crates/zx-im-slack   Slack 平台
  ├─ zx-im-feishu  crates/zx-im-feishu  ⚑ D:5/CHINA FLAG: 飞书平台
  │    └─ zx-im    crates/zx-im         IM 平台接口与按会话分派的服务
  ├─ zx-tools  crates/zx-tools  内置工具、执行环境、长结果落盘、命令与规则策略、续写与脱敏、MCP 客户端
  ├─ zx-core   crates/zx-core   循环、会话、运行层、共享会话、摘要、提示词、扩展接口
  │    └─ zx-ai  crates/zx-ai   消息类型、流式事件协议、provider 契约
  └─ zx-ai-openai  crates/zx-ai-openai  OpenAI 兼容 provider（只依赖 zx-ai）
```

只嵌入内核的程序不编译 HTTP 客户端。代码中没有场景、环境或平台文案语言的规则：场景是 agent 文件，Kubernetes 的规则在 `deploy/kubernetes/`。

一次运行：`AgentSession` 把运行交给 `Runner`；每一步先写入会话再发出事件；发给模型的上下文就是会话的投影；工具调用先逐个检查，再并发执行，按调用顺序写结果；失败的请求按策略重试，最后汇总为 `RunReport`。

### 与 pi 一致的关键语义

1. provider 不抛错：失败编码为 `stopReason` 为 `error` 或 `aborted` 的消息。
2. 模型的回复里没有工具调用时，循环结束。
3. 工具调用先逐个检查，再并发执行，按调用顺序写结果；有一个工具是 `Sequential` 时整批串行。
4. 工具失败（找不到、参数无效、被拒绝、执行报错、panic）都变成工具结果，循环继续。
5. 回复因 `length` 截断时，不执行其中的工具调用。
6. 引导消息在每个轮次结束后插入，后续消息在本来要停下时插入；两个队列共用一种取法（`QueueMode`）。
7. 发给模型的上下文以会话投影为准；重试时用 `context_edit` 省略失败的回答，不重复用户消息。
8. 系统提示词是对话的一部分：第一条 system 消息记录完整提示词，之后只追加变化的段落。

### 与 pi 的差异

| 项目 | zx-agent |
|---|---|
| 上限 | 有轮次与时间上限 |
| 工具确认 | 宿主审批；`run` 无人时拒绝 |
| 输出截断 | 按 agent 的 `continueOnLength` 自动续写，仍未完成以退出码 6 结束 |
| 会话写入失败 | 立即中止运行，之后不再写入 |
| 会话文件 | 启动时立即创建；chat 默认存入会话目录，`--resume` 续接；复制会话与切换分支只在 SDK |
| 会话名称 | 只经 ACP 的 `/name` 与 SDK 的 `AgentSession::set_name` 设置，不能清除；chat 没有起名命令；客户端改标题的 `session/set_title` 在协议中仍不稳定，未实现 |
| 队列取法 | 两个队列共用一种，默认 `All` |
| 扩展的消息与状态 | 运行中排队，下一回合开始或运行结束时写入；消息只有文本；请求只声明当前活跃的工具 |
| 环境信息 | 启动时写入系统提示词 |
| 参数校验 | 严格按 JSON Schema，不转换类型；改写只在工具的 `prepare_arguments` 中 |
| 中断的工具调用 | 有副作用的调用执行前先记录开始；恢复时补写“结果未知”或“未执行”，不重放 |
| 工具结果 | 只有文本 `content`，另有不发给模型的 `details` 与 `usage`；没有图片、`structuredContent`、`runToolCall` |
| 上下文过长 | 只按错误识别（HTTP 413、错误码、通用措辞），不按服务推断 |
| 摘要 | 每次重新读取原始记录，不输入上一份摘要；每条事实引用来源条目 |
| 续跑 | 以失败或被取消的回答结尾时，先省略该回答再续跑 |
| `prepare_request` | 只能替换模型与推理强度，不能改上下文 |
| 推理写法 | 由 `provider.reasoningFormat` 在三种中显式选择 |
| 模型元数据 | 只有 `[[models]]` 给出的上下文窗口与单价，没有内置模型表 |
| 界面 | 没有 TUI，交互界面交给 ACP 客户端 |

### 系统提示词的组成

| 部分 | 内容 | 何时出现 |
|---|---|---|
| 说明 | agent 的说明 | 总是 |
| `<environment>` | 操作系统与版本、CPU 架构、启动时间（UTC）与时区、工作目录；容器执行时描述容器 | 总是 |
| `<project_context>` | 自动发现的 `AGENTS.md` 与 agent 列出的文件 | 有项目说明时 |
| `<available_skills>` | skill 的名称、描述与正文文件路径 | 启用 `read_skill` 且登记了 skill 时 |

环境信息通过读文件获得，不执行命令；主机名和用户名不写入，因为提示词会发给模型服务方。恢复会话时只追加有变化的段落，发送方式见 `provider.systemUpdates`。

## 安全模型

策略只是第一道防线，不是安全边界。生产环境以最小权限账号运行，放在容器或虚拟机中，必要时限制网络。

命令策略（`CommandPolicy`）只检查 bash：

- 拒绝一组危险模式：`rm -rf /` 等多词模式按子串匹配（转小写、合并空白）；`shutdown`、`reboot`、`poweroff`、`halt`、`mkfs`（含 `mkfs.<类型>`）、`init 0`、`init 6` 作为程序出现时拒绝，作为 `grep`、`last`、`journalctl`、`cat`、`echo` 等只读程序的参数时不算。这些规则容易绕过，只起减速作用。
- agent 的 `permissions.deny` 中的 `bash(<文本>)` 拒绝任何位置含该文本的命令。
- 没有命令白名单，其余命令都放行；要只让部分命令运行，用 `ask = ["bash"]` 加 `allow`。

自动放行（`permissions.allow` 的 `bash(<前缀>)`）只决定是否询问。满足以下全部条件的命令不经审批：

- 是由清单中的前缀组成的管道；
- 只由字母、数字、空白和 `- _ . / : = , @ + | ^ ~ !` 组成：引号、`\`、`$`、通配符、括号、`#`、`;`、`&`、`` ` ``、`>`、`<` 都改为询问，因为它们能让 shell 执行的内容与字面不同；
- 不含 URL（`://`），也不含 `permissions.allowExcept` 的字词（内置 assistant 为 `secret`）；
- 程序在 `permissions.allowOptions` 中列出了选项时，只带这些选项。

检查不含任何具体程序的知识。把 kubectl 之类的程序加进 `allow` 时，要同时列出它可带的选项，否则任何选项都不会触发询问；`deploy/kubernetes/agents/assistant.md` 只放行 kubectl 的只读选项，`--server`、`--kubeconfig`、`--token`、`--raw`、`--user`、`--cluster`、`--profile-output`、`-f`、`-w`、`-v` 等都要询问。自动放行的命令能读到什么取决于账号与凭据，读到的内容会进入模型上下文。

工具：

- bash 命令结束、超时或被取消时，终止其进程组；用 `setsid` 等方式脱离进程组的进程可能继续运行，需要保证时用容器执行。
- `read_file`、`grep`、`find`、`ls` 不经命令检查，可以读进程能读的任何文件；`edit_file`、`write_file` 可以写进程能写的任何普通文件，包括工作目录之外的。需要限制时不启用它们，或用容器执行。内置的 `unattended` 与 Kubernetes 的 agent 不启用写文件与搜索工具。
- 输出都有界：`read_file` 单文件至多 10 MiB，每次至多 2000 行或 50 KiB；搜索工具有条数与 60 秒超时上限。
- MCP 服务器与工具都由使用者显式列出；工具描述与结果来自外部服务，可能含注入内容。工具一律按有副作用处理，响应体没有大小上限（SSE 每个事件至多 16 MiB），交给模型的结果至多 2000 行或 50 KiB。

密钥：

- 认证 API key 只从环境变量读取，bash 命令启动前删除该变量。工具输出在写入会话、事件和回传模型之前，把 key 的完整字面值替换为 `[REDACTED]`。
- 其他凭据文件用 `config.toml` 的 `secretFiles` 或 `ZX_SECRET_FILES` 登记（如 Kubernetes ServiceAccount 的 token），程序本身不预设路径。文件每次过滤前重新读取，轮换下来的旧值保留最近 32 个继续过滤。短于 8 个字符的值照常过滤，但所有相同的文本都会被替换，启动时在 stderr 以 `[secret] <文件>: …` 提示。过滤器出错时丢弃原始输出，返回错误结果。
- 过滤不识别未知、编码或拆分后的密钥，也不处理用户输入、模型输出、工具参数、项目上下文与说明。这些内容仍可能进入模型请求、会话和事件，需要控制会话文件的访问权限；chat 默认把会话写到主目录下，不想留下记录时用 `--no-session`。
- 本机执行时，工具与 agent 是同一系统用户，仍可经其他途径读到凭据（如 Linux 的 `/proc/<pid>/environ`）；其他环境变量（如云厂商凭证）bash 仍能读到，需要时在启动前清理。隔离要靠容器执行或不同账号。
- 项目上下文会发给模型服务方；自动发现的 `AGENTS.md` 不能经符号链接指向项目外，在不可信仓库中用 `discover = false` 的 agent。

### 容器执行

在 `config.toml` 中写 `execution = "container"` 并配置 `container.image`，bash、read_file、edit_file、write_file 与搜索工具改在 Docker 容器中执行；read_skill 与 read_output 仍读取本机文件。其他键见[配置说明](docs/CONFIGURATION.md)。容器是沙箱：限制放在容器的外墙上，容器内默认给命令该用户能有的全部权限。

- 每个进程一个容器，启动时创建（chat 的 `/new` 换一个新容器），随进程结束而删除，被 SIGKILL 时也一样。容器带 `zx-agent.session=<会话 ID>` 标签。启动时检查命令不以 root 运行、镜像有 bash 等程序、容器用户能读（可写时能写）工作目录；不满足、镜像不在本地或 Docker 不可用时以退出码 2 结束。
- 容器内默认：工作目录以读写方式挂载在同一路径，改动直接作用于宿主机；容器自身的文件可写；加入 `bridge` 网络，可以联网；命令以运行 zx-agent 的用户的 `uid:gid` 运行。严格模式为 `network = "none"`、`workspace = "readOnly"`、`root = "readOnly"`。
- 外墙：不加 `--privileged`，不挂 Docker socket，不用宿主机的网络与 PID 命名空间，去掉全部 capability 并设 `no-new-privileges`，限制进程数与内存，命令不以 root 运行。命令不继承 agent 的环境变量，API key 不进入容器。
- 同一容器内的调用逐个执行；每条命令前后终止命令用户的全部进程，脱离进程组的后台进程也不例外。
- 审批不变；agent 设 `permissions.approveAll = true` 后不再询问任何调用，拒绝规则仍然生效，它只能与容器执行一起使用。
- 可写又联网时，挂进容器的内容（代码、`.git` 中的远端地址等）可能被发送出去。工作目录整体对容器可见，所以会话文件必须在工作目录之外，否则启动时报配置错误（在主目录中运行 chat 时改用另一个 `ZX_AGENT_HOME` 或 `--no-session`）。
- 需要凭据的在线访问（如集群 API）不经容器执行。

## 输出格式

### 事件（`--output jsonl`）

每行一个事件，带信封字段：`schemaVersion`（当前为 3）、`seq`（运行内从 1 递增）、`atMs`、`sessionId`、`runId` 与 `type`。一次运行以 `run_start` 开始，以 `run_end`（`report`）结束：

```json
{"schemaVersion":3,"seq":12,"atMs":1761700000000,"sessionId":"…","runId":"…","type":"run_end","report":{"outcome":"completed","finalText":"## Summary ...","modelRequests":3,"retries":0,"usage":{"input":1200,"output":300,"cacheRead":0,"totalTokens":1500},"sessionFile":"/var/log/zx-agent/run.jsonl"}}
```

其余事件：`agent_start` / `agent_end`（每次尝试）、`turn_start` / `turn_end`、`request_start`、`message_start` / `message_update`（需 `--deltas`）/ `message_end`、`tool_execution_start` / `tool_execution_update` / `tool_execution_end`、`tool_decision`、`approval_requested` / `approval_resolved`、`retry_scheduled`、`context_edited`、`context_overflow`、`compaction_start` / `compaction_end` / `compaction_failed`、`custom_entry`、`hook_failed`、`tool_call_recovered`。手动 `/compact` 发 `compaction_report`。`agent_start` 与 `request_start` 的 `attempt` 是第几次尝试（首次为 1，重试与上下文溢出恢复都会再开一次尝试）；`retry_scheduled` 的 `retry` 是第几次重试（首次为 1，至多 `maxRetries`）。事件包含用户输入、工具参数、脱敏后的工具输出与模型输出。

`report.modelRequests` 含摘要请求；`report.usage` 只含模型的用量，失败的尝试也计入，被取消的请求记为 0；配置了单价时有 `totalCost`。工具报告的花费在 `report.toolUsage`，被拒的调用数在 `report.deniedCalls`。

### OpenTelemetry 追踪

run、chat 与 im 在设置了 `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`（完整地址）或 `OTEL_EXPORTER_OTLP_ENDPOINT`（后接 `/v1/traces`）时，用 OTLP/HTTP 导出追踪；都没设时不建立任何连接。其余标准变量由 OpenTelemetry SDK 读取，服务名默认 `zx-agent`。只导出 span，不导出指标，需要时由收集端从 span 推导。

一次运行是一条 trace：`run` 之下有 `attempt`、`turn`、`request`、`tool <工具名>` 与 `compaction`，属性带运行 ID（`zx.run.id`）、模型、结果、用量与成本。只含元数据，不含输入、输出、参数与错误文本。`run` 在 `TRACEPARENT`（及 `TRACESTATE`）为合法值时接续调用方的 trace。导出经有界队列，满时丢弃，退出时最多等待 5 秒。命令看不到可能带认证头的 `OTEL_EXPORTER_OTLP_HEADERS`、`OTEL_EXPORTER_OTLP_TRACES_HEADERS` 与 `OTEL_EXPORTER_OTLP_METRICS_HEADERS`。

### 会话文件（`--session`）

第一行是头部，之后每行一个条目，条目之间通过 `parentId` 连成一棵树：

```json
{"type":"session","version":8,"id":"…","timestamp":1761700000000,"cwd":"/srv","model":"…"}
{"type":"message","id":"a1b2c3d4","parentId":"f0e1d2c3","timestamp":…,"message":{"role":"user","content":"…","timestamp":…}}
{"type":"context_edit","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":…,"targetId":"b2c3d4e5","replacement":null}
```

- 只追加：修正上下文靠追加 `context_edit`，原始记录保留；`context_edit` 不能指向 system 或摘要消息。
- 同一文件同一时间只有一个写入者（`flock`）；被占用时 `run` 与 `chat` 以退出码 2 结束。Unix 以外的平台不加锁。
- 崩溃留下的未写完的尾记录在下次打开时截掉，并在 stderr 报告；其他损坏报错，文件保持原样。写入失败后不再写入。
- 只读写版本 8，其他版本的文件拒绝打开，不做升级。
- 每次模型请求前记录 `request_started`（配置变化时先记 `request_config`），可还原当时的请求；不记录 API key、URL 与 HTTP 头。
- 有副作用的工具调用执行前记录 `tool_call_started`。崩溃或取消后，下一次运行为缺少结果的调用补写错误结果，不重放，并发出 `tool_call_recovered`：`started`（结果未知，先核对再重试）、`not_started`（未执行）、`read_only`（只读工具，可重新调用）、`start_unknown`（当前未配置该工具，按可能已改变状态处理）。
- 摘要记录为 `compaction_started`、`compaction` 或 `compaction_failed`。摘要的事实必须引用来源条目；引用只证明出处可定位，不证明陈述正确或没有信息损失。
- 扩展的记录：`custom`（状态，不进入模型上下文）与 `custom_message`（发给模型的消息，投影为一条用户消息）。
- 会话名称：`session_info`（`name`），以文件中最后一条为准，与分支无关，不进入模型上下文；`name` 只有一行、没有首尾空白、不为空，否则文件打开时报错。复制分支时随分支原样复制；起名也会更新列表的“最近活动”。

## 作为库使用

`zx-agent` 库再导出各 crate（`zx_agent::ai`、`zx_agent::agent`、`zx_agent::im` 等）。只需要内核时可以只依赖 `zx-core` 与 `zx-ai`，按需加上 `zx-ai-openai`。各 crate 都不发布到 crates.io。

```rust
let mut config = LoopConfig { system_prompt, tools, cwd, ..LoopConfig::new(provider, model) };
config.hooks.push(PolicyHook::new(policy));
let agent = AgentSession::new(config, RetryPolicy::default(), Limits::default(), session, sink);
let report = agent.prompt("任务描述", &CancellationToken::new()).await?;
```

库默认不限轮次、时间与 token，没有钩子，不自动续写、摘要或落盘；说明、上限与策略都由嵌入方给出。完整的组装见 `examples/library.rs`。

- 入口：`AgentSession`（共享会话：`prompt`、`continue_run`、`compact`、`abort`、`subscribe`、`set_model`、`set_name`、`branch`、`queues`、`active_tools`、`run_command`）、`runner::Runner`（`AgentSession` 独占的一半，只有一个所有者时直接用）、`session::SessionStore`。
- 扩展点都是 trait：`ai::Provider`、`agent::Tool`、`agent::ToolPolicy`、`agent::Hooks`、`agent::Interaction`、`agent::EventSink`、`executor::Executor`、`session::SessionLog`、`im::ImChannel`、`im::StatusStore`、`extensions::Extension`。
- 附带的实现：`ai::openai::OpenAiCompatible`、`mcp::Server`（一个 `Extension`）、`executor::ContainerExecutor`、`redact::RedactSecrets`、`otel::Telemetry`。

接口与契约见 rustdoc：`cargo doc --open`。

## 开发

```bash
cargo test                                   # 不需要网络和 API key；付费的真实 API 测试默认跳过
cargo clippy --all-targets -- -D warnings
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

在仓库根目录运行时覆盖全部 8 个 crate；加 `-p <crate>` 只处理一个。集成测试在根目录的 `tests/`，每个文件覆盖什么见 [docs/TESTS.md](docs/TESTS.md)。

`tests/real_api.rs` 会产生费用：每个测试都标为 `#[ignore]`，没有设置 `ZX_E2E_AUTHORIZED=1` 时在发出请求前失败，运行方式见该文件开头的注释。

## CI 与发布

`Rust` 工作流在 `main` 的 push、PR 和手动运行时检查格式、clippy、Rustdoc、actionlint 与 shellcheck，并在 Linux 与 macOS 上构建和测试，另用 Rust 1.88 编译和测试。

`Release` 工作流在推送 `v*` tag 时构建 release 草稿；tag 必须与 `Cargo.toml` 中 `[workspace.package]` 的版本一致。发布包含二进制、README 和 MIT 许可证，支持 `x86_64-unknown-linux-gnu` 与 `aarch64-apple-darwin`，附 `SHA256SUMS` 与 [GitHub 来源证明](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)。验证：

```bash
ZX_ARCHIVE=zx-agent-v0.1.0-aarch64-apple-darwin.tar.gz
gh attestation verify "$ZX_ARCHIVE" --repo xiaoshicae/zx-agent --bundle "$ZX_ARCHIVE.sigstore.json"
```

## 路线图

设计目标、已定决策与分阶段计划见 [docs/ROADMAP.md](docs/ROADMAP.md)。

## 许可证

MIT，见 [LICENSE](LICENSE)。设计参考的 [pi](https://github.com/earendil-works/pi) 同样采用 MIT 许可证（Copyright (c) 2025 Mario Zechner）。

---

README.md · v0.1.0 · 2026年11月
