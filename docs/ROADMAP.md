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

G1–G4 是验收目标。当前会话日志可重建对话与提示词；G3 所要求的完整请求重建（风险见技术方案第 10.2 节）已在 P5 补齐：每次请求前记录 `request_started`，工具定义与发送配置变化时记录 `request_config`。

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
| D4 | 会话 API 形态 | 进程内方法调用；暂不实现提交队列 / 事件队列（SQ/EQ） | 2026-10-31 | 已实现：P2 进程内共享会话 API |
| D5 | 无人值守的命令执行环境 | 默认仍在本机执行，`run` 行为不变；容器执行须在 profile 或 CLI 中显式开启。理由：在 Kubernetes Job 中运行时 Pod 内通常没有 Docker；无网络的容器做不了在线排障，而在线访问方案尚未评审。此前的候选方案（`troubleshoot` 默认容器、没有 Docker 时拒绝启动）不采用 | 2026-11-01 | 已确认：P6 执行环境部分按此实现 |
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

2026-11-01 复核 P0–P3 后的修正（已完成，PR #7）：

| 项目 | 修正 |
|---|---|
| `read_file` 长行 | 单行超过单页字节上限时只显示前缀，并注明该行被截断、已显示和总字节数，以及 read_file 不能在行内翻页。此前不作提示，或把被截断的行计为已显示 |
| chat 输入 | 超过 1 MiB 或不是 UTF-8 的行被整行丢弃并提示，对话和当前任务继续。此前整个 chat 以退出码 2 结束 |
| 会话写入 | 一次写入失败后不再写入，未写完的记录只会留在文件末尾，下次打开时按尾记录修复。此前后续条目可能接在半行之后，使文件无法再打开 |
| 密钥 | `OpenAiConfig` 的 Debug 输出不含 API key |
| 用量 | 服务端返回的用量按饱和加法累计，异常大的值不再导致溢出 |
| 测试 | 每项修正先补回归测试，并确认修复前失败；补充审批默认 120 秒的断言；改正一项名不副实的 chat 测试名 |

本地 macOS 验证（基于 `07a0b8d`，含 P4）：stable 与 Rust 1.88 各 314 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。本次修正未改动 `src/cli.rs`、`tests/cli.rs`、`Cargo.toml`、`Cargo.lock`；`run --help`、`chat --help` 与 `07a0b8d` 逐字节相同；未新增依赖。

2026-11-01 复核 P4 后的修正（已完成，PR #8）：

| 项目 | 修正 |
|---|---|
| 自动发现的上下文 | 自动发现的 AGENTS.md 解析后须位于 Git 根目录内（无 Git 根时为 cwd），否则报错，不发出请求。此前仓库中的符号链接可让程序在启动时读取仓库外的任意文件，并写进发给模型的系统提示词 |
| `run` 默认上下文 | `troubleshoot` 不再自动发现 AGENTS.md，恢复 P4 前 `run` 的行为；需要时用 `--context-file` 显式指定，或在 profile 中设置 `discover = true`。`chat` 使用的 `assistant` 不变 |
| `--system-prompt-file` | 恢复 P4 前的读取方式：可以是管道，不受 64 KiB 限制。64 KiB 上限只用于 profile 引用的文件 |
| 上下文报错 | 加载失败时提示 `--no-context` |
| 工具名 | 工具名集中为常量（第 5 节整理项） |

