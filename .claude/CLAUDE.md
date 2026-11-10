# zx-agent 项目说明

<!--
维护说明（供维护者阅读）。

- 本文件每个会话都会加载，只放每个任务都用得上、又不能从代码和 README 里看出来的内容：分层职责、数据流、设计取舍（为什么这样做）、红线与不变量。
  架构图、模块与 pi 的对应表、参数说明只在 README.md；配置键只在 docs/CONFIGURATION.md；目标、决策（包括已授权的依赖）与验证记录只在 docs/ROADMAP.md。
- 写 Rust 的细则在 .claude/rules/（01 代码规范、02 所有权与借用、03 错误处理、04 异步与并发、05 类型与 API 设计、06 测试、08 依赖与 MSRV、09 尺寸与重复），
  都带 paths 过滤，读写 .rs 文件时加载（08 还包括 Cargo.toml）。规则只写项目特有、工具查不出、又没写在本文件里的内容。
- clippy 默认就会报错的写法不写进规则，交给 cargo clippy 的 -D warnings（如持有 MutexGuard 跨 .await、1.88 之后才稳定的标准库 API）。
- .claude/settings.json 已启用：PostToolUse hook（hooks/post-edit.sh）在 Edit / Write 之后对改动的 .rs 文件跑 rustfmt 并记下本会话改过代码；
  Stop hook（hooks/stop-check.sh）在本会话改过 Rust 代码时，结束前跑 cargo fmt 检查、clippy、test，不通过就拦下一次；
  ask 规则：git commit、git push、cargo add / remove / update、修改 Cargo.toml 要用户确认。
-->

## 背景

- **是什么**：用 Rust 编写的通用 agent 内核。同一个内核支撑三类场景：和人对话；无人值守运行，例如由 cron 或 Kubernetes Job 触发的排障；被编辑器或其他程序嵌入。场景差异放在内核之外，通过 agent 文件、提示词和插件表达。
- **参照**：分层方式、循环语义和会话格式沿用 pi（https://github.com/earendil-works/pi ，commit `a92283056`）。与 pi 不同的地方记录在 README 的"与 pi 的差异"。
- **现状**：v0.1.0。子命令有 `run`（无人值守）、`chat`（终端，应急与脚本化入口）、`acp`（Agent Client Protocol v1 的 agent 一侧）、`im`（Slack 与 ⚑ D:5/CHINA FLAG: 飞书，按会话分派，D16）与 `sessions`；一个 OpenAI 兼容的流式 provider；工具 `bash`、`read_file`、`edit_file`、`write_file` 与只读的 `grep`、`find`、`ls`；MCP 客户端（只接工具、只用 Streamable HTTP）；可选的容器执行与 OpenTelemetry 追踪导出（只导出 span）。环境在主目录（`~/.zx-agent`，`ZX_AGENT_HOME` 可改）的 `config.toml`，角色在 agent 文件（`run` 用 `unattended`，其余用 `assistant`，主目录的 `agents/<名字>.md` 替换同名内置 agent）。SDK 默认不限轮次、不限时间、不自动续写或压缩、不落盘工具结果。用法与资源上限见 README，配置见 `docs/CONFIGURATION.md`，目标、阶段与决策见 `docs/ROADMAP.md`，改动前先确认当前阶段。
- **验证范围**：付费与真实环境上的运行、各项验证的范围，见 `docs/ROADMAP.md` 第 6 节。

## 架构设计

代码分为一个 workspace 中的 8 个 crate，上层依赖下层，下层 crate 引用不到上层：`zx-ai`（只含类型与 `Provider` 契约）← `zx-core`（内核）← `zx-tools` 与 `zx-im`，二者互不依赖；`zx-ai-openai`（OpenAI 兼容 provider，模型层里唯一依赖 reqwest 的 crate）只依赖 `zx-ai`，内核与它互不依赖；`zx-im-slack`、`zx-im-feishu` 实现 `zx-im` 的接口 ← `zx-agent`（根目录 `src/`：CLI、场景配置与追踪导出；库以原有路径再导出各 crate）。各 crate 的 `lib.rs` 与父模块的 `//!` 文档是模块地图（功能到文件的对照见 `crates/zx-core/src/lib.rs`）；架构图与模块与 pi 的对应表见 README。

