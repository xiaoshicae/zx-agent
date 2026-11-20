# zx-agent 项目说明

## 背景

- **是什么**：Rust 编写的通用 agent 内核，同一个内核支撑和人对话、无人值守运行（cron、Kubernetes Job）与被其他程序嵌入。场景差异放在内核之外，由 agent 文件、提示词和插件表达。
- **参照**：分层、循环语义和会话格式沿用 pi（https://github.com/earendil-works/pi ，commit `a92283056`），不同之处见 `docs/REFERENCE.md` 的"与 pi 的差异"。
- **现状**：v0.1.0，两个程序：`zx-agent`（子命令 `run`、`chat`、`acp`、`sessions`）与 IM 服务 `zx-im`（Slack 与 ⚑ D:5/CHINA FLAG: 飞书）。改动前先看 `docs/ROADMAP.md` 的当前阶段。

## 架构

12 个 crate，都在 `crates/<名字>`，上层依赖下层；模块地图不在这里重复，看各 crate 的 `lib.rs` 与父模块的 `//!`。

| crate | 负责 |
|---|---|
| `zx-util` | 各 crate 共用的小工具：加锁、文本截短、随机数、判断默认值、读环境变量、按名字登记、不会 panic 的输出行；不属于 API。只收没有业务含义、至少两个 crate 要用、只依赖标准库与 tokio 的函数，其余留在用它的 crate |
| `zx-ai` | 消息类型、流式事件、`Provider` 契约 |
| `zx-ai-openai` | OpenAI 兼容的 provider；模型层里依赖 reqwest 的只有两个 provider 与它们共用的 `zx-ai-http`，都不依赖内核 |
| `zx-ai-anthropic` | Anthropic Messages API 的 provider；与 `zx-ai-openai` 互不引用 |
| `zx-ai-http` | 两个 provider 共用的 HTTP 部分：设置检查、取 key 与发请求、按限额读事件流、请求失败（`Failure`、Retry-After）；不属于 API。协议的写法与失败的归类留在各 provider |
| `zx-core` | 内核：入口 `Agent`、`Session`、`Run`，循环，会话记录与存储接口，摘要，工具、策略、钩子、扩展的接口 |
| `zx-builtin` | 随 SDK 附带的实现（库中名为 `zx_agent::builtin`）：内置工具、执行环境（本机与容器）、命令与规则策略、续写、脱敏、落盘、会话文件存储（JSONL）、MCP |
| `zx-agent` | SDK：把 `zx-core`、`zx-ai`、`zx-ai-openai`、`zx-ai-anthropic`、`zx-builtin` 再导出成一个库，没有自己的代码 |
| `zx-app` | 应用层，命令行、IM 服务与其他宿主（如 Web 服务）共用：`config.toml` 与 agent 文件（`config`）、由它们装配 agent 的 `Application`、追踪导出（`otel`）、进程收尾（`shutdown`）。不知道命令行、终端与协议（ACP、IM 平台），不打印，要说的返回给宿主 |
| `zx-cli` | 程序 `zx-agent`（代码都在库里，`main.rs` 只调用它）：参数、输出与退出码；agent 都经 `zx_app::Application` 得到 |
| `zx-im` | 程序 `zx-im`（同上）：平台接口 `ImChannel` 与由 `im.toml` 的段建出它的 `PlatformFactory`、按会话分派的 `Server`、各平台（Slack；⚑ D:5/CHINA FLAG: 飞书在默认开启的 feature `feishu` 后）、`im.toml`、参数、操作日志与退出码；agent 经 `zx_app::Application` 得到，宿主自己的程序经 `Program` 加平台与部件 |
| `zx-testing` | 各包集成测试共用的测试设施，只作 `[dev-dependencies]`；crate 自己的单元测试不用它，它不依赖 `zx-im` |

设计取舍（改动前先理解原因；括号里是 `docs/ROADMAP.md` 的决策编号）：