本地 macOS 验证（基于 `993350e`）：新增 9 项测试，其中 8 项回归测试在修复前失败；3 项既有测试显式开启 `discover = true`，断言不变。stable 与 Rust 1.88 各 323 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`run --help`、`chat --help` 与 `993350e` 逐字节相同；`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。

2026-11-01 复核 P5 后的修正（已完成，PR #11）：

| 项目 | 修正 |
|---|---|
| 结果目录越界 | 默认结果目录（相对工作目录）解析后须位于工作目录内；仓库中的符号链接不能把写入引到外部，越界时不写入。profile 显式配置的目录不受此限制。此前仓库可提交 `.zx-agent` 链接，使结果写到任意可写目录 |
| `run` 默认行为 | `troubleshoot` 默认不启用 `read_output`：工具集与结果内联恢复为 P5 前的行为，也不向工作目录写文件。`chat` 使用的 `assistant` 不变 |
| 写入失败 | 写入失败、超过单文件上限或目录越界时，保留已脱敏的有界结果内联并附原因，并删除写了一半的文件。此前返回 `Tool output withheld`，只读工作目录中所有大于阈值的结果都会丢失 |
| 仓库卫生 | 结果目录中写入内容为 `*` 的 `.gitignore`（已有则不覆盖），避免结果被提交到用户仓库。此前只有本仓库自己的 `.gitignore` 忽略该目录 |
| 自动摘要失败 | 付费的自动摘要请求失败后，本会话暂停自动摘要：之后需要摘要时直接报错并说明原因，不再发请求；手动压缩成功或切换模型后恢复。此前每次提问都会重新付费并以同样的方式失败 |
| 流程 | 新增 PR 模板，提醒在 PR 中回写阶段状态 |

既有测试的调整：两项结果落盘测试按新的写入失败契约改写（失败时保留内联结果，而不是 `Tool output withheld`），修改后在修复前失败；三项依赖 `troubleshoot` 默认启用 `read_output` 的测试改为显式启用，其中一项只统计 UUID 结果文件、不计入新增的 `.gitignore`；验证意图不变。

本地 macOS 验证（基于 `25e5b49`）：新增 6 项测试，其中 5 项回归测试在修复前失败，另 1 项验证手动压缩成功后恢复自动摘要。stable 与 Rust 1.88 各 391 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。另用本地假服务实测：默认 `run` 的工具集与结果内联同 P5 前一致；显式启用 `read_output` 时，只读目录与越界链接都保留内联结果、不写外部目录；结果目录中的 `.gitignore` 经 `git status` 验证生效。

下面的 P0–P9 继续按各自验收标准推进。

状态取值：未开始、进行中、已实现待 PR review、已完成。每个阶段完成时勾选任务，并记录完成日期和提交。

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 场景默认值移出内核 | 已完成 |
| P1 | 钩子、工具调用流水线与人机交互接口 | 已完成（PR #3） |
| P2 | 长期会话 `AgentSession` | 已完成（PR #4） |
| P3 | 终端对话 `chat` | 已完成（PR #5） |
| P4 | profile 文件、上下文文件与 skills | 已完成（PR #6） |
| P5 | 上下文管理 | 已完成（PR #9） |
| P6 | 执行环境与崩溃恢复 | 进行中：崩溃恢复（PR #12）、执行环境接缝（PR #13）、离线容器执行（PR #14）已完成；在线访问的网络与凭据范围待评审 |
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

- [x] `Hooks` trait 与钩子链：`on_input`、`transform_context`、`before_tool`、`after_tool`、`finish_turn`、`on_event`；可失败的钩子通过 `Result` 返回错误
- [x] 组合规则：按注册顺序执行；`before_tool` 取最严格结果（Deny > Ask > Allow）；策略类钩子出错按拒绝处理；每个钩子有超时
- [x] `Decision` 增加 `Ask`；规则求值顺序 deny → ask → allow，首个匹配生效
- [x] `Interaction` trait 与 `NoHuman`（Ask 等同拒绝）
- [x] `CommandPolicy` 与截断续写迁移为钩子；`run_loop` 参数整理（静态配置与钩子分开）
- [x] 脱敏扩展：`after_tool` 中去掉密钥和令牌（原路线图“执行后钩子”）
- [x] 输入与上下文变换先以追加消息或 `context_edit` 记录，写入成功后再请求模型；P5 加入压缩时沿用这一约束

实现提交：`ea147e4`。

验收（2026-11-01）：原有 `tests/cli.rs` 与 `run --help` 均保持不变；覆盖六个钩子的组合、失败处理、超时与取消、精确审批范围、变换先落盘、脱敏与续写。稳定版与 Rust 1.88 均为 196 项通过，6 项付费 API 测试跳过。命令与原始测试摘要附在本阶段 PR 中。

实现接口：`LoopSpec` 保存静态配置，`LoopConfig` 持有钩子与交互宿主，`LoopControl` 借用运行状态。`PolicyHook` 适配原策略；`ContinueOnLength` 替代 `Limits::continue_on_length`；SDK 迁移见 README。终端审批与配置文件仍属 P3、P4。

### P2 长期会话 `AgentSession`

目标：现有 `Runner` 已支持顺序多次运行并保留会话；在此基础上提供统一的 `AgentSession` API、多订阅者、模型切换和共享调用互斥。不引入新依赖。

- [x] `prompt`、`steer`、`follow_up`、`abort`、`subscribe`、`set_model`
- [x] 同一时刻只有一个运行；运行中 `prompt` 返回忙碌错误，插话须显式调用 `steer` 或 `follow_up`
- [x] `abort` 只取消当前运行，会话保留；多个订阅者都收到事件
- [x] `prompt` 返回 `Result<RunReport, SessionError>`；无人值守仍可一行调用：`session.prompt(task).await?` 取得 `RunReport`

P2 接口与所有权约定（2026-11-01）：

- `AgentSession` 在 `Runner` 外提供共享的 `&self` 方法；`Runner` 保留为独占的运行、重试与审计实现，兼容现有 SDK。
- 状态为“空闲并持有 Runner”或“运行中并持有取消令牌”。`prompt` 原子地取出 Runner，RAII guard 在完成、panic 或 future 被丢弃后归还；锁不跨越异步等待，也不包住订阅回调。
- `prompt(task).await` 返回 `Result<RunReport, SessionError>`；`prompt_with_cancel` 接受外部取消令牌。并发 `prompt` 或运行中 `set_model` 立即返回 `Busy`，不排队。
- `abort` 只取消当前运行，空闲时无操作；`steer`、`follow_up` 沿用现有队列。每次运行创建独立的子令牌，已取消的运行不会影响下一次。
- `subscribe` 返回 RAII `Subscription`；持有期间接收后续事件，drop 后退订，已分发快照中的回调可能完成。构造时的原有 sink 保留。单个订阅 panic 被隔离并移除，通过订阅句柄与会话失败计数报告。
- `set_model(ModelRef)` 同步切换 provider 与模型 ID，空模型 ID 报错；保留历史、提示词、工具、钩子、上限和审批缓存；空闲时可通过 `clear_approvals` 撤销授权。API 不需要 I/O，因此将方案草图中的 async 改为同步方法；不会改写会话头部记录的初始模型。
- `run` 与库示例接入 `AgentSession`；CLI 参数和 `tests/cli.rs` 保持不变。终端对话仍属 P3。

实现提交：`8831ec0`。

本地验证（2026-11-01）：stable 与 Rust 1.88 各 221 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 和 MSRV 全目标编译通过。

验收：新增 24 项共享会话行为测试与 1 个 Rustdoc 编译示例，覆盖忙碌错误、取消与丢弃后复用、并发调用、订阅和退订、回调重入与 panic、模型切换、审批撤销及审计顺序。原有 `tests/cli.rs` 和 `run --help` 逐字节不变；未新增依赖。完整检查与原始测试输出附在本阶段 PR 中。

### P3 终端对话 `chat`

目标：最小可用的终端对话。本阶段采用专用输入线程，不新增 crate 或启用 io-std；D2 中已授权的 io-std 与可选 rustyline 本阶段无需引入。

- [x] 流式显示回答与工具调用
- [x] `TerminalInteraction`：展示完整命令、依据与影响范围，可选“允许一次 / 本会话允许 / 拒绝”
- [x] 斜杠命令：`/new`、`/model`、`/compact`（P5 已接入）、`/quit`
- [x] Ctrl+C 只中止当前运行，不退出程序

P3 实施约定（2026-11-01）：

- 新增 `chat` 子命令，采用通用交互说明；每次提问默认不限轮次、不限总时间、不自动续写，外部等待仍受 provider 和审批超时约束。`run` 的测试、帮助文本和默认行为保持兼容。
- 共享连接、工具和会话选项，两个前端都组装同一个 `AgentSession`。chat 流式显示文本；可选 JSONL 沿用既有事件格式。
- 所有 bash 调用在通过原命令策略后请求确认，read_file 沿用原读取能力。完整工具名、参数、工作目录和审批理由先展示；用 `/approve <id> once`、`/approve <id> session`、`/deny <id> [reason]` 回答。会话允许仍只匹配完全相同的工具和参数，明确拒绝不能被审批绕过。
- 单一终端输入所有者协调普通输入与审批，审批通过有界请求通道和一次性回复传递，审批答案必须匹配当前请求编号，取消后丢弃过期请求，旧答案不能授权下一条命令。TTY 运行中普通输入作为 steering；管道脚本逐轮处理，只有待审批时读取审批命令，避免预读的 /quit 取消前一条任务。
- 按 Tokio stdin 源码对交互终端的建议，使用进程生命周期的专用阻塞读取线程和有界通道；单行输入限制 1 MiB。无需启用 io-std 或新增 crate；退出不会等待 Tokio 无法取消的 stdin 读取。
- `/new` 创建新会话并清空授权；启用文件会话时创建唯一的新文件，保留旧日志。`/model [id]` 查看或在空闲时切换同一 provider 的模型；运行中拒绝切换。P3 为 `/compact` 预留入口，P5 已接入共享会话压缩 API。
- `/quit` 退出；Ctrl+C 只取消当前运行或待审批，不退出会话，空闲时保留会话；SIGTERM 取消并退出。审批输入结束时拒绝并结束当前运行，避免无人可答时继续等待。
- 不引入 rustyline；P3 先交付逐行 REPL，行编辑与完整 TUI 不在本阶段。

实现提交：`a5f94a8`。完成日期：2026-11-01（PR #5 合并）。

验收（2026-11-01）：新增 15 项单元测试、26 项 CLI 测试，其中 3 项使用真实 Unix PTY。stable 与 Rust 1.88 各 262 项通过、6 项付费测试跳过；格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。另用本地假服务在实际终端走通中文对话、拒绝审批、模型切换、/new、/compact 提示和退出。真实模型 API 试用未运行。

兼容性：`tests/cli.rs`、`run --help`、`Cargo.toml`、`Cargo.lock` 与 P2 基线逐字节相同；原始测试摘要附在本阶段 PR 中。

### P4 profile 文件、上下文文件与 skills

目标：不写代码即可定义场景。依赖：`toml`（D2 已授权；上游 [Cargo.toml](https://github.com/toml-rs/toml/blob/main/Cargo.toml) 在 2026-10-31 核对的 MSRV 为 1.85，接入时按选定版本复核）。

P4 实施约定（2026-11-01）：

- `run` 默认加载嵌入二进制的 `profiles/troubleshoot.toml`，`chat` 默认加载 `profiles/assistant.toml`；说明放在相邻 Markdown 文件。保留现有预设 SDK 接口，运行时配置以 TOML 为准。
- `--profile` 接受内置名称或 TOML 路径；自定义文件可用 `extends` 选择一个内置基线，不支持任意文件继承。叠加顺序为内置基线、文件、显式 CLI 参数；显式传入与旧默认值相同的参数也必须覆盖文件。原有 `tests/cli.rs` 不改，帮助仅新增配置选项。
- TOML 字段使用 camelCase，拒绝未知字段和无效类型。规则包含命令前缀 allow、命令子串 deny、askTools 与 denyTools；deny 累加，不能被 CLI 工具开关或 allow 覆盖。run 始终使用 NoHuman，chat 保留终端审批，profile 不切换前端。
- 配置支持轮次、总时间、截断续写与已知密钥脱敏；`maxTurns = "unlimited"` 表示不限，0 仍表示不调用模型。当前只支持 `execution = "local"`，脱敏不可关闭；未实现的容器、任意扩展等配置直接报错。
- 自动读取最近 Git 根目录到 cwd 的 AGENTS.md（无 Git 根时仅 cwd），不隐式读取用户主目录配置（2026-11-01 调整：`troubleshoot` 默认关闭自动发现，发现的文件须位于项目根目录内，见上方 P4 复核修正）；profile 可添加上下文文件，路径相对 profile 文件。CLI `--context-file` 相对启动目录，`--no-context` 可禁用上下文加载。
- skills 通过 TOML 的 `[[skills]]` 显式登记 name、description、path，兼容把现有 SKILL.md 当正文文件；不解析 YAML frontmatter、不自动扫描全局 skill 目录。启动只向提示词加入目录信息；`read_skill` 工具按名称加载正文，经过原钩子、策略、取消、脱敏与持久化流程。未启用该工具时不加入目录。
- 配置、说明、单个上下文和 skill 正文均有 64 KiB 上限；上下文总量 256 KiB，skills 最多 64 项。只读普通 UTF-8 文件，超限或无效内容明确报错。上下文与 skill 文本不能改变已组装的工具及策略。
- 引入已授权的 `toml 1.1.8`，仅启用 parse、serde、std；其 crate 声明 MSRV 1.85，仍用 Rust 1.88 验证。

- [x] profile 文件格式（TOML）：说明、工具、规则、上限、扩展、执行环境
- [x] 叠加顺序：内置默认 < profile 文件 < 命令行参数；deny 规则只增不减
- [x] 内置两个 profile：`troubleshoot`（P0 的预设）与 `assistant`（通用对话）
- [x] 读取 AGENTS.md 等上下文文件；skills 按需加载
- [x] 命令行增加 `--profile`；配置错误时退出码 2

验收：两个内置 profile 均由文件定义；配置错误的测试。

实现提交：`7fb3124`。完成日期：2026-11-01（PR #6 合并）。

本地验收（2026-11-01）：新增 24 项单元测试、18 项 CLI 测试；stable 与 Rust 1.88 各 304 项通过、6 项付费 API 测试跳过。格式、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。两个内置 profile 及完整评审示例通过零请求离线试用；真实模型 API 未运行。

兼容性：原 `tests/cli.rs` 和两份预设说明逐字节不变，原 SDK 场景辅助接口保留；`run --help` 仅增加 profile 和上下文选项。新增配置覆盖、deny、上下文边界与预算、skill 懒加载和审计测试；上下文分隔符预算及非 UTF-8 原生路径的回归均先失败后修复。配置说明与完整示例见 [PROFILES.md](PROFILES.md)。

### P5 上下文管理

目标：长对话不超出上下文窗口，且不丢失关键信息（原路线图“上下文压缩”）。不引入新依赖。

开工前置（2026-11-01 补充）：经用户授权，先运行一次 `tests/real_api.rs`，确认目标服务的流式格式、工具调用与用量统计。provider 的自动化测试只使用本地假服务，而压缩依赖真实模型的行为与用量。

开工前置补跑记录（2026-11-01，经用户授权，基于 `0c37a66`，在实现完成后补跑）：DeepSeek（`https://api.deepseek.com/v1`，默认 `collapse`）的 `deepseek-flash` 与 `deepseek-chat` 各运行一次 `tests/real_api.rs`，均为 6 项通过。覆盖流式文本与用量统计、bash 工具往返、策略拒绝传回模型、输出上限截断（`Truncated`）、恢复会话后的提示词增量更新、无效 key 不重试。实测用量：`deepseek-flash` 3045 token，`deepseek-chat` 2810 token，单次回答最多 76 个输出 token。该服务的 `/v1/models` 只列出 `deepseek-flash` 与 `deepseek-v4-pro`；`deepseek-chat` 的请求同样成功，原因未核实。这组测试不含压缩与请求审计用例，`inline` 模式未在该服务上运行。

