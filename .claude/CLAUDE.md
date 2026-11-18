# zx-agent 项目说明

## 背景

- **是什么**：Rust 编写的通用 agent 内核，同一个内核支撑和人对话、无人值守运行（cron、Kubernetes Job）与被其他程序嵌入。场景差异放在内核之外，由 agent 文件、提示词和插件表达。
- **参照**：分层、循环语义和会话格式沿用 pi（https://github.com/earendil-works/pi ，commit `a92283056`），不同之处见 `docs/REFERENCE.md` 的"与 pi 的差异"。
- **现状**：v0.1.0。程序 `zx-agent` 的子命令 `run`、`chat`（应急与脚本化入口）、`acp`、`sessions`；IM 服务是另一个程序 `zx-im`（Slack 与 ⚑ D:5/CHINA FLAG: 飞书）。环境在主目录（`~/.zx-agent`，`ZX_AGENT_HOME` 可改）的 `config.toml`，角色在 agent 文件。改动前先看 `docs/ROADMAP.md` 的当前阶段。

## 架构

10 个 crate，上层依赖下层；模块地图不在这里重复，看各 crate 的 `lib.rs` 与父模块的 `//!`。

| crate | 位置 | 负责 |
|---|---|---|
| `zx-util` | `crates/zx-util` | 各 crate 共用的小工具：加锁（`sync`）、文本截短（`text`）、随机数（`random`）、判断是否为默认值（`default` 的 `is_default`，供 serde 省略默认值）、读环境变量（`env` 的 `env_value`：设了且不为空才算）；不属于 API。只收没有业务含义、至少两个 crate 要用、只依赖标准库与 tokio 的函数，其余留在用它的 crate |
| `zx-ai` | `crates/zx-ai` | 消息类型、流式事件、`Provider` 契约 |
| `zx-ai-openai` | `crates/zx-ai-openai` | OpenAI 兼容的 provider；模型层里只有它依赖 reqwest，只依赖 `zx-ai` |
| `zx-core` | `crates/zx-core` | 内核：入口 `Agent`、`Session`、`Run`，循环，会话记录与存储接口，摘要，工具、策略、钩子、扩展的接口 |
| `zx-builtin` | `crates/zx-builtin` | 随 SDK 附带的实现（库中名为 `zx_agent::builtin`）：内置工具、执行环境（本机与容器）、命令与规则策略、续写、脱敏、落盘、会话文件存储（JSONL）、MCP |
| `zx-agent` | `crates/zx-agent` | SDK：把 `zx-core`、`zx-ai`、`zx-ai-openai`、`zx-builtin` 再导出成一个库，没有自己的代码 |
| `zx-app` | `crates/zx-app` | 应用层：`config`（`config.toml` 与 agent 文件）、由它们装配 agent 的 `Application`、`otel`（追踪导出）、`shutdown`（信号、取消后读完运行、追踪导出的开始与结束）；不知道命令行、终端与协议，不打印（要说的返回给宿主），命令行、IM 服务与其他宿主（如 Web 服务）共用 |
| `zx-cli` | `crates/zx-cli` | 命令行（都在库里，`main.rs` 只调用它；可执行文件仍叫 `zx-agent`）：参数、输出与退出码，`run`、`chat`、`acp`、`sessions` 互不引用，agent 都经 `zx_app::Application` 得到 |
| `zx-im` | `crates/zx-im` | IM 服务（都在库里，`main.rs` 只调用它；可执行文件 `zx-im`）：平台接口 `ImChannel` 与它的词汇（`channel`），按会话分派的 `Server`（`server`），平台与它们共用的部分（`platform`：Slack，⚑ D:5/CHINA FLAG: 飞书在默认开启的 feature `feishu` 后），程序（`program`：`im.toml`、参数、操作日志与退出码）。agent 经 `zx_app::Application` 得到 |
| `zx-testing` | `crates/zx-testing` | 各包集成测试共用的测试设施：由 agent 建会话、读完运行的测试设置（`RunnerSetup`），假的模型服务、MCP 服务器与聊天平台，被测程序的启动与主目录（`program`）。只作 `[dev-dependencies]`，crate 自己的单元测试不用它；不依赖 `zx-im` |

设计取舍（改动前先理解原因；括号里是 `docs/ROADMAP.md` 的决策编号，契约细节在各类型的 rustdoc）：

