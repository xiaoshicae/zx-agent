# 作为库使用

`zx-agent` 库再导出各 crate：内核 `zx-core` 按域模块原样导出（`zx_agent::agent`、`zx_agent::tools`、`zx_agent::policy`、`zx_agent::hooks`、`zx_agent::transcript` 等），模型层在 `zx_agent::ai`，`zx-builtin` 附带的实现在 `zx_agent::builtin`。库里没有接入方式：`config.toml`、agent 文件、OpenTelemetry 导出与进程的收尾在应用层 `zx-app`（见下文“与命令行共用配置”）。只需要内核时可以只依赖 `zx-core` 与 `zx-ai`，按需加上 `zx-ai-openai`。`zx-core` 按 JSON Schema 校验工具参数的部分是默认开启的 feature `json-schema`，关掉时内核只检查参数是 JSON 对象，其余由工具自己检查；`zx-builtin` 的 MCP 客户端是默认开启的 feature `mcp`，关掉时不编译 HTTP 客户端与 MCP SDK。测试用的脚本化 provider `ai::faux::FauxProvider`（按脚本回答，记下每个请求）只在 feature `testing` 打开时编译，默认关闭：嵌入方的测试在 `[dev-dependencies]` 里给 `zx-agent` 加上这个 feature。各 crate 都不发布到 crates.io。

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

库默认不限模型请求数、时间、token 与花费，没有钩子，不自动续写、摘要或落盘，会话只存在内存里；说明、上限、策略与存储都由嵌入方给出。安全相关的默认值也一样：没有策略时每个工具调用都放行；`LocalExecutor` 默认不对命令隐藏任何环境变量（`hidden_env` 为空，API key 也不例外）；工具输出只在注册了 `builtin::redact::RedactSecrets` 钩子时脱敏。完整的组装见仓库的 `crates/zx-agent/examples/library.rs`（`cargo build --example library` 编译它），这三样它都做了；上面的流程在 `Agent` 的 rustdoc 中另有 doctest。

## 导览

接口与契约以 rustdoc 为准（`cargo doc --open`）。下表从要做的事指到类型，并列出容易用错的地方。