P5 实施约定（2026-11-01）：

- 新建会话使用 v3；旧 v1/v2 日志在首次写入压缩记录前追加 `format_upgrade`，不改写头部。新增 `compaction_started`、`compaction`、`compaction_failed`：请求先落盘，成功摘要或失败结果随后追加，取消或崩溃留下的开始记录不改变投影。
- 成功条目记录 firstKeptEntryId、摘要和请求记录引用。投影保留全部 system 消息、摘要及保留范围；原消息与 context_edit 不改写，分支各自投影。摘要模型使用该分支的原始投影及条目 ID，不输入旧摘要，避免摘要的摘要。
- 摘要分 facts（逐条 sourceEntryIds）、inferences、plans。校验引用属于本次源条目、字段与字节上限；拒绝无来源事实、截断回答、工具调用或无效 JSON。引用校验不代表模型事实的语义已被证明。
- 在现有输入与上下文钩子持久化后，Runner 的宿主准备步骤可执行自动压缩，再请求任务模型；循环不判断交互模式。自动摘要调用计入该轮模型请求预算和 usage，失败不伪装成任务回答、不自动重试。
- chat 默认在序列化上下文字节数超过 256 KiB 时触发，保留最近约 64 KiB 及最后用户输入；run 默认关闭自动压缩。阈值是可配置的字节预算，不声称等同模型 token 上限。保留内容无法缩到阈值时明确停止；系统提示词和最新输入不静默截断。（2026-11-01 调整：付费的自动摘要请求失败后，本会话暂停自动摘要，见上方 P5 复核修正。）
- 摘要源最多 2 MiB，正文最多 64 KiB，单次摘要最多 2048 输出 token、120 秒；profile 可调整触发与保留预算。手动 `AgentSession::compact` 与 prompt 共用独占运行权，支持 Busy、取消和丢弃后复用；chat 的 `/compact` 接入同一 API。
- 大工具结果通过最后的 after_tool 扩展，在已知密钥脱敏后写入普通 UTF-8 文件，只将引用送入会话与模型；保存的是原工具返回的有界内容，既有截断/分页边界不变。默认阈值 8 KiB、文件上限 1 MiB，文件名为 UUID，Unix 新目录/文件权限为 0700/0600。
- 注册 `read_output` 按 UUID 与 UTF-8 字节 offset 分页读取结果，单页不超过落盘阈值；读取结果和 read_skill 不再次落盘，避免递归引用。只有启用 read_output 时安装落盘扩展，显式 CLI 工具集合仍生效。结果目录默认 cwd/.zx-agent/outputs，文件不自动删除并可在同目录续用；加入 gitignore。（2026-11-01 调整：`troubleshoot` 默认不启用 `read_output`；写入失败时结果保持内联；默认目录须位于工作目录内，并在目录中写入 `.gitignore`，见上方 P5 复核修正。）
- profile 使用 `[compaction]` 与 `[toolOutput]`；CLI 可覆盖自动触发阈值或关闭自动压缩。默认 SDK 仍不自动压缩、不落盘，已有构造器保持兼容；可在 Runner 上显式配置后交给 AgentSession。
- 用 FauxProvider、本地假 HTTP 服务、会话兼容/分支/失败写入测试验收，不运行付费 API 测试。
- 请求审计（G3，2026-11-01 补充）：每次任务请求和摘要请求调用 `Provider::stream` 前，先追加 `request_started`；provider、模型、工具定义、`max_tokens` 或 provider 转换参数与当前分支上最近一条 `request_config` 不同时，先追加新的 `request_config`。`request_started` 只记录配置引用和消息来源（会话投影，或摘要尝试的源条目与请求时间戳），不复制整份对话；`SessionStore::replay_request` 据此还原与 provider 无关的 `Request`。写入前校验待发消息与持久化投影一致；任何一步写入失败都不发请求，并按不变量 7 取消运行。默认 SDK 也记录，这是会话文件约定的变化：会话中除 message、context_edit、压缩记录外，还会出现这两类条目。旧 v1/v2 日志在首次写入请求或压缩记录前追加 `format_upgrade`，不再只在首次压缩前。不记录 API key、URL 与 HTTP 头；`Provider::request_options` 只返回非机密的转换参数，默认 `null`，OpenAI 兼容 provider 记录格式版本与 `systemUpdates`，可用 `openai::replay_body` 重建请求体。`request_started` 只表示准备发送，不能证明服务端已接受；没有对应回答说明发送结果未知。

