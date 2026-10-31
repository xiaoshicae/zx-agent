---
paths:
  - "**/*.rs"
---

# 06 测试

测试原则和分层见 CLAUDE.md 的"测试"一节。这里是写测试时用到的设施和约定。

## 测试设施

| 设施 | 位置 | 用途 |
|---|---|---|
| `FauxProvider` / `FauxStep` | `src/ai/faux.rs` | 按脚本返回文本、工具调用、截断、失败（`Fail`）、挂起直到取消（`Hang`），并记录每个请求 |
| `EchoTool` | `tests/common/mod.rs` | 可设置延迟、失败、panic、terminate，执行时还能注入引导消息或后续消息 |
| `Collect` | `tests/common/mod.rs` | 收集事件；`kinds()` 返回事件类型序列，`tool_end_order()` 返回工具完成的顺序 |
| `summarize` | `tests/common/mod.rs` | 把消息列表压缩成 `"tool:c1=hi"` 这样的短字符串 |
| `run_prompt` | `tests/common/mod.rs` | 用默认参数（10 轮、不取消）跑一次 `run_loop`，返回结果和事件 |
| `RunnerSetup` | `tests/common/mod.rs` | 带测试默认值（不重试、默认上限、内存会话）的 runner 构造器，用结构体更新语法覆盖字段 |
| `fast_retry` / `header` / `read_lines` | `tests/common/mod.rs` | 毫秒级退避的重试策略、会话头部、读取会话文件的每一行 |
| `CancelOnFirstDelta` | `tests/common/mod.rs` | 收到第一段流式增量就取消，测试中途取消而不依赖计时 |
| `MockServer` / `Reply` | `tests/common/mock_server.rs` | 本地假 HTTP 服务：返回 SSE 流、HTTP 错误，或发出部分数据后卡住（`Stall`）；记录请求，`wait_for_requests` 等到请求到达 |
| `FailingWriter` / `SessionStore::with_writer` | `src/session.rs`（仅测试编译） | 让会话写入失败，测试不变量 5 和 7 |

典型写法：循环测试参照 `tests/agent_loop.rs` 的 `runs_tools_until_the_model_answers`，用 `run_prompt` 跑完后断言 `result.outcome`，再用 `summarize(&context)` 断言消息序列；运行层测试参照 `tests/runner.rs`，写成 `RunnerSetup { retry: fast_retry(3), ..setup(&provider) }.build()`。

## 测试文件

| 文件 | 内容 |
|---|---|
| `src/**` 中的 `mod tests` | 各模块的单元测试，包括会话文件和事件流的 JSON 格式 |
| `tests/agent_loop.rs` | 循环语义 |
| `tests/runner.rs` | 运行层：重试、上限、截断续写、持久化、恢复会话 |
| `tests/openai_mock.rs` | 真实 HTTP 下的 provider：SSE、重试、空闲超时、中途取消 |
| `tests/cli.rs` | 编译出的程序：退出码、输出格式、`main.rs` 中的接线 |
| `tests/hooks.rs`、`tests/hooks_cli.rs` | 六个钩子、审批与失败回退、变换持久化、已知密钥脱敏接线 |
| `tests/real_api.rs` | 付费：真实模型 API，默认跳过 |

只能在 Unix 上运行的测试（如进程组、信号相关的）加 `#[cfg(unix)]`。

## 写付费的真实 API 测试

运行它们需要用户授权（红线 9）。新增时遵守：

- 只放在 `tests/real_api.rs`。每个测试标 `#[ignore = "..."]`，并在发出请求前调用 `Endpoint::from_env()`，它在缺少 `ZX_E2E_AUTHORIZED=1` 时直接失败。
- 不写死服务商：地址、模型、key 所在的变量名都从环境变量读取。只用合成的提示词和临时目录里的文件。
- 限制输出 token、轮次和时间，用 `print_usage` 打印用量；请求授权时说明大致用量。
- 断言只依赖稳定的事实（结果类型、工具是否运行、关键词），不依赖模型的措辞。
