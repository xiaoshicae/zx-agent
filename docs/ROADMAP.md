# zx-agent 路线图

本文档记录 zx-agent 的设计目标、已定决策与分阶段计划，随实施进度更新。

- 现状与用法见 [README](../README.md)。
- 调研、选型论证和接口草图见[通用内核技术方案](zx-agent-general-kernel-proposal.html)。

## 1. 目标

把 zx-agent 从“面向无人值守排障的内核”演进为通用内核：同一个内核支撑三类场景，场景差异全部放在内核之外。

| 场景 | 例子 | 前端 |
|---|---|---|
| 人机对话 | 在终端里和 Agent 多轮对话，类似 pi | `chat` |
| 无人值守 | 由 cron、Kubernetes Job、告警触发的排障 | `run` |
| 被嵌入 | 在编辑器或其他平台中驱动 Agent | `acp`、Rust SDK |

| 编号 | 目标 | 验收标准 |
|---|---|---|
| G1 | 一个内核支撑三类场景 | 三个前端共用同一个 `AgentSession`；内核中不出现“当前是不是交互模式”的判断 |
| G2 | 不改内核即可适配新场景 | 新增一个场景只需新增 profile 文件、提示词或插件 |
| G3 | 保持可审计与安全 | `.claude/CLAUDE.md` 中的不变量全部保留；每次模型请求都能从会话文件还原 |
| G4 | 渐进迁移 | 保持现有 `run` 行为兼容，测试全部通过；D5 若改变默认执行环境，在 P6 明确迁移方式并单独验收 |

G1–G4 是验收目标。当前会话日志可重建对话与提示词；G3 所要求的完整请求重建还需补齐每次请求的工具定义与发送配置记录，实施缺口见技术方案第 10.2 节的风险项。

**本期不做**：pi 级别的完整 TUI、多 Agent 协作、插件热加载、WASM 插件、云端托管运行时。

## 2. 设计原则

1. **机制在内核，策略在外层**：内核只提供循环、会话、钩子和事件；上限、续写、提示词、能否问人都由 profile 决定。
2. **内核不直接面对人**：需要人的地方都经过 `Interaction` 接口；没有人时有确定的回退，即拒绝。
3. **会话日志是唯一事实来源**：模型可见的内容都先记录；原始历史永不改写，压缩和编辑都以追加条目表达。
4. **前端只消费事件、生产输入**：界面、日志、协议适配器都是事件的订阅者。
5. **扩展优先用标准协议**：工具扩展用 MCP，嵌入用 ACP。
6. **纵深防御**：规则（deny → ask → allow）、执行环境隔离（文件系统和网络）、不可信数据不改变控制流。
7. **简单优先，可回退**：没有第二个使用者就不抽象；每一步都能单独测试和回滚。

## 3. 已定决策

| 编号 | 事项 | 结论 | 日期 | 状态 |
|---|---|---|---|---|
| D1 | MSRV | 升级到 1.88 | 2026-10-31 | 已完成：`rust-version = "1.88"`，已用 1.88.0 工具链编译并通过测试 |
| D2 | 新依赖 | 已授权：tokio 的 `io-std` feature、`toml`、`rmcp`（MCP 官方 Rust SDK）、ACP 官方 Rust SDK、`rustyline`（可选）。在用到它们的阶段再引入 | 2026-10-31 | 按阶段引入 |
| D3 | profile 文件格式 | TOML | 2026-10-31 | P4 落实 |
| D4 | 会话 API 形态 | 进程内方法调用；暂不实现提交队列 / 事件队列（SQ/EQ） | 2026-10-31 | P2 落实 |
| D5 | 无人值守的命令执行环境 | 待定。候选方案：`troubleshoot` profile 默认在沙箱容器中执行命令，没有 Docker 时拒绝启动，除非显式指定在本机执行 | — | 待确认 |
| D6 | 项目定位 | 改为“通用内核 + 场景配置” | 2026-10-31 | 已完成：README、`.claude/CLAUDE.md` 已更新 |

