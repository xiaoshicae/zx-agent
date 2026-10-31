# zx-agent 项目说明

<!--
维护说明。HTML 注释在注入 Claude 的上下文之前会被去掉，不占 token。

- 本文件每个会话都会加载，只放每个任务都用得上、又不能从代码和 README 里看出来的内容。
  "背景"和"架构设计"只写看代码看不出来的内容：分层职责、数据流、设计取舍（为什么这样做）。
  架构图、模块与 pi 的对应表、参数说明只维护在 README.md，避免两处不一致。
  "现状""未验证""内部整理待办"在相应工作完成后要及时更新或删除。
- 写 Rust 的细则在 .claude/rules/，都带 paths 过滤，Claude 读写 .rs 文件时才加载（08 还包括 Cargo.toml）：
  01 代码规范、02 所有权与借用、03 错误处理、04 异步与并发、05 类型与 API 设计、06 测试、08 依赖与 MSRV。
  写代码时几乎每次都会读写 .rs，所以规则实际上都会加载。规则只写项目特有、工具查不出、又没写在本文件里的内容；
  不写通用的 Rust 常识，不重复本文件的红线和测试原则。
- clippy 默认就会报错的写法不写进规则，交给 cargo clippy 的 -D warnings：
  &String / &Vec / &Box 参数、可以改用 ? 的 match、for i in 0..v.len()、持有 MutexGuard 跨 .await、
  1.85 之后才稳定的标准库 API（incompatible_msrv）。后两项已实测；格式字符串内联变量实测默认不检查，所以写在 01。
- 每次都必须发生的事准备交给 Claude Code 执行，不依赖 Claude 记得。配置草案在 .claude/settings.proposed.json，
  目前没有启用，改名为 .claude/settings.json 后才生效：
  - PostToolUse hook（.claude/hooks/post-edit.sh）：Edit / Write 之后对改动的 .rs 文件跑 rustfmt，并记下本会话改过代码。
  - Stop hook（.claude/hooks/stop-check.sh）：本会话改过 Rust 代码时，结束前跑 cargo fmt 检查、clippy、test。
    不通过就拦下一次，让 Claude 修；第二次结束时仍不通过就放行，并提示用户。
  - ask 规则：git commit、git push、cargo add / remove / update、修改 Cargo.toml 都要用户确认。
-->

## 背景

- **是什么**：用 Rust 编写的最小 agent 内核，面向无人值守场景，例如由 cron 或 Kubernetes Job 触发的自动化排障。运行期间没有人回答问题、批准操作或中途叫停。
- **参照**：分层方式、循环语义和会话格式沿用 pi（https://github.com/earendil-works/pi ，commit `a92283056`）。为无人值守做的改动记录在 README 的"与 pi 的差异"。
- **现状**：v0.1.0。一个 OpenAI 兼容的流式 provider；`bash`、`read_file` 两个工具。未实现：上下文压缩、扩展、MCP、其他 provider（路线图见 README）。
- **未验证**：只在 macOS 上测试过；没有用 Rust 1.85 工具链实际编译过；`--system-updates inline` 只在一个服务上验证过；付费 e2e 测试还没有运行过。
- **内部整理待办**（不改变行为）：`run_loop` 参数过多，工具名字符串散落各处，`clip` 有两份实现，`BashTool::execute` 过长。

## 架构设计

分层如下，上层依赖下层。架构图、模块与 pi 的对应表、参数说明见 README。

| 层 | 位置 | 职责 |
|---|---|---|
| CLI | `src/main.rs` | 解析参数、组装各层、处理信号、输出 text 或 jsonl、映射退出码 |
| 提示词 | `src/prompt.rs` | 生成系统提示词：说明 + `<environment>` + `<command_policy>` |
| 运行层 | `src/runner.rs` | 一次任务：重试、轮次与时间上限、截断续写、先写盘再转发事件、汇总报告 |
| 会话 | `src/session.rs` | 只追加的 JSONL 条目树，投影出发给模型的消息 |
| agent | `src/agent/` | 双层循环、工具执行与策略检查、事件、消息队列 |
| ai | `src/ai/` | 消息类型、系统提示词段落、流式事件协议、provider 与重试判断 |
| 工具 | `src/tools/` | bash、read_file、输出截断 |

一次运行的数据流：`main` 组装 `Runner` → `Runner::run` 取会话投影作为上下文 → `run_loop` 先追加提示词的变化和用户消息，再循环"请求模型 → 执行工具" → 每条完成的消息由 `PersistingSink` 先写进会话文件，再转发给输出 → 请求失败时 runner 决定是否重试，回答被截断时决定是否续写 → 汇总成 `RunReport`，由 `main` 映射为退出码。

设计取舍（改动前先理解原因；必须保持的约束见下文"不变量"）：

