# zx-agent 项目说明

<!--
维护说明（供维护者阅读）。

- 本文件每个会话都会加载，只放每个任务都用得上、又不能从代码和 README 里看出来的内容。
  "背景"和"架构设计"只写看代码看不出来的内容：分层职责、数据流、设计取舍（为什么这样做）。
  架构图、模块与 pi 的对应表、参数说明只维护在 README.md，避免两处不一致。
  "现状""未验证"在相应工作完成后要及时更新或删除。
  目标、阶段计划、已定决策（包括已授权的依赖）、整理待办只维护在 docs/ROADMAP.md。
- 写 Rust 的细则在 .claude/rules/，都带 paths 过滤，Claude 读写 .rs 文件时才加载（08 还包括 Cargo.toml）：
  01 代码规范、02 所有权与借用、03 错误处理、04 异步与并发、05 类型与 API 设计、06 测试、08 依赖与 MSRV。
  写代码时几乎每次都会读写 .rs，所以规则实际上都会加载。规则只写项目特有、工具查不出、又没写在本文件里的内容；
  不写通用的 Rust 常识，不重复本文件的红线和测试原则。
- clippy 默认就会报错的写法不写进规则，交给 cargo clippy 的 -D warnings：
  &String / &Vec / &Box 参数、可以改用 ? 的 match、for i in 0..v.len()、持有 MutexGuard 跨 .await、
  1.88 之后才稳定的标准库 API（incompatible_msrv）。后两项已实测；格式字符串内联变量实测默认不检查，所以写在 01。
- 每次都必须发生的事准备交给 Claude Code 执行，不依赖 Claude 记得。配置草案在 .claude/settings.proposed.json，
  目前没有启用，改名为 .claude/settings.json 后才生效：
  - PostToolUse hook（.claude/hooks/post-edit.sh）：Edit / Write 之后对改动的 .rs 文件跑 rustfmt，并记下本会话改过代码。
  - Stop hook（.claude/hooks/stop-check.sh）：本会话改过 Rust 代码时，结束前跑 cargo fmt 检查、clippy、test。
    不通过就拦下一次，让 Claude 修；第二次结束时仍不通过就放行，并提示用户。
  - ask 规则：git commit、git push、cargo add / remove / update、修改 Cargo.toml 都要用户确认。
-->

## 背景

- **是什么**：用 Rust 编写的通用 agent 内核。同一个内核支撑三类场景：和人对话；无人值守运行，例如由 cron 或 Kubernetes Job 触发的排障；被编辑器或其他程序嵌入。场景差异放在内核之外，通过 profile、提示词和插件表达。
- **参照**：分层方式、循环语义和会话格式沿用 pi（https://github.com/earendil-works/pi ，commit `a92283056`）。与 pi 不同的地方记录在 README 的"与 pi 的差异"。
- **现状**：v0.1.0。已实现共享的 `AgentSession`、无人值守的 `run` 和终端 `chat`、一个 OpenAI 兼容的流式 provider、`bash` 与 `read_file`，以及文件化 profile、项目上下文、按需 `read_skill`、有界摘要和长工具结果落盘 / `read_output`。CLI 的场景默认值位于 `profiles/*.toml` 与相邻说明文件；`presets` 保留已有 SDK 接口。SDK 默认不限轮次、不限时间、不自动续写或压缩、不落盘工具结果。接口与资源上限见 README，目标、阶段计划与已定决策见 `docs/ROADMAP.md`，改动前先确认当前阶段。
- **验证范围**：本地使用 macOS，CI 覆盖 Linux 与 macOS；`--system-updates inline` 只在一个服务上验证过；付费 e2e 测试（`tests/real_api.rs`）于 2026-11-01 在 DeepSeek 的 `deepseek-flash` 与 `deepseek-chat` 上各运行一次，均为 6 项通过（只覆盖 `collapse` 模式，不含压缩用例）。

## 架构设计

分层如下，上层依赖下层。架构图、模块与 pi 的对应表、参数说明见 README。

