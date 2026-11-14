---
paths:
  - "**/*.rs"
---

# 06 测试

测试原则和分层见 CLAUDE.md 的"测试"一节，每个测试文件覆盖什么见 `docs/TESTS.md`。这里是写测试时用到的设施和约定。

## 测试设施

测试设施分两处。只用库的测试（根目录 `tests/`）用 `tests/common/`；驱动程序的测试（`crates/zx-cli/tests/`）用 `crates/zx-cli/tests/common/mod.rs`，它经 `#[path]` 把 `tests/common/` 整个带进来再导出，所以下表的设施在那里都能用 `common::` 取到，只有驱动程序才用的那几项写在它自己那里。

| 设施 | 位置 | 用途 |
|---|---|---|
| `FauxProvider` / `FauxStep` | `crates/zx-ai/src/faux.rs` | 按脚本返回文本、工具调用、截断、失败（`Fail`）、挂起直到取消（`Hang`），或按请求内容现场决定（`FauxStep::compute`，例如引用摘要请求给出的条目 ID），并记录每个请求 |
| `EchoTool` | `tests/common/mod.rs` | 可设置延迟、失败、panic、terminate，报告进度（`progress`）、返回 `details`；会话建好后用 `feeds(runner.queues())` 把会话的队列交给它，调用就能注入引导消息或后续消息 |
| `Collect` | `tests/common/mod.rs` | 收集事件；`envelopes()` 返回带信封（`seq`、`runId` 等）的事件，`events()` 返回事件本体，`kinds()` 返回事件类型序列，`steps()` 返回开始的任务步数 |
| `summarize` | `tests/common/mod.rs` | 把消息列表压缩成 `"tool:c1=hi"` 这样的短字符串 |
| `run_prompt` / `run_prompt_with` | `tests/common/mod.rs` | 在以给定消息开头的内存会话上跑一次提问（不重试；`run_prompt` 至多 10 次模型请求，`run_prompt_with` 给出请求上限），返回 `RunReport` 和事件，并把会话投影写回传入的消息。循环是 crate 内部类型，测试都经运行驱动它 |
| `RunnerSetup` / `Driven` | `tests/common/mod.rs` | 驱动会话的测试适配层。`RunnerSetup`：带测试默认值（不重试、默认上限、内存会话、不自动摘要）的构造器，用结构体更新语法覆盖字段；`config` 是 `AgentConfig`（`config(provider, tools)` 给出），重试、上限与摘要设置以 `RunnerSetup` 的 `retry`、`limits`、`compaction` 字段为准；会话在 `session` 头部的目录工作（`header_in(cwd)`）；`build()` 给出 `Driven` 与收集事件的 `Collect`，`build_with(watch)` 另在读到每个事件时调用 `watch(事件, 会话)`。`Driven`：解引用为 `Session`，另有把一次运行读到结束的 `run`（会话拒绝时 panic）、`prompt`、`continue_run`、`compact`（返回会话的拒绝），以及给出会话记录只读副本的 `session()`；要取消运行就调用会话自己的 `cancel()`。测 `Run` 本身（事件顺序、读才推进、丢弃即取消）时直接用 `Session::send`。会话经隐藏的 `Agent::adopt_session` 由测试给的 `Transcript` 建成 |
| `Approver` / `Asked` / `run_approved` | `tests/common/mod.rs` | 回答运行的审批请求，像接入方那样：`Approver::confirm` 给出决定，`None` 表示不回答；它考虑期间运行照常被读取，所以超时与取消仍然生效。经 `RunnerSetup { approver: Some(..), .. }` 交给会话（未指定时等待两分钟，要别的时长就设 `config.approvals`），`run_approved` 是在新的内存会话上跑一次提问的简写 |
| `zx_agent` / `zx_agent_in` / `Home` | `crates/zx-cli/tests/common/mod.rs` | 编译出的程序；`zx_agent_in` 在给定的工作目录、以给定的主目录启动它（`Home::Agent` 设 `ZX_AGENT_HOME`，`Home::User` 设 `HOME` 并去掉 `ZX_AGENT_HOME`，chat 与 acp 的默认会话目录在其下），测试结束时杀掉；驱动程序的测试都用这几个 |
| `stdout` / `stderr` | `tests/common/mod.rs` | 一个结束了的程序写到两个输出的文本 |
| `AgentHome` / `write_config` / `write_agent` / `fake_model_config` | `crates/zx-cli/tests/common/mod.rs` | 测试自己的 zx-agent 主目录：`AgentHome` 是交给 `ZX_AGENT_HOME` 的临时目录，带 `config.toml` 与替换内置 agent 的 `agents/<名字>.md`；`write_config`、`write_agent` 写进测试设了 `HOME` 时的 `<HOME>/.zx-agent`；`fake_model_config` 是对假模型的 `config.toml`（key 所在的变量、不重试，再加给出的键），服务地址经 `ZX_BASE_URL` 传入。权限、上下文、摘要、模型元数据、执行环境与 provider 设置没有命令行参数，驱动程序的测试经它们设置。`zx_agent()` 默认指向一个空的主目录，并去掉 `ZX_MODEL`、`ZX_BASE_URL`、`OPENAI_API_KEY`，开发者自己的配置不会进入测试 |
| `fast_retry` / `header` / `header_in` / `read_lines` / `json_lines` | `tests/common/mod.rs` | 毫秒级退避的重试策略、在当前目录或给定目录工作的会话头部、读取会话文件的每一行、把 JSON 行文本解析成值 |
| `refusal` | `tests/common/mod.rs` | `Agent::new` 拒绝一份配置的原因：重名或声明有误的工具、不合规则的扩展 |
| `create_session_file` / `open_session_file` / `read_session_file` | `tests/common/mod.rs` | 在给定路径的会话文件上新建、续接（都经 `JsonlStorage::file`）与读取快照；`read_session_file` 是同步的，回调里也能用，写入者仍在运行时同样可读 |
| `NotifyingProvider` | `tests/common/mod.rs` | 每个请求开始时通知（`started`）并记下取消令牌的脚本化 provider，测试"运行中"的行为时等它，不用 `sleep` |
| `seeded_session` / `summary_json` / `replayed_requests` | `tests/common/mod.rs` | 一条值得摘要的旧消息加一条新输入的会话、模型回答摘要请求的 JSON、从会话文件的检查点还原的全部请求 |
| `tool_call_answer` / `answer_reporting` / `signal` | `tests/common/mod.rs` | 崩溃前已记录的工具调用回答、上报了给定输入 token 数的已完成回答、给子进程发信号（Unix） |
| `session_dir` / `session_files` | `crates/zx-cli/tests/common/mod.rs` | chat 与 acp 的默认会话目录及其中的文件 |
| `Captured::user_texts` / `system_text` / `tool_results` | `tests/common/mock_server.rs` | 假模型服务记录的请求里的用户消息、系统消息与工具结果 |
| `run_channel` / `Running` | `tests/common/im.rs` | 在自己的任务里运行一个 `ImChannel`，`next` 等它转交的事件，`stop` 取消并取回结果，`ended` 等它自行结束 |
| `pty` | `crates/zx-cli/tests/common/mod.rs` | Unix 伪终端：主端给测试，从端给子进程当终端 |
| `summary_eval` | `tests/common/summary_eval.rs` | 摘要评测：按脚本合成的 8 轮排障会话（24 条埋入的事实，其中 6 条后来被取代），`evaluate` 每轮之后摘要并评分（仍成立的事实写到了几条；被取代的事实是否只剩旧值，另计写着旧值的条数；从会话还原的摘要请求大小）；付费评测与常规测试共用 |
| `FakeFeishu` / `TestFrame` | `tests/common/fake_feishu.rs` | 假 ⚑ D:5/CHINA FLAG: 飞书开放平台：token、长连接地址与消息接口（可按接口安排错误码或 429），长连接由测试逐帧收发；帧编解码独立于被测代码 |
| `read_request` / `response` | `tests/common/http.rs` | 假服务读取一个 HTTP 请求、拼出响应 |
| `FakeMcp` / `Reply` | `tests/common/fake_mcp.rs` | 假 MCP 服务器（Streamable HTTP），按规范手写、不用 rmcp：可选 2026-07-28（`server/discover`）或 2025-11-25（`initialize` 与会话）；工具一页一个；按工具安排结果、JSON-RPC 错误、不应答（`Hang`）或以 SSE 流按间隔发进度通知再给结果（`Progress`，令牌取自请求的 `_meta.progressToken`），并记录每个请求的方法、参数与请求头、被客户端放弃的调用 |
| `FakeSlack` / `Answer` | `tests/common/fake_slack.rs` | 假 Slack：Web API 记录每次调用，可按方法安排错误、429 或指定的 JSON 应答；Socket Mode 连接由测试逐帧发送、读取确认 |
| `MockServer` / `Reply` | `tests/common/mock_server.rs` | 本地假 HTTP 服务：返回 SSE 流（`Reply::text`、`Reply::bash`、任意工具调用 `Reply::tool_call`）、HTTP 错误，发出部分数据后卡住（`Stall`），或等待测试通知后结束（`Gate`）；记录请求，`wait_for_requests` 等到请求到达 |
| `FailingWriter` / `Transcript::with_writer` | `crates/zx-core/src/transcript/test_support.rs` 与 `transcript.rs`（仅测试编译） | 让会话写入失败，测试不变量 5 和 7；`FailingWriter` 经 `crate::transcript::FailingWriter` 引用 |
| `create_file` / `open_file` / `read_file` | `crates/zx-core/src/transcript/test_support.rs`（仅测试编译） | transcript 各子模块的单元测试在一个会话文件上新建、续接与读取快照，都经 `JsonlStorage::file` |
| `header` / `assistant` / `texts` / `result_with_details` | `crates/zx-core/src/transcript/test_support.rs`（仅测试编译） | transcript 各子模块的单元测试共用：会话头部、一条回答、会话投影的文本、带 details 的工具结果 |
| `MemoryStorage` | `crates/zx-core/src/transcript/memory.rs` | 内存中的会话存储：需要“存储”而不需要文件的测试用它，如 `tests/im_server.rs` 的 agent 把各对话的会话存在其中，服务按标签找回 |