| 要做的事 | 用什么 | 要注意 |
|---|---|---|
| 建 agent | `Agent::new(AgentConfig)` | `AgentConfig` 是所有会话共用的角色：模型、工具、策略、钩子、扩展、上限、重试、摘要与存储 |
| 接入别的模型服务 | 实现 `ai::Provider`，参照 `ai::openai::OpenAiCompatible` | `stream` 立即返回、从不报错：失败编码为最后的 `StreamEvent::Error`，回答的 `provider_error` 给出失败的种类（`ProviderErrorKind`）。内核只按种类决定重试与上下文溢出恢复，没有种类的失败不重试；服务的状态、错误码与措辞属于哪一种由 provider 判断。测试里用 `FauxStep::fail(种类, 文本)` 脚本化失败 |
| 开、续、关会话 | `Agent` 的 `create_session`、`open_session`、`session`、`close_session`、`list_sessions` | 会话在创建时给定的目录工作（`SessionOptions::cwd`），再次打开时不变。再打开同一个会话得到同一个 `Session`；`close_session` 取消进行中的运行，之后旧句柄返回 `SessionError::Closed`。记录写不进去的会话（写入失败、超时，或在写入途中丢弃了 `Run`）在那次操作结束时自行关闭，`open_session` 从存储重新打开它 |
| 运行 | `Session` 的 `send`、`continue_run`、`compact`，各返回一个 `Run` | 读它才推进，事件经有界通道，读得慢运行就等；丢弃即取消并归还会话。`Session` 可以克隆，克隆出来的是同一个会话；同一时间只有一次运行，重叠调用返回 Busy |
| 给单个会话工具、钩子与提示词段落 | 实现 `agent::SessionSetup` | 创建或打开会话时调用，随会话释放；失败时新会话不会留在存储里。给出的段名经 `section_names` 声明，扩展不能再用这些名字与 `preamble`。不需要按会话区分时，工具直接放进 `AgentConfig::tools` |
| 让人审批 | `AgentConfig::approvals` 设为 `ApprovalMode::Wait(时长)`，回答用 `Session::resolve_approval(编号, 决定)` | 默认 `ApprovalMode::Deny`：策略询问的调用立即被拒绝。等待时运行发出带审批编号、工具名与参数的 `approval_requested`，超时或取消按拒绝处理。读事件的循环里不要等人回答，否则超时与取消要到下一次读取才生效 |
| 观察所有运行 | 实现 `hooks::Hook::on_event` | 事件只从 `Run` 读出；不等待的钩子按顺序看到它观察的每个事件（默认不含流式增量 `message_update`，见 `Hook::observes`），被取消的运行也一样。追踪导出（`zx_app::otel::Telemetry`）就这样注册为钩子 |
| 标明调用者 | `SessionOptions::labels`（如租户或所属对话）、`Input::User { text, actor }` | 经 `PolicyContext` 与 `HookContext` 的 `labels`、`actor` 交给策略与钩子；`actor` 是最近一条提问的发送者，扩展发的消息不改变它。内核只记录与传递，不据此判断 |
| 限制待处理的输入 | `PendingInput::held(输入, 凭证)`，交给 `queues().steer` 或 `follow_up` | 内核的输入队列不限量，也不计配额：宿主自己计量，凭证在输入离开队列时丢弃（何时离开见 `PendingInput`），`Session::take_queued` 连同凭证一起取出。IM 服务就这样限制每个对话（`crates/zx-im/src/server/admission.rs`） |
| 存会话 | `AgentConfig::storage`：`transcript::MemoryStorage`（默认）、`builtin::jsonl::JsonlStorage`，或实现 `transcript::SessionStorage` 与 `SessionLog` | `open` 让 `Agent` 成为会话的写入者，`read` 是只读快照（交给 `Transcript::snapshot`）。自己实现时，列出用 `SessionList::new`、读头部用 `SessionQuery::arrange_headers` 排序与截断 |
| 建自己的会话索引 | `Agent::session_headers` | 只读头部、不读记录，按开始时间从新到旧；打开会话时仍完整校验。读不出头部或版本不符的会话不在其中，`list_sessions` 把它们列为读不出。JSONL 存储不缓存，每次读取查询范围内每个会话文件的第一行 |
| 按模型的上下文窗口自动摘要 | `AgentConfig::compaction` 的 `trigger_tokens` 设为 `TokenTrigger::BelowWindow { reserve_tokens }`，窗口写进 `AgentConfig::models` | 每次请求前按会话当时的模型取窗口，`Session::set_model` 换了模型就随之改变；窗口未知的模型没有 token 阈值 |
| 自己产出摘要 | 实现 `compaction::Summarizer`，放进 `AgentConfig::summarizer` | 何时摘要、读哪一段（至多 2 MiB）、记录与校验引用、超时与取消仍由内核负责；一次摘要计为运行的一次模型请求，它报告的用量计入预算 |
| 打包工具、策略、钩子与命令 | 实现 `extensions::Extension`，放进 `AgentConfig::extensions` | 建 `Agent` 时取一次并校验，所有会话共用 |
| 让内置工具在别处运行 | 实现 `builtin::executor::Executor`；`builtin::tools::executor_tools` 按名字（`EXECUTOR_TOOL_NAMES`）建出跑在它上面的工具 | 命令行交给执行器自己的 shell，两个内置执行器都用 bash。`environment_section` 写提示词里描述执行环境的段落，默认只有工作目录。`edit_file` 与 `write_file` 在宿主给的 `FileMutationQueue` 里按文件排队，几个会话要协调时共用一个 |
| 启动前检查附带实现的设置 | `ContainerConfig`、`OutputStoreConfig`、`mcp::ServerConfig` 与 `policy::ProgramOptions` 的 `validate` | 错误按字段名说明。容器与落盘的错误是枚举（`InvalidContainerConfig`、`InvalidOutputStoreConfig`），容器启动时工作目录不可用的错误可向下转型为 `UnusableWorkspace`：从自己的配置文件读设置的宿主按变体换成自己的键。`ServerConfig::validate` 返回模型看到的工具名 |
| 长结果落盘 | `OutputStore::new(cwd, OutputStoreConfig::new(目录))`，交给 `SpillOutputs::with_reader` | 目录由宿主指定，相对路径相对会话的工作目录。宿主自己分页读文件的工具（如 `zx-app` 的 `read_skill`）用 `builtin::tools::paging` 的 `read_text` 与 `Pager`，脚注的写法与 `read_output` 相同 |
| 调整内置的策略与钩子 | `CommandPolicy` 的 `deny_also`、`deny_programs_also`、`read_only_programs_also`；`ContinueOnLength::with_prompt` | 默认列表（`DEFAULT_DENIED_PATTERNS`、`DEFAULT_DENIED_PROGRAMS`、`DEFAULT_READ_ONLY_PROGRAMS`）只能追加。`AutoApprove` 不是策略：它交给 `RulePolicy::auto_approve`，只把 bash 命令的询问改为放行 |