| 层 | 位置 | 职责 |
|---|---|---|
| CLI | `src/main.rs`、`src/cli.rs`、`src/cli/`、`src/run.rs`、`src/chat.rs`、`src/chat/`、`src/acp.rs`、`src/acp/`、`src/im.rs`、`src/process.rs`、`src/render.rs`、`src/session_files.rs`、`src/list_sessions.rs` | main 只分派子命令。`cli/settings.rs` 合并 `config.toml` 与命令行；`cli/shared.rs` 的 `Shared` 是一个进程的所有会话共用的东西（读一次设置、连一次 MCP、查一次密钥文件）；`cli/prepare.rs` 的 `prepare(shared, model, cwd, file)` 组装一个会话（每个会话只加载一次 agent）；本机还是容器执行在 `cli/execution.rs`。`process` 是退出码、stdout 行、关机信号与追踪导出的包装；`render` 是 run、chat、acp 共用的文案。chat 管理单一输入所有者、审批、信号和 REPL；acp 把会话映射到协议，stdout 只走协议；im 读取配置、为每个会话准备会话并隐藏 IM 凭据 |
| 配置 | `src/config/`、`agents/` | `environment.rs`：主目录与 `config.toml`；`definition.rs`：内置 agent 与替换它们的用户文件；`agent_file.rs`：front matter、说明与合并规则；`permissions.rs`：`[permissions]` 到策略的映射；项目上下文与 skill。不进入循环机制 |
| 提示词 | `crates/zx-core/src/prompt.rs` | 说明 + `<environment>` + `<project_context>` + `<available_skills>`（空段落不出现） |
| 共享会话 | `crates/zx-core/src/agent_session.rs` 与 `agent_session/` | 共享调用与 Busy、RAII 归还运行权、取消、空闲时切换模型、订阅 |
| 运行层 | `crates/zx-core/src/runner.rs` | 以独占借用控制单次运行、重试、上限、审批缓存、持久化和报告 |
| 会话 | `crates/zx-core/src/session.rs` 与 `session/` | 只追加的条目树与校验、投影、恢复所需的查询；记录经 `SessionLog` 保存，默认是单写者的 JSONL 文件（`jsonl.rs`） |
| 上下文管理 | `crates/zx-core/src/compaction.rs` 与 `compaction/`、`session/summary.rs` 与 `session/summary/`、`crates/zx-tools/src/output_store.rs` | 宿主摘要（全量，读取原始投影）、长结果脱敏落盘与分页 |
| agent | `crates/zx-core/src/agent/` | 回合循环、工具执行、Hooks 宿主、事件、消息队列、活跃工具集、扩展的斜杠命令 |
| ai | `crates/zx-ai/src/`、`crates/zx-ai-openai/src/` | 消息类型、系统提示词段落、流式事件协议、provider 契约与重试判断、共用的 `sync` 与 `text`；OpenAI 兼容 provider 与 SSE 解析 |
| 外层策略与交互 | `crates/zx-tools/src/policy/`、`continuation.rs`、`redact.rs`，`crates/zx-core/src/extensions.rs`、`agent/interaction.rs` | 命令与规则策略、续写、已知密钥脱敏、扩展的打包与注册、宿主审批与 NoHuman |
| 工具 | `crates/zx-tools/src/tools/` | 内置工具（写文件的工具共用按文件串行的 `FileMutationQueue`；搜索工具经执行器调用 ripgrep 或 POSIX 程序）与输出截断 |
| 执行环境 | `crates/zx-tools/src/executor/` | `LocalExecutor`（进程组、超时与取消、输出捕获上限）与 `ContainerExecutor`（每个进程一个 Docker 容器，容器内按进程组终止命令）；`capture.rs` 的 `Capture` 两者共用 |
| IM 接入 | `crates/zx-im/src/`、`crates/zx-im-slack/`、`crates/zx-im-feishu/` | 平台接口 `ImChannel`；`Server` 做访问控制并按会话分派，每个会话（私聊或线程）一个任务，任务之间只经 channel 通信；宿主经 `SessionFactory` 打开会话；可选的 `StatusStore` 让重启后替换遗留的“处理中…”；平台共用的辅助在 `platform.rs`；Slack 与 ⚑ D:5/CHINA FLAG: 飞书的实现各成 crate |
| MCP 客户端 | `crates/zx-tools/src/mcp.rs` | 经 Streamable HTTP 连接 MCP 服务器，只把配置中列出的工具交给模型；`Server` 是一个 `Extension`；调用有超时、可取消、结果截断，工具一律按有副作用处理 |
| 追踪导出 | `src/otel.rs`、`src/otel/` | 设置 OTLP 地址时把运行事件转成 span 经 OTLP/HTTP 导出；只含元数据，事件经有界通道交给后台任务，满时丢弃，运行不等待导出 |