- [x] 会话条目 `compaction`：记录保留的第一条条目与摘要（沿用 pi 的结构）
- [x] 首次写入 `compaction` 前升级会话格式，兼容读取旧版本；版本号按当时的格式递增，不延后到 P6
- [x] 同一次格式升级加入请求配置条目：记录工具定义、模型与发送参数，变化时追加，使每次请求都能从会话文件还原（G3）
- [x] 摘要分“环境事实（附原始条目出处）”与“推断和计划”两部分；原始历史不改写
- [x] 触发方式：达到阈值自动触发，或在 `chat` 中用 `/compact` 手动触发；无人值守的短任务默认关闭
- [x] 工具侧预算：大结果写入文件，只返回路径

实现提交：`f78cbcb`、`e9be6a9`。完成日期：2026-11-01（PR #9 合并）。PR 的 CI（macOS、Ubuntu、Rust 1.88、格式 / clippy / 文档）在 `0c37a66` 上全部通过。

验收（2026-11-01）：FauxProvider、本地 HTTP、会话 v1/v2/v3 与分支、四种压缩写入失败、取消 / 超时 / 丢弃、请求计费与摘要期间 steering、CLI profile 与 Unix PTY、长结果脱敏与分页均通过。stable 与 Rust 1.88.0 各 **356 项通过（含 2 项 doctest），6 项付费测试跳过**；fmt、clippy、严格 Rustdoc 均通过。`tests/cli.rs` 逐字节保留，Cargo 依赖未变。