- **机制在内核，场景在外层**（D10）：提示词、上限与续写默认值来自 agent 文件，命令策略、续写与脱敏由外层注册；某个环境、平台或厂商的知识写在 agent、部署清单或配置里，代码不解析私有格式；provider 的协议方言显式开启（如 `provider.fillMissingReasoning`），不按厂商推断。
- **环境、角色与项目分开**（D17）：`config.toml` 决定环境，agent 决定角色，命令行只选模型、工作目录、工具集、预算、会话与输出，不设环境与权限。没有项目级配置文件：项目内容（含 AGENTS.md 与 skill 正文）不能改变环境、权限或控制策略。
- **`Agent` 是唯一入口**（D21）：`AgentConfig` 是所有会话共用的角色，属于单个会话的工具、钩子与提示词段落由宿主的 `SessionSetup` 给出。`Session` 是可克隆的句柄，短锁只切换 Runner 所有权、不跨 await，重叠调用返回 Busy；`send`、`continue_run`、`compact` 返回 `Run`，读它才推进，结束、取消、panic 或丢弃后由 RAII 归还。事件只从 `Run` 读出，观察所有运行用 `Hook::on_event`。
- **会话是审计日志，也是模型上下文的唯一来源**（不变量 5、6）：修正上下文与提示词的变化都靠追加，原始记录始终保留；可替换的只是存储（`SessionStorage` 与 `SessionLog`，接口里没有文件路径）。
- **审批由内核等待，不是接口**：策略询问时调用在 `ApprovalGate` 等待；`ApprovalMode::Deny`（默认）立即拒绝，`Wait` 等宿主经 `Session::resolve_approval` 按内核生成的审批编号回答；超时与取消按拒绝处理，审批不写入会话。
- **新能力优先实现 trait，不改循环**（见下文"新增功能放在哪里"；执行环境 `Executor` 与 IM 的 `StatusStore` 也是 trait）。调用者身份（会话的标签、输入的 actor）经 `PolicyContext` 与 `HookContext` 只传给策略与钩子，内核不据此判断。
- **分层由 crate 边界保证**（D24）：内核引用不到工具、执行环境、策略、MCP、IM 和 CLI；库 `zx-agent` 引用不到命令行、配置文件、追踪导出与 IM；`zx-app` 引用不到命令行、ACP 与 IM；`zx-util` 不依赖工作区里任何 crate；`zx-testing` 只是测试依赖；同层模块互不引用（规则 09），`zx-im` 的两个平台也是：它们是 `platform` 下的同层模块，内部的项只对自己的模块可见，共用的在 `platform.rs`。跨 crate 使用但不属于 API 的项写成 `#[doc(hidden)] pub` 并注明。
- **界面交给 ACP 客户端**（D13），`chat` 不再加功能。**安全靠部署隔离**：策略只是第一道防线（`docs/SECURITY.md`）。

## 命令

都在仓库根目录运行，覆盖全部 10 个 crate（根目录的 `Cargo.toml` 是虚拟 workspace，不是包，所有 crate 都在 `crates/` 下）；在 `crates/*` 里运行或加 `-p <crate>` 只处理那一个。

- 格式：`cargo fmt`（`rustfmt.toml`：`max_width = 120`）
- lint：`cargo clippy --all-targets -- -D warnings`，零警告；改动内核的工具校验时另跑 `cargo clippy -p zx-core --no-default-features --all-targets -- -D warnings`（按 JSON Schema 校验参数是 `zx-core` 默认开启的 feature `json-schema`），改动 `zx-builtin` 时另跑 `cargo clippy -p zx-builtin --no-default-features --all-targets -- -D warnings`（MCP 客户端是它默认开启的 feature `mcp`），改动 `zx-im` 时另跑 `cargo clippy -p zx-im --no-default-features --all-targets -- -D warnings`（⚑ D:5/CHINA FLAG: 飞书平台是它默认开启的 feature `feishu`）。有意保留的写法用 `#[expect(lint, reason = "...")]`
- 测试：`cargo test`，不需要网络和 API key。只跑一个文件：`cargo test --test step_loop`；按名字过滤：`cargo test retries_transient`
- 文档：`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`，零警告，断掉的文档链接也算（引号必须有）
- MSRV：`cargo +1.88.0 check --all-targets`，用到新语法或新标准库 API 时运行
- 自查：`.claude/scripts/self-check.sh`，列出需要说明理由的写法；每一处命中都要能说出理由

改完代码后，格式、lint、测试、文档四条命令都要通过（本会话改过 Rust 代码时，Stop hook `.claude/hooks/stop-check.sh` 会运行它们）。报告时附上 `test result` 那几行原始输出；没有实际运行，就不能说"已编译通过"或"测试通过"。

## 工作方式