一次运行的数据流：`main` 组装 `AgentSession` → `prompt` 独占取得 Runner → `Runner::run` 开始一次运行（`Run`），每次尝试是一个 `Attempt` → 尝试先持久化提示词、输入和上下文改动，再经宿主准备步骤执行可选摘要 → 新增插话与钩子改动持久化后请求模型，发出的上下文就是此刻的会话投影 → 执行工具、脱敏、可选落盘 → 循环不保存对话副本，每一步先写入会话宿主（crate 内部的 `agent::SessionHost`；`SessionStore` 自身就是宿主，Runner 的 `RunHost` 在其上加摘要），再发出事件 → `Run` 处理重试并汇总 `RunReport`，由 `main` 映射退出码。自动摘要单独记录、消耗请求预算且不重试；手动 compact 共用独占权并独立报告。

设计取舍（改动前先理解原因；必须保持的约束见"不变量"）：

- **场景策略不进内核**：提示词、运行上限与续写默认值由 agent 文件提供；命令策略、续写、脱敏由外层注册，无人时拒绝 Ask。`run` 用退出码 0 到 9 表达结果（9 只在 `--fail-on-denied` 时出现），调度器不必解析输出。
- **配置与文件内容分工**：`config.toml` 决定环境，agent 决定角色；命令行只剩 `--model`、`--tools`、预算与会话参数，显式传参覆盖文件；agent 文件写了的键整个替换继承的值。没有项目级配置文件：项目内容不能改变环境或权限，项目上下文和 skill 正文也不能更改控制策略。自动发现的 AGENTS.md 解析后必须位于项目根目录内，`run` 的默认 agent 不自动发现，`run` 默认不落盘。skill 启动时只读元数据，正文按需读取并经过既有流水线。
- **共享 API、独占执行**：AgentSession 的短锁只切换 Runner 所有权，不跨 await。重叠 prompt 和运行中改配置返回 Busy；取消、panic 或 future 丢弃后由 RAII 归还。订阅列表先取快照再调用，panic 的 subscriber 被隔离并摘除，不持锁调用宿主回调。
- **会话既是审计日志，也是模型上下文的唯一来源**：修正上下文靠追加 `context_edit`，提示词变化靠追加 system 消息，原始记录始终保留。可替换的只是最底层的记录保存（`SessionLog`）。`SessionStore` 负责先写后改内存、写失败后停写、每种记录的格式与校验（追加与加载用同一套）、投影和恢复所需的查询；摘要请求、补写中断调用的结果等执行在上层（`compaction`、`runner`）。持久化经会话宿主以 `Result` 返回错误，`EventSink` 只接收事件。
- **摘要不替代证据**：开始、成功或失败都有记录；每次摘要都读取原始投影，保留全部 system，不读旧摘要（摘要源至多 2 MiB，超过后要开新会话）。摘要引用只校验来源存在；原始事实被修正后，摘要整份失效。
- **终端输入只有一个所有者**：chat 的专用线程通过有界通道提供逐行输入；REPL 协调提问、审批和信号，Interaction 不直接读 stdin。审批回复必须匹配主机生成的请求编号，取消后失效。按 Tokio stdin 源码对交互终端的建议，避免不可取消的 stdin 阻塞任务卡住运行时退出。
- **事实由程序提供，不让模型去查**：操作系统、启动时间在启动时写进系统提示词，不浪费轮次，也不会被权限拦住。
- **兼容性默认保守，能力按需打开**：提示词更新默认合并到开头（`collapse`，与 pi 对 OpenAI 兼容服务的默认一致），服务端支持时才按位置发送（`inline`）。
- **安全靠部署隔离**：策略只是第一道防线（检查边界见 README 的"安全模型"）。Unix 上新建会话文件的权限为 0600，打开已有文件不修改权限；真正的隔离要靠不同账号或容器。
- **扩展点都是 trait**：`Provider`、`Tool`、`ToolPolicy`、`Hooks`、`Interaction`、`EventSink`、`Executor`、`SessionLog`、`ImChannel`、`StatusStore`、`Extension`。新能力优先以实现 trait 的方式加入，不改循环本身。
- **通用，不为场景定制**：代码只定义接口与通用机制。某个环境、平台或厂商才有的知识（kubectl 的只读选项、Kubernetes 的 token 路径、IM 文案的语言、告警的格式）写在 agent、部署清单或配置里，或由外部适配器实现接口、调用 `run` 或 SDK；不在代码里解析某个系统的私有格式。provider 对协议方言的处理也要显式开启（如 `provider.fillMissingReasoning`），不按厂商推断。
- **界面交给 ACP 客户端**（D13）：交互式 UI 由实现 ACP 的开源客户端提供，`acp` 只做协议与会话的映射，协议类型以 SDK 的 `v1::schema` 为准；`chat` 冻结为应急与脚本化入口，不再加功能；`im` 按 D16 解冻；`run` 保留并维护。
- **同层互不依赖，入口只分派**：同一层的模块互不引用（run、acp、sessions 不引用 chat，两个 IM 平台互不引用），要共用的下沉到共同的下层（`render`、`process`、`zx_im::platform`）。拆分与重复的判断见规则 09。
- **分层由 crate 边界保证**：内核引用不到工具、执行环境、策略、MCP、IM 和 CLI，只依赖 `zx-core` 与 `zx-ai` 的嵌入方不会引入 reqwest、rmcp、tokio-tungstenite 和 OpenTelemetry。新模块放进它所属的层；需要引用上层时，说明分层或接口该改了。各 crate 在根部把下层引入为 `crate::ai`、`crate::agent`、`crate::sync` 等；跨 crate 使用、但不属于 API 的项写成 `#[doc(hidden)] pub` 并在文档注释里注明（如 `zx_ai::sync`、`SystemPrompt::has_section`）。

