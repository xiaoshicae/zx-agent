# zx-agent

一个用 Rust 编写的通用 agent 内核，目标是让同一个内核支撑人机对话、无人值守运行和被其他程序嵌入。场景之间的差异将通过 profile（提示词、工具、规则、上限）和插件表达，不改内核。整体架构参考 [pi](https://github.com/earendil-works/pi)（commit `a92283056`）：分层方式、循环语义和会话文件格式都沿用 pi 的设计；与 pi 不同的地方列在[与 pi 的差异](#与-pi-的差异)一节。

当前版本为 v0.1.0，已实现无人值守的 `run`、终端对话 `chat`、TOML profile、项目上下文、按需 skills 与上下文管理。嵌入协议 `acp` 和插件按[路线图](docs/ROADMAP.md)分阶段推进。

- **已具备**：
  - OpenAI 兼容的流式 provider，带事件队列、SSE 事件与响应大小上限
  - agent 回合循环：工具调用先检查、再并发执行，结果保持调用顺序
  - 长期 `AgentSession`：共享提问、单次运行互斥、取消后复用、多订阅者和模型切换
  - 终端 `chat`：流式回答、审批、运行中插话、Ctrl+C 后继续对话
  - 文件化的 `troubleshoot` 与 `assistant` profile；显式 CLI 参数覆盖、deny 累加
  - 项目 `AGENTS.md` 和显式上下文文件；`read_skill` 按需读取登记的 skill 正文
  - `bash` 与 `read_file` 工具
  - 可选的离线容器执行（`--execution container`）：bash 与 read_file 在无网络、工作目录只读的 Docker 容器中逐个执行，见[容器执行](#容器执行)
  - 可选的自动 / 手动压缩：摘要引用源条目，原始历史保留
  - 长工具结果脱敏后落盘，`read_output` 按 UUID 分页读取
  - 声明式命令策略
  - 结构化错误分类与自动重试
  - 可选的轮次上限与时间上限；无人值守默认值集中在 `troubleshoot` profile
  - 只追加的会话记录：默认存为 JSONL 文件（可续跑，同一时间只有一个写入者），也可经 `SessionLog` 存到数据库等其他位置
  - JSONL 事件流输出，流式工具参数携带调用 ID
  - 可配置的截断续写（`run` 默认续写 1 次）
  - 启动时把环境信息与命令策略写入系统提示词
  - 对 bash 命令隐藏 API key 所在的环境变量
  - 系统提示词记录在对话中，恢复会话时只追加有变化的段落
  - 运行 future 被丢弃时取消后台请求、定时任务和工具子进程组
- **尚未实现**：插件与 MCP、其他 provider。详见[路线图](docs/ROADMAP.md)。

## 快速开始

需要 Rust 1.88 及以上版本，以及 C 编译器（TLS 使用的 aws-lc 在构建时编译）。HTTPS 使用系统的根证书：Linux 上没有安装 CA 证书（如 Debian 的 `ca-certificates`）时，程序启动即报错 `No CA certificates were loaded from the system`，只连 `http://` 的本地模型也一样。

```bash
cargo build --release
export OPENAI_API_KEY=...            # 变量名可通过 --api-key-env 修改
./target/release/zx-agent run --model <模型 ID> "检查本机磁盘与内存使用情况，找出资源占用最高的进程"
```

本地或私有部署的模型（vLLM、Ollama、llama.cpp server 等 OpenAI 兼容服务）：

```bash
./target/release/zx-agent run \
  --base-url http://127.0.0.1:11434/v1 --model <模型名> \
  "排查 nginx 返回 502 的原因"
```

如果找不到 API key，请求中就不带认证头。本地服务通常不需要认证，所以缺少 key 不会报错。

### 终端对话

```bash
zx-agent chat --model "$ZX_MODEL" --session ./chat.jsonl
```

直接输入问题即可开始下一轮。TTY 中运行时输入普通文本会作为 steering，在下一次模型请求前进入上下文；任务结束时还没被读到的 steering 作为下一轮提问发送，按 Ctrl+C 取消任务时则丢弃并提示。通过管道输入脚本时逐轮消费，避免后面的命令提前打断任务。单行输入最多 1 MiB（含行尾）；超限或无效 UTF-8 的行会被整行丢弃并提示，对话和当前任务继续，不把截断片段发送给模型。

上限取自所选 profile，内置的 assistant 每轮不限轮次、不限总时间、不自动续写；可用 `--max-turns` 和 `--timeout-secs` 设限。连接、工具、会话和输出选项与 `run` 共用；`chat --help` 列出实际支持项。`--output jsonl` 为每轮发送一个 `run_end`，`--deltas` 可包含增量。

| 输入 | 行为 |
|---|---|
| `/help` | 查看命令 |
| `/model`、`/model <id>` | 查看模型，或空闲时切换同一 provider 的模型，保留历史 |
| `/new` | 创建新会话，清空历史和授权；运行中先取消任务。启用文件会话时在同目录创建唯一新文件，保留旧日志。重新加载 profile 与项目说明，`--system-prompt-file` 沿用启动时读取的内容 |
| `/compact` | 请求模型压缩旧上下文，保留原始日志；没有可压缩范围时不发请求。运行中拒绝重叠调用 |
| `/quit` | 取消当前任务并退出 |
| Ctrl+C | 取消当前任务或审批，继续保留会话；空闲时保持会话 |
| EOF、SIGTERM | EOF 结束会话，管道中的正常任务先完成；待审批时 EOF 拒绝并取消。SIGTERM 取消并退出 |

chat 中的 bash 调用先经过原命令策略，再显示完整工具名、参数、工作目录、理由及权限范围，等待审批；read_file 仍按原读取权限执行。内置 assistant profile 列出的只读命令（如 `kubectl get`、`grep`）不经审批直接执行；带 shell 语法、`secret`、URL 或连接参数的命令仍要审批，见[安全模型](#安全模型)。菜单中的编号绑定当前请求，例如：

```text
/approve 7 once
/approve 7 session
/deny 7 需要换一种做法
```

`once` 只允许当前调用，`session` 只复用完全相同的工具名与参数；命令策略的 Deny 始终优先。取消、超时后的旧编号不能批准新请求。审批默认超时 120 秒；未收到答案不会执行工具。模型文本和诊断中的 ESC、回车及双向控制字符按字面显示，保留正常换行和制表符。

`assistant` 默认在上下文超过 256 KiB 时自动压缩，`troubleshoot` 默认关闭。自动摘要消耗一次请求预算并统计用量；`/compact` 使用独立的有界请求并报告结果。可用 `--no-auto-compact` 禁用自动摘要，或 `--compact-threshold-bytes` 修改字节阈值。该阈值不是模型 token 窗口的保证；摘要有信息损失风险，引用校验也不证明事实正确。配置、审计和边界见[上下文管理](docs/PROFILES.md#上下文管理)。

TTY 中手动压缩时可暂存一个普通输入，完成后自动执行；Ctrl+C 取消压缩并明确清除暂存输入。`/model` 等待空闲，`/new` 与 `/quit` 和正常 EOF 退出码为 0；参数或配置错误、读取终端输入失败为 2，超限或无效 UTF-8 的输入行只会被丢弃，不会退出；SIGTERM 为 5。单轮模型或预算错误会报告后继续对话，JSONL 消费者可读取每轮报告的 `outcome`。这是逐行 REPL，尚未提供历史行编辑或完整 TUI。

### 场景配置

`run` 默认使用 `profiles/troubleshoot.toml`，`chat` 默认使用 `profiles/assistant.toml`，均嵌入发布二进制。可用 `--profile` 选择内置名称或自定义 TOML：

```bash
zx-agent run --model "$ZX_MODEL" --profile examples/profiles/review.toml "评审 src/profile/ 的实现"
zx-agent chat --model "$ZX_MODEL" --profile assistant
```

配置按内置基线、文件、显式 CLI 参数叠加；deny 规则累加。`chat` 默认加载 Git 根目录到工具 cwd 的 `AGENTS.md`，`run` 默认不加载；自动发现的文件解析后必须位于项目内。可用 `--context-file` 追加文件，或 `--no-context` 禁用上下文。skills 通过 profile 登记目录，模型调用 `read_skill` 时才读取正文。配置格式、路径规则、读取预算和完整示例见[场景配置说明](docs/PROFILES.md)。

### 无人值守示例

可用于 cron、systemd timer、Kubernetes Job 等定时或批处理任务：

```bash
zx-agent run \
  --model "$ZX_MODEL" --base-url "$ZX_BASE_URL" \
  --tools bash \
  --allow-cmd "kubectl get" --allow-cmd "kubectl describe" --allow-cmd "kubectl logs" \
  --allow-cmd grep --allow-cmd tail --allow-cmd head \
  --max-turns 20 --timeout-secs 600 \
  --session "/var/log/zx-agent/$(date +%Y%m%d-%H%M%S).jsonl" \
  --output jsonl \
  "命名空间 shop 中的 Pod 频繁重启，排查原因" > events.jsonl
echo "exit code: $?"
```

在 Kubernetes 中常驻部署（`kubectl exec` 进 Pod 对话、CronJob 定时巡检、可选的写权限）：见 [deploy/kubernetes/README.md](deploy/kubernetes/README.md)。

### IM 服务

`zx-agent serve` 作为常驻进程接入 IM：在频道里 @ 机器人或私聊它，每个线程一个会话，写操作在 IM 里审批。目前支持 Slack（Socket Mode）与 ⚑ D:5/CHINA FLAG: 飞书（长连接），都只需出网，不需要公网入口。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：对话内容、命令输出与用户 ID 发往该平台] 该平台的使用 Subject to Legal/Export Compliance approval。设计见 `docs/serve-im-review.md`；部署到 Kubernetes 见 `deploy/kubernetes/README.md` 的“IM 服务”。

```bash
export SLACK_APP_TOKEN=xapp-...   # app-level token，用于 Socket Mode
export SLACK_BOT_TOKEN=xoxb-...   # bot token，用于发消息
# 使用 ⚑ D:5/CHINA FLAG: 飞书时改为 FEISHU_APP_ID 与 FEISHU_APP_SECRET（企业自建应用的凭据）
zx-agent serve --config serve.toml --model <model>
```

配置文件（TOML，未知的键视为错误）：

```toml
[slack]
# appTokenEnv = "SLACK_APP_TOKEN"   # 存放 app-level token 的环境变量名，此为默认值
# botTokenEnv = "SLACK_BOT_TOKEN"   # 存放 bot token 的环境变量名，此为默认值

# ⚑ D:5/CHINA FLAG: 飞书；两个平台可以同时配置。名单中写作 feishu:oc_…（群或私聊）与 feishu:ou_…（用户）
# [feishu]
# appIdEnv = "FEISHU_APP_ID"           # 存放 App ID 的环境变量名，此为默认值
# appSecretEnv = "FEISHU_APP_SECRET"   # 存放 App Secret 的环境变量名，此为默认值

[access]
conversations = ["slack:C0123456789"]   # 只在这些频道或私聊中回答，其他地方只回一句“未授权”
approvers = ["slack:U0123456789"]       # 只有这些用户的点击能批准写操作

[limits]
maxRunning = 4              # 同时运行的会话数，默认 4，其余排队
approvalTimeoutSecs = 600   # 审批卡片的等待时长，默认 600，超时按拒绝处理
idleSecs = 1800             # 会话空闲多久后释放其文件锁，默认 1800

[sessions]
dir = "/data/sessions"      # 会话文件位于 <dir>/slack/<频道>/<线程>.jsonl
```

| 项 | 说明 |
|---|---|
| Slack 应用 | 打开 Socket Mode 与 Interactivity；订阅 bot 事件 `app_mention` 与 `message.im`。bot token 需要读取 @ 消息的权限（Slack 文档两处分别写作 `app_mentions:read` 与 `app_mentions.read`，以应用设置页显示的为准）、`im:history` 与 `chat:write`；app-level token 带 `connections:write`（Slack 的权限页把 `apps.connections.open` 列为它的适用方法） |
| ⚑ D:5/CHINA FLAG: 飞书应用 | 企业自建应用；事件与回调的订阅方式都选长连接；订阅事件 `im.message.receive_v1` 与回调 `card.action.trigger`；开通发送消息的权限，群聊只开通读取 @ 机器人消息的权限（`im:message.group_at_msg:readonly`）。连接方式没有公开文档，按官方 Go SDK 实现，见评审稿 3.5 节 |
| 使用 | Slack：频道中 @ 机器人，回答在这条消息的线程里；线程中追问时同样要 @ 机器人。⚑ D:5/CHINA FLAG: 飞书：群里 @ 机器人，回答以话题回复；在话题中追问时同样要 @。两个平台的私聊都直接发送。单独发送 `/cancel` 取消本线程的任务 |
| 审批 | 与 chat 相同，使用 assistant profile：自动放行清单中的只读命令直接执行，其余 bash 命令发审批卡片，只有 `approvers` 中的用户点击才生效 |
| 凭据 | Slack 的两个 token 与 ⚑ D:5/CHINA FLAG: 飞书的 App Secret 只从环境变量读取。命令的环境中删除这些变量，命令输出中出现它们的值时脱敏 |
| 日志 | stdout 每行一个 JSON：`approval`（审批记录：线程、工具调用、审批人、结果、时间）、`released`（释放空闲会话）、`failed`（未能发出的消息、连不上平台等）。对话内容在会话文件中 |
| 退出 | 收到 SIGINT 或 SIGTERM 时取消进行中的任务，各线程发出结果后以 0 退出；配置错误与被拒的凭据为 2；平台连接意外结束为 1 |

其余参数与 `chat` 相同（`--model`、`--base-url`、`--api-key-env`、`--profile`、`--max-turns`、`--timeout-secs` 等）；`--max-turns` 与 `--timeout-secs` 作用于每一次运行。不接受 `--session`。

### MCP 工具

在 profile 中用 `[[mcp]]` 列出外部 MCP 服务器及其工具，run、chat、serve 启动时各连接一次，模型以 `<服务器名>__<工具名>` 调用这些工具。只支持 Streamable HTTP；MCP 服务器由外部服务提供。配置项见 [场景配置说明](docs/PROFILES.md#mcp-服务器)。

```bash
export PROM_MCP_TOKEN=...   # 与 profile 中的 tokenEnv 一致
./target/release/zx-agent chat --model <模型 ID> --profile prom.toml
```

- 只有 `tools` 列出的工具交给模型；`allowTools` 中的直接执行，其余每次都要审批（run 中没有人审批，因此拒绝）。`rules.denyTools` 可以拒绝其中任何一个。
- token 只以 `Authorization: Bearer` 发给该服务器，命令看不到存放它的变量，工具输出中出现它的值时脱敏。
- 连不上、token 变量没有值、服务器缺少列出的工具时，以退出码 2 报错；成功时 stderr 每个服务器一行 `[mcp] …`。

### run 退出码

| 退出码 | 含义 |
|---|---|
| 0 | 完成：模型给出了最终回答 |
| 1 | 失败：模型请求出错且重试无效，或会话文件写入失败 |
| 2 | 参数或配置错误 |
| 3 | 达到 `--max-turns` 上限 |
| 4 | 达到 `--timeout-secs` 时间上限 |
| 5 | 被 SIGINT 或 SIGTERM 中止 |
| 6 | 截断：最终回答达到输出 token 上限，自动续写（`--continue-on-length`）后仍未完成；已生成的部分照常输出 |

### 主要参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--model` | 环境变量 `ZX_MODEL` | 模型 ID |
| `--base-url` | `https://api.openai.com/v1`，或环境变量 `ZX_BASE_URL` | OpenAI 兼容地址 |
| `--api-key-env` | `OPENAI_API_KEY` | 存放 API key 的**环境变量名**。key 不接受命令行传入，避免在进程列表中暴露 |
| `--system-updates` | `collapse` | 恢复会话时，提示词的变化怎样发给模型。`collapse`：合并进开头的 system 消息，兼容不接受对话中途 system 消息的服务端；`inline`：在变化发生的位置发送，保持此前的请求前缀，便于服务端复用提示词缓存，模型也能看出提示词从哪里开始变化，但要求服务端接受对话中途的 system 消息，使用前需确认 |
| `--tools` | profile：`run` 为 `bash,read_file`，`chat` 为 `bash,read_file,read_output`；有 skill 目录时加入 `read_skill` | 显式传入时替换工具集合；仅启用 `read_output` 时安装长结果落盘钩子 |
| `--cwd` | 当前目录 | 工具的工作目录，同时写入 `<environment>` |
| `--allow-cmd` / `--deny-cmd` | 无 | bash 命令白名单前缀 / 额外的拒绝子串，均可重复 |
| `--auto-approve-cmd` | profile：`chat`（assistant）为一组只读命令，`run`（troubleshoot）为空 | 不经审批直接执行的 bash 命令前缀，可重复，替换 profile 的清单；只在 bash 需要审批时起作用，见[安全模型](#安全模型) |
| `--secret-file` | 无；另读取环境变量 `ZX_SECRET_FILES`（逗号分隔） | 内容需要从工具输出中过滤的凭据文件，或存放这类文件的目录，可重复；与环境变量中的文件合并登记。登记的路径在启动时必须存在，其中每个文件须为不超过 64 KiB 的普通文件，否则启动时报配置错误，见[安全模型](#安全模型) |
| `--max-turns` | profile：`run`（troubleshoot）为 30，`chat`（assistant）不限 | 本次运行的模型请求次数上限，失败的尝试也计入；`chat` 中为每轮上限 |
| `--timeout-secs` | profile：`run`（troubleshoot）为 900，`chat`（assistant）为 0 | 整次运行的时间上限，0 表示不限制；`chat` 中为每轮上限 |
| `--approval-timeout-secs` | 120 | 仅 `chat`：审批请求等待 `/approve` 或 `/deny` 的秒数，至少为 1。超时按拒绝处理，任务继续，REPL 立即提示该请求已过期 |
| `--tool-timeout-secs` | 120 | bash 命令的默认超时，至少为 1。模型可以通过工具参数 `timeout_secs` 为单条命令申请更长时间，上限为本参数与 600 秒中较大的一个；整次运行仍受 `--timeout-secs` 限制 |
| `--idle-timeout-secs` | 120 | 等待响应头，或两段流数据之间的最长等待时间，至少为 1 |
| `--max-retries` | 3 | 遇到临时性错误时的重试次数 |
| `--continue-on-length` | profile：`run`（troubleshoot）为 1 | 仅 `run`。回答因输出 token 上限被截断时，自动请模型续写的次数。每次续写都计入 `--max-turns`；设为 0 时直接以退出码 6 结束 |
| `--max-tokens` | 无（由服务端决定） | 每次回复的输出 token 上限。推理模型的推理部分也计入这个上限，设得太小时回答可能为空 |
| `--max-response-bytes` | 16777216（16 MiB） | 每次模型请求的流式响应字节上限，包含 SSE 封装与推理内容 |
| `--max-sse-event-bytes` | 1048576（1 MiB） | 单个 SSE 事件的字节上限，包含封装；累计多行和未换行的数据也受限制 |
| `--session` | 无（只保存在内存） | 会话文件路径。文件不存在则创建；已存在则在原有对话上继续 |
| `--output` | `text` | `text`：进度与诊断（工具、重试、摘要、恢复、钩子失败）输出到 stderr，最终回答输出到 stdout；stdout 是终端时转义回答中的控制字符，输出到管道时保持原文。`jsonl`：每个事件一行 JSON，输出到 stdout。`run` 在两种模式下遇到已关闭的 stdout（如读取方已退出）都不再写出，退出码仍由运行结果决定 |
| `--deltas` | 关闭 | 仅用于 `jsonl`：同时输出流式的 `message_update` 事件 |
| `--system-prompt-file` | 所选 profile 的说明 | 替换所选 profile 的说明。只在启动时读取一次，可以是管道；`chat` 的 `/new` 沿用同一内容。下文生成的段落仍会追加在末尾 |
| `--compact-threshold-bytes` | profile：run 关闭，chat 262144 | 自动摘要的序列化上下文字节阈值；0 关闭，非零值须大于 profile 的保留预算 |
| `--no-auto-compact` | 关闭 | 禁用自动摘要，与上项互斥；保留手动 `/compact` |
| `--execution` | profile 决定，内置 profile 为 `local` | `local`：在本机执行；`container`：bash 与 read_file 在无网络、工作目录只读的 Docker 容器中执行，见[容器执行](#容器执行) |
| `--container-image` | 无 | `--execution container` 使用的镜像，须已在本地（`docker pull`）；本机执行时给出该参数视为配置错误 |

表中标注 profile 的默认值取自所选 profile，命令行显式传入时覆盖；`run --help` 与 `chat --help` 不再显示固定默认值。

重试优先依据结构化的 HTTP 状态和错误码。服务端数值秒形式的 `Retry-After` 会参与退避计算，仍受 `RetryPolicy::max_delay` 约束（默认 60 秒）；HTTP 日期形式和无效提示使用本地退避。未提供结构化元数据的 provider 保留错误文本判断。服务端以 `stop`、`end`、`length`、`tool_calls`、`function_call` 以外的 `finish_reason` 结束回答时（如 `content_filter`），按 provider 错误处理，错误码为 `finish_reason:<取值>`（如 `finish_reason:content_filter`），不与可重试的错误码同名，因此不重试；这组正常取值与 pi 的 `openai-completions.ts` 相同。以 `stop` 或 `end` 结束但带工具调用时按工具调用处理，以兼容这样上报的服务端。成功状态的响应若 `Content-Type` 为 `application/json`，不按事件流解析：正文含 `error` 对象时与流内的错误对象一样报告其信息与错误码，否则按协议错误报告（服务端可能不支持流式）并附正文摘录，不重试。

事件队列采用有界缓冲与背压，慢消费者会使生产者等待。达到响应或 SSE 事件上限时保留已接受的部分内容，以退出码 1 结束且不重试；确需更大响应时可显式提高对应上限。两个字节上限都必须大于零。

## 架构

```
CLI (main.rs + cli.rs + chat/)
  ├─ profile/ + profiles/  文件化场景、上下文与 skill 工具
  ├─ presets/             保留已有 SDK 场景常量与辅助方法
  └─ agent_session  共享会话、取消、订阅与模型切换           ← pi: coding-agent AgentSession
       └─ runner    独占运行、重试、轮次与时间上限、持久化
            ├─ session  只追加的条目树 + 投影；SessionLog 存储 ← pi: SessionManager
            └─ agent    回合循环、工具执行、钩子与事件       ← pi: packages/agent/src/agent-loop.ts
                 ├─ ai    消息类型、流式事件协议、provider  ← pi: packages/ai
                 └─ tools bash、read_file、输出截断         ← pi: coding-agent/src/core/tools
```

| 模块 | 文件 | 对应的 pi 实现 |
|---|---|---|
| 消息与事件协议 | `src/ai/types.rs` | `ai/src/types.ts`（`AssistantMessageEvent`） |
| provider 约定 | `src/ai/provider.rs` | `agent/src/types.ts`（`StreamFn` 约定） |
| OpenAI 兼容 provider | `src/ai/openai.rs` | `ai/src/api/openai-completions.ts`、`transform-messages.ts` |
| 重试判断 | `src/ai/retry.rs` | `ai/src/utils/retry.ts` |
| 测试用 provider | `src/ai/faux.rs` | `ai/src/providers/faux.ts` |
| agent 循环 | `src/agent/agent_loop.rs` | `agent/src/agent-loop.ts`（`runLoop`、`executeToolCalls`） |
| 钩子宿主 | `src/agent/hooks.rs` | 输入、上下文、工具、回合与事件扩展点 |
| 工具调用策略 | `src/policy.rs` | `PolicyHook` 适配 `ToolPolicy`；命令策略与 deny → ask → allow 规则 |
| 人机交互 | `src/interaction.rs` | 宿主审批与 `NoHuman` |
| 内置扩展 | `src/extensions.rs` | 截断续写与已知密钥脱敏 |
| 会话 | `src/session.rs` | `coding-agent/src/core/session-manager.ts` |
| 运行控制 | `src/runner.rs` | `coding-agent/src/core/agent-session.ts`（`_startRun`、`_prepareRetry`） |
| 共享会话 | `src/agent_session.rs` | `AgentSession` 的长期生命周期与多订阅者接口 |
| 系统提示词 | `src/prompt.rs` | `coding-agent/src/core/system-prompt.ts` |
| 场景预设 | `src/presets/troubleshoot.rs`、`assistant.rs` | 内核之外的场景策略 |
| 文件化配置 | `src/profile/`、`profiles/` | 加载与叠加 TOML、项目上下文、显式 skill 目录及按需读取 |
| 终端前端 | `src/chat/`、`src/cli.rs` | 单输入所有者、审批、显示与共享 CLI 选项 |
| IM 接入 | `src/im/` | 无对应实现：`ImChannel` 平台接口；`Server` 按线程分派，每个线程一个任务独占会话，审批卡片与运行上限 |

### 与 pi 一致的关键语义

1. **provider 不抛错。** 失败统一编码为 `stopReason` 为 `error` 或 `aborted` 的消息，循环里不需要 try/catch。
2. **由模型决定何时停止。** 模型的回复里没有工具调用时，内层循环结束。
3. **并行执行工具分三个阶段**：先按顺序逐个检查（包括策略判断），再并发执行，最后按模型发出调用的顺序写入结果；`tool_execution_end` 事件则按完成先后发出。只要有一个工具声明为 `Sequential`，整批调用改为串行。
4. **所有工具失败都变成工具结果，循环继续**，模型可以自行修正。覆盖的失败包括：找不到工具、参数无效、被策略拒绝、执行报错、执行 panic。
5. **回复因 `length` 截断时，不执行其中的工具调用**，因为参数可能不完整。
6. **两个消息队列**：引导消息在每个轮次结束后插入；后续消息在 agent 本来要停下时才插入。
7. **发给模型的上下文以会话文件的投影为准。** 重试时不重复用户消息：先追加一条 `context_edit` 把失败的回答从投影中省略，再从上一条消息续跑。失败记录本身仍保留在文件中，供审计。
8. **系统提示词是对话的一部分。** 会话中第一条 system 消息记录完整的提示词，之后提示词有变化时，只追加一条记录变化段落的 system 消息，不改写之前的版本。因此会话文件能还原每次请求实际使用的提示词。发给模型时，默认把所有版本合并成开头的一条，与 pi 对 OpenAI 兼容服务的默认做法一致；服务端支持时可以按位置发送（`--system-updates inline`）。

### 与 pi 的差异

| 项目 | zx-agent | 原因 |
|---|---|---|
| 轮次与时间上限 | 有（`--max-turns`、`--timeout-secs`） | 无人值守时没有人能手动中止 |
| 工具调用确认 | SDK 支持宿主审批；`run` 使用 `NoHuman` 拒绝 Ask | 无人值守时没有人可以回答确认请求 |
| 输出被截断 | 自动请模型续写（`--continue-on-length`），仍未完成则以退出码 6 结束 | pi 不自动续写，交给用户决定；无人值守时没有人可以决定 |
| 会话文件写入失败 | 立即中止运行，之后不再写入该文件 | 不允许在缺少审计记录的情况下继续执行操作；未写完的记录只会留在文件末尾，可按尾记录修复 |
| 会话文件创建时机 | 启动时立即创建并写入头部 | 进程中途崩溃也能留下记录 |
| 环境信息 | 启动时由程序写入系统提示词，见下文 | pi 只写入工作目录；无人值守时没有人提供这些信息，白名单也可能禁止 `uname`、`date` 等命令 |
| 参数校验 | 由各工具反序列化成自己的参数类型来完成 | 避免引入 JSON Schema 校验器 |
| 缺少结果的工具调用 | 有副作用的调用执行前先记录开始；下一次运行开始时，为缺少结果的调用写入"结果未知""未执行"等错误结果，不重放。运行被取消时，执行中的有副作用调用的结果写明"可能已改变状态，先检查再重试"，尚未开始的调用写明"未执行" | 发送时补占位（沿用 pi 的 transform-messages.ts）区分不了调用是否执行过；崩溃后需要依据记录判断 |
| 上下文摘要 | 重新摘要时读取应用过 `context_edit` 的原始记录，不输入上一份摘要；摘要中的每条事实必须引用来源条目，见 [PROFILES.md](docs/PROFILES.md) | pi 把上一份摘要交给模型更新，允许删去旧内容；这里优先保证每条事实可追溯、不经多次转述。代价是摘要请求随历史增长，累计约 2 MiB 后无法再摘要；增量摘要的设计稿见[路线图](docs/ROADMAP.md)，待评审 |
| 完整 TUI | 未实现，`chat` 是逐行 REPL | 路线图将其列为本期不做 |

### 系统提示词的组成

系统提示词由说明和其后的生成段落依次拼接。生成段落由程序在启动时生成，各自用同名标签包裹（参照 pi 的 `system-prompt.ts`），内容为空的段落不出现：

| 部分 | 内容 | 何时出现 |
|---|---|---|
| 说明 | 所选 profile 的说明（`run` 默认为排障说明，`chat` 默认为通用对话说明），或 `--system-prompt-file` 的内容 | 总是 |
| `<environment>` | 操作系统与版本、CPU 架构、启动时间（UTC）与本地时区、工作目录；容器执行时描述容器、镜像与隔离方式 | 总是 |
| `<command_policy>` | 允许的命令前缀、禁止的语法、引号内 `\|` 的写法提示 | 启用了 bash 且设置了命令白名单（`--allow-cmd` 或 profile 的 `rules.allow`）时 |
| `<project_context>` | 项目说明文件：自动发现的 AGENTS.md，以及 `--context-file` 或 profile 列出的文件 | 有项目说明时 |
| `<available_skills>` | 已注册 skill 的名称与描述；正文由 `read_skill` 按需读取 | 启用 `read_skill` 且注册了 skill 时 |

本机执行时，这些信息都通过读文件获得（如 `/etc/os-release`、`/etc/localtime`），不执行命令；容器执行时，容器的系统信息来自启动检查在容器内运行的 `uname` 与 `/etc/os-release`。主机名和用户名不会写入，因为提示词会发送给第三方模型。

恢复会话时，程序重新生成提示词，只把有变化的段落追加为一条新的 system 消息。启动时间按分钟记录；跨分钟恢复时，`<environment>` 通常会出现在更新中。发给模型的方式由 `--system-updates` 决定：

- `collapse`（默认）：最新版本合并到开头的 system 消息。开头变化会影响服务端对既有请求前缀的缓存复用。
- `inline`：更新出现在对话中变化发生的位置，内容为 `Updated system prompt section "environment": …`。在模型、工具等发送配置不变时，之前的请求前缀保持不变，便于复用缓存；是否实际命中由服务端决定。

## 安全模型

`CommandPolicy` 只检查 `bash` 工具：

- 所有命令都会匹配一组危险模式：`rm -rf /` 等多词模式在转小写、合并空白后按子串匹配；`shutdown`、`reboot`、`poweroff`、`halt`、`mkfs`（含 `mkfs.<类型>`）以及 `init 0`、`init 6` 按程序匹配，即出现在命令的词中（先去掉引号与反斜杠，按路径的文件名比较），但 `grep`、`last`、`journalctl`、`cat`、`echo` 等只读程序的参数不算，因此 `last reboot`、`grep -i shutdown` 可以执行。这组规则同样容易绕过，只起减速作用。
- 设置 `--allow-cmd` 后，命令只能是由白名单程序组成的管道（`a | b`），并且拒绝 `;`、`&`、`` ` ``、`$(`、`>`、`<` 和换行。检查只识别这些符号：变量展开（如 `$VAR`）和通配符不会被拒绝，白名单程序自身的参数也不检查，例如允许 `find` 就等于允许 `find -exec`。

示例（白名单为 `kubectl get`、`grep`）：

| 命令 | 结果 |
|---|---|
| `kubectl get pods -A \| grep -v Running` | 允许 |
| `kubectl delete pod x` | 拒绝：不在白名单 |
| `kubectl get pods && rm x` | 拒绝：包含 `&` |
| `grep x $(cat list)` | 拒绝：包含 `$(` |

自动放行（`--auto-approve-cmd` 或 profile 的 `rules.autoApprove`）只决定是否询问，不决定能运行什么。bash 需要审批时（如 chat），满足以下条件的命令不经审批直接执行：

- 是由清单中的前缀组成的管道；
- 只由字母、数字、空白和 `- _ . / : = , @ + | ^ ~ !` 组成。引号、`\`、`$`、通配符、花括号、括号、`#` 以及白名单拒绝的语法都会改为询问：它们能让 shell 实际执行的内容与字面不同，例如 `kubectl get sec""rets` 去掉引号后就是 `secrets`；
- 不含 `secret`、URL（`://`），也不含把请求发往别处的参数 `--server`、`--kubeconfig`、`--token`、`--insecure-skip-tls-verify`。2026-11-01 实测（kubectl v1.36.1，加 `--insecure-skip-tls-verify`），kubectl 把 kubeconfig 中的 token 发给了 `--server` 指定的 HTTPS 地址；
- kubectl 只能带只读参数：`-n`/`--namespace`、`--context`、`-A`、`-o`、`-l`、`-L`、`--field-selector`、`--sort-by`、`--no-headers`、`--show-labels`，日志的 `-c`、`-p`/`--previous`、`--since`、`--tail`、`--timestamps` 等（完整列表见 `src/policy.rs` 的 `KUBECTL_OPTIONS`）。其他参数一律询问，例如 `--raw`（可读任意 API 路径）、`--user` 与 `--cluster`（换用别的凭据或集群）、`--profile-output` 与 `--cache-dir`（写文件，实测 `--profile-output` 会覆盖目标文件）、`-f`、`-w`、`-v`。

拒绝规则始终优先。内置 assistant 清单只收不能执行其他程序、不能写文件的命令：`kubectl` 的 `get`、`describe`、`logs`、`top`、`events`、`explain`、`api-resources`、`version`、`config get-contexts`、`config current-context`，以及 `grep`、`head`、`tail`、`wc`、`jq`。检查同样只看字面；这些命令能读到什么取决于运行它们的账号与凭据，读到的内容会进入模型上下文。

在 Unix 上，bash 命令正常退出、超时、被取消或工具 future 被丢弃时，程序都会终止其进程组，并清理输出读取任务。因此不能用 bash 工具启动常驻进程。本机执行只能终止进程组：用 `setsid` 或守护化方式脱离进程组的进程不在此列，可能在命令结束后继续运行；需要保证时使用容器执行，见下文。

`read_file` 只接受普通文件，默认单文件上限为 10 MiB，同时检查文件元数据与实际读取量。目录、FIFO 和设备文件会被拒绝，并且不会被打开：先查看路径类型再打开，打开后再检查一次，防止中途被替换。取消会停止等待文件 I/O；底层已经启动的普通文件 I/O 仍可能由 Tokio 的阻塞线程继续完成。每次最多返回 2000 行或 50 KiB（51200 字节），按行翻页；单行超过这个字节上限时只显示前缀，并注明该行被截断及其字节数。

MCP 工具（见上文“MCP 工具”）：服务器由使用者在 profile 中显式列出，工具也要逐个列出，没列出的工具连描述都不会进入上下文。工具描述与结果来自外部服务，可能含注入内容；`allowTools` 以外的工具每次都要审批，服务器给出的 annotations 不影响放行。工具一律按有副作用处理，中断后恢复为“结果未知”。响应体没有大小上限（rmcp 读取 JSON 响应时不限制，SSE 每个事件上限 16 MiB），交给模型的结果最多 2000 行或 50 KiB。

### 容器执行

默认在本机执行。在 profile 中写 `execution = "container"` 并配置镜像，或使用 `--execution container --container-image <镜像>`，bash 与 read_file 改在 Docker 容器中执行；read_skill 与 read_output 仍读取本机文件。

```toml
execution = "container"

[container]
image = "debian:bookworm-slim"   # 必填；须已在本地，不会自动拉取
user = "65534:65534"             # 可选，默认 nobody
memory = "1g"                    # 可选，Docker 的内存上限写法
pidsLimit = 256                  # 可选，容器内进程数上限
```

- 每个进程一个容器，在启动时创建；`chat` 的 `/new` 会换一个新容器。启动时检查命令不以 root 运行、镜像提供 bash 与 `cat`、`head`、`uname`，以及容器用户能读取工作目录。任一项不满足、镜像不在本地或 Docker 不可用时，以配置错误结束（退出码 2），不请求模型。
- 隔离：无网络（`--network none`）、只读根文件系统、工作目录以只读方式挂载在同一路径、只有 `/tmp`（tmpfs）可写、去掉全部 capability、`no-new-privileges`、进程数与内存上限。命令以非 root 用户运行：`container.user` 为 UID 0 或 `root` 时报配置错误。保持容器存活的 PID 1 及其 `cat` 以去掉全部 capability 的 root 运行，命令无法向它们发信号。命令不继承 agent 的环境变量，API key 不进入容器。`<environment>` 描述容器的系统与这些限制，而不是宿主机。
- 工作目录整体对容器只读可见，包括其中的硬链接和 `.zx-agent/outputs` 中已脱敏的工具结果。因此容器执行时，`--session` 指向的会话文件必须在工作目录之外，否则启动时报配置错误；遮蔽单个路径挡不住硬链接，也挡不住同目录中的其他会话文件。
- 同一容器内的命令与文件读取逐条执行：容器执行器声明为串行，bash 与 read_file 随之声明 `Sequential`，同一批调用因此逐个检查、记录并执行，运行在其间被取消时，尚未轮到的调用在恢复时记为未执行。命令总用容器配置中、启动检查过的 shell。每条命令执行前后，以命令用户的身份终止该用户的全部进程（`kill -1`）：超时、取消、运行被丢弃或正常结束后，命令留下的后台进程都会被终止，用 `setsid` 脱离进程组的也不例外；命令写入 `/tmp` 的内容不影响清理。被信号终止的命令报告 `128 + 信号编号` 的退出码（如 137），而不是本机执行时的 `terminated by a signal`。
- 容器随 zx-agent 进程结束而删除，进程被 SIGKILL 时也一样（容器读取的 stdin 随之关闭）。容器带 `zx-agent.session=<会话 ID>` 标签，可用 `docker ps --filter label=zx-agent.session` 查看。
- 原生 Linux 的 Docker 保留宿主机的文件权限：默认用户 65534 只能读取对其他用户可读的文件，工作目录本身也需要可读，否则启动时报错；需要时用 `container.user` 指定其他用户。macOS 的 Docker Desktop 把挂载的文件显示为容器用户所有（2026-11-01 实测），权限位不起限制作用。
- 在线排障所需的网络与凭据访问尚未提供，见路线图 P6。

**局限（必须了解）**：

- 策略只是第一道防线，**不是安全边界**。生产环境请以最小权限账号运行，并放在容器或虚拟机中，必要时限制网络访问。
- `read_file` 不受命令白名单约束，可以读取进程有权限读取的普通文件。需要限制时，使用 `--tools bash` 关闭它。
- 项目上下文（`AGENTS.md`、`--context-file` 与 profile 中的上下文文件）会进入系统提示词并发送给模型服务方，可能引导模型的行为。自动发现的 `AGENTS.md` 不能通过符号链接指向项目外；在不可信仓库中运行时，建议使用 `--no-context`。
- `run` 和 `chat` 在 `after_tool` 中将认证 API key 的完整字面值替换为 `[REDACTED]`，处理后的结果才会写入会话、事件并回传模型。在 Kubernetes Pod 中，ServiceAccount token（`/var/run/secrets/kubernetes.io/serviceaccount/token`）同样被过滤。其他凭据文件可以用 `--secret-file` 或 `ZX_SECRET_FILES` 登记，两者合并，例如其他集群的 token；登记目录时，表示其中名字不以点开头的文件，之后新增的文件也会过滤。这些文件在每次过滤前重新读取，文件中的当前值总是过滤；轮换下来的旧值在过期前仍然有效，因此还会保留最近 32 个继续过滤。过滤是对文件内容（去掉首尾空白）的精确替换；内容不是 UTF-8 时，按 bash 与 read_file 输出所用的有损解码匹配，与文件被完整打印时的样子一致。文件不存在或本进程无权读取时跳过。启动时，登记的路径必须存在，其中每个文件须为不超过 64 KiB 的普通文件：更大的文件不可能整段出现在一次工具输出中（至多 50 KiB），通常是结构化数据，应改为逐个登记其中的值。启动之后才出现超过上限、不是普通文件或其他读取错误时，过滤器失败。SDK 可用 `RedactSecrets` 显式配置其他密钥，或用 `with_secret_files` 登记这类文件，并用 `check_secret_files` 在启动时做同样的检查；过滤器失败时丢弃原始输出，返回错误结果。
- 长工具结果的文件同样在脱敏后写入，Unix 新目录 / 文件权限为 0700 / 0600；结果目录默认 `.zx-agent/outputs`，不会自动删除，目录中写入忽略全部文件的 `.gitignore`。默认目录必须位于工作目录内，仓库中的符号链接不能把写入引到外部；写入失败时结果保持内联。`run` 默认不落盘。保存的是工具已经截断的结果，详见[工具结果落盘](docs/PROFILES.md#工具结果落盘)。
- 该过滤不识别未知密钥、编码或拆分后的密钥，也不处理用户输入、模型输出和工具调用参数。这些内容及工具输出中的其他敏感数据仍可能进入模型请求、会话和事件；请控制会话文件访问权限。
- 认证 key 只从环境变量读取；bash 命令启动前删除 `--api-key-env` 指定的变量。但本机执行时，工具与 agent 以同一系统用户运行，仍可能通过其他途径读取凭证。Linux 上，agent 启动时把自身设为不可 dump（`PR_SET_DUMPABLE`），同一用户的命令因此读不到它的 `/proc/<pid>/environ`。以下情况不在保护范围内：带 `CAP_SYS_PTRACE` 的进程；其他持有 key 的进程，如启动 agent 的 shell。在 Pod 中运行时，容器入口应直接运行 zx-agent，并且不要添加 `SYS_PTRACE` 能力。需要隔离时，请使用容器执行，或以不同账号运行。
- 除 `--api-key-env` 指定的变量外，其他环境变量（如云厂商凭证）bash 命令仍然可以读到。需要时请在启动 zx-agent 前清理环境变量。

## 输出格式

### 事件（`--output jsonl`）

每行一个事件。事件字段与信封字段在同一个 JSON 对象中：`schemaVersion`（当前为 1，不兼容的变化时加一）、`seq`（运行内从 1 开始递增）、`atMs`（毫秒时间戳）、`sessionId`、`runId`（每次运行一个；手动 `/compact` 使用自己的 ID）与 `type`。未加 `--deltas` 时省略的 `message_update` 会在 `seq` 中留下空缺。

一次运行以 `run_start`（`model`、`provider`）开始，以 `run_end`（`report`）结束，中断调用的恢复事件在两者之间。每次尝试由 `agent_start`（`attempt`，重试时加一）与 `agent_end` 包围。回合（`turn_start` / `turn_end`，带 `turn`）只对应任务请求，成对出现；自动摘要的请求计入轮次预算，但不是回合。其余事件：

| `type` | 内容 |
|---|---|
| `request_start` | 调用 provider 之前发出：`kind`（`task` 或 `summary`）、`entryId`（会话中的 `request_started` 条目）、`model`、`attempt`。HTTP 失败的请求也有这条 |
| `message_start`、`message_update`、`message_end` | 消息开始、流式增量（需加 `--deltas`）与完成；`message_end` 包括 system 消息，带会话条目的 `entryId` |
| `tool_execution_start`、`tool_execution_end` | 一个工具调用的处理开始与结束；`durationMs` 只含执行 |
| `tool_decision` | 策略结论：`verdict`（`allow`、`deny`、`ask`）与 `reason`；策略钩子出错按 `deny` 报告 |
| `approval_requested`、`approval_resolved` | 审批的询问与结果：`approval`（`allow_once`、`allow_session`、`remembered`、`denied`、`timed_out`、`cancelled`、`failed`）、`reason`、`waitMs`。会话内已授权过的同一调用不再询问，只有 `approval_resolved`（`remembered`） |
| `retry_scheduled`、`context_edited`、`hook_failed` | 重试、上下文编辑（带编辑条目的 `entryId`）与钩子失败 |
| `compaction_start`、`compaction_end`、`compaction_failed` | 摘要；带 `automatic`，成功事件包含条目 ID、前后字节数与用量 |
| `tool_call_recovered` | `toolCallId`、`toolName` 与 `state`，取值见下文会话文件一节 |

事件内容完整，包括用户输入、工具参数、经 `after_tool` 处理后的工具输出与模型输出。手动 `/compact` 另发 `compaction_report`，不产生任务回答或 `run_end`。`run` 的最后一行、`chat` 每轮的最后一行是 `run_end`：

```json
{"schemaVersion":1,"seq":12,"atMs":1761700000000,"sessionId":"…","runId":"…","type":"run_end","report":{"outcome":"completed","finalText":"## Summary ...","turns":3,"retries":0,"usage":{"input":1200,"output":300,"cacheRead":0,"totalTokens":1500},"sessionFile":"/var/log/zx-agent/run.jsonl"}}
```

`report.turns` 是模型请求数（含摘要请求），与回合编号不同。

`usage` 统计本次运行中每个回复的用量，失败的尝试也计入。被取消的请求记为 0，因为服务端在流的末尾才返回用量；实际计费以服务商账单为准。

`message_update.delta` 的 `tool_call_args` 包含 `id` 和 `text`，`id` 与对应 `tool_call_start` 相同，便于关联交错返回的工具参数。旧事件缺少 `id` 时，反序列化为空字符串。

模型失败消息可附带 `providerError`，区分 HTTP、服务端流内错误、传输、超时、协议与资源上限。HTTP 元数据包含 `status`、可选 `code` 和 `retryAfterMs`；给人看的原因仍在 `errorMessage` 中。

### 会话文件（`--session`）

第一行是头部，之后每行一个条目，条目之间通过 `parentId` 连成一棵树：

```json
{"type":"session","version":4,"id":"…","timestamp":1761700000000,"cwd":"/srv","model":"…"}
{"type":"message","id":"f0e1d2c3","parentId":null,"timestamp":…,"message":{"role":"system","sections":[{"name":"preamble","content":"…"},{"name":"environment","content":"…"}],"timestamp":…}}
{"type":"message","id":"a1b2c3d4","parentId":"f0e1d2c3","timestamp":…,"message":{"role":"user","content":"…","timestamp":…}}
{"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":…,"message":{"role":"assistant","stopReason":"error","errorMessage":"HTTP 503: …",…}}
{"type":"context_edit","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":…,"targetId":"b2c3d4e5","replacement":null}
```

`context_edit` 不能指向 system 或合成摘要消息：提示词变化以新的 system 消息记录，摘要通过新压缩生成。修正被摘要引用的原消息会使该摘要失效，投影回到已应用修正的原始记录。版本 1 的文件（头部带 `systemPrompt`）仍可打开，下次运行会把提示词作为 system 消息补进对话。

同一会话文件同一时间只能有一个写入者。创建或打开时用 `flock(2)` 加独占锁，锁持续到会话关闭；进程退出（包括被 SIGKILL）时由系统释放。其他进程，或同一进程里的另一个 `SessionStore`，再打开该文件会报错（`is in use`），`run` 与 `chat` 以退出码 2 结束，不写入任何内容。报错前最多重试 0.5 秒：本进程正在启动的子进程在运行自己的程序之前持有父进程文件描述符的副本，刚关闭的会话可能因此短暂保持锁定。文件系统不支持 `flock` 时打开失败；Unix 以外的平台不加锁。只查看内容可用 `SessionStore::read`：不加锁、不修改文件，未写完的尾记录不计入，得到的快照不能追加。

如果进程在写入最后一行时崩溃，未换行且确实未完成的尾记录会在下次打开时截掉，包括截断在 UTF-8 字符中间的情况。写入失败（如磁盘已满）后不再写入，因此留下的残缺同样只在文件末尾，按同一规则修复。CLI 会在 stderr 报告修复；库调用方可读取 `SessionStore::recovered_tail_bytes()`。完整记录缺少末尾换行时会补换行；已换行的损坏记录、未知记录类型和完整记录的字段错误会报错并保留文件。截掉尾记录发生在其余记录全部通过校验之后，因此打开失败时文件保持原样。

新建会话格式为 v4，兼容读取 v1–v3。旧文件在首次写入请求或压缩记录前追加 `format_upgrade`（version 3），在首次写入 `tool_call_started` 前追加 `format_upgrade`（version 4），都不改写头部。只支持到 v3 的程序会拒绝打开 v4 会话。压缩时先追加 `compaction_started`，请求完成后追加 `compaction` 或 `compaction_failed`。未完成或失败记录不改变投影；成功记录保留全部 system、结构化摘要及 `firstKeptEntryId` 之后的原消息，原始历史仍可审计。摘要来源是应用过 `context_edit` 的原始投影，不输入旧摘要。

每次模型请求（任务请求与摘要请求）发出前，先追加 `request_started`；provider、模型、工具定义、`max_tokens` 或 provider 转换参数变化时，先追加一条 `request_config`。`request_started` 引用配置，并记录消息来源：会话投影（以该条目的父节点为准），或摘要尝试的源条目与请求时间戳。库调用方可用 `SessionStore::replay_request` 还原当时与 provider 无关的 `Request`，OpenAI 兼容服务可再用 `openai::replay_body` 重建请求体。会话文件不记录 API key、URL 和 HTTP 头；自定义 provider 通过 `Provider::request_options` 提供非机密的转换参数，默认为 `null`，此时只能还原到 `Request`。这些记录写入失败时不发请求，并中止运行。`request_started` 只表示准备发送，后面没有对应的回答，说明发送结果未知。

有副作用的工具（`Tool::effects()` 为 `SideEffects`；内置工具中只有 bash）通过检查与审批后、执行前，先追加 `tool_call_started`（`toolCallId`、`toolName`）。并行批次先按调用顺序写完全部开始记录，再并发执行。写入失败时不执行该调用，并中止运行。只读工具不写开始记录。

进程崩溃、被终止或运行 future 被丢弃后，下一次运行在追加新输入之前检查分支末尾的 assistant 消息：没有结果记录的调用，各补一条 `isError` 的工具结果，写入后发出 `tool_call_recovered` 事件；调用不会被重放。`state` 按记录取值：

| `state` | 依据 | 告诉模型 |
|---|---|---|
| `started` | 有开始记录 | 结果未知，可能已改变状态，重新执行前先核对 |
| `not_started` | 会话已按 v4 记录开始，工具有副作用，但没有开始记录；或发出调用的回答以 `Error` / `Aborted` 结束（这类回答中的调用从不执行） | 未执行，没有产生影响 |
| `read_only` | 只读工具 | 没有结果，没有改变任何状态，可以重新调用 |
| `start_unknown` | 旧格式会话，或当前未配置该工具 | 是否开始未知，按可能已改变状态处理 |

文本输出在 stderr 打印 `[recover] <工具名> <调用 ID>: <状态>`。只处理分支末尾的调用：之后已有其他消息、已被 `context_edit` 省略或已被摘要替代的调用不补结果；这些情况下，发给模型时仍补 `No result provided` 占位。重试时省略的失败回答属于"已被省略"。

`providerError` 仍是可选消息字段；续写请求可带 `kind: "continueAnswer"`，报告据此拼接答案。普通 user 消息的 JSON 不变，旧文件缺少 `kind` 时按普通输入处理。

## 作为库使用

完整示例见 `examples/library.rs`（参与编译检查）。核心步骤如下：

```rust
let mut config = LoopConfig::new(LoopSpec {
    provider, model, system_prompt, tools, cwd, max_tokens: None, sequential_tools: false,
});
config.hooks.push(PolicyHook::new(policy));
let agent = AgentSession::new(config, RetryPolicy::default(), Limits::default(), session, sink);
let report = agent.prompt("任务描述").await?;
```

`Limits::default()` 不限制轮次和运行时间；`LoopConfig::new` 默认没有钩子，不自动续写、压缩或落盘工具结果。
需要排障预设时，设置 `config.hooks = presets::troubleshoot::hooks()`，并传入 `presets::troubleshoot::limits()`；再注册命令策略与脱敏钩子。排障说明位于 `presets::troubleshoot::INSTRUCTIONS`。
`examples/library.rs` 演示了这一预设的完整用法。需要外部取消令牌时使用 `prompt_with_cancel(task, &cancel).await?`。

P2 增加 `zx_agent::AgentSession`；已有 `Runner` 可通过 `AgentSession::from(runner)` 转为共享会话。
调用 `prompt` 只需 `&self`，可由多个前端持有 `Arc<AgentSession>`；同一时刻仅一个调用获得运行权。

| API | 契约 |
|---|---|
| `prompt`、`prompt_with_cancel` | 返回 `Result<RunReport, SessionError>`；重叠调用立即返回 `Busy`，不排队。provider、钩子和持久化失败由 `RunReport` 表达 |
| `compact`、`compact_with_cancel` | 返回 `Result<compaction::Report, SessionError>`；与 prompt 共用独占权、Busy、取消和丢弃后复用；报告包含结果、请求数、用量、前后字节数和审计条目 ID |
| `steer`、`follow_up` | 分别在下一次模型请求前和当前任务结束时消费，沿用原有队列 |
| `take_queued` | 空闲时取出排入队列但没有被运行消费的输入：先引导消息、后后续消息，各自按先后；运行中返回 `Busy`。运行只在每个回合末查看队列，所以运行结束时才到的输入会留到下一次运行；自己发起运行的宿主应在运行结束时调用它，参照 `im::Server` |
| `abort` | 只取消当前运行；空闲时返回 `false`。等待当前 `prompt` 返回，或 drop 它的 future 后，可继续提问；外部父令牌不受影响 |
| `subscribe` | 返回 `Subscription`，必须持有句柄才能继续接收事件；drop 后退订。只发送后续事件，不重放历史，已经分发的回调可能完成 |
| `set_model(ModelRef { provider, model })` | 空闲时同步切换，运行中返回 `Busy`，空模型 ID 返回 `InvalidModel`。保留历史、工具、提示词、钩子、上限与审批缓存；会话头部仍记录初始模型 |
| `clear_approvals` | 空闲时撤销所有本会话授权，运行中返回 `Busy` |

`ModelRef`、`SessionError` 和 `Subscription` 位于 `agent_session` 模块。新 provider 的认证和过滤配置由宿主负责。
构造时的 sink 持续接收事件；每条消息只写入会话一次，再通知所有订阅者。额外订阅示例：

```rust
let _subscription = agent.subscribe(extra_sink);
let first = agent.prompt("先检查日志").await?;
let second = agent.prompt("结合上面的证据总结").await?;
```

订阅回调同步执行，必须及时返回。单个回调 panic 会被隔离并摘除，可通过 `Subscription::has_panicked()` 或
`AgentSession::failed_subscribers()` 查询；初始 sink 的 panic 也计数。快照分发期间不持有订阅列表锁，可在回调中取消或退订。
运行期间由当前 `prompt` 独占持有 `Runner`，结束、panic 或 future 被丢弃时由 RAII 归还；状态锁不跨越异步等待。

P5 保持原构造器兼容。自动压缩通过 `Runner::new(...).with_compaction(compaction::Config { trigger_bytes: Some(256 * 1024), ..Default::default() })?` 显式启用，再转为 `AgentSession`。手动压缩默认最多 2048 输出 token、120 秒，无可压缩范围时返回 `Noop`。SDK 落盘需显式注册同配置的 `OutputStore`、`ReadOutputTool` 和 `SpillOutputs`，写入钩子放在脱敏之后，并排除分页工具。

P0 的 SDK 接口调整：`Limits::max_turns` 和 `agent::run_loop` 的轮次预算改为 `Option<u32>`。
`None` 表示不限，`Some(0)` 表示不调用模型，原来的数值上限 `n` 改为 `Some(n)`。
原 `prompt::DEFAULT_INSTRUCTIONS` 已移到上述预设。CLI 的参数、提示词及默认行为保持兼容。

P1 的 SDK 接口调整：静态配置移入 `LoopSpec`；`LoopConfig` 持有钩子、交互宿主与队列。
原 `config.policy` 改为注册 `PolicyHook::new(policy)`；`Limits::continue_on_length` 改为 `ContinueOnLength::new(n)`。
直接调用循环改为 `run_loop(context, prompts, config, &mut LoopControl::new(max_turns, approvals, cancel, sink))`，
其中 `approvals` 是宿主持有的 `&mut ApprovalCache`；使用 `Runner` 时由它管理。

`Hooks` 的六个回调均有默认实现，最后一个参数都是 `&HookContext`：会话 ID、运行 ID（与事件的 `runId` 相同）、工作目录与运行的取消信号 `CancelSignal`；它标为 `#[non_exhaustive]`，以后只增加字段。`CancelSignal` 可以等待、查询，也可以派生自己的子令牌，但不能取消运行。`HookChain::push` 以类型名注册，每次回调默认超时 5 秒；`push_with(hook, HookOptions { name, timeout })` 可以给出名称（用于 `hook_failed` 与追踪）和超时。`on_event` 收到带信封的 `Event`；每次调用都会被等待，所以默认不观察流式增量 `message_update`，需要时覆盖 `observes`（它 panic 时按不观察处理并报告 `hook_failed`）。并行执行工具时，同一钩子的 `on_event` 可能并发、乱序；运行被取消后，仍在等待的调用会被停止，可能收不到其余事件（包括 `run_end`）。需要完整、有序的事件流时用 `subscribe_bounded`。

钩子分阶段执行：每个回调都按 `Policy`（内置工具策略）、`Extension`（默认）、`Redaction`（脱敏）、`Storage`（长结果落盘）的顺序，同一阶段内按注册顺序。阶段由钩子自己声明（`Hooks::stage`）：`PolicyHook`、`RedactSecrets`、`SpillOutputs` 各自声明所属阶段，因此无论宿主以什么顺序注册，`before_tool` 都先经过内置策略，`after_tool` 中扩展的改动都会再经过脱敏，落盘的总是脱敏后的结果。
`before_tool` 取 Deny > Ask > Allow，首个 Deny 立即拒绝；同级 Ask 保留第一条理由。扩展用 `before_tool` 放行或拒绝；`ToolPolicy` 是内置策略经 `PolicyHook` 接入的方式，具体策略（如 `CommandPolicy`）在 `policy` 模块。`RulePolicy` 按 deny → ask → allow 匹配，同级按注册顺序，无匹配时拒绝。
策略与审批的错误、panic、超时均拒绝执行；`after_tool` 失败会丢弃输出；输入、上下文与回合钩子失败会结束运行且不触发 provider 重试。`on_event` 的错误只生成 `hook_failed`，诊断不再递归送入钩子。

`on_input` 完成后才写入输入：改写时依次追加原始输入和替换它的 `context_edit`，拒绝时追加原始输入和省略它的 `context_edit`，原始内容都留在会话中供审计。钩子被取消或运行 future 被丢弃时什么也不写，从队列取出的插话或后续消息仍留在队首，由下一次运行处理。两次追加之间进程崩溃时，原始输入会出现在投影中。
`transform_context` 返回按顺序应用的 `ContextEdit`：追加消息（`Append { message }`）、替换文本或省略消息；不能追加、修改或省略 system 消息（系统提示词只来自配置，配置之外的段落会在下一次尝试开始时被删除），也不能用文本替换抹掉工具调用。会话根据压缩记录合成的摘要（`UserMessageKind::Summary`）不能被编辑、省略或追加；要修正摘要，应编辑它引用的原始消息，摘要随之失效，投影回到原始记录。包含这类编辑的计划在校验时整份拒绝，不写入任何条目。
整份变换计划先校验，随后逐项持久化；写入失败时立即停止运行，不发送模型请求。写入失败前已追加的条目仍保留。

审批默认超时 120 秒。`NoHuman` 明确拒绝 Ask；`Approval::AllowOnce` 只允许一次，
`AllowSession` 只缓存当前会话内完全相同的工具名和参数，仍须通过每次策略检查，可用 `AgentSession::clear_approvals` 或底层 `Runner::clear_approvals` 撤销；不会写入会话或跨进程恢复。
异步回调必须让出执行权，合作式超时无法抢占阻塞线程。

组装系统提示词（`ai::SystemPrompt`）可用 `prompt::build_system_prompt`、`prompt::environment_section` 和 `CommandPolicy::prompt_section`。如果 API key 来自环境变量，构造 `BashTool` 时把该变量名放进 `hidden_env`。两者的用法见示例。

底层 `Runner::run(&mut self)` 仍可用于独占调用；共享前端使用 `AgentSession` 的方法。两种方式都保留会话，外层 `select!` 或 `timeout` 丢弃运行 future 时只取消本次子令牌。

自定义 provider 使用 `ai::provider::event_channel()` 创建有界通道，并通过 `futures::SinkExt::send` 异步发送，发送失败后停止生产。手动构造 `AssistantMessage` 的调用方需填写 `provider_error`（没有元数据时为 `None`）；`AssistantMessage::failure` 已提供这一默认值。

扩展点均为 trait：

- `ai::Provider`：接入新的模型服务，必须遵守 `src/ai/provider.rs` 中的约定。
- `agent::Tool`：自定义工具。返回 `Err` 等价于 pi 中的抛出异常，会变成错误结果交给模型。`effects()` 默认返回 `SideEffects`，Runner 会在执行前记录开始；不会改变任何状态的工具应返回 `Effects::ReadOnly`，否则中断后只能报告结果未知。
- `agent::ToolPolicy`：内置风格的工具调用策略，经 `policy::PolicyHook` 注册，运行在 `Policy` 阶段；扩展改用 `Hooks::before_tool`。
- `agent::Hooks`：输入、上下文、工具调用前后、回合结束与事件观察。钩子的输入输出类型（`Decision`、`TurnDecision`、`ContextEdit`、`ToolOutput`、`Approval`、`ApprovalRequest`、`ToolCall`、`HookContext`、`TurnSummary`）都有固定的 JSON 格式，供钩子进程与审批转发使用，格式由 `tests/hook_contract.rs` 固定。
- `interaction::Interaction`：由宿主回答审批；内核不读取终端。
- `agent::EventSink`：订阅事件，收到带信封的 `agent::Event`（事件本体在 `event.event`）。任何 `Fn(&Event) + Send + Sync` 都可以直接用作 sink。事件发出时，它报告的消息与编辑已写入会话；sink 只接收事件，不参与持久化。导出器可用 `AgentSession::subscribe_bounded(capacity)` 得到有界通道：通道满时丢弃新事件并计数（`EventStream::dropped()`），不阻塞运行；会话释放、已发出的事件读完后，`recv` 返回 `None`。订阅句柄不持有 sink，sink 随会话释放。
- `session::SessionLog`：会话记录存在哪里，见下文[会话存储](#会话存储)。
- `executor::Executor`：工具执行命令与读文件的位置。`BashTool` 与 `ReadFileTool` 的 `executor` 字段默认是 `LocalExecutor`（本机）；`run` 结束、超时或取消时须终止命令启动的全部进程，`read_file` 只读不超过上限的普通文件。`execution_mode` 默认 `Parallel`；一次只能处理一个调用的执行器返回 `Sequential`，内置工具随之声明，循环就不会提前开始并记录只能排队等待的调用。`ContainerExecutor::start(ContainerConfig::new(image, workspace))` 启动[容器执行](#容器执行)所用的容器，`tools::builtin_tools_with` 用指定的执行器构造内置工具；容器随执行器释放而删除。
- `agent::MessageQueues`：在运行期间注入引导消息或后续消息。
- `im::ImChannel`：接入一个 IM 平台，见下文[IM 接入](#im-接入)。
- `extensions::Extension`：把工具、钩子与系统提示词段落打包注册，见下文[扩展](#扩展)。

### 扩展

`extensions::Extension` 声明名称、工具、提示词段落与钩子，`extensions::register(&mut config, &[Arc::new(扩展)])` 把它们加入 `LoopConfig`。示例见 `Extension` 的文档注释（作为 doctest 编译）。

- 工具加在已有工具之后，名字不能与任何工具重名。
- 钩子以扩展的名称注册，`hook_failed` 中显示这个名称；它们只能声明 `Extension` 阶段，因此内置策略总在它们之前、脱敏与落盘总在它们之后：扩展放宽不了策略的结论，也恢复不了已脱敏的密钥。
- 段落加在系统提示词的末尾。名称只能是小写 ASCII 字母、数字与 `_`，以字母开头；不能是 `preamble`、zx-agent 生成的段落（`prompt::GENERATED_SECTIONS`：`environment`、`command_policy`、`project_context`、`available_skills`，无论提示词中是否已有）或提示词中已有的段落，因为同名段落会被替换。
- 先检查全部扩展再注册：扩展名为空或重复、钩子不在 `Extension` 阶段、工具重名或声明有误、段落名不合规时返回错误，`config` 保持原样。
- 一个扩展可以注册进多个循环（例如 serve 的每个线程），工具共用同一份；每次注册都会调用一次 `hooks()`。

### MCP 客户端

`mcp::Server::connect(ServerConfig::new(名称, 地址, 工具列表))` 连接一个外部 MCP 服务器，只支持 Streamable HTTP。返回的 `Server` 是一个扩展，用 `extensions::register` 加入循环。CLI 的用法见上文“MCP 工具”。

- 协议：先发 `server/discover`（2026-07-28）；服务器按旧版本应答时，改用 `initialize` 握手与会话（2025-11-25）。`Server::protocol_version()` 返回最终使用的版本。
- 工具：只把 `ServerConfig.tools` 中列出的工具交给模型，名字为 `<名称>__<工具名>`。名称只能是小写 ASCII 字母、数字与 `-`；工具名中字母、数字、`_`、`-` 以外的字符换成 `_`，全名不超过 64 个字符（模型 API 的限制）。服务器缺少列出的工具、换名后重名或超长时，连接失败。工具描述超过 4 KiB 时截断。
- 凭据：`token` 以 `Authorization: Bearer` 随每个请求发送，错误信息中不含它。
- 上限：连接与列出工具不超过 `connect_timeout`（默认 30 秒），每次调用不超过 `call_timeout`（默认 120 秒）。调用超时或被取消时请求随之结束：关闭连接，旧版协议另发 `notifications/cancelled`。
- 结果：文本原样拼接；图片、音频与二进制资源只写一行说明；没有其他内容时给出 `structuredContent` 的 JSON；`isError` 的结果作为错误结果交给模型；最多 2000 行或 50 KiB。协议错误、HTTP 失败、超时与取消作为工具错误返回。
- 副作用：工具一律按有副作用处理（`effects()` 为 `SideEffects`）。服务器给出的 annotations 只是提示，不据此放行，也不据此判断中断的调用是否已执行。
- 响应体没有大小上限：rmcp 读取 JSON 响应时不限制大小，SSE 每个事件上限 16 MiB。只连接自己选定的服务器。

### 会话存储

`SessionStore` 负责校验、条目树、投影、压缩、请求审计与崩溃恢复；记录本身交给 `SessionLog` 保存。`SessionStore::create` / `open` 使用 JSONL 文件；其他位置（例如数据库）实现 `SessionLog` 即可，只需一个方法：

```rust
#[async_trait]
pub trait SessionLog: Send + Sync {
    async fn append(&mut self, record: &LogRecord<'_>) -> anyhow::Result<()>;
}
```

`LogRecord` 带会话内从 1 开始的 `seq`、条目 ID、父条目 ID、条目类型、时间戳和 `json`；`json` 与 JSONL 文件中的一行逐字节相同，其余字段便于建索引。约定：

- 新会话由后端自己的构造函数保存头部（`SessionHeader`），再交给 `SessionStore::with_log`；续接会话时，后端按 `seq` 顺序读出头部与全部 `json`，组成 `LoadedSession` 交给 `SessionStore::load`，后者按打开文件的同一套规则校验每条记录。
- `append` 在记录保存后才返回。同一 `seq` 已有记录时必须报错而不是覆盖，例如对 (会话 ID, `seq`) 建唯一约束，这样第二个写入者无法分叉同一会话。
- `SessionStore` 先等 `append` 成功再更新内存。`append` 失败或 30 秒内未返回时，正在进行的运行以 `Error` 结束。此后，以及 `append` 的 future 在等待中被丢弃（结果未知）之后，这个 `SessionStore` 不再写入，之后的运行在第一次写入时同样以 `Error` 结束；要继续，需按存储中的实际内容重新加载。原因可用 `SessionStore::writes_stopped()` 或空闲时的 `AgentSession::writes_stopped()` 查询，`WritesStopped` 区分结果未知（`Interrupted`、`TimedOut`）、写入失败（`Failed`）与只读快照（`ReadOnly`）。
- 超时只对会让出执行权的 `append` 有效：卡在同步 I/O 里的实现会一直占住它所在的线程。内置的 JSONL 文件在自己的写入线程上写入，所以卷卡住时超时照样生效。

可运行的完整示例见 `session::SessionLog` 的 Rustdoc（`cargo test` 会执行）。`Runner::session()` 返回当前会话的只读借用。

### IM 接入

`im::Server` 在 IM 中回答问题：每个线程一个会话，写操作在 IM 中审批。设计见 `docs/serve-im-review.md`；命令行用法见上文[IM 服务](#im-服务)。Slack 的实现是 `im::slack::SlackChannel`，用 `SlackConfig::new(app_token, bot_token)` 构造；⚑ D:5/CHINA FLAG: 飞书的实现是 `im::feishu::FeishuChannel`，用 `FeishuConfig::new(app_id, app_secret)` 构造。两者的卡片文字都在 `CardLabels`，默认中文，可替换。

平台实现 `ImChannel` 的四个方法：`platform`、`run`（接收直到取消）、`send`、`update`。实现负责：自行重连；收到平台投递后立即确认，再转交事件；按消息 ID 去重；只转交发给机器人的消息（@ 与单聊），不转交机器人自己的消息；遵守平台限频；超长文本自行拆分。`Destination.thread` 是线程根消息的 ID。

宿主提供两样东西：`SessionFactory` 为一个线程打开会话，须把传入的 `interaction` 设为 `LoopConfig::interaction`；`ServerSink` 接收运维事件。`SessionKey::file_in(dir)` 给出 `<dir>/<平台>/<聊天>/<线程>.jsonl`，各段按字节编码，ID 不能指向其他目录。

| 行为 | 说明 |
|---|---|
| 线程即会话 | 聊天中的新消息以它为根开出线程；线程中的后续消息进入同一个会话 |
| 访问控制 | 只在 `ServerConfig::conversations` 列出的聊天中回答；其他聊天只收到一句“未授权” |
| 运行中的消息 | 作为插话（`steer`）；运行结束时才到的，由 `take_queued` 取出后另起一次运行 |
| `/cancel` | 单独发在线程中，取消该线程的运行；排队时取消排队 |
| 运行上限 | 同时运行 `max_running` 个会话，其余按到达顺序排队，并收到一句排队提示 |
| 回复 | 开始时发“处理中”，结束时把它替换为回答；未完成、超时、取消与失败各有固定说明（`Replies`，默认中文，可替换） |
| 审批 | 需要审批时发卡片；只有 `approvers` 中的用户点击才生效，其他人点击只收到提示。卡片按运行发出的 `approval_resolved` 改为结果，所以超时与取消也会显示 |
| 审计 | 每次审批结束，`ServerSink` 收到 `ServerEvent::Approval`：会话、工具调用、审批人、结果与时间 |
| 空闲释放 | `idle` 内没有输入的会话释放其文件锁，下一条消息重新打开 |
| 停止 | `run` 在取消后中止所有运行，等各线程发出结果后返回；平台返回错误时，以该错误结束 |

## 开发

```bash
cargo test                                   # 不需要网络和 API key；付费的真实 API 测试默认跳过
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

| 测试文件 | 覆盖内容 |
|---|---|
| `src/**`（单元测试） | 会话文件和事件流的 JSON 格式、事件信封与每次运行的编号、会话写入失败 / 超时 / 被中断后停止写入、单写者锁与只读快照、自定义存储的记录与重新加载、SSE 解析、流式累积、推理内容的记录与回传、请求体组装、错误链、重试判断、命令策略及其提示词、会话投影、崩溃恢复与文件权限、截断、bash（超时后杀掉整个进程组、取消、隐藏环境变量）、read_file、系统提示词的段落对比与两种发送模式 |
| `tests/agent_loop.rs` | 循环语义：工具调用往返、并行与结果顺序、串行模式、各类失败、length 截断、轮次上限、预算用完时不开新回合且排队输入留在队列、terminate、引导与后续消息、出错与中止、provider 未发最终事件、系统提示词的记录与增量更新 |
| `tests/runner.rs` | 重试且不重复 prompt、失败记录留在文件中、放弃不可重试的错误、轮次与时间上限、调用方取消、截断续写与拼接、最终回答只取本次运行的消息、续跑会话、恢复会话时提示词变化写入文件、先写盘再转发事件、自定义 `SessionLog` 存储的会话重新加载后继续对话、事件的编号与 ID、`run_start` / `run_end`、尝试与请求起点及其会话条目、消息与上下文编辑的条目 ID、预算用完不重试并保留错误、重试省略失败回答的 `context_edited`、无预算时排队输入留在队列、运行在观察者处理时被丢弃时排队输入只写一次 |
| `tests/agent_session.rs` | 共享提问与 Busy、取消与 future 丢弃后复用、队列边界、订阅和退订、回调重入与 panic 隔离、模型切换、授权撤销、先写审计再多订阅分发、有界订阅按序接收、通道满时丢弃计数且不阻塞运行、会话释放后有界订阅结束 |
| `tests/openai_mock.rs` | 本地假 HTTP 服务上的端到端测试：SSE、执行 bash、回传工具结果、认证头、HTTP 错误重试、空闲超时、中途取消后保留已输出内容、成功状态下的 JSON 正文（错误对象与整份回答） |
| `tests/cli.rs` | 编译出的程序：退出码 0 到 6、jsonl 输出、从 stdin 读提示词、bash 读不到 API key、系统提示词的生成段落、`--system-updates inline` |
| `tests/chat_cli.rs` | 多轮历史、审批与授权范围、审批过期后下一行作为新提问、只读命令自动放行、重置、模型切换、EOF、信号、超时、控制字符显示、恢复中断会话的提示、钩子失败的提示；Unix PTY 验证运行中插话、取消后丢弃残留插话、取消重置和退出 |
| `tests/profiles_cli.rs` | profile 覆盖、显式默认值、预算参数的 help 写明取自 profile、不可绕过的 deny、上下文范围与审计、上下文默认值与越界链接、`--system-prompt-file` 兼容、skill 懒加载与脱敏、配置错误（含 provider 参数）退出码且不建会话文件 |
| `tests/run_cli.rs` | `run` 在 P0 之后新增的约定：进度与报告行转义终端控制字符；另一个 `run` 正在写的会话文件不能再被打开写入；JSONL 事件的信封；为零的超时在请求前被拒绝；stdout 已关闭时退出码仍由运行结果决定（原有 `run` 测试仍在 `tests/cli.rs`） |
| `tests/session_compaction.rs`、`tests/compaction.rs` | v1/v2 追加升级、分支和引用、原始投影、省略保留边界后摘要仍生效、编辑摘要的变换计划在写入前被拒绝、摘要失败与取消、请求 / 用量预算、超时和上限 |
| `tests/request_audit.rs` | 每次任务与摘要请求可从会话文件还原、配置只在变化时追加、旧会话首次请求前升级、与投影不一致的请求不记录、固定会话文件的还原结果不变（版本守护） |
| `tests/executor.rs` | 执行器接缝：bash 与 read_file 只经自身执行器运行命令和读文件，命令的 6 种结束方式均正确报告，执行方式随执行器 |
| `tests/container.rs` | 容器执行：用假 docker 程序验证启动参数与失败报告、命令与读文件的转发、超时 / 取消 / 丢弃后的清理、工具逐个执行（排队的调用在取消后记为未执行）、命令用配置的 shell、启动失败不留会话文件、CLI 接线与配置错误；设置 `ZX_DOCKER_TEST_IMAGE` 时另对真实 Docker 验证用户、只读工作目录与无网络、命令进程树与 `setsid` 子进程的终止、清理不受命令写入影响、命令逐条执行、容器内读文件、chat 中 Ctrl-C 后容器仍在、zx-agent 正常结束或被 SIGKILL 后容器删除（CI 在 Linux 上用 `debian:bookworm-slim` 运行）；另验证 root 用户与工作目录内的会话文件被拒绝 |
| `tests/crash_recovery.rs` | 内置工具的副作用声明、开始记录先于执行（串行与并行批次）、中断批次的四种恢复结果且不重放、失败或取消的回答中的调用记为未执行、恢复事件在结果写入之后、旧格式会话、丢弃 future 后的同进程恢复、`run` 的 `[recover]` 输出；Unix 上 SIGKILL 掉正在执行命令的 `run`，再由下一次 `run` 恢复且不重放 |
| `tests/output_store.rs` | 先脱敏后落盘、UUID 边界、文件权限、UTF-8 分页、容量、取消、写盘失败或超时时保留内联结果、目录越界与 `.gitignore` |
| `tests/hooks.rs`、`tests/hooks_cli.rs` | 钩子组合与失败处理、审批范围与取消、审计变换、输入钩子被取消或运行被丢弃时不写入输入且插话留在队列、策略结论与审批事件（询问、会话内授权、拒绝、失败、超时、取消、等待时长）、观察者默认跳过增量、钩子上下文（会话、运行、工作目录与取消信号）、具名钩子、按阶段执行与先注册的脱敏仍在扩展之后、重名或声明名不一致的工具在请求前被拒绝、工具元数据 panic 时运行失败、`observes` 的 panic 被报告且运行继续、钩子不能追加 system 消息、工具取消自己的令牌不影响运行、续写预算、工具输出脱敏的 CLI 接线、脱敏失败在文本输出中的提示、非 UTF-8 密钥文件按有损解码匹配、无法匹配的密钥文件在启动时报配置错误 |
| `tests/hook_contract.rs` | 钩子输入输出的 JSON 契约：策略结论、回合决定、上下文编辑、审批及其请求、工具输出与调用、钩子上下文、回合摘要 |
| `tests/real_api.rs` | 真实模型 API（付费，默认跳过）：文本回答与用量、工具往返、策略拒绝、输出截断、恢复会话、无效 key 不重试 |

新增回归还覆盖：取消与 future 丢弃、特殊文件与读取上限、UTF-8 尾记录修复及损坏记录保留、交错工具参数 ID、有界队列背压、结构化错误与数值 `Retry-After`、响应预算和 CLI 参数接线。P0 回归覆盖内核不限轮次、无时限、不自动续写，以及零轮次、显式续写与预设取值。Rustdoc 的编译失败用例保护单一运行的借用约束。

`tests/real_api.rs` 会产生费用。每个测试都标为 `#[ignore]`，而且没有设置 `ZX_E2E_AUTHORIZED=1` 时，在发出请求前就失败。服务地址、模型和 key 所在的变量名都从环境变量读取，运行方式见该文件开头的注释。

## CI 与发布

`Rust` 工作流在 `main` 的 push、PR 和手动运行时检查：

- stable Rust 下的格式、clippy、严格 Rustdoc 构建，以及 actionlint 和发布脚本的 shellcheck。
- Linux（Ubuntu 24.04）和 macOS（Apple Silicon）的全目标构建、单元测试、集成测试及 doctest。
- Rust 1.88 的全目标编译和测试。构建与测试使用 `--locked`；真实模型 API 测试保持默认跳过。

`Release` 工作流在推送 `v*` tag 时构建发布草稿；tag 必须与 `Cargo.toml` 的版本一致。
也可以按既有 tag 重跑：`gh workflow run release.yml --ref v0.1.0`。手动运行时须选择 tag，
使来源证明记录的工作流版本与实际构建源码一致。

发布包包含二进制、README 和 MIT 许可证，支持 `x86_64-unknown-linux-gnu`（Ubuntu 22.04 构建）
及 `aarch64-apple-darwin`。两个平台都通过测试并完成
[GitHub 来源证明](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)
后，汇总安装包、`SHA256SUMS` 和 `.sigstore.json` 证明到同一份 release 草稿。
重跑可更新同 tag 的草稿附件；已发布的 release 会拒绝覆盖。确认附件齐全后再发布草稿，也适用于
[不可变 release](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)。

下载包及对应证明后，可用 GitHub CLI 验证构建来源：

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
