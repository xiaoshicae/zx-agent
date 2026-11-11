# zx-agent 项目说明

<!-- 维护说明：本文件每个会话都会加载，只放每个任务都用得上、又不能从代码看出来的内容。用法与架构图在 README.md，配置键在 docs/CONFIGURATION.md，决策与验证记录在 docs/ROADMAP.md，写 Rust 的细则在 .claude/rules/（带 paths 过滤，读写 .rs 时加载）。.claude/settings.json 的 hook：编辑 .rs 后跑 rustfmt；改过 Rust 代码时结束前跑 fmt 检查、clippy、test；git commit、git push、cargo add/remove/update、改 Cargo.toml 要用户确认。 -->

## 背景

- **是什么**：Rust 编写的通用 agent 内核，同一个内核支撑和人对话、无人值守运行（cron、Kubernetes Job）与被其他程序嵌入。场景差异放在内核之外，由 agent 文件、提示词和插件表达。
- **参照**：分层、循环语义和会话格式沿用 pi（https://github.com/earendil-works/pi ，commit `a92283056`），不同之处见 README 的"与 pi 的差异"。
- **现状**：v0.1.0。子命令 `run`、`chat`（应急与脚本化入口）、`acp`、`im`（Slack 与 ⚑ D:5/CHINA FLAG: 飞书）、`sessions`。环境在主目录（`~/.zx-agent`，`ZX_AGENT_HOME` 可改）的 `config.toml`，角色在 agent 文件。改动前先看 `docs/ROADMAP.md` 的当前阶段。

## 架构设计

8 个 crate，上层依赖下层：`zx-ai`（类型与 `Provider` 契约）← `zx-core`（内核）← `zx-tools` 与 `zx-im`（互不依赖）；`zx-ai-openai`（唯一依赖 reqwest 的模型层 crate）只依赖 `zx-ai`；`zx-im-slack`、`zx-im-feishu` 实现 `zx-im` ← `zx-agent`（根目录 `src/`：CLI、配置与追踪导出）。模块地图在各 crate 的 `lib.rs` 与父模块的 `//!` 文档。

| 层 | 位置 |
|---|---|
| CLI | `src/main.rs`（只分派）、`src/cli/`（`settings.rs` 合并配置与命令行，`shared.rs` 的 `Shared` 是进程内共用的东西，`prepare.rs` 组装一个会话，`execution.rs` 本机或容器）、`src/run.rs`、`src/chat/`、`src/acp/`、`src/im.rs`、`src/process.rs`、`src/render.rs`、`src/session_files.rs`、`src/list_sessions.rs` |
| 配置 | `src/config/`（`environment.rs`、`definition.rs`、`agent_file.rs`、`permissions.rs`）、`agents/` |
| 内核 | `crates/zx-core/src/`：`agent_session.rs`（共享调用、Busy、订阅）、`runner.rs`（单次运行、重试、上限、审批缓存）、`session/`（只追加的条目树、投影、恢复、`SessionLog`、`jsonl.rs`）、`compaction/` 与 `session/compaction/`（摘要，统一叫 compaction，见 D22）、`agent/`（循环、工具执行、Hooks、事件、队列）、`prompt.rs`、`extensions.rs` |
| 模型 | `crates/zx-ai/src/`、`crates/zx-ai-openai/src/` |
| 工具与外层策略 | `crates/zx-tools/src/`：`tools/`、`executor/`（本机与容器）、`policy/`、`continuation.rs`、`redact.rs`、`output_store.rs`、`mcp.rs` |
| IM | `crates/zx-im/src/`（`ImChannel`、`Server`、`platform.rs`）、`crates/zx-im-slack/`、`crates/zx-im-feishu/` |
| 追踪导出 | `src/otel.rs`、`src/otel/` |

一次运行：`main` 组装 `AgentSession` → `prompt` 独占取得 Runner → `Runner::run` 开始一次 `Run`，每次尝试运行一遍循环 `AgentLoop`（crate 内部） → 先持久化提示词、输入和上下文改动，再执行可选摘要 → 请求模型，发出的上下文就是此刻的会话投影 → 执行工具、脱敏、可选落盘 → 每一步先写入会话宿主（crate 内部的 `agent::SessionHost`），再发出事件 → `Run` 处理重试并汇总 `RunReport`，`main` 映射退出码。

设计取舍（改动前先理解原因）：

