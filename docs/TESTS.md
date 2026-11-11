# 测试文件一览

每个测试文件覆盖什么，具体行为见测试名。设施与约定见 `.claude/rules/06-testing.md`。

单元测试写在被测模块里或其 `tests.rs` 中，覆盖各模块的逻辑与边界，包括会话文件与事件的 JSON 格式；`config.toml` 与 agent 文件（含 `deploy/kubernetes/agents/*.md`）的配置规则也在这一层测。

| 文件 | 内容 |
|---|---|
| `tests/agent_loop.rs` | 循环语义：工具往返与顺序、失败、截断、上限、队列 |
| `tests/runner.rs` | 运行层：重试、上限与预算、续写、持久化、恢复会话 |
| `tests/agent_session.rs` | 共享会话：Busy、取消、订阅、模型与推理强度切换、起名 |
| `tests/openai_mock.rs` | 真实 HTTP 下的 provider：SSE、重试、超时、取消、错误正文 |
| `tests/prepare_request.rs` | `Hooks::prepare_request` 对单次请求的替换 |
| `tests/continue_run.rs` | 不加新输入的续跑 |
| `tests/overflow_recovery.rs` | 上下文过长后摘要并开始新的尝试 |
| `tests/run_cli.rs` | `run`：退出码 0 到 9、jsonl 输出、追踪导出、请求设置 |
| `tests/otel.rs` | OpenTelemetry 的 span 层级、属性与 traceparent |
| `tests/model_meta.rs` | 模型单价、成本与 token 阈值 |
| `tests/extension_capabilities.rs` | 扩展在运行中发消息、记状态、改工具集与命令 |
| `tests/custom_records.rs` | 扩展的 `custom` 与 `custom_message` 记录 |
| `tests/session_branch.rs` | `AgentSession::branch` 切换分支 |
| `tests/session_fork.rs` | 复制分支成新会话 |
| `tests/sessions_cli.rs` | chat 的会话目录、`--resume` 与 `zx-agent sessions` |
| `tests/chat_cli.rs` | chat：审批、输出行、信号、PTY |
| `tests/acp_cli.rs` | `acp` 经 stdio 的协议、权限请求、会话加载与 `/name` |
| `tests/agents_cli.rs` | agent 文件、权限、上下文与 skill；配置错误以退出码 2 结束 |
| `tests/config_cli.rs` | `config.toml` 的位置与优先级 |
| `tests/session_compaction.rs` | 会话层的摘要记录、引用与校验 |
| `tests/compaction.rs` | 宿主的手动与自动摘要、计费、失败与上限 |
| `tests/request_audit.rs` | 从会话文件还原请求（G3），含固定会话的逐字节比对 |
| `tests/executor.rs` | 工具只经执行器运行命令与读写文件 |
| `tests/search_tools.rs` | grep、find、ls |
| `tests/file_tools.rs` | edit_file 与 write_file |
| `tests/container.rs` | 容器执行；`real_` 测试在设置 `ZX_DOCKER_TEST_IMAGE` 时用真实 Docker |
| `tests/crash_recovery.rs` | 崩溃后补写中断调用的结果且不重放 |
| `tests/output_store.rs` | 长结果落盘与分页 |
| `tests/hooks.rs` | 钩子、审批、策略事件与脱敏 |
| `tests/hooks_cli.rs` | 程序中的密钥脱敏与钩子失败输出 |
| `tests/hook_contract.rs` | 钩子输入输出的 JSON 契约 |
| `tests/extensions.rs` | `extensions::register` 的注册与拒绝 |
| `tests/mcp.rs` | MCP 客户端对假服务器 |
| `tests/mcp_cli.rs` | `[[mcp]]` 接入 run、chat 与 im |
| `tests/im_feishu.rs` | ⚑ D:5/CHINA FLAG: 飞书 channel 对假开放平台 |
| `tests/im_slack.rs` | Slack channel 对假 Slack |
| `tests/im_cli.rs` | `im` 子命令的配置与完整流程 |
| `tests/im_server.rs` | IM 服务的会话划分、接话、审批与停止 |
| `tests/real_api.rs` | 付费的真实模型 API，默认跳过（红线 9） |

---

TESTS.md · v0.2 · 2026年11月