D2 之外的新依赖、依赖升级，仍需先征得同意。

## 4. 阶段计划

2026-10-31 已完成一轮内核健壮性优化（本轮改动）：

| 项目 | 已实现 |
|---|---|
| 单一运行与取消 | `Runner::run(&mut self)`；丢弃运行 future 时取消子令牌，定时任务、输出读取任务和 Unix 进程组由 RAII 清理 |
| 文件读取 | 只接受普通文件，实际读取量受限，等待响应取消；大分页参数不会溢出 |
| 会话恢复 | 支持 UTF-8 字符中途截断；只修复未完成尾记录，完整损坏记录报错并保留；提供修复字节数 |
| provider 错误 | 可选 `ProviderError`；状态与错误码分类优先，支持数值 `Retry-After`，兼容旧错误文本与旧会话 |
| 流式事件 | 工具参数增量携带调用 ID；事件队列有界，响应与单个 SSE 事件有字节预算 |
| 验证 | 回归覆盖上述边界，原有 CLI 退出码与正常任务行为继续验证；新增资源限制有单独 CLI 用例 |

下面的 P0–P9 继续按各自验收标准推进。

状态取值：未开始、进行中、已完成。每个阶段完成时勾选任务，并记录完成日期和提交。

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 场景默认值移出内核 | 已完成 |
| P1 | 钩子、工具调用流水线与人机交互接口 | 未开始 |
| P2 | 长期会话 `AgentSession` | 未开始 |
| P3 | 终端对话 `chat` | 未开始 |
| P4 | profile 文件、上下文文件与 skills | 未开始 |
| P5 | 上下文管理 | 未开始 |
| P6 | 执行环境与崩溃恢复 | 未开始 |
| P7 | 插件与 MCP | 未开始 |
| P8 | 嵌入协议 `acp` | 未开始 |
| P9 | 可选增强 | 未开始 |

### P0 场景默认值移出内核

目标：内核的默认值保持中性，无人值守的取值集中到一个预设里。不引入新依赖。

实现约定：`Limits::max_turns` 与 `run_loop` 的轮次预算使用 `Option<u32>`；`None` 表示不限，`Some(0)` 表示不调用模型。`run --max-turns 0` 保留原有的零轮次语义。预设先放在 Rust 模块 `presets::troubleshoot`，提示词组装与运行机制不依赖该模块。

- [x] 内置说明 `DEFAULT_INSTRUCTIONS` 移到 `troubleshoot` 预设（先用 Rust 代码表达，P4 改为文件）
- [x] `Limits::default()` 改为不限轮次、不限时间、不续写；`troubleshoot` 预设设置 30 轮、15 分钟、续写 1 次
- [x] `run` 命令默认使用 `troubleshoot` 预设