- 用户提问时先回答问题，再动手修改。
- 只改任务需要的部分。顺手发现的问题单独提出，不要把无关的重构、改名、格式调整混进改动。
- 测试的原则与约定见 `.claude/rules/06-testing.md`，每个测试文件覆盖什么见 `docs/TESTS.md`。

## 红线

与 `.claude/rules/` 里的规则冲突时，以这里为准。

1. 不用 `Rc<RefCell<_>>`、`Arc<Mutex<_>>`、`.clone()`、`'static`、`Box::leak`、`unsafe` 来绕过借用检查器。遇到借用错误，先按 `.claude/rules/02-ownership-borrowing.md` 的顺序重新设计数据流。
2. 生产代码不用 `unwrap()`。`expect("原因")` 只用于可以证明成立的不变量。测试代码可以用 `unwrap()`。
3. 不用 `#[allow(...)]` 压制警告。确实需要例外时用 `#[expect(lint, reason = "...")]`。唯一的例外见 `.claude/rules/01-code-style.md`。
4. 持有 `std::sync::Mutex` 的 guard 时不 `.await`；async 代码里不调用阻塞 API。
5. 不新增依赖、不升级依赖、不修改 `rust-version`、不修改 `edition`，除非用户明确同意（`.claude/rules/08-dependencies.md`；已获授权的依赖见 `docs/ROADMAP.md` 的决策 D2）。
6. 不使用 Rust 1.88 之后才稳定的语言特性和标准库 API。标准库 API 由 clippy 的 `incompatible_msrv` 检查；语言特性用 `cargo +1.88.0 check --all-targets` 验证。
7. 测试失败时先查实现，不改断言去迁就错误的实现，不删除或 `#[ignore]` 失败的测试。`#[ignore]` 只用于付费的真实 API 测试。
8. 不编造 crate、API、feature 名。不确定时查 `~/.cargo/registry/src/` 里对应版本的源码。
9. 未经用户明确授权，不运行调用真实模型 API 的测试（`cargo test --test real_api -- --ignored`），也不替用户设置 `ZX_E2E_AUTHORIZED`。这些测试会产生费用。

## 不变量

修改相关模块时必须保持；每一条都有测试覆盖，测试失败说明不变量被破坏了。

1. **provider 不报错。** `Provider::stream` 立即返回；所有失败都编码成最终的 `StreamEvent::Error`，其 `stop_reason` 为 `Error` 或 `Aborted`；接收端存续时，每个流恰好以一个 `Done` 或 `Error` 结束（`crates/zx-ai/src/provider.rs`、`retry.rs`）。
2. **由模型决定何时停止。** 工具失败（找不到、参数无效、被策略拒绝、执行报错、panic、同一回答里重复的调用编号）都变成 `is_error` 的工具结果，循环继续（`crates/zx-core/src/runner/step_loop/tools.rs`）。
3. **并行执行工具分三个阶段：** 逐个检查 → 并发执行 → 按调用顺序写结果。只要有一个工具是 `Sequential`，整批改为串行。
4. **回复因 `Length` 截断时，不执行其中的工具调用。**
5. **会话只追加。** 不改写、不删除已写入的记录；`SessionLog::append` 成功后才更新内存（`crates/zx-core/src/transcript.rs`）。唯一的例外是 JSONL 文件在打开之后、追加下一条记录之前，截掉崩溃或写入失败留下的未完成尾记录。一次写入失败、超时或被中断后不再写入，会话在那次操作结束时自行关闭。一个会话同一时间只有一个写入者（JSONL 用 `flock`）。只读写当前格式（`SESSION_VERSION`）：其他版本一律拒绝，不升级、不兼容读取；格式变化时提高版本号。
6. **发给模型的上下文就是会话投影，系统提示词也在其中。** 系统提示词以 system 消息记录，变化时只追加变化的段落（`crates/zx-ai/src/system_prompt.rs`）。重试时用 `context_edit` 省略失败的回答再续跑，不重复用户消息（`crates/zx-core/src/runner/failure.rs`）。
7. **会话写入失败时立即停止运行**（审计要求）：循环不再执行后续步骤，运行以 `Failed` 结束，即使同时被取消。
8. **工具输出必须截断**（`crates/zx-builtin/src/tools/truncate.rs`）；耗时的工具必须响应 `ToolContext::cancel`；文件写入一旦开始就写完，调用被丢弃也一样（`Executor::write_file`）。
9. **认证用的 API key 只从环境变量读取**，不主动写入会话文件、事件或日志。命令行程序把它的环境变量从本机执行的 bash 与搜索子进程中删除（`LocalExecutor::hidden_env`），并用 `RedactSecrets` 在工具输出持久化前过滤 key 的完整字面值；边界见 `docs/SECURITY.md`。
10. **有副作用的工具调用先记录开始再执行，恢复时不重放。** `Tool::effects()` 为 `SideEffects` 的调用通过检查与审批后，先追加 `tool_call_started`，写入失败则不执行（`runner/step_loop/tools.rs`）。每次运行开始时（以及空闲时经 `Session::append_custom_message` 追加消息之前），为分支末尾缺少结果的调用补写错误结果，按开始记录区分结果未知与未执行（开始记录在已离开的分支上也算）；以 `Error` 或 `Aborted` 结束的回答中的调用从不执行（`transcript/recovery.rs`、`runner/interrupted.rs`）。

