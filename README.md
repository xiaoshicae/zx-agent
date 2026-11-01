# zx-agent

一个用 Rust 编写的通用 agent 内核，目标是让同一个内核支撑人机对话、无人值守运行和被其他程序嵌入。场景之间的差异将通过 profile（提示词、工具、规则、上限）和插件表达，不改内核。整体架构参考 [pi](https://github.com/earendil-works/pi)（commit `a92283056`）：分层方式、循环语义和会话文件格式都沿用 pi 的设计；与 pi 不同的地方列在[与 pi 的差异](#与-pi-的差异)一节。

当前版本为 v0.1.0，已实现无人值守的 `run` 和终端对话 `chat`。嵌入协议 `acp`、profile 文件和插件按[路线图](docs/ROADMAP.md)分阶段推进。

- **已具备**：
  - OpenAI 兼容的流式 provider，带事件队列、SSE 事件与响应大小上限
  - agent 回合循环：工具调用先检查、再并发执行，结果保持调用顺序
  - 长期 `AgentSession`：共享提问、单次运行互斥、取消后复用、多订阅者和模型切换
  - 终端 `chat`：流式回答、审批、运行中插话、Ctrl+C 后继续对话
  - `bash` 与 `read_file` 工具
  - 声明式命令策略
  - 结构化错误分类与自动重试
  - 可选的轮次上限与时间上限；无人值守默认值集中在 `troubleshoot` 预设
  - 只追加的 JSONL 会话文件（可续跑）
  - JSONL 事件流输出，流式工具参数携带调用 ID
  - 可配置的截断续写（`run` 默认续写 1 次）
  - 启动时把环境信息与命令策略写入系统提示词
  - 对 bash 命令隐藏 API key 所在的环境变量
  - 系统提示词记录在对话中，恢复会话时只追加有变化的段落
  - 运行 future 被丢弃时取消后台请求、定时任务和工具子进程组
- **尚未实现**：profile 文件、插件与 MCP、上下文压缩、其他 provider。详见[路线图](docs/ROADMAP.md)。

## 快速开始

需要 Rust 1.88 及以上版本。

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

直接输入问题即可开始下一轮。TTY 中运行时输入普通文本会作为 steering，在下一次模型请求前进入上下文；通过管道输入脚本时逐轮消费，避免后面的命令提前打断任务。单行输入最多 1 MiB（含行尾），无效 UTF-8 或超限会报错，不把截断片段发送给模型。

每轮默认不限轮次、不限总时间、不自动续写；可用 `--max-turns` 和 `--timeout-secs` 设限。连接、工具、会话和输出选项与 `run` 共用；`chat --help` 列出实际支持项。`--output jsonl` 为每轮发送一个 `run_end`，`--deltas` 可包含增量。

| 输入 | 行为 |
|---|---|
| `/help` | 查看命令 |
| `/model`、`/model <id>` | 查看模型，或空闲时切换同一 provider 的模型，保留历史 |
| `/new` | 创建新会话，清空历史和授权；运行中先取消任务。启用文件会话时在同目录创建唯一新文件，保留旧日志 |
| `/compact` | 提示压缩尚未实现，计划在 P5 加入 |
| `/quit` | 取消当前任务并退出 |
| Ctrl+C | 取消当前任务或审批，继续保留会话；空闲时保持会话 |
| EOF、SIGTERM | EOF 结束会话，管道中的正常任务先完成；待审批时 EOF 拒绝并取消。SIGTERM 取消并退出 |

chat 中的 bash 调用先经过原命令策略，再显示完整工具名、参数、工作目录、理由及权限范围，等待审批；read_file 仍按原读取权限执行。菜单中的编号绑定当前请求，例如：

```text
/approve 7 once
/approve 7 session
/deny 7 需要换一种做法
```

`once` 只允许当前调用，`session` 只复用完全相同的工具名与参数；命令策略的 Deny 始终优先。取消、超时后的旧编号不能批准新请求。审批默认超时 120 秒；未收到答案不会执行工具。模型文本和诊断中的 ESC、回车及双向控制字符按字面显示，保留正常换行和制表符。

`/quit` 和正常 EOF 退出码为 0；参数、配置或输入错误为 2；SIGTERM 为 5。单轮模型或预算错误会报告后继续对话，JSONL 消费者可读取每轮报告的 `outcome`。这是逐行 REPL，尚未提供历史行编辑或完整 TUI。

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
| `--tools` | `bash,read_file` | 启用的工具 |
| `--cwd` | 当前目录 | 工具的工作目录，同时写入 `<environment>` |
| `--allow-cmd` / `--deny-cmd` | 无 | bash 命令白名单前缀 / 额外的拒绝子串，均可重复 |
| `--max-turns` | 30 | 本次运行的模型请求次数上限，失败的尝试也计入 |
| `--timeout-secs` | 900 | 整次运行的时间上限，0 表示不限制 |
| `--tool-timeout-secs` | 120 | bash 命令的默认超时。模型可以通过工具参数 `timeout_secs` 为单条命令申请更长时间，上限为本参数与 600 秒中较大的一个；整次运行仍受 `--timeout-secs` 限制 |
| `--idle-timeout-secs` | 120 | 等待响应头，或两段流数据之间的最长等待时间 |
| `--max-retries` | 3 | 遇到临时性错误时的重试次数 |
| `--continue-on-length` | 1 | 回答因输出 token 上限被截断时，自动请模型续写的次数。每次续写都计入 `--max-turns`；设为 0 时直接以退出码 6 结束 |
| `--max-tokens` | 无（由服务端决定） | 每次回复的输出 token 上限。推理模型的推理部分也计入这个上限，设得太小时回答可能为空 |
| `--max-response-bytes` | 16777216（16 MiB） | 每次模型请求的流式响应字节上限，包含 SSE 封装与推理内容 |
| `--max-sse-event-bytes` | 1048576（1 MiB） | 单个 SSE 事件的字节上限，包含封装；累计多行和未换行的数据也受限制 |
| `--session` | 无（只保存在内存） | 会话文件路径。文件不存在则创建；已存在则在原有对话上继续 |
| `--output` | `text` | `text`：进度输出到 stderr，最终回答输出到 stdout；`jsonl`：每个事件一行 JSON，输出到 stdout |
| `--deltas` | 关闭 | 仅用于 `jsonl`：同时输出流式的 `message_update` 事件 |
| `--system-prompt-file` | 内置排障说明 | 替换内置的排障说明。下文的 `<environment>` 和 `<command_policy>` 两段仍会追加在末尾 |

重试优先依据结构化的 HTTP 状态和错误码。服务端数值秒形式的 `Retry-After` 会参与退避计算，仍受 `RetryPolicy::max_delay` 约束（默认 60 秒）；HTTP 日期形式和无效提示使用本地退避。旧会话和未提供结构化元数据的 provider 保留错误文本判断。

事件队列采用有界缓冲与背压，慢消费者会使生产者等待。达到响应或 SSE 事件上限时保留已接受的部分内容，以退出码 1 结束且不重试；确需更大响应时可显式提高对应上限。两个字节上限都必须大于零。

## 架构

```
CLI (main.rs + cli.rs + chat/)
  ├─ presets/troubleshoot  排障说明与 run 的默认上限、续写
  ├─ presets/assistant     chat 通用说明与终端审批
  └─ agent_session  共享会话、取消、订阅与模型切换           ← pi: coding-agent AgentSession
       └─ runner    独占运行、重试、轮次与时间上限、持久化
            ├─ session  只追加的 JSONL 条目树 + 投影         ← pi: SessionManager
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
| 终端前端 | `src/chat/`、`src/cli.rs` | 单输入所有者、审批、显示与共享 CLI 选项 |

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
| 会话文件写入失败 | 立即中止运行 | 不允许在缺少审计记录的情况下继续执行操作 |
| 会话文件创建时机 | 启动时立即创建并写入头部 | 进程中途崩溃也能留下记录 |
| 环境信息 | 启动时由程序写入系统提示词，见下文 | pi 只写入工作目录；无人值守时没有人提供这些信息，白名单也可能禁止 `uname`、`date` 等命令 |
| 参数校验 | 由各工具反序列化成自己的参数类型来完成 | 避免引入 JSON Schema 校验器 |
| 上下文压缩、TUI | 未实现 | 按路线图逐步加入；已提供类型化 Hooks |

### 系统提示词的组成

系统提示词由三个段落依次拼接，后两个由程序在启动时生成，各自用同名标签包裹（参照 pi 的 `system-prompt.ts`）：

| 部分 | 内容 | 何时出现 |
|---|---|---|
| 说明 | 内置排障说明，或 `--system-prompt-file` 的内容 | 总是 |
| `<environment>` | 操作系统与版本、CPU 架构、启动时间（UTC）与本地时区、工作目录 | 总是 |
| `<command_policy>` | 允许的命令前缀、禁止的语法、引号内 `\|` 的写法提示 | 启用了 bash 且设置了 `--allow-cmd` 时 |

这些信息都通过读文件获得（如 `/etc/os-release`、`/etc/localtime`），不执行命令。主机名和用户名不会写入，因为提示词会发送给第三方模型。

恢复会话时，程序重新生成提示词，只把有变化的段落追加为一条新的 system 消息。启动时间按分钟记录；跨分钟恢复时，`<environment>` 通常会出现在更新中。发给模型的方式由 `--system-updates` 决定：

- `collapse`（默认）：最新版本合并到开头的 system 消息。开头变化会影响服务端对既有请求前缀的缓存复用。
- `inline`：更新出现在对话中变化发生的位置，内容为 `Updated system prompt section "environment": …`。在模型、工具等发送配置不变时，之前的请求前缀保持不变，便于复用缓存；是否实际命中由服务端决定。

## 安全模型

`CommandPolicy` 只检查 `bash` 工具：

- 所有命令都会匹配一组危险模式（如 `rm -rf /`、`mkfs`、`shutdown`），匹配前会先转小写并合并空白。
- 设置 `--allow-cmd` 后，命令只能是由白名单程序组成的管道（`a | b`），并且拒绝 `;`、`&`、`` ` ``、`$(`、`>`、`<` 和换行。检查只识别这些符号：变量展开（如 `$VAR`）和通配符不会被拒绝，白名单程序自身的参数也不检查，例如允许 `find` 就等于允许 `find -exec`。

示例（白名单为 `kubectl get`、`grep`）：

| 命令 | 结果 |
|---|---|
| `kubectl get pods -A \| grep -v Running` | 允许 |
| `kubectl delete pod x` | 拒绝：不在白名单 |
| `kubectl get pods && rm x` | 拒绝：包含 `&` |
| `grep x $(cat list)` | 拒绝：包含 `$(` |

在 Unix 上，bash 命令正常退出、超时、被取消或工具 future 被丢弃时，程序都会终止其进程组，并清理输出读取任务。因此不能用 bash 工具启动常驻进程。

`read_file` 只接受普通文件，默认单文件上限为 10 MiB，同时检查文件元数据与实际读取量。目录、FIFO 和设备文件会被拒绝。取消会停止等待文件 I/O；底层已经启动的普通文件 I/O 仍可能由 Tokio 的阻塞线程继续完成。

**局限（必须了解）**：

- 策略只是第一道防线，**不是安全边界**。生产环境请以最小权限账号运行，并放在容器或虚拟机中，必要时限制网络访问。
- `read_file` 不受命令白名单约束，可以读取进程有权限读取的普通文件。需要限制时，使用 `--tools bash` 关闭它。
- `run` 和 `chat` 在 `after_tool` 中将认证 API key 的完整字面值替换为 `[REDACTED]`，处理后的结果才会写入会话、事件并回传模型。SDK 可用 `RedactSecrets` 显式配置其他密钥；过滤器失败时丢弃原始输出，返回错误结果。
- 该过滤不识别未知密钥、编码或拆分后的密钥，也不处理用户输入、模型输出和工具调用参数。这些内容及工具输出中的其他敏感数据仍可能进入模型请求、会话和事件；请控制会话文件访问权限。
- 认证 key 只从环境变量读取；bash 命令启动前删除 `--api-key-env` 指定的变量。但工具与 agent 以同一系统用户运行，仍可能通过其他途径读取凭证。需要隔离时，请以不同账号或在容器中运行。
- 除 `--api-key-env` 指定的变量外，其他环境变量（如云厂商凭证）bash 命令仍然可以读到。需要时请在启动 zx-agent 前清理环境变量。

## 输出格式

### 事件（`--output jsonl`）

每行一个事件。`type` 的取值有：`agent_start`、`turn_start`、`message_start`、`message_update`（需加 `--deltas`）、`message_end`（包括 system 消息）、`tool_execution_start`、`tool_execution_end`、`turn_end`、`agent_end`、`retry_scheduled`、`context_edited`、`hook_failed`。`run` 最后一行、`chat` 每轮最后一行是汇总：

```json
{"type":"run_end","report":{"outcome":"completed","finalText":"## Summary ...","turns":3,"retries":0,"usage":{"input":1200,"output":300,"cacheRead":0,"totalTokens":1500},"sessionFile":"/var/log/zx-agent/run.jsonl"}}
```

`usage` 统计本次运行中每个回复的用量，失败的尝试也计入。被取消的请求记为 0，因为服务端在流的末尾才返回用量；实际计费以服务商账单为准。

`message_update.delta` 的 `tool_call_args` 包含 `id` 和 `text`，`id` 与对应 `tool_call_start` 相同，便于关联交错返回的工具参数。旧事件缺少 `id` 时，反序列化为空字符串。

模型失败消息可附带 `providerError`，区分 HTTP、服务端流内错误、传输、超时、协议与资源上限。HTTP 元数据包含 `status`、可选 `code` 和 `retryAfterMs`；给人看的原因仍在 `errorMessage` 中。

### 会话文件（`--session`）

第一行是头部，之后每行一个条目，条目之间通过 `parentId` 连成一棵树：

```json
{"type":"session","version":2,"id":"…","timestamp":1761700000000,"cwd":"/srv","model":"…"}
{"type":"message","id":"f0e1d2c3","parentId":null,"timestamp":…,"message":{"role":"system","sections":[{"name":"preamble","content":"…"},{"name":"environment","content":"…"}],"timestamp":…}}
{"type":"message","id":"a1b2c3d4","parentId":"f0e1d2c3","timestamp":…,"message":{"role":"user","content":"…","timestamp":…}}
{"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":…,"message":{"role":"assistant","stopReason":"error","errorMessage":"HTTP 503: …",…}}
{"type":"context_edit","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":…,"targetId":"b2c3d4e5","replacement":null}
```

`context_edit` 不能指向 system 消息：提示词的变化只能以新的 system 消息记录。版本 1 的文件（头部带 `systemPrompt`）仍可打开，下次运行会把提示词作为 system 消息补进对话。

如果进程在写入最后一行时崩溃，未换行且确实未完成的尾记录会在下次打开时截掉，包括截断在 UTF-8 字符中间的情况。CLI 会在 stderr 报告修复；库调用方可读取 `SessionStore::recovered_tail_bytes()`。完整记录缺少末尾换行时会补换行；已换行的损坏记录、未知记录类型和完整记录的字段错误会报错并保留文件。

会话格式仍为版本 2。`providerError` 是可选消息字段；续写请求增加可选的 `kind: "continueAnswer"`，报告据此拼接答案。普通 user 消息的 JSON 不变，旧文件缺少 `kind` 时按普通输入处理。

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

`Limits::default()` 不限制轮次和运行时间；`LoopConfig::new` 默认没有钩子，不自动续写。
需要排障预设时，设置 `config.hooks = presets::troubleshoot::hooks()`，并传入 `presets::troubleshoot::limits()`；再注册命令策略与脱敏钩子。排障说明位于 `presets::troubleshoot::INSTRUCTIONS`。
`examples/library.rs` 演示了这一预设的完整用法。需要外部取消令牌时使用 `prompt_with_cancel(task, &cancel).await?`。

P2 增加 `zx_agent::AgentSession`；已有 `Runner` 可通过 `AgentSession::from(runner)` 转为共享会话。
调用 `prompt` 只需 `&self`，可由多个前端持有 `Arc<AgentSession>`；同一时刻仅一个调用获得运行权。

| API | 契约 |
|---|---|
| `prompt`、`prompt_with_cancel` | 返回 `Result<RunReport, SessionError>`；重叠调用立即返回 `Busy`，不排队。provider、钩子和持久化失败由 `RunReport` 表达 |
| `steer`、`follow_up` | 分别在下一次模型请求前和当前任务结束时消费，沿用原有队列 |
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

P0 的 SDK 接口调整：`Limits::max_turns` 和 `agent::run_loop` 的轮次预算改为 `Option<u32>`。
`None` 表示不限，`Some(0)` 表示不调用模型，原来的数值上限 `n` 改为 `Some(n)`。
原 `prompt::DEFAULT_INSTRUCTIONS` 已移到上述预设。CLI 的参数、提示词及默认行为保持兼容。

P1 的 SDK 接口调整：静态配置移入 `LoopSpec`；`LoopConfig` 持有钩子、交互宿主与队列。
原 `config.policy` 改为注册 `PolicyHook::new(policy)`；`Limits::continue_on_length` 改为 `ContinueOnLength::new(n)`。
直接调用循环改为 `run_loop(context, prompts, config, &mut LoopControl::new(max_turns, approvals, cancel, sink))`，
其中 `approvals` 是宿主持有的 `&mut ApprovalCache`；使用 `Runner` 时由它管理。

`Hooks` 的六个方法均有默认实现。`HookChain::push` 按顺序注册，每次回调默认超时 5 秒，可用 `push_with_timeout` 覆盖。
`before_tool` 取 Deny > Ask > Allow，首个 Deny 立即拒绝；同级 Ask 保留第一条理由。`RulePolicy` 按 deny → ask → allow 匹配，同级按注册顺序，无匹配时拒绝。
策略与审批的错误、panic、超时均拒绝执行；`after_tool` 失败会丢弃输出；输入、上下文与回合钩子失败会结束运行且不触发 provider 重试。`on_event` 的错误只生成 `hook_failed`，诊断不再递归送入钩子。

`on_input` 的原始输入先追加审计记录，改写或拒绝通过 `context_edit` 表达。
`transform_context` 返回按顺序应用的 `ContextEdit`：追加消息、替换文本或省略消息；不能修改或省略 system 消息，也不能用文本替换抹掉工具调用。
整份变换计划先校验，随后逐项持久化；写入失败时立即取消，不发送模型请求。写入失败前已追加的条目仍保留。

审批默认超时 120 秒。`NoHuman` 明确拒绝 Ask；`Approval::AllowOnce` 只允许一次，
`AllowSession` 只缓存当前会话内完全相同的工具名和参数，仍须通过每次策略检查，可用 `AgentSession::clear_approvals` 或底层 `Runner::clear_approvals` 撤销；不会写入会话或跨进程恢复。
异步回调必须让出执行权，合作式超时无法抢占阻塞线程。

组装系统提示词（`ai::SystemPrompt`）可用 `prompt::build_system_prompt`、`prompt::environment_section` 和 `CommandPolicy::prompt_section`。如果 API key 来自环境变量，构造 `BashTool` 时把该变量名放进 `hidden_env`。两者的用法见示例。

底层 `Runner::run(&mut self)` 仍可用于独占调用；共享前端使用 `AgentSession` 的方法。两种方式都保留会话，外层 `select!` 或 `timeout` 丢弃运行 future 时只取消本次子令牌。

自定义 provider 使用 `ai::provider::event_channel()` 创建有界通道，并通过 `futures::SinkExt::send` 异步发送，发送失败后停止生产。手动构造 `AssistantMessage` 的调用方需填写 `provider_error`（没有元数据时为 `None`）；`AssistantMessage::failure` 已提供这一默认值。

扩展点均为 trait：

- `ai::Provider`：接入新的模型服务，必须遵守 `src/ai/provider.rs` 中的约定。
- `agent::Tool`：自定义工具。返回 `Err` 等价于 pi 中的抛出异常，会变成错误结果交给模型。
- `agent::ToolPolicy`：自定义工具调用策略，例如接入审批系统或资源级权限。
- `agent::Hooks`：输入、上下文、工具调用前后、回合结束与事件观察。
- `interaction::Interaction`：由宿主回答审批；内核不读取终端。
- `agent::EventSink`：订阅事件。任何 `Fn(AgentEvent) + Send + Sync` 都可以直接用作 sink。
- `agent::MessageQueues`：在运行期间注入引导消息或后续消息。

## 开发

```bash
cargo test                                   # 不需要网络和 API key；付费的真实 API 测试默认跳过
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

| 测试文件 | 覆盖内容 |
|---|---|
| `src/**`（单元测试） | 会话文件和事件流的 JSON 格式、会话写入失败时取消运行、SSE 解析、流式累积、推理内容的记录与回传、请求体组装、错误链、重试判断、命令策略及其提示词、会话投影、崩溃恢复与文件权限、截断、bash（超时后杀掉整个进程组、取消、隐藏环境变量）、read_file、系统提示词的段落对比与两种发送模式 |
| `tests/agent_loop.rs` | 循环语义：工具调用往返、并行与结果顺序、串行模式、各类失败、length 截断、轮次上限、terminate、引导与后续消息、出错与中止、provider 未发最终事件、系统提示词的记录与增量更新 |
| `tests/runner.rs` | 重试且不重复 prompt、失败记录留在文件中、放弃不可重试的错误、轮次与时间上限、调用方取消、截断续写与拼接、最终回答只取本次运行的消息、续跑会话、恢复会话时提示词变化写入文件、先写盘再转发事件 |
| `tests/agent_session.rs` | 共享提问与 Busy、取消与 future 丢弃后复用、队列边界、订阅和退订、回调重入与 panic 隔离、模型切换、授权撤销、先写审计再多订阅分发 |
| `tests/openai_mock.rs` | 本地假 HTTP 服务上的端到端测试：SSE、执行 bash、回传工具结果、认证头、HTTP 错误重试、空闲超时、中途取消后保留已输出内容 |
| `tests/cli.rs` | 编译出的程序：退出码 0 到 6、jsonl 输出、从 stdin 读提示词、bash 读不到 API key、系统提示词的生成段落、`--system-updates inline` |
| `tests/chat_cli.rs` | 多轮历史、审批与授权范围、重置、模型切换、EOF、信号、超时、控制字符显示；Unix PTY 验证运行中插话、取消重置和退出 |
| `tests/hooks.rs`、`tests/hooks_cli.rs` | 钩子组合与失败处理、审批范围与取消、审计变换、续写预算、工具输出脱敏的 CLI 接线 |
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