## 命令

- 以下命令都在仓库根目录运行，覆盖全部 8 个 crate（test、clippy 与 doc 经 `Cargo.toml` 的 `default-members`）。在 `crates/*` 的目录里运行或加 `-p <crate>` 时只处理那一个 crate；集成测试都在根目录的 `tests/`，属于 `zx-agent`
- 格式：`cargo fmt`（`rustfmt.toml`：`max_width = 120`）
- lint：`cargo clippy --all-targets -- -D warnings`，要求零警告。范围是 clippy 的默认项加 `Cargo.toml` 的 `[workspace.lints]`（阈值在 `clippy.toml`）；有意保留的写法用 `#[expect(lint, reason = "...")]`，见规则 09
- 测试：`cargo test`，不需要网络和 API key。只跑一个集成测试文件：`cargo test --test agent_loop`；按名字过滤：`cargo test retries_transient`。付费的 `tests/real_api.rs` 默认跳过，见红线 9
- 文档：`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`，要求零警告；断掉的文档链接也算（引号必须有，否则 shell 把 `warnings` 当命令）
- MSRV：`cargo +1.88.0 check --all-targets`，用到新语法或新标准库 API 时运行（本机已安装 1.88.0 工具链）
- 自查：`.claude/scripts/self-check.sh`，列出需要说明理由的写法（`Rc` / `RefCell` / `unsafe`、`#[allow]`、生产代码里的 `unwrap()`、`let _ =`、调试宏）。每一处命中都要能说出理由