| 层 | 位置 | 职责 |
|---|---|---|
| CLI | `src/main.rs`、`src/cli.rs`、`src/chat/` | 共用参数与运行组装；run 处理单次任务；chat 管理单一输入所有者、审批、信号和 REPL |
| 场景配置 | `src/profile/`、`profiles/` | TOML 基线与覆盖、项目上下文、显式 skill 目录及按需读取；不进入循环机制 |
| 场景预设 | `src/presets/` | 提供外层显式选择的说明与运行默认值；内核机制不依赖预设 |
| 提示词 | `src/prompt.rs` | 生成系统提示词：说明 + `<environment>` + `<command_policy>` |
| 共享会话 | `src/agent_session.rs` | 共享调用与 Busy、RAII 归还运行权、取消、订阅与退订、空闲时切换模型 |
| 运行层 | `src/runner.rs` | 持有可顺序复用的会话；以独占借用控制单次运行、重试、上限、审批缓存、持久化和报告 |
| 会话 | `src/session.rs` | 只追加的 JSONL 条目树，投影出发给模型的消息 |
| 上下文管理 | `src/compaction.rs`、`src/session/compaction.rs`、`src/output_store.rs` | 宿主摘要、追加升级与原始投影、脱敏后的长结果保存及分页 |
| agent | `src/agent/` | 回合循环、工具执行、Hooks 宿主、事件、消息队列 |
| ai | `src/ai/` | 消息类型、系统提示词段落、流式事件协议、provider 与重试判断 |
| 外层策略与交互 | `src/policy.rs`、`src/extensions.rs`、`src/interaction.rs` | 命令与规则策略、续写、已知密钥脱敏、宿主审批与 NoHuman |
| 工具 | `src/tools/` | bash、read_file、输出截断 |

一次运行的数据流：`main` 组装 `AgentSession` → `prompt_with_cancel` 独占取得 Runner → `Runner::run` 取会话投影作为上下文 → `run_loop` 先持久化提示词、输入和上下文改动，再经宿主准备步骤执行可选摘要 → 新增插话与钩子改动持久化后请求任务模型 → 执行工具、脱敏、可选落盘 → `PersistingSink` 先写完成消息，再转发事件 → runner 处理任务请求重试并汇总 `RunReport`，由 `main` 映射退出码。自动摘要单独记录、消耗请求预算且不重试；手动 compact 共用独占权并独立报告。

设计取舍（改动前先理解原因；必须保持的约束见下文"不变量"）：

- **场景策略不进内核**：提示词、运行上限与续写默认值由预设提供；P1 已实现六个 Hooks 和 Interaction；命令策略、续写、脱敏由外层注册，无人时拒绝 Ask。无人值守的 `run` 用退出码 0 到 6 表达结果，调度器不必解析输出。
- **配置与文件内容分工**：profile 决定工具和规则，CLI 只用显式传参覆盖文件，deny 累加；项目上下文和 skill 正文不能更改控制策略。自动发现的 AGENTS.md 解析后必须位于项目根目录内，`run` 的默认 profile 不自动发现，避免仓库内容让程序读取并发送项目外的文件；长结果的默认目录同样必须解析在工作目录内，`run` 默认不落盘。skill 目录启动时只读取元数据，正文由工具按需读取并经过既有流水线，不隐式执行脚本或加载全局 AI 配置。
- **共享 API、独占执行**：AgentSession 的短锁只切换 Runner 所有权，不跨 await。重叠 prompt 和运行中改配置返回 Busy；取消、panic 或 future 丢弃后由 RAII 归还。订阅列表先取快照再调用，单个 subscriber panic 会被隔离并可查询，不持锁调用宿主回调。
- **会话文件既是审计日志，也是模型上下文的唯一来源**：修正上下文靠追加 `context_edit`，提示词变化靠追加 system 消息，原始记录始终保留。
- **摘要不替代证据**：v3 用开始、成功或失败记录审计摘要，旧文件先追加格式升级；重新摘要读取原始投影，保留全部 system。摘要引用只校验来源存在；原始事实被修正后摘要失效。预算与真实模型信息保真是不同的验收项。
- **终端输入只有一个所有者**：chat 的专用线程通过有界通道提供逐行输入；REPL 协调提问、审批和信号，Interaction 不直接读 stdin。审批回复必须匹配主机生成的请求编号，取消后失效；TTY 运行中输入 steering，管道脚本逐轮消费。按 Tokio stdin 源码对交互终端的建议避免不可取消的 stdin 阻塞任务卡住运行时退出。
- **事实由程序提供，不让模型去查**：操作系统、启动时间、命令白名单在启动时写进系统提示词。这样不浪费轮次，也不会被白名单拦住。
- **兼容性默认保守，能力按需打开**：提示词更新默认合并到开头（`collapse`，与 pi 对 OpenAI 兼容服务的默认一致），服务端支持时才按位置发送（`inline`）。
- **安全靠部署隔离**：策略只是第一道防线（检查边界见 README 的"安全模型"）。认证 key 只从环境变量读取，该变量从 bash 子进程环境中删除；Unix 上新建会话文件的权限为 0600，打开已有文件不修改权限；真正的隔离要靠不同账号或容器。
- **扩展点都是 trait**：`Provider`、`Tool`、`ToolPolicy`、`Hooks`、`Interaction`、`EventSink`。新能力优先以实现 trait 的方式加入，不改循环本身。

