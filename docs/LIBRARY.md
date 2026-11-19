# 作为库使用

`zx-agent` 库再导出各 crate：内核 `zx-core` 按域模块原样导出（`zx_agent::agent`、`zx_agent::tools`、`zx_agent::policy`、`zx_agent::hooks`、`zx_agent::transcript` 等），模型层在 `zx_agent::ai`，`zx-builtin` 附带的实现在 `zx_agent::builtin`。库里没有接入方式：`config.toml`、agent 文件、OpenTelemetry 导出与进程的收尾在应用层 `zx-app`（见下文“与命令行共用配置”）。只需要内核时可以只依赖 `zx-core` 与 `zx-ai`，按需加上 `zx-ai-openai` 或 `zx-ai-anthropic`。`zx-core` 按 JSON Schema 校验工具参数的部分是默认开启的 feature `json-schema`，关掉时内核只检查参数是 JSON 对象，其余由工具自己检查；`zx-builtin` 的 MCP 客户端是默认开启的 feature `mcp`，关掉时不编译 HTTP 客户端与 MCP SDK。测试用的脚本化 provider `ai::faux::FauxProvider`（按脚本回答，记下每个请求）只在 feature `testing` 打开时编译，默认关闭：嵌入方的测试在 `[dev-dependencies]` 里给 `zx-agent` 加上这个 feature。各 crate 都不发布到 crates.io。

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
| 接入别的模型服务 | 实现 `ai::Provider`，参照 `ai::openai::OpenAiCompatible` 与 `ai::anthropic::AnthropicMessages`；API key 用 `ai::ApiKey`（固定的，或每次请求前经 `ApiKeySource` 取得）。要让 `config.toml` 按名字选用它，经 `zx_app::Application::register_provider` 注册（见下文）。走 HTTP 的可以用 `zx-ai-http` 的共用件：设置检查、取 key 与发送、按限额读事件流（`stream::Events`）、按请求的结果结束 provider 的流（`stream::end`）、请求失败（`Failure`）与 Retry-After | `stream` 立即返回、从不报错：失败编码为最后的 `StreamEvent::Error`，回答的 `provider_error` 给出失败的种类（`ProviderErrorKind`）。内核只按种类决定重试与上下文溢出恢复，没有种类的失败不重试；服务的状态、错误码与措辞属于哪一种由 provider 判断。测试里用 `FauxStep::fail(种类, 文本)` 脚本化失败 |
| 开、续、关会话 | `Agent` 的 `create_session`、`open_session`、`session`、`close_session`、`list_sessions` | 会话在创建时给定的目录工作（`SessionOptions::cwd`），再次打开时不变。再打开同一个会话得到同一个 `Session`；`close_session` 取消进行中的运行，之后旧句柄返回 `SessionError::Closed`。记录写不进去的会话（写入失败、超时，或在写入途中丢弃了 `Run`）在那次操作结束时自行关闭，`open_session` 从存储重新打开它 |
| 运行 | `Session` 的 `send`、`continue_run`、`compact`，各返回一个 `Run` | 读它才推进，事件经有界通道，读得慢运行就等；丢弃即取消并归还会话。取消发生在输入钩子（`Hook::on_input`）运行期间时，这次的输入不写入会话，以免没有钩子的改写就进入模型读的上下文；没有输入钩子时，输入先写入，运行随后停止。`Session` 可以克隆，克隆出来的是同一个会话；同一时间只有一次运行，重叠调用返回 Busy |
| 给单个会话工具、钩子与提示词段落 | 实现 `agent::SessionSetup` | 创建或打开会话时调用，随会话释放；失败时新会话不会留在存储里。给出的段名经 `section_names` 声明，扩展不能再用这些名字与 `preamble`。不需要按会话区分时，工具直接放进 `AgentConfig::tools` |
| 让人审批 | `AgentConfig::approvals` 设为 `ApprovalMode::Wait(时长)`，回答用 `Session::resolve_approval(编号, 决定)` | 默认 `ApprovalMode::Deny`：策略询问的调用立即被拒绝。等待时运行发出带审批编号、工具名与参数的 `approval_requested`，超时或取消按拒绝处理。读事件的循环里不要等人回答，否则超时与取消要到下一次读取才生效 |
| 观察所有运行 | 实现 `hooks::Hook::on_event` | 事件只从 `Run` 读出；不等待的钩子按顺序看到它观察的每个事件（默认不含流式增量 `message_update`，见 `Hook::observes`），被取消的运行也一样。追踪导出（`zx_app::otel::Telemetry`）就这样注册为钩子 |
| 标明调用者 | `SessionOptions::labels`（如租户或所属对话）、`Input::User { text, actor }` | 经 `PolicyContext` 与 `HookContext` 的 `labels`、`actor` 交给策略与钩子；`actor` 是最近一条提问的发送者，扩展发的消息不改变它；`finish_step` 钩子要来的追问没有发送者，此后为空。内核只记录与传递，不据此判断 |
| 限制待处理的输入 | `PendingInput::held(输入, 凭证)`，交给 `queues().steer`、`follow_up` 或 `Session::send` | 内核的输入队列不限量，也不计配额：宿主自己计量，凭证在输入离开队列时丢弃（何时离开见 `PendingInput`），`send` 的提问写入会话后丢弃；`Session::take_queued` 连同凭证一起取出，`PendingInput::into_parts` 把凭证还给宿主。IM 服务就这样限制每个对话，并在消息写入会话后才把它从 backlog 删除（`crates/zx-im/src/server/admission.rs`） |
| 存会话 | `AgentConfig::storage`：`transcript::MemoryStorage`（默认）、`builtin::jsonl::JsonlStorage`，或实现 `transcript::SessionStorage` 与 `SessionLog` | `open` 让 `Agent` 成为会话的写入者，`read` 是只读快照（交给 `Transcript::snapshot`）。自己实现时，列出用 `SessionList::new`、读头部用 `SessionQuery::arrange_headers` 排序与截断 |
| 建自己的会话索引 | `Agent::session_headers` | 只读头部、不读记录，按开始时间从新到旧；打开会话时仍完整校验。读不出头部或版本不符的会话不在其中，`list_sessions` 把它们列为读不出。JSONL 存储不缓存，每次读取查询范围内每个会话文件的第一行 |
| 按模型的上下文窗口自动摘要 | `AgentConfig::compaction` 的 `trigger_tokens` 设为 `Some(TokenTrigger::BelowWindow { reserve_tokens })`（`None` 关闭 token 阈值），窗口写进 `AgentConfig::models` | 每次请求前按会话当时的模型取窗口，`Session::set_model` 换了模型就随之改变；窗口未知的模型没有 token 阈值 |
| 自己产出摘要 | 实现 `compaction::Summarizer`，放进 `AgentConfig::summarizer` | 何时摘要、读哪一段（至多 2 MiB）、记录与校验引用、超时与取消仍由内核负责；一次摘要计为运行的一次模型请求，它报告的用量计入预算 |
| 打包工具、策略、钩子与命令 | 实现 `extensions::Extension`，放进 `AgentConfig::extensions` | 建 `Agent` 时取一次并校验，所有会话共用 |
| 让内置工具在别处运行 | 实现 `builtin::executor::Executor`；`builtin::tools::executor_tools` 按名字（`EXECUTOR_TOOL_NAMES`）建出跑在它上面的工具。要让 `config.toml` 的 `execution` 按名字选用它，实现 `zx_app::ExecutorFactory`，经 `register_executor` 注册（见下文） | 命令行交给执行器自己的 shell，两个内置执行器都用 bash。`environment_section` 写提示词里描述执行环境的段落，默认只有工作目录。`edit_file` 与 `write_file` 在宿主给的 `FileMutationQueue` 里按文件排队，几个会话要协调时共用一个 |
| 启动前检查附带实现的设置 | `ContainerConfig`、`OutputStoreConfig`、`mcp::ServerConfig` 与 `policy::ProgramOptions` 的 `validate` | 错误按字段名说明。容器与落盘的错误是枚举（`InvalidContainerConfig`、`InvalidOutputStoreConfig`），容器启动时工作目录不可用的错误可向下转型为 `UnusableWorkspace`：从自己的配置文件读设置的宿主按变体换成自己的键。`ServerConfig::validate` 返回模型看到的工具名 |
| 长结果落盘 | `OutputStore::new(cwd, OutputStoreConfig::new(目录))`，交给 `SpillOutputs::with_reader` | 目录由宿主指定，相对路径相对会话的工作目录。宿主自己分页读文件的工具（如 `zx-app` 的 `read_skill`）用 `builtin::tools::paging` 的 `read_text` 与 `Pager`，脚注的写法与 `read_output` 相同 |
| 调整内置的策略与钩子 | `CommandPolicy` 的 `deny_also`、`deny_programs_also`、`read_only_programs_also`；`ContinueOnLength::with_prompt` | 默认列表（`DEFAULT_DENIED_PATTERNS`、`DEFAULT_DENIED_PROGRAMS`、`DEFAULT_READ_ONLY_PROGRAMS`）只能追加。`AutoApprove` 不是策略：它交给 `RulePolicy::auto_approve`，只把 bash 命令的询问改为放行 |