改完代码后，格式、lint、测试、文档四条命令都要通过。报告时附上 `test result` 那几行原始输出；没有实际运行，就不能说"已编译通过"或"测试通过"。

## 工作方式

- 用户提问时先回答问题，再动手修改。
- 只改任务需要的部分。顺手发现的问题单独提出，不要把无关的重构、改名、格式调整混进改动。

## 测试

原则（单元测试和集成测试都适用）：

- **测行为，不测实现。** 断言外部可观察的结果：返回值、事件顺序、工具结果、会话文件内容、退出码。重构内部实现时不需要改测试。
- **一个测试只验证一种行为**，测试名说清楚是什么行为，例如 `retries_transient_errors_without_repeating_the_prompt`。
- **修 bug 先写回归测试**，并确认它在修复前失败。
- **新功能至少覆盖**：成功路径、每一类失败、取消与超时、边界（空输入、刚好截断、多字节字符被切开）。
- **确定、隔离、快速。** 不访问网络（付费 e2e 除外）；不依赖执行顺序和全局状态；不用 `std::env::set_var`；文件只写到 `tempfile::tempdir()`；不用 `sleep` 等结果，而是等待对应的事件或请求。
- **复用测试设施，不复制粘贴。** 清单见 `.claude/rules/06-testing.md`，每个测试文件覆盖什么见 `docs/TESTS.md`。

| 层次 | 位置 | 测什么 | 怎么驱动 |
|---|---|---|---|
| 单元测试 | 被测模块里的 `#[cfg(test)] mod tests`，超过约 300 行时在 `foo/tests.rs`（规则 09） | 单个模块的逻辑和边界，可以测私有函数；包括会话文件和事件流的 JSON 格式 | 直接调用 |
| 集成测试 | `tests/*.rs` | 跨模块的契约，只用公开 API：循环语义、运行层、HTTP provider、命令行 | `FauxProvider`、本地假 HTTP 服务、编译出的程序 |
| 付费 e2e | `tests/real_api.rs` | 与真实模型 API 的兼容性 | 默认跳过，经用户授权才运行（红线 9） |

## 红线

与 `.claude/rules/` 里的规则冲突时，以这里为准。

1. 不用 `Rc<RefCell<_>>`、`Arc<Mutex<_>>`、`.clone()`、`'static`、`Box::leak`、`unsafe` 来绕过借用检查器。遇到借用错误，先按 `.claude/rules/02-ownership-borrowing.md` 的顺序重新设计数据流。
2. 生产代码不用 `unwrap()`。`expect("原因")` 只用于可以证明成立的不变量。测试代码可以用 `unwrap()`。
3. 不用 `#[allow(...)]` 压制警告。确实需要例外时用 `#[expect(lint, reason = "...")]`。唯一的例外见 `.claude/rules/01-code-style.md`。
4. 持有 `std::sync::Mutex` 的 guard 时不 `.await`；async 代码里不调用阻塞 API。
5. 不新增依赖、不升级依赖、不修改 `rust-version`、不修改 `edition`，除非用户明确同意。提议新增依赖时要说明的内容见 `.claude/rules/08-dependencies-msrv.md`；已获授权、按阶段引入的依赖见 `docs/ROADMAP.md` 的决策 D2。
6. 不使用 Rust 1.88 之后才稳定的语言特性和标准库 API。标准库 API 由 clippy 的 `incompatible_msrv` 检查；语言特性 clippy 查不出，用 `cargo +1.88.0 check --all-targets` 验证。
7. 测试失败时先查实现，不改断言去迁就错误的实现，不删除或 `#[ignore]` 失败的测试。`#[ignore]` 只用于付费的真实 API 测试。
8. 不编造 crate、API、feature 名。不确定时查 `~/.cargo/registry/src/` 里对应版本的源码。
9. 未经用户明确授权，不运行调用真实模型 API 的测试（`cargo test --test real_api -- --ignored`），也不替用户设置 `ZX_E2E_AUTHORIZED`。这些测试会产生费用。

