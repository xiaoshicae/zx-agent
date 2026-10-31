# zx-agent

一个用 Rust 编写的最小 agent 内核，面向无人值守场景，例如自动化排障。整体架构参考 [pi](https://github.com/earendil-works/pi)（commit `a92283056`）：分层方式、循环语义和会话文件格式都沿用 pi 的设计；凡是针对无人值守做的改动，都单独列在[与 pi 的差异](#与-pi-的差异)一节。

当前版本为 v0.1.0（最小内核）。

- **已具备**：
  - OpenAI 兼容的流式 provider
  - 双层 agent 循环：工具调用先检查、再并发执行，结果保持调用顺序
  - `bash` 与 `read_file` 工具
  - 声明式命令策略
  - 自动重试
  - 轮次上限与时间上限
  - 只追加的 JSONL 会话文件（可续跑）
  - JSONL 事件流输出
  - 回答被输出上限截断时自动续写
  - 启动时把环境信息与命令策略写入系统提示词
  - 对 bash 命令隐藏 API key 所在的环境变量
  - 系统提示词记录在对话中，恢复会话时只追加有变化的段落
- **尚未实现**：上下文压缩、扩展系统、MCP、其他 provider。详见[路线图](#路线图)。

## 快速开始

需要 Rust 1.85 及以上版本。

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

### 退出码

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
| `--system-updates` | `collapse` | 恢复会话时，提示词的变化怎样发给模型。`collapse`：合并进开头的 system 消息，适用于任何服务端；`inline`：在变化发生的位置发送，保留服务端的提示词缓存，模型也能看出提示词从哪里开始变化，但要求服务端接受对话中途的 system 消息，使用前需确认 |
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
| `--session` | 无（只保存在内存） | 会话文件路径。文件不存在则创建；已存在则在原有对话上继续 |
| `--output` | `text` | `text`：进度输出到 stderr，最终回答输出到 stdout；`jsonl`：每个事件一行 JSON，输出到 stdout |
| `--deltas` | 关闭 | 仅用于 `jsonl`：同时输出流式的 `message_update` 事件 |
| `--system-prompt-file` | 内置排障说明 | 替换内置的排障说明。下文的 `<environment>` 和 `<command_policy>` 两段仍会追加在末尾 |

## 架构

```
CLI (main.rs)
  └─ runner      重试、轮次与时间上限、写入会话文件          ← pi: coding-agent AgentSession（精简）
       ├─ session  只追加的 JSONL 条目树 + 投影              ← pi: SessionManager
       └─ agent    双层循环、工具执行、策略检查、事件         ← pi: packages/agent/src/agent-loop.ts
            ├─ ai    消息类型、流式事件协议、provider        ← pi: packages/ai
            └─ tools bash、read_file、输出截断               ← pi: coding-agent/src/core/tools
```

| 模块 | 文件 | 对应的 pi 实现 |
|---|---|---|
| 消息与事件协议 | `src/ai/types.rs` | `ai/src/types.ts`（`AssistantMessageEvent`） |
| provider 约定 | `src/ai/provider.rs` | `agent/src/types.ts`（`StreamFn` 约定） |
| OpenAI 兼容 provider | `src/ai/openai.rs` | `ai/src/api/openai-completions.ts`、`transform-messages.ts` |
| 重试判断 | `src/ai/retry.rs` | `ai/src/utils/retry.ts` |
| 测试用 provider | `src/ai/faux.rs` | `ai/src/providers/faux.ts` |
| agent 循环 | `src/agent/agent_loop.rs` | `agent/src/agent-loop.ts`（`runLoop`、`executeToolCalls`） |
| 工具调用策略 | `src/agent/policy.rs` | `beforeToolCall` 以及扩展的 `tool_call` 事件 |
| 会话 | `src/session.rs` | `coding-agent/src/core/session-manager.ts` |
| 运行控制 | `src/runner.rs` | `coding-agent/src/core/agent-session.ts`（`_startRun`、`_prepareRetry`） |
| 系统提示词 | `src/prompt.rs` | `coding-agent/src/core/system-prompt.ts` |

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
| 工具调用确认 | 只有声明式策略 | 没有人可以回答确认请求 |
| 输出被截断 | 自动请模型续写（`--continue-on-length`），仍未完成则以退出码 6 结束 | pi 不自动续写，交给用户决定；无人值守时没有人可以决定 |
| 会话文件写入失败 | 立即中止运行 | 不允许在缺少审计记录的情况下继续执行操作 |
| 会话文件创建时机 | 启动时立即创建并写入头部 | 进程中途崩溃也能留下记录 |
| 环境信息 | 启动时由程序写入系统提示词，见下文 | pi 只写入工作目录；无人值守时没有人提供这些信息，白名单也可能禁止 `uname`、`date` 等命令 |
| 参数校验 | 由各工具反序列化成自己的参数类型来完成 | 避免引入 JSON Schema 校验器 |
| 上下文压缩、扩展、TUI | 未实现 | 最小内核 |

### 系统提示词的组成

系统提示词由三个段落依次拼接，后两个由程序在启动时生成，各自用同名标签包裹（参照 pi 的 `system-prompt.ts`）：

| 部分 | 内容 | 何时出现 |
|---|---|---|
| 说明 | 内置排障说明，或 `--system-prompt-file` 的内容 | 总是 |
| `<environment>` | 操作系统与版本、CPU 架构、启动时间（UTC）与本地时区、工作目录 | 总是 |
| `<command_policy>` | 允许的命令前缀、禁止的语法、引号内 `\|` 的写法提示 | 启用了 bash 且设置了 `--allow-cmd` 时 |

这些信息都通过读文件获得（如 `/etc/os-release`、`/etc/localtime`），不执行命令。主机名和用户名不会写入，因为提示词会发送给第三方模型。

恢复会话时，程序重新生成提示词，只把有变化的段落追加为一条新的 system 消息。启动时间每次都不同，所以 `<environment>` 通常会出现在更新中。发给模型的方式由 `--system-updates` 决定：

- `collapse`（默认）：最新版本合并到开头的 system 消息。开头变化后，第一次请求无法命中服务端的提示词缓存。
- `inline`：更新出现在对话中变化发生的位置，内容为 `Updated system prompt section "environment": …`。之前的请求内容不变，可以命中缓存。

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

bash 命令结束时，无论是正常退出、超时还是被取消，它所在的整个进程组都会被结束，命令放到后台的进程不会残留。因此不能用 bash 工具启动常驻进程。

**局限（必须了解）**：

- 策略只是第一道防线，**不是安全边界**。生产环境请以最小权限账号运行，并放在容器或虚拟机中，必要时限制网络访问。
- `read_file` 不受命令白名单约束，可以读取进程有权限读取的任何文件。需要限制时，使用 `--tools bash` 关闭它。
- 工具输出可能包含密钥或个人数据，这些内容会发送给模型，并写入会话文件和事件流。请控制会话文件的访问权限；输出脱敏功能在路线图中。
- API key 只从环境变量读取，不会写入会话文件，也不会出现在事件中。bash 命令启动前，会从其环境中删除 `--api-key-env` 指定的变量，因此 `env`、`echo $NAME` 读不到 key。但工具与 agent 以同一系统用户运行，仍可能通过其他途径读到 key，例如在 Linux 上读取 agent 进程的 `/proc/<pid>/environ`，或在白名单允许 `ps` 时执行 `ps e`。需要隔离时，请以不同账号或在容器中运行。
- 除 `--api-key-env` 指定的变量外，其他环境变量（如云厂商凭证）bash 命令仍然可以读到。需要时请在启动 zx-agent 前清理环境变量。

## 输出格式

### 事件（`--output jsonl`）

每行一个事件。`type` 的取值有：`agent_start`、`turn_start`、`message_start`、`message_update`（需加 `--deltas`）、`message_end`（提示词首次记录或发生变化时，也包括 system 消息）、`tool_execution_start`、`tool_execution_end`、`turn_end`、`agent_end`、`retry_scheduled`。最后一行是汇总：

```json
{"type":"run_end","report":{"outcome":"completed","finalText":"## Summary ...","turns":3,"retries":0,"usage":{"input":1200,"output":300,"cacheRead":0,"totalTokens":1500},"sessionFile":"/var/log/zx-agent/run.jsonl"}}
```

`usage` 统计本次运行中每个回复的用量，失败的尝试也计入。被取消的请求记为 0，因为服务端在流的末尾才返回用量；实际计费以服务商账单为准。

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

如果进程在写入最后一行时崩溃，留下的不完整行会在下次打开会话时被截掉；文件其他位置出现损坏则直接报错。

## 作为库使用

完整示例见 `examples/library.rs`（参与编译检查）。核心步骤如下：

```rust
let config = LoopConfig { provider, model, system_prompt, tools, policy, cwd, max_tokens: None,
                          sequential_tools: false, queues: Arc::new(MessageQueues::default()) };
let runner = Runner::new(config, RetryPolicy::default(), Limits::default(), session, sink);
let report = runner.run("任务描述", &CancellationToken::new()).await;
```

组装系统提示词（`ai::SystemPrompt`）可用 `prompt::build_system_prompt`、`prompt::environment_section` 和 `CommandPolicy::prompt_section`。如果 API key 来自环境变量，构造 `BashTool` 时把该变量名放进 `hidden_env`。两者的用法见示例。

扩展点均为 trait：

- `ai::Provider`：接入新的模型服务，必须遵守 `src/ai/provider.rs` 中的约定。
- `agent::Tool`：自定义工具。返回 `Err` 等价于 pi 中的抛出异常，会变成错误结果交给模型。
- `agent::ToolPolicy`：自定义工具调用策略，例如接入审批系统或资源级权限。
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
| `tests/openai_mock.rs` | 本地假 HTTP 服务上的端到端测试：SSE、执行 bash、回传工具结果、认证头、HTTP 错误重试、空闲超时、中途取消后保留已输出内容 |
| `tests/cli.rs` | 编译出的程序：退出码 0 到 6、jsonl 输出、从 stdin 读提示词、bash 读不到 API key、系统提示词的生成段落、`--system-updates inline` |
| `tests/real_api.rs` | 真实模型 API（付费，默认跳过）：文本回答与用量、工具往返、策略拒绝、输出截断、恢复会话、无效 key 不重试 |

`tests/real_api.rs` 会产生费用。每个测试都标为 `#[ignore]`，而且没有设置 `ZX_E2E_AUTHORIZED=1` 时，在发出请求前就失败。服务地址、模型和 key 所在的变量名都从环境变量读取，运行方式见该文件开头的注释。

## 路线图

1. **上下文压缩**：参考 pi 的 `CompactionEntry`（`firstKeptEntryId` + 摘要）。长时间排障需要这项能力。
2. **执行后钩子**：工具输出先脱敏（密钥、令牌），再交给模型和写入文件。
3. **结构化最终报告**：按 JSON Schema 约束最终回答，便于告警或工单系统直接使用。
4. **更多 provider**：例如 Anthropic Messages API。
5. **异步审批策略**：高风险命令发 webhook 请求批准，超时则拒绝。
6. **MCP 客户端，以及只读的领域工具**：如 Kubernetes、Prometheus 查询。

## 许可证

MIT，见 [LICENSE](LICENSE)。设计参考的 [pi](https://github.com/earendil-works/pi) 同样采用 MIT 许可证（Copyright (c) 2025 Mario Zechner）。

---

README.md · v0.1.0 · 2026年10月