典型写法：循环测试参照 `tests/step_loop.rs` 的 `runs_tools_until_the_model_answers`，用 `run_prompt` 跑完后断言 `result.outcome`，再用 `summarize(&context)` 断言消息序列；运行层测试参照 `tests/runner.rs`，写成 `RunnerSetup { retry: fast_retry(3), ..setup(&provider) }.build()`。

## 约定

- 只能在 Unix 上运行的测试（如进程组、信号相关的）加 `#[cfg(unix)]`。
- 驱动程序的测试经 `zx_agent()`（指向空的主目录、去掉 `ZX_MODEL`、`ZX_BASE_URL`、`OPENAI_API_KEY`）或 `zx_agent_in` 启动，要自己的主目录时用 `zx_agent_in` 的 `Home`，开发者的配置与真实的家目录都进不了测试。
- 测试函数同样受 `too_many_lines` 约束；一个脚本化场景或一张"每个变体一条"的表可以带 `#[expect(clippy::too_many_lines, reason = "...")]`，理由说明它是什么表（规则 09）。
- 新的集成测试按是否需要编译出的程序放在 `tests/` 或 `crates/zx-cli/tests/`（后者也放用到 `zx_cli::config`、`zx_cli::otel` 的），并在 `docs/TESTS.md` 里登记一行。

## 写付费的真实 API 测试

运行它们需要用户授权（红线 9）。新增时遵守：

- 只放在 `tests/real_api.rs`。每个测试标 `#[ignore = "..."]`，并在发出请求前调用 `Endpoint::from_env()`，它在缺少 `ZX_E2E_AUTHORIZED=1` 时直接失败。
- 不写死服务商：地址、模型、key 所在的变量名都从环境变量读取。只用合成的提示词和临时目录里的文件。
- 限制输出 token、轮次和时间，用 `print_usage` 打印用量；请求授权时说明大致用量。
- 用量远高于其余测试的（如摘要评测），再要求一个专门的变量（摘要评测为 `ZX_E2E_EVAL=1`），未设置时只打印跳过，免得运行整个文件时连带产生这部分费用。
- 断言只依赖稳定的事实（结果类型、工具是否运行、关键词），不依赖模型的措辞。