## 不变量

修改相关模块时必须保持以下不变量。每一条都有测试覆盖，测试失败说明不变量被破坏了：

1. **provider 不报错。** `Provider::stream` 立即返回；所有失败都编码成最终的 `StreamEvent::Error`，其 `stop_reason` 为 `Error` 或 `Aborted`；接收端存续时，每个流恰好以一个 `Done` 或 `Error` 结束。通过有界事件通道发送，临时性失败优先使用 `ProviderError` 判断（`crates/zx-ai/src/provider.rs`、`crates/zx-ai/src/retry.rs`）。
2. **由模型决定何时停止。** 工具失败（找不到、参数无效、被策略拒绝、执行报错、panic）都变成 `is_error` 的工具结果，循环继续（`crates/zx-core/src/agent/agent_loop.rs`）。
3. **并行执行工具分三个阶段：** 逐个检查 → 并发执行 → 按调用顺序写结果。只要有一个工具是 `Sequential`，整批改为串行。
4. **回复因 `Length` 截断时，不执行其中的工具调用。**
5. **会话只追加。** 不改写、不删除已写入的记录；`SessionLog::append` 成功后才更新内存（`crates/zx-core/src/session.rs`）。唯一的例外是打开 JSONL 文件时截掉崩溃或写入失败留下的未完成尾记录（在其余记录校验通过之后），并报告删除的字节数；一次写入失败、超时或被中断（结果未知）后不再写入，保证残缺只出现在末尾。JSONL 文件同一时间只有一个写入者（`flock`，`crates/zx-core/src/session/jsonl.rs`）。只读写当前格式（`SESSION_VERSION`）：头部是其他版本的会话一律拒绝打开，不升级、不兼容读取；格式变化时提高版本号。
6. **发给模型的上下文就是会话投影，系统提示词也在其中。** 系统提示词以 system 消息记录，变化时只追加变化的段落，不改写之前的版本（`crates/zx-ai/src/system_prompt.rs`）。重试时用 `context_edit` 省略失败的回答再续跑，不重复用户消息（`crates/zx-core/src/runner.rs`）。
7. **会话写入失败时立即停止运行**（审计要求）：会话宿主返回错误，循环不再执行后续步骤，运行以 `Error` 结束，即使同时被取消。
8. **工具输出必须截断**（`crates/zx-tools/src/tools/truncate.rs`）；耗时的工具必须响应 `ToolContext::cancel`。
9. **认证用的 API key 只从环境变量读取**，不主动写入会话文件、事件或日志；其环境变量从本机执行的 bash 与搜索子进程中删除（`LocalExecutor::hidden_env`）。CLI 的 `RedactSecrets` 在工具输出持久化前过滤认证 key 的完整字面值；不识别未知或变形的密钥，也不覆盖输入、模型输出与参数，边界见 README 的"安全模型"。
10. **有副作用的工具调用先记录开始再执行，恢复时不重放。** `Tool::effects()` 为 `SideEffects` 的调用通过检查与审批后，先追加 `tool_call_started`，写入失败则不执行（`crates/zx-core/src/agent/agent_loop.rs`）。Runner 每次运行开始时，为分支末尾缺少结果的调用补写错误结果，按开始记录区分结果未知与未执行；以 `Error` 或 `Aborted` 结束的回答中的调用从不执行，一律记为未执行（`crates/zx-core/src/session/recovery.rs`、`crates/zx-core/src/runner.rs`）。

