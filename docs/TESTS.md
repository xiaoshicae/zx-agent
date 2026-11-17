# 测试文件一览

每个测试文件覆盖什么，具体行为见测试名。测试设施的索引在 `tests/common/mod.rs` 的模块文档，原则与约定见 `.claude/rules/06-testing.md`。

单元测试写在被测模块里或其 `tests.rs` 中，覆盖各模块的逻辑与边界，包括会话文件与事件的 JSON 格式；`config.toml` 与 agent 文件（含 `deploy/kubernetes/agents/*.md`）的配置规则也在这一层测。

集成测试分三处，按用到的 crate 与是否需要编译出的程序划分。各处的测试都可以在仓库根目录按文件名运行，如 `cargo test --test step_loop`、`cargo test --test run_cli`。

## 只用库：`tests/`

属于库 `zx-agent`（根包），经它的公开路径测内核、内置工具与 provider。IM 不在这个库里：三个 `im_*` 测试直接用 `zx-im` 与平台 crate，它们只是根包的测试依赖。

| 文件 | 内容 |
|---|---|
| `tests/step_loop.rs` | 循环语义：工具往返与顺序、失败、截断、上限、队列 |
| `tests/runner.rs` | 运行层：重试、上限与预算、续写、持久化、恢复会话 |
| `tests/session.rs` | 共享会话：Busy、取消、`Run`（事件顺序、读才推进、丢弃即取消、多个句柄）、经会话回答审批（按事件给出的审批编号；超时后迟到的回答落空）、模型与推理强度切换、起名 |
| `tests/openai_mock.rs` | 真实 HTTP 下的 provider：SSE、重试、超时、取消、错误正文 |
| `tests/prepare_request.rs` | `Hook::prepare_request` 对单次请求的替换 |
| `tests/continue_run.rs` | 不加新输入的续跑 |
| `tests/overflow_recovery.rs` | 上下文过长后摘要并开始新的尝试 |
| `tests/model_meta.rs` | 模型单价、成本与 token 阈值；模型切换、请求钩子、摘要与溢出恢复的费用预算校验 |
| `tests/extension_capabilities.rs` | 扩展在运行中发消息、记状态、改工具集与命令 |
| `tests/custom_records.rs` | 扩展的 `custom` 与 `custom_message` 记录 |
| `tests/session_branch.rs` | `Session::branch` 切换分支 |
| `tests/session_fork.rs` | 复制分支成新会话 |
| `tests/session_storage.rs` | 宿主自己实现的 `SessionStorage`：`Agent` 在其中创建会话、按标签列出、由另一个 `Agent` 续接，以及唯一键拦住第二个写入者 |
| `tests/session_compaction.rs` | 会话层的摘要记录、引用与校验；回答上报的用量在摘要或修改之后是否还度量上下文 |
| `tests/compaction.rs` | 宿主的手动与自动摘要、计费、失败与上限 |
| `tests/summarizer.rs` | 宿主自己的 `Summarizer`：产出摘要而不请求模型、开始记录写它的名字、失败、引用校验、超时、取消与 panic |
| `tests/actors.rs` | 会话标签与输入的 actor：策略与钩子看到什么、换人说话、不带输入的续跑、记入会话 |
| `tests/request_audit.rs` | 从会话文件还原请求（G3），含固定会话的逐字节比对与分支配置索引校验 |
| `tests/executor.rs` | 工具只经执行器运行命令与读写文件 |
| `tests/search_tools.rs` | grep、find、ls |
| `tests/file_mutations.rs` | 文件锁跨会话共享；取消调用后仍保护后台写入，防止覆盖后续修改 |
| `tests/file_tools.rs` | edit_file 与 write_file；调用被丢弃时文件仍写完整 |
| `tests/output_store.rs` | 长结果落盘（留下第一页）与分页；不经符号链接读写 |
| `tests/hooks.rs` | 钩子、审批、策略事件与脱敏 |
| `tests/hook_contract.rs` | 钩子输入输出的 JSON 契约 |
| `tests/extensions.rs` | `AgentConfig::extensions`：扩展给会话加了什么，哪些扩展让 `Agent::new` 拒绝 |
| `tests/mcp.rs` | MCP 客户端对假服务器；运行被取消时通知服务器放弃调用 |
| `tests/im_feishu.rs` | ⚑ D:5/CHINA FLAG: 飞书 channel 对假开放平台 |
| `tests/im_slack.rs` | Slack channel 对假 Slack |
| `tests/im_server.rs` | IM 服务的会话划分、接话、`/new`、审批与停止；启动时的会话索引、先读线程再建会话、慢的会话准备不挡其他对话、运行名额（残留插话重新排队、闲聊等名额再打开会话）、`/cancel` 丢弃未读插话与待处理消息容量（因容量被拒的闲聊不回复） |
| `tests/real_api.rs` | 付费的真实模型 API，默认跳过（红线 9） |

## 应用层：`crates/zx-app/tests/`

属于应用层 `zx-app`，只经它的公开接口测，不用测试设施。

| 文件 | 内容 |
|---|---|
| `crates/zx-app/tests/application.rs` | 非 CLI 宿主的显式配置装配、会话创建与续接、模型/预算校验、密钥文件与启动提示 |

## 驱动程序：`crates/zx-cli/tests/`

属于命令行 `zx-cli`：运行编译出的 `zx-agent`，或既用 `zx-app` 又用测试设施（`tests/common` 依赖 IM 的 crate，`zx-app` 不依赖它们）。

| 文件 | 内容 |
|---|---|
| `crates/zx-cli/tests/run_cli.rs` | `run`：退出码 0 到 9、jsonl 输出、追踪导出、请求设置、续接的会话在原目录工作 |
| `crates/zx-cli/tests/chat_cli.rs` | chat：审批、输出行、信号、PTY |
| `crates/zx-cli/tests/sessions_cli.rs` | chat 的会话目录、`--resume` 与 `zx-agent sessions` |
| `crates/zx-cli/tests/acp_cli.rs` | `acp` 经 stdio 的协议、权限请求、取消、会话加载与关闭、`/name`、终止信号 |
| `crates/zx-cli/tests/agents_cli.rs` | agent 文件、权限、上下文与 skill；配置错误以退出码 2 结束 |
| `crates/zx-cli/tests/config_cli.rs` | `config.toml` 的位置与优先级 |
| `crates/zx-cli/tests/hooks_cli.rs` | 程序中的密钥脱敏与钩子失败输出 |
| `crates/zx-cli/tests/mcp_cli.rs` | `[[mcp]]` 接入 run、chat 与 im |
| `crates/zx-cli/tests/im_cli.rs` | `im` 子命令的配置与完整流程 |
| `crates/zx-cli/tests/container.rs` | 容器执行；`real_` 测试在设置 `ZX_DOCKER_TEST_IMAGE` 时用真实 Docker |
| `crates/zx-cli/tests/crash_recovery.rs` | 崩溃后补写中断调用的结果且不重放 |
| `crates/zx-cli/tests/otel.rs` | OpenTelemetry 的 span 层级、属性与 traceparent |

---

TESTS.md · v0.6 · 2026年11月