## 新增功能放在哪里

契约的细节（出错条件、调用次数、校验规则）在各 trait 的 rustdoc，以它为准。

| 新能力 | 做法 | 参照 |
|---|---|---|
| provider | 实现 `ai::Provider`，遵守不变量 1；每个失败归入 `ProviderErrorKind`（内核只按它重试与恢复溢出），服务的状态、错误码与措辞属于哪一种只写在 provider 的 crate 里；解析与归类写单元测试 | `crates/zx-agent/tests/openai_mock.rs` |
| 工具 | 实现 `tools::Tool`：参数是带 `#[serde(deny_unknown_fields)]` 的结构体，输出截断，响应取消，不改状态的 `effects()` 返回 `Effects::ReadOnly`。共用的进 `AgentConfig::tools`，绑定会话目录或执行环境的由 `SessionSetup` 给出 | `crates/zx-builtin/src/tools/` |
| 策略 | 实现 `policy::ToolPolicy`，经 `PolicyChain::push` 进 `AgentConfig::policies`；无法判断时返回 `Deny`，所有策略取最严的结论。钩子不做放行判断 | `crates/zx-builtin/src/policy/` |
| 钩子 | 实现 `hooks::Hook`：回调必须异步让出执行权，`transform_context` 不能追加 system 消息。经 `HookChain::push(名字, Arc)` 注册，名字出现在 `hook_failed` 事件与追踪里；共用的进 `AgentConfig::hooks`，单个会话的进 `SessionParts::hooks` | `crates/zx-agent/tests/hook_contract.rs` |
| 打包的扩展 | 实现 `extensions::Extension`，进 `AgentConfig::extensions`：建 `Agent` 时向它要一次并校验，所有会话共用。运行中发消息、记状态或改工具集经 `HookContext` 的 `queues` 与 `active_tools` | `crates/zx-builtin/src/mcp.rs` |
| 会话存储 | 实现 `transcript::SessionStorage` 与 `SessionLog`，进 `AgentConfig::storage`：`append` 存好才返回，一个会话一个写入者，接口里不出现文件路径，列出时用 `SessionList::new`、读头部时用 `SessionQuery::arrange_headers` 排序与截断 | `transcript/memory.rs`；只换写入方式时 `crates/zx-agent/tests/runner.rs` 的 `TableLog` |
| 摘要器 | 实现 `compaction::Summarizer`，进 `AgentConfig::summarizer`；何时摘要、读哪一段、记录与校验仍由内核做 | `crates/zx-agent/tests/summarizer.rs` |
| 单个会话的部分 | 实现 `agent::SessionSetup`，返回 `SessionParts`（工具、钩子、提示词段落），并经 `section_names` 声明它给出的段名 | `crates/zx-app/src/assembly/setup.rs` |
| 接入方式（如 Web 服务） | 独立的 crate，依赖 `zx-agent`（只要内核时依赖 `zx-core`；要与命令行共用 `config.toml` 与 agent 文件时依赖 `zx-app`，经 `Application` 得到 agent，信号与追踪导出的收尾用 `zx_app::shutdown`），构造 `Agent`，为每个对话创建或打开 `Session`，读它返回的 `Run`；不放进库 `zx-agent` | `crates/zx-cli`、`crates/zx-im` |
| IM 平台 | `zx-im` 的 `platform` 下的一个模块实现 `ImChannel`：立即确认投递、按消息 ID 去重（`Seen`）、只转交私聊、@ 机器人的群消息与群里线程中的回复、遵守限频（`Pacer`、`rate_limit_wait`）、拆分超长文本（`split_text`）、文案用 `CardLabels`，连接用 `next_message` 读（ping、静默与取消）。它的 `im.toml` 段、默认的变量名、要对命令隐藏的密钥变量与由段建 channel 的函数都在这个模块里；不引用别的平台，可选的平台放在 cargo feature 后 | `crates/zx-im/src/platform/slack.rs`；`crates/zx-im/tests/im_server.rs` 的 `FakeChannel` |