完成日期：2026-10-31。实施提交：[50a2773](https://github.com/xiaoshicae/zx-agent/commit/50a2773e8cb1c030c2bef5ae0df29b616457d378)。
本地 macOS 验证：stable 与 Rust 1.88 各 160 项测试通过、6 项付费 API 测试跳过；格式、clippy、全目标构建、MSRV 检查与严格 Rustdoc 构建通过。
`tests/cli.rs`、`run --help` 和排障说明字符串均与实施前逐字节一致；未新增依赖。

验收：CLI 测试不改即通过；新增测试覆盖内核默认值。

### P1 钩子、工具调用流水线与人机交互接口

目标：扩展点到位，现有的策略检查和截断续写改为钩子实现。不引入新依赖。

- [ ] `Hooks` trait 与钩子链：`on_input`、`transform_context`、`before_tool`、`after_tool`、`finish_turn`、`on_event`；可失败的钩子通过 `Result` 返回错误
- [ ] 组合规则：按注册顺序执行；`before_tool` 取最严格结果（Deny > Ask > Allow）；策略类钩子出错按拒绝处理；每个钩子有超时
- [ ] `Decision` 增加 `Ask`；规则求值顺序 deny → ask → allow，首个匹配生效
- [ ] `Interaction` trait 与 `NoHuman`（Ask 等同拒绝）
- [ ] `CommandPolicy` 与截断续写迁移为钩子；`run_loop` 参数整理（静态配置与钩子分开）
- [ ] 脱敏扩展：`after_tool` 中去掉密钥和令牌（原路线图“执行后钩子”）
- [ ] 输入与上下文变换先以追加消息或 `context_edit` 记录，写入成功后再请求模型；P5 加入压缩时沿用这一约束

验收：无人值守行为不变；新增钩子组合、出错时拒绝、超时的测试。

### P2 长期会话 `AgentSession`

目标：现有 `Runner` 已支持顺序多次运行并保留会话；在此基础上提供统一的 `AgentSession` API、多订阅者、模型切换和共享调用互斥。不引入新依赖。

- [ ] `prompt`、`steer`、`follow_up`、`abort`、`subscribe`、`set_model`
- [ ] 同一时刻只有一个运行；运行中 `prompt` 返回忙碌错误，插话须显式调用 `steer` 或 `follow_up`
- [ ] `abort` 只取消当前运行，会话保留；多个订阅者都收到事件
- [ ] `prompt` 返回 `Result<RunReport, SessionError>`；无人值守仍可一行调用：`session.prompt(task).await?` 取得 `RunReport`

验收：中止后会话可继续使用；多订阅者测试；运行中提问的测试。

### P3 终端对话 `chat`

目标：最小可用的终端对话。依赖：tokio 的 `io-std` feature；可选 `rustyline`（D2 已授权）。

- [ ] 流式显示回答与工具调用
- [ ] `TerminalInteraction`：展示完整命令、依据与影响范围，可选“允许一次 / 本会话允许 / 拒绝”
- [ ] 斜杠命令：`/new`、`/model`、`/compact`（P5 后生效）、`/quit`
- [ ] Ctrl+C 只中止当前运行，不退出程序

验收：用脚本化的 stdin 驱动 CLI 测试；人工试用。

### P4 profile 文件、上下文文件与 skills

目标：不写代码即可定义场景。依赖：`toml`（D2 已授权；上游 [Cargo.toml](https://github.com/toml-rs/toml/blob/main/Cargo.toml) 在 2026-10-31 核对的 MSRV 为 1.85，接入时按选定版本复核）。

- [ ] profile 文件格式（TOML）：说明、工具、规则、上限、扩展、执行环境
- [ ] 叠加顺序：内置默认 < profile 文件 < 命令行参数；deny 规则只增不减
- [ ] 内置两个 profile：`troubleshoot`（P0 的预设）与 `assistant`（通用对话）
- [ ] 读取 AGENTS.md 等上下文文件；skills 按需加载
- [ ] 命令行增加 `--profile`；配置错误时退出码 2

验收：两个内置 profile 均由文件定义；配置错误的测试。

### P5 上下文管理

目标：长对话不超出上下文窗口，且不丢失关键信息（原路线图“上下文压缩”）。不引入新依赖。

- [ ] 会话条目 `compaction`：记录保留的第一条条目与摘要（沿用 pi 的结构）
- [ ] 首次写入 `compaction` 前升级会话格式，兼容读取旧版本；版本号按当时的格式递增，不延后到 P6
- [ ] 摘要分“环境事实（附原始条目出处）”与“推断和计划”两部分；原始历史不改写
- [ ] 触发方式：达到阈值自动触发，或在 `chat` 中用 `/compact` 手动触发；无人值守的短任务默认关闭
- [ ] 工具侧预算：大结果写入文件，只返回路径

验收：faux provider 测试；经授权后运行付费 e2e 测试。

### P6 执行环境与崩溃恢复

目标：命令可以放进隔离环境执行；进程崩溃后能正确恢复。依赖：无（调用本机 `docker` 命令）；D5 确认后定稿。

- [ ] `Executor` 接缝：`LocalExecutor`（现有行为）与 `ContainerExecutor`
- [ ] `ContainerExecutor`：每次运行创建一个沙箱容器（网络默认拒绝，在线任务按评审方案限制访问范围；只读根文件系统、非 root、去掉全部 capability、进程数与内存上限）；每条命令通过 `docker exec` 执行；`read_file` 也在容器内读取；运行结束删除容器
- [ ] API key 和会话文件只留在 Agent 进程，不进入容器；工具需要的凭据以只读方式挂载
- [ ] 列出在线排障所需的 API 地址、网络与凭据范围；无网络的容器只用于离线排障，在线场景的受限访问方案在实施前评审
- [ ] `Tool` trait 增加 `effects()`：只读或有副作用
- [ ] 有副作用的工具执行前追加 `tool_call_started` 条目；恢复时把“已开始、无结果”的调用标记为结果未知，不自动重放
- [ ] 首次写入 `tool_call_started` 前再次升级会话格式，兼容读取 P5 及更早版本

验收：离线任务在无网络的容器中执行命令；在线任务按已评审的网络与凭据范围访问目标 API；崩溃恢复测试。D5 若改变 `run` 的默认执行环境，补充迁移说明和相应 CLI 测试。

### P7 插件与 MCP

目标：第三方能力可以不改内核接入（原路线图“MCP 客户端，以及只读的领域工具”）。依赖：`rmcp`（D2 已授权，要求 MSRV 1.88）。

- [ ] 公开 `Extension` trait：注册工具、钩子、提示词段落、命令
- [ ] MCP 客户端扩展：支持 stdio 与 Streamable HTTP 两种传输
- [ ] MCP 工具同样受规则约束；MCP 服务器需要显式信任；无人值守默认不启用外部 MCP
- [ ] 只读的领域工具（如 Kubernetes、Prometheus 查询）以扩展或 MCP 服务器提供

验收：MCP 工具可以列出、调用，并受规则约束。

### P8 嵌入协议 `acp`

目标：可被编辑器和其他平台驱动。依赖：ACP 官方 Rust SDK（D2 已授权，要求 MSRV 1.88）。

- [ ] `acp` 子命令：通过 stdio 收发 ACP 消息
- [ ] `AcpInteraction`：审批请求转发给客户端

验收：可被 ACP 客户端驱动，审批经客户端完成。

### P9 可选增强

按需求排序，每项单独评估：

- [ ] 结构化最终报告：`--output-schema`，按 JSON Schema 约束最终回答
- [ ] 更多 provider：例如 Anthropic Messages API
- [ ] 异步审批：`WebhookInteraction`，高风险命令发 webhook 请求批准，超时则拒绝
- [ ] OS 级沙箱的 Executor：Linux 的 bubblewrap + seccomp、macOS 的 Seatbelt
- [ ] 由 webhook 或告警触发运行
- [ ] OpenTelemetry 追踪
- [ ] 拆分为多个 crate（`zx-ai`、`zx-core`、`zx-tools`、`zx-cli`），用编译器保证分层

## 5. 随各阶段处理的整理项

这些整理不改变行为，在改到相关代码时顺带完成：

- [ ] `run_loop` 参数过多（P1）
- [ ] 工具名字符串散落在多个文件（P1 或 P4）
- [ ] `clip` 有两份实现（任意阶段）
- [ ] `BashTool::execute` 过长（P6）

## 6. 推进方式

- 每个阶段可以拆成多个提交；完成后在本文档中勾选任务、更新状态，并记录日期和提交。
- 每个阶段结束时：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 通过；用到新语法或新标准库 API 时，`cargo +1.88.0 check --all-targets` 也要通过。
- 从 P0 起，`tests/cli.rs` 中 `run` 的测试不改即通过，以此保证兼容性；D5 若改变默认执行环境，仅在 P6 按明确的迁移规则更新相关测试，其余行为仍须保持兼容。
- 需要新依赖时，先确认在 D2 的授权范围内；范围之外的要先征得同意。
- 设计有变化时，先更新本文档，再改代码。

---

ROADMAP.md · v0.1 · 2026年10月
