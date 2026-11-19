# 测试文件一览

每个测试文件覆盖什么，具体行为见测试名。原则与放置位置见 `.claude/rules/06-testing.md`，测试设施的索引在 `crates/zx-testing/src/lib.rs` 的模块文档。

会话文件与事件的 JSON 格式，以及 `config.toml` 与 agent 文件的配置规则（含 `deploy/kubernetes/agents/*.md` 与 `examples/agents/review.md`），也由各模块的单元测试覆盖。集成测试按 crate 分节列在下面，都可以在仓库根目录按文件名运行，如 `cargo test --test step_loop`、`cargo test --test run_cli`。

脚本化的模型 `FauxProvider` 只在 `zx-ai` 的 feature `testing` 打开时编译：`zx-core` 与 `zx-agent` 的测试依赖经 `zx-ai` 打开它，`zx-app`、`zx-cli` 与 `zx-im` 的经 `zx-agent`；测试设施 `zx-testing` 不打开它，所以在仓库根目录构建发布版时它不在其中。`crates/zx-cli/tests/common/` 与 `crates/zx-im/tests/common/` 里是只有本 crate 的测试用到的辅助，内容与理由见两处 `mod.rs` 的模块文档。

## 只用库：`crates/zx-agent/tests/`

属于库 `zx-agent`，经它的公开路径测内核、内置工具与 provider。

| 文件 | 内容 |
|---|---|
| `crates/zx-agent/tests/step_loop.rs` | 循环语义：工具往返与顺序、失败、截断、上限、取消、队列；系统提示词的记录、参数预处理，工具的进度、详情与用量 |
| `crates/zx-agent/tests/runner.rs` | 运行层：重试、上限与预算、续写、持久化、恢复会话；事件的编号与信封 |
| `crates/zx-agent/tests/session.rs` | 共享会话：Busy、取消、`Run`（事件顺序、读才推进、丢弃即取消、多个句柄）、提问的 hold 在写入会话后才释放、插话与后续消息、`take_queued`、经会话回答审批（按事件给出的审批编号；超时后迟到的回答落空）、模型与推理强度切换、起名、记录写不进去时自行关闭 |
| `crates/zx-agent/tests/openai_mock.rs` | 真实 HTTP 下的 provider：SSE、失败的归类与重试、超时、取消、错误正文；请求头与 API key 的来源、缓存键与输出上限的字段 |
| `crates/zx-agent/tests/anthropic_mock.rs` | 真实 HTTP 下的 Anthropic provider：带思考与签名的工具往返、请求头与请求体、含缓存的用量；思考块属于另一段对话时去掉它重发一次；过载与拒绝的归类 |
| `crates/zx-agent/tests/prepare_request.rs` | `Hook::prepare_request` 对单次请求的替换 |
| `crates/zx-agent/tests/continue_run.rs` | 不加新输入的续跑 |
| `crates/zx-agent/tests/overflow_recovery.rs` | 上下文过长后摘要并开始新的尝试 |
| `crates/zx-agent/tests/model_meta.rs` | 模型单价、成本与 token 阈值（随窗口的阈值跟着切换后的模型）；缓存键；模型切换、请求钩子、摘要与溢出恢复的费用预算校验 |
| `crates/zx-agent/tests/extension_capabilities.rs` | 扩展在运行中发消息、记状态、改工具集与命令；排队输入一次取几条 |
| `crates/zx-agent/tests/custom_records.rs` | 扩展的 `custom` 与 `custom_message` 记录 |
| `crates/zx-agent/tests/session_branch.rs` | `Session::branch` 切换分支 |
| `crates/zx-agent/tests/session_fork.rs` | 复制分支成新会话 |
| `crates/zx-agent/tests/session_storage.rs` | 宿主自己实现的 `SessionStorage`：`Agent` 在其中创建会话、按标签列出、由另一个 `Agent` 续接，以及唯一键拦住第二个写入者 |
| `crates/zx-agent/tests/session_compaction.rs` | 存储中的摘要记录：会话文件拒绝读不了的记录且不改动文件；从摘要留下的会话续跑，上下文计划结束了摘要仍修改它点名的消息。摘要记录本身与投影在 `zx-core` 的单元测试 |
| `crates/zx-agent/tests/compaction.rs` | 宿主的手动与自动摘要：计费、失败、上限、超时与取消 |
| `crates/zx-agent/tests/summarizer.rs` | 宿主自己的 `Summarizer`：产出摘要而不请求模型、开始记录写它的名字、失败、引用校验、超时、取消与 panic |
| `crates/zx-agent/tests/actors.rs` | 会话标签与输入的 actor：策略与钩子看到什么、换人说话、不带输入的续跑、记入会话 |
| `crates/zx-agent/tests/request_audit.rs` | 从会话文件还原请求（G3），含固定会话的逐字节比对；分支配置索引的校验在 `zx-core` 的单元测试 |
| `crates/zx-agent/tests/executor.rs` | 工具只经执行器运行命令与读写文件；本机执行器用 bash 运行命令，命令看不到隐藏的变量；执行器为提示词描述自己 |
| `crates/zx-agent/tests/search_tools.rs` | grep、find、ls |
| `crates/zx-agent/tests/file_mutations.rs` | 共用队列的两组工具协调同一文件的修改；调用被丢弃后，文件锁到写完为止 |
| `crates/zx-agent/tests/file_tools.rs` | edit_file 与 write_file；调用被丢弃时文件仍写完整 |
| `crates/zx-agent/tests/output_store.rs` | 长结果先脱敏再落盘（留下第一页）与分页；不经符号链接读写 |
| `crates/zx-agent/tests/hooks.rs` | 钩子、审批、策略事件与脱敏；`Agent::new` 拒绝的工具 |
| `crates/zx-agent/tests/hook_contract.rs` | 钩子输入输出的 JSON 契约 |
| `crates/zx-agent/tests/extensions.rs` | `AgentConfig::extensions`：扩展给会话加了什么，哪些扩展让 `Agent::new` 拒绝 |
| `crates/zx-agent/tests/mcp.rs` | MCP 客户端对假服务器：两代协议、工具的选择与命名、调用结果、错误与超时；运行被取消时通知服务器放弃调用 |
| `crates/zx-agent/tests/real_api.rs` | 付费的真实模型 API，默认跳过（红线 9） |