## 命令

- 格式：`cargo fmt`（`rustfmt.toml`：`max_width = 120`）
- lint：`cargo clippy --all-targets -- -D warnings`，要求零警告
- 测试：`cargo test`，不需要网络和 API key。只跑一个集成测试文件：`cargo test --test agent_loop`；按名字过滤：`cargo test retries_transient`。付费的 `tests/real_api.rs` 默认跳过，见红线 9
- MSRV：`cargo +1.88.0 check --all-targets`，用到新语法或新标准库 API 时运行（本机已安装 1.88.0 工具链）
- 自查：`.claude/scripts/self-check.sh`，列出需要说明理由的写法（`Rc` / `RefCell` / `unsafe`、`#[allow]`、生产代码里的 `unwrap()`、`let _ =`、调试宏）。每一处命中都要能说出理由

改完代码后，格式、lint、测试三条命令都要通过。报告时附上 `test result` 那几行原始输出；没有实际运行，就不能说"已编译通过"或"测试通过"。

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
- **复用测试设施，不复制粘贴。** `FauxProvider` 和 `tests/common/` 里的 `run_prompt`、`RunnerSetup`、`MockServer` 等，清单见 `.claude/rules/06-testing.md`。

分层：

| 层次 | 位置 | 测什么 | 怎么驱动 |
|---|---|---|---|
| 单元测试 | 模块内的 `#[cfg(test)] mod tests` | 单个模块的逻辑和边界，可以测私有函数；包括会话文件和事件流的 JSON 格式 | 直接调用 |
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

1. **provider 不报错。** `Provider::stream` 立即返回；所有失败都编码成最终的 `StreamEvent::Error`，其 `stop_reason` 为 `Error` 或 `Aborted`；接收端存续时，每个流恰好以一个 `Done` 或 `Error` 结束。通过有界事件通道发送，临时性失败优先使用 `ProviderError` 判断（`src/ai/provider.rs`、`src/ai/retry.rs`）。
2. **由模型决定何时停止。** 工具失败（找不到、参数无效、被策略拒绝、执行报错、panic）都变成 `is_error` 的工具结果，循环继续（`src/agent/agent_loop.rs`）。
3. **并行执行工具分三个阶段：** 逐个检查 → 并发执行 → 按调用顺序写结果。只要有一个工具是 `Sequential`，整批改为串行。
4. **回复因 `Length` 截断时，不执行其中的工具调用。**
5. **会话文件只追加。** 不改写、不删除已写入的行；先写文件，再更新内存（`src/session.rs`）。唯一的例外是打开文件时截掉崩溃或写入失败留下的未完成尾记录，并报告删除的字节数；一次写入失败后不再写入，保证残缺只出现在文件末尾。
6. **发给模型的上下文就是会话投影，系统提示词也在其中。** 系统提示词以 system 消息记录，变化时只追加变化的段落，不改写之前的版本（`src/ai/system_prompt.rs`）。重试时用 `context_edit` 省略失败的回答再续跑，不重复用户消息（`src/runner.rs`）。
7. **会话文件写入失败时立即取消运行**（审计要求）。
8. **工具输出必须截断**（`src/tools/truncate.rs`）；耗时的工具必须响应 `ToolContext::cancel`。
9. **认证用的 API key 只从环境变量读取**，不主动写入会话文件、事件或日志；其环境变量从 bash 子进程中删除（`BashTool::hidden_env`）。CLI 的 `RedactSecrets` 在工具输出持久化前过滤认证 key 的完整字面值；不识别未知或变形的密钥，也不覆盖输入、模型输出与参数，边界见 README 的"安全模型"。

## 新增功能放在哪里

- **新 provider**：实现 `ai::Provider` 并遵守不变量 1。为解析逻辑写单元测试，并在 `tests/` 下用本地假 HTTP 服务（`tests/common/mock_server.rs`）写端到端测试，参照 `tests/openai_mock.rs`。
- **新工具**：实现 `agent::Tool`。参数定义成带 `#[serde(deny_unknown_fields)]` 的结构体再反序列化；输出要截断；要支持取消。测试覆盖成功、参数错误、超时或取消三种情况。
- **新策略**：实现 `agent::ToolPolicy`，通过 `policy::PolicyHook` 注册，无法判断时返回 `Decision::Deny`。
- **新扩展**：实现 `agent::Hooks`，按需覆盖默认方法；回调必须异步让出执行权。错误、panic、取消与超时由宿主处理，具体规则见 README。
- **上下文改动**：`on_input` 改写或拒绝通过 `context_edit` 记录；`transform_context` 返回 `ContextEdit` 计划，整份计划先校验，逐项写入成功后再请求模型。