附带的实现：`ai::openai::OpenAiCompatible`、`ai::anthropic::AnthropicMessages`、`transcript::MemoryStorage`、`builtin::jsonl::JsonlStorage`、`builtin::mcp::Server`（一个 `Extension`，feature `mcp`）、`builtin::executor::LocalExecutor` 与 `ContainerExecutor`、`builtin::redact::RedactSecrets`。IM 的扩展点 `zx_im::ImChannel` 与 `zx_im::StatusStore` 在程序 `zx-im` 的库里。

## 与命令行共用配置

`zx_app::Application` 让其他 Rust 宿主（如 Web 服务）与命令行共用配置装配（`config.toml`、agent 文件、执行环境与 MCP），不依赖命令行解析、ACP 与 IM；命令行的 `run`、`chat`、`acp` 与 `zx-im` 也经它得到 agent。`zx-app` 不打印：启动与收尾要说的都交给宿主，由宿主输出或记日志。

- `Application::from_config` 用显式传入的配置，先经 `Config::validate` 检查，不读 `ZX_MODEL`、`ZX_BASE_URL`、`ZX_SECRET_FILES`；配置里命名的 API 与 MCP 凭据仍从环境变量读取。`Application::from_environment` 读主目录的 `config.toml`：模型取 `Overrides::model` > `ZX_MODEL` > `model`，`ZX_BASE_URL` 覆盖服务地址，`ZX_SECRET_FILES` 中的文件另外脱敏。两者都按 agent 文件的规则检查 `Overrides`（花费预算为负数或不是有限值时报错）；没有模型时的错误可向下转型为 `NoModel`，宿主据此说明自己给模型的方式。
- `build` 为给定的模型（通常是 `model()`）得到 `ConfiguredAgent`，还不创建会话；会话存在内存、文件还是目录由宿主经 `SessionPlace` 选择（内存中关闭的会话会被遗忘），存在宿主自己的存储（如数据库）里时用 `SessionPlace::from_storage`；谁来审批经 `Approvals` 选择。启动提示（`notices`）的输出归宿主。agent 的说明不提任何宿主；宿主要让模型知道自己的事（如回答怎样送达）时，经 `add_prompt_section` 给每个会话的提示词加一段，排在装配给出的段落之后。
- 宿主自己的部件只用三个动词给出：`add_*` 在装配自带的旁边加一个（`add_tool`、`add_hook`、`add_policy`、`add_extension`、`add_prompt_section`、`add_hidden_env`），`set_*` 换掉唯一的那个（`set_summarizer`、`set_traces`），`register_*` 登记由配置按名字选用的实现（见下一条）。它们与自带的同等对待：宿主的工具像内置工具一样由 agent 文件的 `tools` 启用，不经询问直接执行（`permissions.ask` 列出的除外），也能被 `permissions.deny` 拒绝；它和内置工具一样要自己截断输出、响应取消，脱敏与长结果落盘照常作用于它。宿主的扩展像 MCP 服务器一样，工具对所有 agent 提供，`permissions.allow` 没列出就询问。钩子按阶段执行，脱敏总在宿主的钩子之后；策略取最严的结论，宿主的策略只能收紧。工具、钩子、策略与扩展的名字与同类中内置的或已有的重复时，`build` 报错；扩展的钩子与策略都用扩展的名字，所以扩展也不能与内置的钩子或策略同名。宿主的一个钩子与一个策略可以同名，内核的两条链各管各的名字。
- 由 `config.toml` 按名字选用的实现用 `register_*` 注册，内置的也这样注册，名字重复时 `build` 报错。`register_provider(名字, factory)` 加一个 provider：`provider.api` 写这个名字时，`build` 调用它的 `ProviderFactory::build`，交给它 `[provider]` 中除 `api`、`apiKeyEnv`、`maxRetries` 以外的键（`ProviderInput::options`，由它检查，错误按 `config.toml` 的写法说明键名）、从 `apiKeyEnv` 读到的 key（没写 `apiKeyEnv` 时读 `api_key_env()` 给出的变量）与 `config.toml` 的推理强度；内置的是 `openai` 与 `anthropic`。存放 key 的变量照样对命令隐藏，key 照样脱敏。`register_executor(名字, factory)` 加一个执行环境：`execution` 写这个名字时，每个会话创建或打开时调用它的 `ExecutorFactory::start`，交给它会话的工作目录、ID、记录所在的位置与要对命令隐藏的变量，得到的 `Executor` 随会话保留；内置的是 `local` 与 `container`。只有 `isolates()` 返回 `true` 的执行环境允许 agent 设 `permissions.approveAll`。
- `zx_app::shutdown` 是每个程序都要的收尾：`with_telemetry(服务名, body)` 在环境变量给出 OTLP 地址时启动追踪导出（宿主给的服务名只在环境变量没有指定时使用，见 [REFERENCE.md](REFERENCE.md#opentelemetry-追踪)），把导出交给 `body`（经 `Application::set_traces` 注册它的钩子），`body` 返回后导出剩下的部分，并把丢弃的事件数与导出失败作为 `ExportEnd` 与 `body` 的结果一起返回；`spawn_signal_handler` 让 SIGINT 与 SIGTERM 取消一个 `CancellationToken`（Unix 上第二个信号立即结束进程），注册不了时返回错误，由宿主决定是否继续；`next_event` 读运行的下一个事件，取消后让运行停下并读到结束。自己逐个处理信号的宿主用 `Signals` 与 `Interrupt`。

用法与验收见 `crates/zx-app/tests/application.rs`。

## IM 平台

程序 `zx-im` 的库导出 `zx_im::Program`：`zx_im::main` 就是 `Program::new().run()`。宿主要接入别的聊天平台时，实现 `ImChannel` 与从 `im.toml` 的段建出它的 `PlatformFactory`，在自己程序的 `main` 里写 `Program::new().register_platform(名字, factory).run().await`；`im.toml` 中以这个名字为段名的段交给它的 `channel`（`PlatformInput` 给出段的内容、审批卡片的文案与报告错误的回调），返回的 `BuiltChannel` 带上存放凭据的变量名，它们对命令隐藏、值被脱敏。channel 的 `platform()` 须与注册的名字相同，名字与内置的 `slack`、⚑ D:5/CHINA FLAG: `feishu` 或别的重复时启动报错。自己的程序还要给 agent 加工具、钩子、策略、扩展、provider 或执行环境时，用 `Program::customize(|app| app.add_tool(..).register_provider(..))`：闭包拿到的是 `Application`，动词与上文相同；程序自己的部件排在其后：平台的凭据经 `add_hidden_env` 加在闭包给出的变量旁边，追踪导出归程序。

---

LIBRARY.md · v0.26 · 2026年11月