- **场景策略不进内核**：提示词、上限与续写默认值由 agent 文件提供；命令策略、续写、脱敏由外层注册，无人时拒绝 Ask。`run` 用退出码表达结果，调度器不必解析输出。
- **配置与内容分工**：`config.toml` 决定环境，agent 决定角色，命令行只剩 `--model`、`--tools`、预算与会话参数。没有项目级配置文件：项目内容（含 AGENTS.md 与 skill 正文）不能改变环境、权限或控制策略；自动发现的 AGENTS.md 必须在项目根目录内。
- **共享 API、独占执行**：AgentSession 的短锁只切换 Runner 所有权，不跨 await；重叠调用返回 Busy；取消、panic 或 future 丢弃后由 RAII 归还；panic 的 subscriber 被隔离并摘除，不持锁调用回调。
- **会话是审计日志，也是模型上下文的唯一来源**：修正上下文靠追加 `context_edit`，提示词变化靠追加 system 消息，原始记录始终保留；可替换的只是最底层的 `SessionLog`。摘要每次读取原始投影，原始事实被修正后整份失效。
- **终端输入只有一个所有者**：chat 的专用线程经有界通道提供输入，Interaction 不直接读 stdin；审批回复必须匹配主机生成的请求编号。
- **扩展点都是 trait**：`Provider`、`Tool`、`ToolPolicy`、`Hooks`、`Interaction`、`EventSink`、`Executor`、`SessionLog`、`ImChannel`、`StatusStore`、`Extension`。新能力优先实现 trait，不改循环。
- **通用，不为场景定制**：某个环境、平台或厂商才有的知识写在 agent、部署清单或配置里，或由外部适配器实现；代码不解析某个系统的私有格式；provider 的协议方言处理显式开启（如 `provider.fillMissingReasoning`），不按厂商推断。
- **界面交给 ACP 客户端**（D13）：`acp` 只做协议与会话的映射；`chat` 不再加功能。
- **安全靠部署隔离**：策略只是第一道防线（见 README 的"安全模型"），真正的隔离靠不同账号或容器。
- **分层由 crate 边界保证**：内核引用不到工具、执行环境、策略、MCP、IM 和 CLI；同层模块互不引用（run、acp、sessions 不引用 chat，两个 IM 平台互不引用），共用的下沉到下层。跨 crate 使用但不属于 API 的项写成 `#[doc(hidden)] pub` 并注明。

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

- **新 provider**：实现 `ai::Provider`，遵守不变量 1；解析逻辑写单元测试，端到端测试用 `tests/common/mock_server.rs`，参照 `tests/openai_mock.rs`。
- **新工具**：实现 `agent::Tool`。参数是带 `#[serde(deny_unknown_fields)]` 的结构体；输出截断；支持取消；不改变任何状态的工具 `effects()` 返回 `Effects::ReadOnly`，其余保留 `SideEffects`（不变量 10）；容忍常见参数错误时覆盖 `prepare_arguments`，只依据参数本身改写。测试覆盖成功、参数错误、超时或取消。
- **新策略**：实现 `agent::ToolPolicy`，经 `policy::PolicyHook` 注册（`Policy` 阶段），无法判断时返回 `PolicyDecision::Deny`；扩展的放行判断写在 `Hooks::before_tool`。
- **新扩展**：实现 `agent::Hooks`；回调的最后一个参数是 `&HookContext`，回调必须异步让出执行权；`transform_context` 不能追加 system 消息。阶段默认 `Extension`，用 `HookChain::push_with` 注册时给出名称。钩子 JSON 格式由 `tests/hook_contract.rs` 固定。
- **打包的扩展**：实现 `extensions::Extension`，经 `extensions::register` 加入 `LoopConfig`；钩子只能是 `Extension` 阶段；段落名不能是 `preamble`、`prompt::GENERATED_SECTIONS` 中的名字或已有段落；命令名只用 ASCII 字母、数字、`_`、`-` 且不重名。运行中发消息、记状态或改工具集经 `HookContext` 的 `queues` 与 `active_tools`，不直接碰会话。
- **上下文改动**：`on_input` 完成后才写入原始输入，改写或拒绝再追加 `context_edit`；`transform_context` 返回的 `ContextEdit` 计划整份先校验，逐项写入成功后再请求模型。
- **新的会话存储**：实现 `session::SessionLog`；`append` 在记录保存后才返回，同一 `seq` 已有记录时报错；新会话交给 `SessionStore::with_log`，续接时把 `LoadedSession` 交给 `SessionStore::load`；参照 `tests/runner.rs` 的 `TableLog`。
- **新 IM 平台**：在独立 crate 中实现 `zx_im::ImChannel`（参照 `crates/zx-im-slack`）：立即确认投递、按消息 ID 去重（`zx_im::Seen`）、只转交发给机器人的消息、遵守限频、拆分超长文本（`zx_im::split_text`）、文案用 `CardLabels`。与 `Server` 的配合参照 `tests/im_server.rs` 的 `FakeChannel`。
