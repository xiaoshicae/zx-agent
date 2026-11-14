# 作为库使用

`zx-agent` 库再导出各 crate：内核 `zx-core` 按域模块原样导出（`zx_agent::runner`、`zx_agent::tools`、`zx_agent::policy`、`zx_agent::hooks`、`zx_agent::transcript` 等），模型层在 `zx_agent::ai`，`zx-tools` 附带的实现在 `zx_agent::builtin`。库里没有接入方式：IM 服务在 `zx-im`（平台在 `zx-im-slack` 与 `zx-im-feishu`），`config.toml`、agent 文件与 OpenTelemetry 导出在命令行的 crate `zx-cli`（`zx_cli::config`、`zx_cli::otel`）。只需要内核时可以只依赖 `zx-core` 与 `zx-ai`，按需加上 `zx-ai-openai`。`zx-core` 按 JSON Schema 校验工具参数的部分是默认开启的 feature `json-schema`；写 `default-features = false` 时参数只由工具自己解析，内核的依赖从 103 个 crate 减到 40 个（含它自己）。各 crate 都不发布到 crates.io。

```rust
let mut config = AgentConfig { system_prompt, tools, ..AgentConfig::new(provider, model) };
config.policies.push("permissions", policy);
let agent = Agent::new(config)?;              // 校验工具、扩展与摘要设置
let session = agent.create_session(SessionOptions::new(cwd)).await?;
let mut run = session.send("任务描述")?;      // 只取得会话；读它，运行才推进
while let Some(event) = run.next().await {
    // 渲染事件
}
let report = run.wait().await;                // 只要结果时直接 session.send(..)?.wait().await
```

库默认不限轮次、时间与 token，没有钩子，不自动续写、摘要或落盘，会话只存在内存里；说明、上限、策略与存储都由嵌入方给出。完整的组装见 `examples/library.rs`。

- 入口：`Agent`（所有会话共用的角色 `AgentConfig`：模型、工具、策略、钩子、扩展、上限、重试、摘要与存储；`create_session`、`open_session(id)`、`session(id)`、`close_session(id)`、`list_sessions`）、`Session`（一个会话的句柄，克隆出来的是同一个会话：`send`、`continue_run`、`compact` 各返回一个 `Run`；另有 `cancel`、`set_model`、`set_name`、`branch`、`queues`、`active_tools`、`tool_names`、`cwd`、`run_command`）、`Run`（一次执行：事件流与结果。读它才推进，事件经有界通道，读得慢运行就等；丢弃即取消并归还会话）、`transcript::Transcript`。
- 会话在创建时给定的目录工作（`SessionOptions::cwd`，记在头部），再次打开时仍是那个目录。打开着的会话由 `Agent` 记着：再打开同一个会话得到同一个 `Session`；`close_session` 取消它正在进行的运行并释放它占用的东西，之后旧句柄的调用返回 `SessionError::Closed`，会话仍在存储里，可以再打开。记录写不进去的会话（写入失败、超时，或调用方在写入途中丢弃了 `Run`）在那次操作结束时自行关闭，句柄同样返回 `SessionError::Closed`，`open_session` 从存储重新打开它。
- 属于单个会话的东西（绑定会话目录或为它启动的容器的工具、相应的钩子与提示词段落）由宿主实现的 `agent::SessionSetup` 在创建或打开会话时给出，随会话释放；它失败时新会话不会留在存储里。不需要按会话区分时不用实现，工具直接放进 `AgentConfig::tools`。
- 调用者身份：会话的标签（`SessionOptions::labels`，如租户或所属对话）与输入的 actor（`Input::User { text, actor }`，这条输入是谁发的）经 `PolicyContext` 与 `HookContext` 的 `labels`、`actor` 交给策略与钩子。`actor` 是会话最近一条提问的发送者，没有给出时为 `None`；扩展发的消息不改变它。内核只记录与传递，不据此做任何判断。
- 审批不是接口而是会话的一个操作：`AgentConfig::approvals` 默认 `ApprovalMode::Deny`（没有人回答，策略询问的调用立即被拒绝）；设为 `ApprovalMode::Wait(时长)` 后，运行发出带审批编号、工具名与参数的 `approval_requested` 事件并等待 `Session::resolve_approval(编号, 决定)`，超时或取消都按拒绝处理。读事件的循环里不要等人回答，否则超时与取消要到下一次读取才生效。
- 事件只从 `Run` 读出。要观察所有运行的（如追踪导出）实现 `hooks::Hook::on_event`：不等待、不失败的钩子按顺序看到每个事件，被取消的运行也一样。
- 扩展点都是 trait：`ai::Provider`、`tools::Tool`、`policy::ToolPolicy`、`hooks::Hook`、`extensions::Extension`、`agent::SessionSetup`、`compaction::Summarizer`（可选：自己产出摘要）、`transcript::SessionStorage`（会话存在哪里）及其 `transcript::SessionLog`（一个会话的记录）、`builtin::executor::Executor`；IM 另有 `zx_im::ImChannel` 与 `zx_im::StatusStore`。
- 附带的实现：`ai::openai::OpenAiCompatible`、`transcript::JsonlStorage`（会话文件）与 `transcript::MemoryStorage`、`builtin::mcp::Server`（一个 `Extension`）、`builtin::executor::ContainerExecutor`、`builtin::redact::RedactSecrets`；追踪导出 `zx_cli::otel::Telemetry` 在命令行的 crate 里（它的 `hook()` 注册为 agent 的钩子）。
- 会话存储：放进 `AgentConfig::storage`，默认是 `MemoryStorage`。`Agent` 用 `storage.create(&header)` 建新会话，用 `storage.open(id)` 读回会话并成为它的写入者；`storage.read(id)` 是只读快照，宿主交给 `Transcript::snapshot` 来读；`storage.list(&SessionQuery { cwd, labels, limit })` 按工作目录或标签列出，最近活动的在前（`Agent::list_sessions` 即它）。换一种存储（如数据库）只需实现这两个 trait。
- 摘要：默认由内核请求模型，请求可从会话还原。给 `AgentConfig::summarizer` 一个 `compaction::Summarizer` 后由它产出摘要与用量：何时摘要、读哪一段（至多 2 MiB）、写开始与结果记录、校验每条事实引用的记录、超时与取消仍由内核负责；一次摘要计为运行的一次模型请求，它报告的用量计入运行的预算。

接口与契约见 rustdoc：`cargo doc --open`。

---

LIBRARY.md · v0.1 · 2026年11月