请求审计验收（2026-11-01）：每次任务请求与摘要请求都能从重新打开的会话文件还原，并与实际发送的 `Request` 逐项相等；配置只在变化时追加（含切换模型）；v1/v2 会话在首次请求前只升级一次且原有字节不变；`request_config` 或 `request_started` 写入失败时不发请求；OpenAI 兼容 provider 的还原请求体与实际请求体在两种 `systemUpdates` 模式下一致，且不含 API key。原有 4 项测试按新的会话文件约定更新了预期：3 项只统计 message 行，1 项允许出现请求审计条目，原本要验证的行为不变。stable 与 Rust 1.88.0 各 **385 项通过，6 项付费测试跳过**；fmt、clippy、严格 Rustdoc、MSRV 全目标检查均通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动。

未验证：真实模型的摘要准确率、信息保真和 token 窗口适配；压缩与请求审计未在真实模型上运行（付费测试只覆盖上面开工前置补跑记录所列项）。配置预算及限制见 [PROFILES.md](PROFILES.md#上下文管理)。

### P6 执行环境与崩溃恢复

目标：命令可以放进隔离环境执行；进程崩溃后能正确恢复。依赖：无（调用本机 `docker` 命令）。D5 已于 2026-11-01 确认：默认本机执行，容器显式开启。

实施拆分（2026-11-01）：先做崩溃恢复（`effects()`、`tool_call_started`、恢复、会话 v4），不依赖执行环境；再做执行环境（`Executor` 接缝、离线 `ContainerExecutor`、profile 容器选项），`Executor` 接缝与容器设计一起定稿；在线访问的网络与凭据范围评审后再实施。

崩溃恢复实施约定（2026-11-01）：

- `Tool::effects()` 返回 `Effects::ReadOnly` 或 `Effects::SideEffects`，默认 `SideEffects`，未声明的工具按有副作用处理。内置工具中 bash 有副作用，read_file、read_output、read_skill 只读。
- 有副作用的调用通过检查与审批后、执行前，追加 `tool_call_started`（`toolCallId`、`toolName`）。并行批次先按调用顺序写完全部开始记录，再并发执行。写入失败时该调用不执行，并按不变量 7 取消运行。只读工具不写开始记录。
- 新建会话为 v4。旧 v1–v3 会话在首次写入 `tool_call_started` 前追加 `format_upgrade`（version 4），不改写头部；请求与压缩记录仍只要求 v3。只支持到 v3 的程序会拒绝打开 v4 会话。
- 恢复：Runner 每次运行开始、追加提示词之前，检查当前分支末尾的 assistant 消息。其中没有结果记录的工具调用，各追加一条 `isError` 的工具结果，并发出 `tool_call_recovered` 事件；不重放调用。按记录分四种：有开始记录为 `started`（结果未知，可能已改变状态）；会话已按 v4 记录开始、工具有副作用、没有开始记录为 `not_started`（未执行）；只读工具为 `read_only`；旧格式或当前未配置该工具为 `start_unknown`（是否开始未知，按可能已改变状态处理）。
- 只处理分支末尾的调用。之后已有其他消息、已被 `context_edit` 省略或已被摘要替代的调用不补结果，provider 转换时补占位的行为不变。`Error` / `Aborted` 回答里的调用从未执行，不处理。恢复结果写入失败时，按不变量 7 取消运行。
- 用 FauxProvider、构造的崩溃会话文件、写入失败、丢弃 future，以及对正在执行命令的 `run` 进程发 SIGKILL 的测试验收；不涉及执行环境，`tests/cli.rs` 不改。

执行环境实施约定（2026-11-01，经用户确认）：

- 分两个 PR：先加 `Executor` 接缝与 `LocalExecutor`，行为不变；再加离线 `ContainerExecutor` 与配置。
- `Executor` 接缝（`src/executor/`）提供两个操作：`run` 在给定目录执行一条 shell 命令，响应超时与取消，返回有上限的 stdout / stderr 与结束方式；`read_file` 只读取不超过上限的普通文件。bash 负责参数、结果格式与截断，read_file 负责分页；两者各持有一个执行器，默认 `LocalExecutor`。`LocalExecutor` 是原有实现搬迁而来：进程组、超时与取消后终止、输出捕获上限、只读普通文件与实际读取量上限。read_skill 读取宿主机上的 skill 文件，固定使用 `LocalExecutor`。
- 容器生命周期：每个进程一个容器（`run` 对应一次任务，`chat` 对应整个会话），在启动时创建（要先读取容器的系统信息写进提示词，配置错误也在启动时暴露），进程退出时删除；容器带 `zx-agent.session` 标签，便于识别。
- 工作目录只读挂载进容器；命令只能写容器内的 tmpfs `/tmp`。网络这一轮固定为 `none`，不提供配置项，在线访问方案评审后再加。
- 超时与取消：2026-11-01 在本机 Docker 29.9.2 上实测，SIGKILL 或 SIGTERM 结束本机的 `docker exec` 客户端（不带 `-t`）后，容器内的命令仍在运行（单次观测，机制未从源码确认）。因此容器执行器在容器内按进程组终止命令，不依赖结束客户端。
- 容器模式下，系统提示词的 `<environment>` 描述容器而不是宿主机。
- 测试：默认 `cargo test` 用记录参数的假 docker 程序，不访问网络；真实 Docker 测试仅在设置 `ZX_DOCKER_TEST_IMAGE` 时运行，并在 CI 的 Linux 任务中拉取镜像后开启。

容器执行器实现细节（2026-11-01，依据同日在本机 Docker 29.9.2、Ubuntu 系镜像上的实测，均为单次观测）：

- 容器以 `docker run -i --rm --init --sig-proxy=false` 启动，PID 1 读取 agent 持有的 stdin。实测关闭 stdin、或用 SIGKILL 结束 `docker run` 客户端后，容器在 5 秒内被删除；因此正常退出（结束客户端）和 agent 崩溃（stdin 随之关闭）都会删除容器，不需要在析构时同步调用 `docker rm`。docker 客户端放在独立进程组，终端发给前台进程组的信号只送达 agent，由它决定停止什么；另外实测即使不放进独立进程组，`docker run --sig-proxy=false` 客户端收到 SIGINT（默认处置）后仍在运行、容器保留，所以 chat 中按 Ctrl-C 不会删除容器（机制未从 Docker 源码确认）。
- 加固参数：`--network none --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges --pids-limit --memory --user`，实测均被接受；默认用户 `65534:65534`、内存 1g、进程数 256，profile 可调。镜像不自动拉取（`--pull never`），缺少时提示先 `docker pull`。
- 启动检查：实测 `docker exec` 启动的进程既是进程组组长也是会话首进程，但这只是对一个运行时的观测，所以启动时在容器内核对；不成立、缺少配置的 shell 与 `cat`、`head`、`sleep`、`mkdir`、`rm`、`uname`、或容器用户不能读取工作目录时，报配置错误。`docker run` 不设工作目录（PID 1 不需要），否则不可读的工作目录会让容器直接启动失败，只留下 Docker 自己的报错。
- 命令：包装脚本先把自身 PID（即进程组号）写到 tmpfs，再 `exec` 配置的 shell。超时或取消时另起一次 `docker exec`，先 SIGSTOP 再 SIGKILL 整个进程组（实测以非 root 用户可行）；命令正常结束后也清理一次进程组，结束后台子进程。实测主进程退出但后台子进程仍占用输出时，`docker exec` 约 2 秒后返回。命令被信号终止时，退出码沿用 `docker exec` 的 128+信号编号（实测 SIGKILL 为 137），与本机执行的“terminated by a signal”不同。运行中的 future 被丢弃时，尽力在后台发起同样的清理。
- 会话文件：实测在 Docker Desktop 上，只读挂载的文件对容器用户显示为其所有，权限为 0600 的文件也能读到。因此会话文件位于工作目录内时，用 `/dev/null` 覆盖挂载到同一路径（实测容器内读到空文件，宿主机文件不变）。工作目录中的其他文件（含 `.zx-agent/outputs` 中已脱敏的工具结果）对容器只读可见。原生 Linux 的 Docker 保留宿主机权限，默认用户 65534 只能读取对其他用户可读的文件，工作目录本身也须可读（`tempfile` 创建的 0700 目录不行，测试因此使用 0755 的工作目录），可用 `container.user` 改用其他用户。
- read_file 在容器内读取，目录、不存在、特殊文件、无读权限分别报错，读取量上限同本机；单次读取 30 秒超时。
- 配置：profile 的 `execution` 取 `local`（默认）或 `container`；`[container]` 表含 `image`（容器模式必填）、`user`、`memory`、`pidsLimit`。CLI 的 `--execution`、`--container-image` 覆盖 profile；在本机模式下给出 `--container-image` 视为配置错误。

- [x] `Executor` 接缝：`LocalExecutor`（现有行为）与 `ContainerExecutor`（接缝与 `LocalExecutor`：PR #13；`ContainerExecutor`：PR #14）
- [x] profile 的 `execution` 增加容器选项（D5：显式开启，默认仍为本机；PR #14）
- [x] `ContainerExecutor`：每个进程创建一个沙箱容器（2026-11-01 由“每次运行”调整，见上方约定；网络默认拒绝，在线任务按评审方案限制访问范围；只读根文件系统、非 root、去掉全部 capability、进程数与内存上限）；每条命令通过 `docker exec` 执行；`read_file` 也在容器内读取；运行结束删除容器（离线部分已完成，PR #14；在线访问随下一项评审）
- [ ] API key 和会话文件只留在 Agent 进程，不进入容器；工具需要的凭据以只读方式挂载（API key 与会话文件已完成，PR #14；凭据只读挂载随在线方案评审）
- [ ] 列出在线排障所需的 API 地址、网络与凭据范围；无网络的容器只用于离线排障，在线场景的受限访问方案在实施前评审
- [x] `Tool` trait 增加 `effects()`：只读或有副作用
- [x] 有副作用的工具执行前追加 `tool_call_started` 条目；恢复时把“已开始、无结果”的调用标记为结果未知，不自动重放
- [x] 首次写入 `tool_call_started` 前再次升级会话格式，兼容读取 P5 及更早版本

执行环境接缝验收（2026-11-01，已完成，PR #13）：`src/executor/` 提供 `Executor`、`ShellCommand`、`CommandOutput`、`CommandEnd` 与 `LocalExecutor`；bash 与 read_file 新增 `executor` 字段，默认 `LocalExecutor`，`..Default::default()` 的构造方式不受影响，按字段完整列出的字面量需补上新字段。本机执行代码从 bash 与 read_file 原样搬迁，bash 与 read_file 原有测试不改断言即通过（仅 read_file 测试中两处字面量补 `..ReadFileTool::default()`，bash 测试模块补一行 `use tokio::process::Command;`）。新增 `tests/executor.rs` 3 项，用记录请求的替身执行器验证：bash 把命令、目录、超时与隐藏变量交给执行器，并正确报告 6 种结束方式；read_file 经执行器读取解析后的路径与上限。反向验证：让工具绕过自身执行器时，3 项全部失败，修改已还原。本地 macOS 基于 `bb91974`，stable 与 Rust 1.88.0 各 416 项通过、6 项付费测试跳过；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动。

离线容器执行验收（2026-11-01，已完成，PR #14；在线访问未开始）：

- 新增测试：模块内 6 项（`docker run` 参数与校验、启动探测输出的解析、容器的 `<environment>`、profile 的容器设置）；`tests/container.rs` 在 macOS 上 24 项、在 Linux 上 25 项。其中 14 项用假 docker 程序，始终运行：启动参数与失败报告、命令与读文件的转发、超时 / 取消 / 丢弃后的清理、CLI 接线与配置错误。其余以 `real_` 开头的测试只在设置 `ZX_DOCKER_TEST_IMAGE` 时运行：用户与只读工作目录、无网络、整个进程组的终止、后台进程清理、隐藏会话文件、容器内读文件、chat 中 Ctrl-C 后容器仍在、zx-agent 正常结束或被 SIGKILL 后容器删除、不可读工作目录在启动时报错（后者只在 Linux 上编译）。
- 本地真实 Docker：macOS Docker Desktop（Docker 29.9.2）上分别用 `debian:bookworm-slim` 与一个 Ubuntu 系镜像运行，全部通过，未跳过；测试后没有遗留容器或 docker 客户端进程。CI 在 Linux 任务中拉取 `debian:bookworm-slim` 后运行同一文件，以 CI 结果为准；workflow 的改动未在本地用 actionlint 检查，由 CI 的检查步骤校验。
- 反向验证：容器内只终止进程组组长、不终止整个组时，整棵命令树与后台进程两项真实测试失败；去掉 docker 客户端的独立进程组时，Ctrl-C 测试仍通过，说明它验证的是“Ctrl-C 不删除容器”这一行为，不能证明独立进程组的必要性（见上方实现细节）。修改均已还原。
- 修正一项测试自身的问题：读文件测试曾按 `read_dir` 的顺序取第一次调用，而该顺序未定义，约半数运行失败；改为与顺序无关的断言后连续 10 次通过。
- PR #14 第一次 CI 在 Linux 上发现另一项测试问题：Ctrl-C 测试用外部 `kill -INT -<进程组>` 发信号且未检查结果，而 procps-ng 4.0.4 的 `kill` 不接受这种写法（在 Ubuntu 系镜像中实测返回 1），信号没有发出，测试等待 60 秒后失败；其余 24 项在 Linux 上通过。PR #14 合并时尚未包含修复，合并后 `main`（`20b9809`）的 CI 在同一项失败。PR #15 改用 shell 内置的 `kill` 并断言发送成功。
- 原有测试的调整：profile 测试中表示“不支持的后端”的取值由 `container`（现已支持）改为 `kubernetes`，仍验证未知后端报错。
- 本地 macOS 基于 `f45fd98`：stable 与 Rust 1.88.0 各 446 项通过、6 项付费测试跳过（未设置 `ZX_DOCKER_TEST_IMAGE`，真实 Docker 测试只打印提示）；fmt、clippy、严格 Rustdoc 与 MSRV 全目标检查通过。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。

未验证：原生 Linux 的 Docker 只由 CI 覆盖，本地没有运行；rootless Docker 与 Podman；Windows；`docker exec` 在后台进程占用输出时约 2 秒返回，以及 `docker run` 客户端在 SIGINT 后继续运行，都只是单次观测。

崩溃恢复验收（2026-11-01，已完成，PR #12；执行环境部分未开始）：

- 新增 22 项测试：会话层 10 项（条目格式、引用校验、调用 ID 复用、旧会话升级到 v4、打开时的格式检查、中断调用判定）；运行层 2 项（开始记录写入失败时不执行、恢复结果写入失败时不发请求）；事件 1 项（`tool_call_recovered` 的 JSON 与四种 `state`）；`tests/crash_recovery.rs` 8 项（内置工具声明、串行与并行批次的开始记录时序、四种恢复结果且不重放、旧格式会话、丢弃 future 后恢复、`run` 的输出，以及 Unix 上 SIGKILL 掉正在执行命令的 `run` 后由下一次 `run` 恢复）；`tests/chat_cli.rs` 1 项。
- 反向验证（整套测试）：不写开始记录时，4 项集成测试和 1 项单元测试失败；去掉恢复步骤时，6 项集成测试和 1 项单元测试失败；其余测试不受影响。两次修改都已还原。
- 原有测试中 2 项按 v4 更新预期：新会话版本号由 3 改为 4；拒绝“未来版本升级”的用例由 4 改为 `SESSION_VERSION + 1`。
- 本地 macOS（基于 `4ccb1bb`，含 PR #11）：stable 与 Rust 1.88.0 各 **413 项通过，6 项付费测试跳过**；fmt、clippy、严格 Rustdoc、MSRV 全目标检查均通过。反向验证的结果在 rebase 前后各测一次，相同。`tests/cli.rs`、`Cargo.toml`、`Cargo.lock` 未改动，未新增依赖。

未验证：Windows 上的进程终止场景；断电等导致已写入的记录未落到磁盘的情况（会话写入只调用 `flush`，不调用 `fsync`）。

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
- [ ] 无人值守的 token 预算：profile 上限增加 token 总量，超出时结束运行；现有退出码 0–6 的含义不变（2026-11-01 补充）
- [ ] 更多 provider：例如 Anthropic Messages API
- [ ] 异步审批：`WebhookInteraction`，高风险命令发 webhook 请求批准，超时则拒绝
- [ ] OS 级沙箱的 Executor：Linux 的 bubblewrap + seccomp、macOS 的 Seatbelt
- [ ] 由 webhook 或告警触发运行
- [ ] OpenTelemetry 追踪
- [ ] 拆分为多个 crate（`zx-ai`、`zx-core`、`zx-tools`、`zx-cli`），用编译器保证分层

## 5. 随各阶段处理的整理项

这些整理不改变行为，在改到相关代码时顺带完成：

- [x] `run_loop` 参数过多（P1）：整理为 4 个参数，运行状态通过 `LoopControl` 借用
- [x] 工具名字符串散落在多个文件（P1 或 P4）：2026-11-01 集中为 `tools::BASH`、`tools::READ_FILE` 与 `profile::skills::TOOL_NAME`
- [ ] `clip` 有两份实现（任意阶段）
- [ ] `BashTool::execute` 过长（P6）

## 6. 推进方式

- 每个阶段可以拆成多个提交；完成后在本文档中勾选任务、更新状态，并记录日期和提交。
- PR 合并后，在本文档中把阶段状态改为“已完成”，并记录 PR 编号与合并日期。也可以在 PR 中直接写成“已完成（PR #编号）”，合并后即准确；`.github/pull_request_template.md` 中有对应检查项。
- 每个阶段结束时：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 通过；用到新语法或新标准库 API 时，`cargo +1.88.0 check --all-targets` 也要通过。
- 从 P0 起，`tests/cli.rs` 中 `run` 的测试不改即通过，以此保证兼容性；D5 若改变默认执行环境，仅在 P6 按明确的迁移规则更新相关测试，其余行为仍须保持兼容。
- 需要新依赖时，先确认在 D2 的授权范围内；范围之外的要先征得同意。
- 设计有变化时，先更新本文档，再改代码。

---

ROADMAP.md · v0.5 · 2026年11月