- **机制在内核，场景在外层**（D10）：提示词、上限与续写默认值来自 agent 文件，命令策略、续写与脱敏由外层注册；某个环境、平台或厂商的知识写在 agent、部署清单或配置里，代码不解析私有格式；provider 的协议方言显式开启（如 `provider.fillMissingReasoning`），不按厂商推断。
- **环境、角色与项目分开**（D17）：主目录（`~/.zx-agent`，`ZX_AGENT_HOME` 可改）的 `config.toml` 决定环境，agent 文件决定角色，命令行只选模型、工作目录、工具集、预算、会话与输出，不设环境与权限。没有项目级配置文件：项目内容（含 AGENTS.md 与 skill 正文）不能改变环境、权限或控制策略。
- **`Agent` 是唯一入口**（D21）：`AgentConfig` 是所有会话共用的角色，属于单个会话的工具、钩子与提示词段落由宿主的 `SessionSetup` 给出。`Session` 的 `send`、`continue_run`、`compact` 返回 `Run`，读它才推进；事件只从 `Run` 读出，观察所有运行用 `Hook::on_event`。
- **审批由内核等待，不是接口**：策略询问时，默认（`ApprovalMode::Deny`）立即拒绝，`Wait` 时调用在内核里等宿主经 `Session::resolve_approval` 回答；审批不写入会话。
- **新能力优先实现 trait，不改循环**（见下文"新增功能放在哪里"；执行环境 `Executor` 与 IM 的 `StatusStore`、`Backlog` 也是 trait）。调用者身份（会话的标签、输入的 actor）经 `PolicyContext` 与 `HookContext` 只传给策略与钩子，内核不据此判断。
- **分层由 crate 边界保证**（D24）：内核引用不到工具、执行环境、策略、MCP、IM 和命令行；同层模块互不引用（规则 08）。跨 crate 使用但不属于 API 的项写成 `#[doc(hidden)] pub` 并注明。
- **界面交给 ACP 客户端**（D13）：`chat` 只作应急与脚本化入口，不再加功能。**安全靠部署隔离**：策略只是第一道防线（`docs/SECURITY.md`）。

## 命令

根目录的 `Cargo.toml` 是虚拟 workspace，在根目录运行覆盖全部 crate；加 `-p <crate>` 或在 `crates/<crate>` 里运行只处理那一个。

- 格式：`cargo fmt`
- lint：`cargo clippy --all-targets -- -D warnings`，零警告。以下改动另跑 `cargo clippy -p <crate> --no-default-features --all-targets -- -D warnings`，检查关掉默认 feature 的构建：内核的工具校验（`zx-core`，feature `json-schema`）、`zx-builtin`（`mcp`，MCP 客户端）、`zx-im`（⚑ D:5/CHINA FLAG: `feishu`，飞书平台）
- 测试：`cargo test`，不需要网络和 API key。只跑一个文件：`cargo test --test step_loop`；按名字过滤：`cargo test retries_transient`
- 文档：`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`（引号不能省），零警告，断掉的文档链接也算
- MSRV：`cargo +1.88.0 check --all-targets`，用到新语法或新标准库 API 时运行
- 自查：提交前运行 `.claude/scripts/self-check.sh`，它列出的每一处都要能说出理由

改完代码后，格式、lint、测试、文档四条命令都要通过（用 Edit 或 Write 改过 `.rs` 或 `Cargo.toml` 时，Stop hook `.claude/hooks/stop-check.sh` 会运行它们）。报告时附上 `test result` 那几行原始输出；没有实际运行，就不能说"已编译通过"或"测试通过"。

## 工作方式

- 用户提问时先回答问题，再动手修改。
- 只改任务需要的部分。顺手发现的问题单独提出，不要把无关的重构、改名、格式调整混进改动。
- 构建产物：每个 worktree 有自己的 `target/`，cargo 不清理旧产物，36 个 worktree 曾积累到 424 GB。Claude Code 会话因此关闭了增量编译（`.claude/settings.json` 的 `CARGO_INCREMENTAL=0`），不要再打开；不另指定 `--target-dir`，任务不需要时不做 `--release` 构建。worktree 的分支合并后，收尾时提醒用户可以移除它，不自行删除。

## 红线

与 `.claude/rules/` 里的规则冲突时，以这里为准。

