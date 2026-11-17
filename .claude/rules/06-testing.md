---
paths:
  - "**/*.rs"
---

# 06 测试

测试原则见 CLAUDE.md 的"测试"一节，每个测试文件覆盖什么见 `docs/TESTS.md`。这里是写测试时用到的设施和约定。

## 测试设施

每一项的用法写在它自己的文档注释里，这里只是索引。只用库的测试（根目录 `tests/`）用 `tests/common/`；驱动程序的测试（`crates/zx-cli/tests/`）用 `crates/zx-cli/tests/common/mod.rs`，它经 `#[path]` 把 `tests/common/` 整个带进来再导出，所以下面的设施在那里都能用 `common::` 取到。

| 位置 | 设施 |
|---|---|
| `crates/zx-ai/src/faux.rs` | `FauxProvider` / `FauxStep`：按脚本返回文本、工具调用、截断、失败（`Fail`）、挂起直到取消（`Hang`），或按请求内容现场决定（`compute`），并记录每个请求 |
| `tests/common/mod.rs` | 驱动会话：`RunnerSetup`（带测试默认值的构造器：`config`、`retry`、`limits`、`compaction`、`session`、`approver`）给出 `Driven`，它解引用为 `Session`，`run`、`prompt`、`continue_run`、`compact` 把一次运行读到结束，`session()` 给出记录的只读副本，取消就调用会话自己的 `cancel()`；简写 `run_prompt`、`run_prompt_with`、`run_approved`；`Approver` / `Asked` 回答审批请求。测 `Run` 本身时直接用 `Session::send`。替身：`EchoTool`、`NotifyingProvider`（请求开始时通知，用来等"运行中"）。观察：`Collect`（`kinds()`、`events()`、`envelopes()`、`steps()`）、`summarize`、`replayed_requests`。会话：`header` / `header_in`、`create_session_file` / `open_session_file` / `read_session_file`、`seeded_session` / `summary_json`、`tool_call_answer` / `answer_reporting`、拒绝一切记录、会话随即自行关闭的 `RefusingLog`。其他：`config`、`refusal`、`fast_retry`、`test_limits`、`read_lines` / `json_lines`、`stdout` / `stderr`、`signal` |
| `tests/common/` 的其他文件 | `mock_server.rs`（`MockServer` / `Reply`：本地假模型服务，SSE、HTTP 错误、发出部分数据后卡住 `Stall`、等通知后结束 `Gate`，`wait_for_requests`）、`fake_mcp.rs`（`FakeMcp`，按规范手写，不用 rmcp）、`fake_slack.rs`（`FakeSlack`）、`fake_feishu.rs`（`FakeFeishu`，假 ⚑ D:5/CHINA FLAG: 飞书开放平台）、`im.rs`（`run_channel`）、`http.rs`（读与答一次 HTTP 请求；接受连接却从不应答的 `silent_listener`）、`summary_eval.rs`（摘要评测，付费评测与常规测试共用） |
| `crates/zx-cli/tests/common/mod.rs` | 编译出的程序：`zx_agent()`（指向空的主目录，去掉 `ZX_MODEL`、`ZX_BASE_URL`、`OPENAI_API_KEY`）、`zx_agent_in` 与 `Home`；测试自己的主目录 `AgentHome`、`write_config`、`write_agent`、`fake_model_config`；会话文件 `session_dir`、`session_files`、`seed_session`；Unix 伪终端 `pty` |
| `crates/zx-core/src/transcript/test_support.rs`（仅测试编译） | transcript 各子模块的单元测试共用：让会话写入失败的 `FailingWriter`，在阻塞线程上把记录写给一个 writer 的 `WriterLog`（`Transcript::with_writer` 用它，不变量 5 和 7），以及由记录构造 `LoadedSession` 的辅助 |
| `crates/zx-tools/src/jsonl/test_support.rs`（仅测试编译） | 会话文件各测试共用的头部、回答与投影文本 |
| `crates/zx-core/src/transcript/memory.rs` | `MemoryStorage`：要"存储"而不要文件的测试用它 |

典型写法：循环测试参照 `tests/step_loop.rs` 的 `runs_tools_until_the_model_answers`，用 `run_prompt` 跑完后断言 `result.outcome`，再用 `summarize(&context)` 断言消息序列；运行层测试参照 `tests/runner.rs`，写成 `RunnerSetup { retry: fast_retry(3), ..setup(&provider) }.build()`。

## 约定

- 只能在 Unix 上运行的测试（如进程组、信号相关的）加 `#[cfg(unix)]`。
- 驱动程序的测试经 `zx_agent()` 或 `zx_agent_in` 启动，开发者的配置与真实的家目录都进不了测试。权限、上下文、摘要、模型元数据、执行环境与 provider 设置没有命令行参数，经 `AgentHome` 与 `fake_model_config` 设置。
- 测试函数同样受 `too_many_lines` 约束，例外见规则 09。
- 新的集成测试按是否需要编译出的程序放在 `tests/` 或 `crates/zx-cli/tests/`（后者也放用到 `zx_cli::config`、`zx_cli::otel` 的），并在 `docs/TESTS.md` 里登记一行。

## 写付费的真实 API 测试

运行它们需要用户授权（红线 9）。新增时遵守：

- 只放在 `tests/real_api.rs`。每个测试标 `#[ignore = "..."]`，并在发出请求前调用 `Endpoint::from_env()`，它在缺少 `ZX_E2E_AUTHORIZED=1` 时直接失败。
- 不写死服务商：地址、模型、key 所在的变量名都从环境变量读取。只用合成的提示词和临时目录里的文件。
- 限制输出 token、模型请求数和时间，用 `print_usage` 打印用量；请求授权时说明大致用量。
- 用量远高于其余测试的（如摘要评测），再要求一个专门的变量（摘要评测为 `ZX_E2E_EVAL=1`），未设置时只打印跳过。
- 断言只依赖稳定的事实（结果类型、工具是否运行、关键词），不依赖模型的措辞。