附带的实现：`ai::openai::OpenAiCompatible`、`transcript::MemoryStorage`、`builtin::jsonl::JsonlStorage`、`builtin::mcp::Server`（一个 `Extension`，feature `mcp`）、`builtin::executor::LocalExecutor` 与 `ContainerExecutor`、`builtin::redact::RedactSecrets`。IM 的扩展点 `zx_im::ImChannel` 与 `zx_im::StatusStore` 在程序 `zx-im` 的库里。

## 与命令行共用配置

`zx_app::Application` 让其他 Rust 宿主（如 Web 服务）与命令行共用配置装配（`config.toml`、agent 文件、执行环境与 MCP），不依赖命令行解析、ACP 与 IM；命令行的 `run`、`chat`、`acp` 与 `zx-im` 也经它得到 agent。`zx-app` 不打印：启动与收尾要说的都交给宿主，由宿主输出或记日志。

- `Application::from_config` 用显式传入的配置，先经 `Config::validate` 检查，不读 `ZX_MODEL`、`ZX_BASE_URL`、`ZX_SECRET_FILES`；配置里命名的 API 与 MCP 凭据仍从环境变量读取。`Application::from_environment` 读主目录的 `config.toml`：模型取 `Overrides::model` > `ZX_MODEL` > `model`，`ZX_BASE_URL` 覆盖服务地址，`ZX_SECRET_FILES` 中的文件另外脱敏。两者都按 agent 文件的规则检查 `Overrides`（花费预算为负数或不是有限值时报错）；没有模型时的错误可向下转型为 `NoModel`，宿主据此说明自己给模型的方式。
- `build` 为给定的模型（通常是 `model()`）得到 `ConfiguredAgent`，还不创建会话；会话存在内存、文件还是目录由宿主经 `SessionPlace` 选择（内存中关闭的会话会被遗忘），谁来审批经 `Approvals` 选择。启动提示（`notices`）的输出归宿主。
- `zx_app::shutdown` 是每个程序都要的收尾：`with_telemetry(服务名, body)` 在环境变量给出 OTLP 地址时启动追踪导出（宿主给的服务名只在环境变量没有指定时使用，见 [REFERENCE.md](REFERENCE.md#opentelemetry-追踪)），把导出交给 `body`（经 `Application::with_traces` 注册它的钩子），`body` 返回后导出剩下的部分，并把丢弃的事件数与导出失败作为 `ExportEnd` 与 `body` 的结果一起返回；`spawn_signal_handler` 让 SIGINT 与 SIGTERM 取消一个 `CancellationToken`（Unix 上第二个信号立即结束进程），注册不了时返回错误，由宿主决定是否继续；`next_event` 读运行的下一个事件，取消后让运行停下并读到结束。自己逐个处理信号的宿主用 `Signals` 与 `Interrupt`。

用法与验收见 `crates/zx-app/tests/application.rs`。

---

LIBRARY.md · v0.15 · 2026年11月