1. 不用 `Rc<RefCell<_>>`、`Arc<Mutex<_>>`、`.clone()`、`'static`、`Box::leak`、`unsafe` 来绕过借用检查器。遇到借用错误，先按 `.claude/rules/02-ownership-borrowing.md` 的顺序重新设计数据流。
2. 生产代码不用 `unwrap()`。`expect("原因")` 只用于可以证明成立的不变量。测试代码可以用 `unwrap()`。
3. 不用 `#[allow(...)]` 压制警告。确实需要例外时用 `#[expect(lint, reason = "...")]`。
4. 持有 `std::sync::Mutex` 的 guard 时不 `.await`；async 代码里不调用阻塞 API，少量、有界的同步 I/O 除外（见 `.claude/rules/04-async-concurrency.md`）。
5. 不新增依赖、不升级依赖、不修改 `rust-version`、不修改 `edition`，除非用户明确同意（`.claude/rules/07-dependencies.md`；已获授权的依赖见 `docs/ROADMAP.md` 的决策 D2）。
6. 不使用 Rust 1.88 之后才稳定的语言特性和标准库 API。标准库 API 由 clippy 的 `incompatible_msrv` 检查；语言特性用 `cargo +1.88.0 check --all-targets` 验证。
7. 测试失败时先查实现，不改断言去迁就错误的实现，不删除或 `#[ignore]` 失败的测试。`#[ignore]` 只用于付费的真实 API 测试。
8. 不编造 crate、API、feature 名。不确定时查 `~/.cargo/registry/src/` 里对应版本的源码。
9. 未经用户明确授权，不运行调用真实模型 API 的测试（`cargo test --test real_api -- --ignored`），也不替用户设置 `ZX_E2E_AUTHORIZED`。这些测试会产生费用。

## 不变量

修改相关模块时必须保持；每一条都有测试覆盖，测试失败说明不变量被破坏了。

1. **provider 不报错。** `Provider::stream` 立即返回；所有失败都编码成最终的 `StreamEvent::Error`，其 `stop_reason` 为 `Error` 或 `Aborted`；接收端存续时，每个流恰好以一个 `Done` 或 `Error` 结束（`crates/zx-ai/src/{provider,retry}.rs`）。
2. **由模型决定何时停止。** 工具失败（找不到、参数无效、被策略拒绝、执行报错、panic、同一回答里重复的调用编号）都变成 `is_error` 的工具结果，循环继续（`crates/zx-core/src/runner/step_loop/tools.rs`）。
3. **并行执行工具分三个阶段：** 逐个检查 → 并发执行 → 按调用顺序写结果。只要有一个工具是 `Sequential`，整批改为串行。
4. **回复因 `Length` 截断时，不执行其中的工具调用。**
5. **会话只追加。** 不改写、不删除已写入的记录；`SessionLog::append` 成功后才更新内存（`crates/zx-core/src/transcript.rs`）。唯一的例外是 JSONL 文件在打开之后、追加下一条记录之前，截掉崩溃或写入失败留下的未完成尾记录。一次写入失败、超时或被中断后不再写入，会话在那次操作结束时自行关闭。一个会话同一时间只有一个写入者（JSONL 用 `flock`）。只读写当前格式（`SESSION_VERSION`）：其他版本一律拒绝，不升级、不兼容读取；格式变化时提高版本号。
6. **发给模型的上下文就是会话投影，系统提示词也在其中。** 系统提示词以 system 消息记录，变化时只追加变化的段落（`crates/zx-ai/src/system_prompt.rs`）。重试时用 `context_edit` 省略失败的回答再续跑，不重复用户消息（`crates/zx-core/src/runner/failure.rs`）。
7. **会话写入失败时立即停止运行**（审计要求）：循环不再执行后续步骤，运行以 `Failed` 结束，即使同时被取消。
8. **工具输出必须截断**（`crates/zx-builtin/src/tools/truncate.rs`）；耗时的工具必须响应 `ToolContext::cancel`；文件写入一旦开始就写完，调用被丢弃也一样（`Executor::write_file`）。
9. **认证用的 API key 不主动写入会话文件、事件或日志。** `zx-app` 的装配只从环境变量读取它，把这个变量从本机执行的 bash 与搜索子进程中删除（`LocalExecutor::hidden_env`），并用 `RedactSecrets` 在工具输出持久化前过滤 key 的完整字面值；库的使用者也可以直接给出 key（`ApiKey::Fixed`、`ApiKey::Dynamic`）。边界见 `docs/SECURITY.md`。
10. **有副作用的工具调用先记录开始再执行，恢复时不重放。** `Tool::effects()` 为 `SideEffects` 的调用通过检查与审批后，先追加 `tool_call_started`，写入失败则不执行（`crates/zx-core/src/runner/step_loop/tools.rs`）。每次运行开始时（以及空闲时经 `Session::append_custom_message` 追加消息之前），为分支末尾缺少结果的调用补写错误结果，按开始记录区分结果未知与未执行（开始记录在已离开的分支上也算）；以 `Error` 或 `Aborted` 结束的回答中的调用从不执行（`crates/zx-core/src/transcript/recovery.rs`、`crates/zx-core/src/runner/interrupted.rs`）。

## 新增功能放在哪里