## 应用层：`crates/zx-app/tests/`

属于应用层 `zx-app`，只经它的公开接口测。

| 文件 | 内容 |
|---|---|
| `crates/zx-app/tests/application.rs` | 非 CLI 宿主的显式配置装配（与配置文件同样检查）、会话创建与续接、模型/预算校验、密钥文件与启动提示；宿主加的工具、钩子、策略、扩展与摘要器与自带的同等对待，名字重复时 `build` 报错；`provider.api` 与 `execution` 选用宿主注册的或内置的实现，名字未注册或重复时 `build` 报错，`approveAll` 要求隔离命令的执行环境 |
| `crates/zx-app/tests/otel.rs` | OpenTelemetry 的 span 层级、属性与 traceparent |

## 驱动程序：`crates/zx-cli/tests/`

属于命令行 `zx-cli`：运行编译出的 `zx-agent`。`container.rs` 与 `crash_recovery.rs` 里还有只用库的测试，待拆分（`docs/ROADMAP.md` 第 5 节）。

| 文件 | 内容 |
|---|---|
| `crates/zx-cli/tests/run_cli.rs` | `run`：退出码 0 到 9、jsonl 输出、追踪导出、请求设置、续接的会话在原目录工作 |
| `crates/zx-cli/tests/chat_cli.rs` | chat：审批、斜杠命令、摘要、输出行、信号、PTY |
| `crates/zx-cli/tests/sessions_cli.rs` | chat 的会话目录、`--resume` 与 `zx-agent sessions` |
| `crates/zx-cli/tests/acp_cli.rs` | `acp` 经 stdio 的协议、权限请求、取消、会话加载（摘要之后仍回放整段对话）、续接、列出与关闭、`/name` 与 `/compact`、终止信号 |
| `crates/zx-cli/tests/agents_cli.rs` | agent 文件、权限、上下文、skill 与工具结果落盘；配置错误以退出码 2 结束 |
| `crates/zx-cli/tests/config_cli.rs` | `config.toml` 的位置与优先级 |
| `crates/zx-cli/tests/hooks_cli.rs` | 程序中的密钥脱敏与钩子失败输出 |
| `crates/zx-cli/tests/mcp_cli.rs` | `[[mcp]]` 接入 run 与 chat |
| `crates/zx-cli/tests/container.rs` | 容器执行；`real_` 测试在设置 `ZX_DOCKER_TEST_IMAGE` 时用真实 Docker |
| `crates/zx-cli/tests/crash_recovery.rs` | 崩溃后补写中断调用的结果且不重放 |

## IM 服务：`crates/zx-im/tests/`

属于程序 `zx-im`：服务与平台经它的库测，程序运行编译出的 `zx-im`。⚑ D:5/CHINA FLAG: 飞书 channel 的测试要 feature `feishu`（`[[test]]` 的 `required-features`），`cargo test -p zx-im --no-default-features` 时不编译。

| 文件 | 内容 |
|---|---|
| `crates/zx-im/tests/im_server.rs` | IM 服务：会话划分与接话、`/new`、`/cancel` 与 `/compact`、审批、运行名额与插话、待处理消息的容量、空闲释放与停止（没处理的消息留在 backlog）、启动时的会话索引与先读线程再建会话（之前没有会话时听到的话不另记）、回答替换不了状态时另发、重启后处理 backlog 里留下的消息与替换遗留的状态、处理完的消息再投递时不再回答 |
| `crates/zx-im/tests/im_slack.rs` | Slack channel 对假 Slack；存不进 backlog 的消息不确认投递；其他机器人的消息只听不答，自己的不听 |
| ⚑ D:5/CHINA FLAG: `crates/zx-im/tests/im_feishu.rs` | 飞书 channel 对假开放平台；存不进 backlog 的消息不确认投递 |
| `crates/zx-im/tests/im_cli.rs` | 程序 `zx-im`：配置错误（⚑ D:5/CHINA FLAG: 不带 feature `feishu` 的构建按名字拒绝 `[feishu]` 段）、开始服务前连上 MCP 服务器、完整流程、提示词中说明对话怎样送达的一段、被 SIGKILL 结束后再启动时回答它没处理完的消息（仅 Unix） |

---

TESTS.md · v0.20 · 2026年11月