- **无人值守优先**：不做交互确认，用声明式策略代替；轮次和时间默认有上限；结果用退出码 0 到 6 表达，调度器不必解析输出。
- **会话文件既是审计日志，也是模型上下文的唯一来源**：修正上下文靠追加 `context_edit`，提示词变化靠追加 system 消息，原始记录始终保留。
- **事实由程序提供，不让模型去查**：操作系统、启动时间、命令白名单在启动时写进系统提示词。这样不浪费轮次，也不会被白名单拦住。
- **兼容性默认保守，能力按需打开**：提示词更新默认合并到开头（`collapse`，与 pi 对 OpenAI 兼容服务的默认一致），服务端支持时才按位置发送（`inline`）。
- **安全靠部署隔离**：策略只是第一道防线（检查边界见 README 的"安全模型"）。key 只从环境变量读取并对 bash 隐藏，会话文件权限为 0600；真正的隔离要靠不同账号或容器。
- **扩展点都是 trait**：`Provider`、`Tool`、`ToolPolicy`、`EventSink`。新能力优先以实现 trait 的方式加入，不改循环本身。

## 命令

- 格式：`cargo fmt`（`rustfmt.toml`：`max_width = 120`）
- lint：`cargo clippy --all-targets -- -D warnings`，要求零警告
- 测试：`cargo test`，不需要网络和 API key。只跑一个集成测试文件：`cargo test --test agent_loop`；按名字过滤：`cargo test retries_transient`。付费的 `tests/real_api.rs` 默认跳过，见红线 9
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
5. 不新增依赖、不升级依赖、不修改 `rust-version`、不修改 `edition`，除非用户明确同意。提议新增依赖时要说明的内容见 `.claude/rules/08-dependencies-msrv.md`。
6. 不使用 Rust 1.85 之后才稳定的语言特性和标准库 API。例如 let chains（`if let Some(x) = a && ...`）要到 1.88 才稳定，并且 clippy 检查不出来。
7. 测试失败时先查实现，不改断言去迁就错误的实现，不删除或 `#[ignore]` 失败的测试。`#[ignore]` 只用于付费的真实 API 测试。
8. 不编造 crate、API、feature 名。不确定时查 `~/.cargo/registry/src/` 里对应版本的源码。
9. 未经用户明确授权，不运行调用真实模型 API 的测试（`cargo test --test real_api -- --ignored`），也不替用户设置 `ZX_E2E_AUTHORIZED`。这些测试会产生费用。

## 不变量

修改相关模块时必须保持以下不变量。每一条都有测试覆盖，测试失败说明不变量被破坏了：

1. **provider 不报错。** `Provider::stream` 立即返回；所有失败都编码成最终的 `StreamEvent::Error`，其 `stop_reason` 为 `Error` 或 `Aborted`；每个流恰好以一个 `Done` 或 `Error` 结束（`src/ai/provider.rs`）。
2. **由模型决定何时停止。** 工具失败（找不到、参数无效、被策略拒绝、执行报错、panic）都变成 `is_error` 的工具结果，循环继续（`src/agent/agent_loop.rs`）。
3. **并行执行工具分三个阶段：** 逐个检查 → 并发执行 → 按调用顺序写结果。只要有一个工具是 `Sequential`，整批改为串行。
4. **回复因 `Length` 截断时，不执行其中的工具调用。**
5. **会话文件只追加。** 不改写、不删除已写入的行；先写文件，再更新内存（`src/session.rs`）。
6. **发给模型的上下文就是会话投影，系统提示词也在其中。** 系统提示词以 system 消息记录，变化时只追加变化的段落，不改写之前的版本（`src/ai/system_prompt.rs`）。重试时用 `context_edit` 省略失败的回答再续跑，不重复用户消息（`src/runner.rs`）。
7. **会话文件写入失败时立即取消运行**（审计要求）。
8. **工具输出必须截断**（`src/tools/truncate.rs`）；耗时的工具必须响应 `ToolContext::cancel`。
9. **API key 只从环境变量读取**，不写入会话文件、事件或日志，也不传给 bash 子进程（`BashTool::hidden_env`）。

## 新增功能放在哪里

- **新 provider**：实现 `ai::Provider` 并遵守不变量 1。为解析逻辑写单元测试，并在 `tests/` 下用本地假 HTTP 服务（`tests/common/mock_server.rs`）写端到端测试，参照 `tests/openai_mock.rs`。
- **新工具**：实现 `agent::Tool`。参数定义成带 `#[serde(deny_unknown_fields)]` 的结构体再反序列化；输出要截断；要支持取消。测试覆盖成功、参数错误、超时或取消三种情况。
- **新策略**：实现 `agent::ToolPolicy`，无法判断时返回 `Decision::Deny`。