契约的细节（出错条件、调用次数、校验规则）在各 trait 的 rustdoc，以它为准。

| 新能力 | 做法 | 参照 |
|---|---|---|
| provider | 实现 `ai::Provider`，遵守不变量 1；每个失败归入 `ProviderErrorKind`，服务的状态、错误码与措辞属于哪一种只写在 provider 的 crate 里；解析与归类写单元测试；走 HTTP 的用 `zx-ai-http` 的共用件。由 `config.toml` 的 `provider.api` 按名字选用的，再实现 `zx_app::ProviderFactory`，经 `Application::register_provider` 注册 | `crates/zx-agent/tests/openai_mock.rs`、`anthropic_mock.rs`；`crates/zx-app/src/assembly/provider.rs` |
| 工具 | 实现 `tools::Tool`，遵守不变量 8：参数是带 `#[serde(deny_unknown_fields)]` 的结构体，不改状态的 `effects()` 返回 `Effects::ReadOnly`。共用的进 `AgentConfig::tools`，绑定会话目录或执行环境的由 `SessionSetup` 给出 | `crates/zx-builtin/src/tools/` |
| 策略 | 实现 `policy::ToolPolicy`，经 `PolicyChain::push` 进 `AgentConfig::policies`；无法判断时返回 `Deny`，所有策略取最严的结论。钩子不做放行判断 | `crates/zx-builtin/src/policy/` |
| 钩子 | 实现 `hooks::Hook`：回调必须异步让出执行权，`transform_context` 不能追加 system 消息。经 `HookChain::push(名字, Arc)` 注册；共用的进 `AgentConfig::hooks`，单个会话的进 `SessionParts::hooks` | `crates/zx-agent/tests/hook_contract.rs` |
| 打包的扩展 | 实现 `extensions::Extension`，进 `AgentConfig::extensions`，所有会话共用。运行中发消息、记状态或改工具集经 `HookContext` 的 `queues` 与 `active_tools` | `crates/zx-builtin/src/mcp.rs` |
| 会话存储 | 实现 `transcript::SessionStorage` 与 `SessionLog`，进 `AgentConfig::storage`：`append` 存好才返回，一个会话一个写入者，接口里不出现文件路径，列出时用 `SessionList::new`、读头部时用 `SessionQuery::arrange_headers` 排序与截断 | `crates/zx-core/src/transcript/memory.rs`；只换写入方式时 `crates/zx-agent/tests/runner.rs` 的 `TableLog` |
| 摘要器 | 实现 `compaction::Summarizer`，进 `AgentConfig::summarizer` | `crates/zx-agent/tests/summarizer.rs` |
| 单个会话的部分 | 实现 `agent::SessionSetup`，返回 `SessionParts`（工具、钩子、提示词段落），并经 `section_names` 声明它给出的段名 | `crates/zx-app/src/assembly/setup.rs` |
| 接入方式（如 Web 服务） | 独立的 crate，依赖 `zx-agent`（只要内核时依赖 `zx-core`；要与命令行共用 `config.toml` 与 agent 文件时依赖 `zx-app`，经 `Application` 得到 agent，自己的工具、钩子、策略、扩展与摘要器经它的 `add_*`、`set_*` 加入，由配置按名字选用的 provider 与执行环境经 `register_provider`、`register_executor` 注册（D34），信号与追踪导出的收尾用 `zx_app::shutdown`），构造 `Agent`，为每个对话创建或打开 `Session`，读它返回的 `Run`；不放进库 `zx-agent` | `crates/zx-cli`、`crates/zx-im` |
| IM 平台 | `zx-im` 的 `platform` 下的一个模块实现 `ImChannel`（职责见它的 rustdoc），收到的消息经 `Inlet` 先存入 backlog 再确认投递（去重也在这里），用 `platform.rs` 的共用件：一次投递的存入 `keep`，限频 `Pacer` 与 `rate_limit_wait`，拆分超长文本 `split_text`，读连接 `next_message`，开连接与连接结束的 `Connection`、`Ended`；文案用 `CardLabels`。它的 `im.toml` 段、默认的变量名、要对命令隐藏的密钥变量与由段建 channel 的 `PlatformFactory` 都在这个模块里，在 `platform::built_in` 登记；可选的平台放在 cargo feature 后。库外的平台由宿主自己的程序经 `zx_im::Program::register_platform` 登记，宿主的其他部件经 `Program::customize` | `crates/zx-im/src/platform/slack.rs`；`crates/zx-im/tests/im_server.rs` 的 `FakeChannel` |