## 新增功能放在哪里

- **新 provider**：实现 `ai::Provider` 并遵守不变量 1。为解析逻辑写单元测试，并在 `tests/` 下用本地假 HTTP 服务（`tests/common/mock_server.rs`）写端到端测试，参照 `tests/openai_mock.rs`。
- **新工具**：实现 `agent::Tool`。参数定义成带 `#[serde(deny_unknown_fields)]` 的结构体再反序列化；输出要截断；要支持取消；不会改变任何状态的工具让 `effects()` 返回 `Effects::ReadOnly`，其余保留默认的 `SideEffects`（不变量 10）。要容忍模型常犯的参数错误时覆盖 `prepare_arguments`，只依据参数本身改写。测试覆盖成功、参数错误、超时或取消三种情况。
- **新策略**：内置策略实现 `agent::ToolPolicy`，通过 `policy::PolicyHook` 注册（运行在 `Policy` 阶段），无法判断时返回 `PolicyDecision::Deny`；扩展的放行判断写在 `Hooks::before_tool`。
- **新扩展**：实现 `agent::Hooks`，按需覆盖默认方法；每个回调最后一个参数是 `&HookContext`（会话、运行、工作目录、只读的取消信号）。回调必须异步让出执行权。`transform_context` 不能追加 system 消息，系统提示词只来自配置。错误、panic、取消与超时由宿主处理，规则见 README。阶段（`Hooks::stage`）默认 `Extension`，只有内置的策略适配、脱敏与落盘声明其他阶段。用 `HookChain::push_with` 注册时给出名称。钩子输入输出的 JSON 格式由 `tests/hook_contract.rs` 固定，改动前先确认钩子进程与审批转发能否兼容。
- **打包的扩展**：工具、钩子、提示词段落与命令要一起注册时，实现 `extensions::Extension`，经 `extensions::register` 加入 `LoopConfig`。扩展的钩子只能声明 `Extension` 阶段；段落名不能是 `preamble`、`prompt::GENERATED_SECTIONS` 中的名字或提示词中已有的段落；命令名只能是 ASCII 字母、数字、`_`、`-` 且不重名。运行中要发消息、记状态或改工具集，经 `HookContext` 的 `queues` 与 `active_tools`，不直接碰会话（运行期间会话归循环独占，状态在下一回合开始或运行结束时写入）。行为由 `tests/extensions.rs` 与 `tests/extension_capabilities.rs` 覆盖。
- **上下文改动**：`on_input` 完成后才写入原始输入，改写或拒绝再追加 `context_edit`，钩子被取消时不写入；`transform_context` 返回 `ContextEdit` 计划，整份计划先校验，逐项写入成功后再请求模型。
- **新的会话存储**：实现 `session::SessionLog`。`append` 在记录保存后才返回，同一 `seq` 已有记录时报错；新会话由后端保存头部后交给 `SessionStore::with_log`，续接时读出 `LoadedSession` 交给 `SessionStore::load`。用内存实现测往返，参照 `tests/runner.rs` 的 `TableLog`。
- **新 IM 平台**：在独立的 crate 中实现 `zx_im::ImChannel`（参照 `crates/zx-im-slack`），遵守其文档列出的职责：立即确认投递、按消息 ID 去重（`zx_im::Seen`）、只转交发给机器人的消息、遵守限频、拆分超长文本（`zx_im::split_text`）。文案用 `CardLabels`，不写死语言或标点。协议用本地假服务测；与 `Server` 的配合参照 `tests/im_server.rs` 的 `FakeChannel`；Slack 的协议测试参照 `tests/im_slack.rs` 与 `tests/common/fake_slack.rs`。
