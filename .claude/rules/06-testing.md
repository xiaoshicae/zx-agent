---
paths:
  - "**/*.rs"
---

# 06 测试

每个测试文件覆盖什么见 `docs/TESTS.md`；测试设施在 `crates/zx-testing`，索引在它 `src/lib.rs` 的模块文档。

## 原则

- **测行为，不测实现。** 断言外部可观察的结果：返回值、事件顺序、工具结果、会话文件内容、退出码。
- **一个测试只验证一种行为**，测试名说清楚是什么行为（`retries_transient_errors_without_repeating_the_prompt`）。
- **修 bug 先写回归测试**，并确认它在修复前失败。
- **新功能至少覆盖**：成功路径、每一类失败、取消与超时、边界（空输入、刚好截断、多字节字符被切开）。
- **确定、隔离、快速。** 不访问网络（付费 e2e 除外）；不依赖执行顺序和全局状态；不用 `std::env::set_var`；文件只写到 `tempfile::tempdir()`；不用 `sleep` 等结果，而是等待对应的事件或请求。
- **放在哪里**：单元测试在被测的模块里（规则 09）。集成测试放在被测的 crate 里：只用库的放在那个库的 crate，用到几个库时放在最上层的那个（如 `crates/zx-app/tests/otel.rs`）；运行程序的放在那个程序的 crate（`zx-agent` 在 `crates/zx-cli/tests/`，`zx-im` 在 `crates/zx-im/tests/`）。付费测试只在 `crates/zx-agent/tests/real_api.rs`。几个 crate 共用的设施放进 `zx-testing`，只有一个 crate 用的留在它的 `tests/common`，不复制粘贴。新文件在 `docs/TESTS.md` 登记一行。

## 约定

- 循环测试参照 `crates/zx-agent/tests/step_loop.rs` 的 `runs_tools_until_the_model_answers`：用 `run_prompt` 跑完后断言 `result.outcome`，再用 `summarize(&context)` 断言消息序列。运行层测试参照 `crates/zx-agent/tests/runner.rs`，写成 `RunnerSetup { retry: fast_retry(3), ..setup(&provider) }.build().await`。
- 驱动程序的测试经 `zx_agent_in` 或 `zx_im_in`（即 `zx_testing::program::command`）启动，开发者的配置与家目录进不了测试。权限、上下文、摘要、模型元数据、执行环境与 provider 设置没有命令行参数，经 `zx_testing::program` 的 `AgentHome` 与 `fake_model_config` 设置。
- 只能在 Unix 上运行的测试（如进程组、信号）加 `#[cfg(unix)]`。

## 付费的真实 API 测试

运行要用户授权（红线 9）。新增时：

- 每个测试标 `#[ignore = "..."]`，并在发出请求前调用 `Endpoint::from_env()`，它在缺少 `ZX_E2E_AUTHORIZED=1` 时直接失败。
- 不写死服务商：地址、模型、key 所在的变量名都从环境变量读取。只用合成的提示词和临时目录里的文件。
- 限制输出 token、模型请求数和时间，用 `print_usage` 打印用量；请求授权时说明大致用量。用量远高于其余测试的（如摘要评测）再要求一个专门的变量（`ZX_E2E_EVAL=1`），未设置时只打印跳过。
- 断言只依赖稳定的事实（结果类型、工具是否运行、关键词），不依赖模型的措辞。
