# 技术方案 · P7 扩展接口与 MCP 客户端

| 信息 | 内容 |
|-|-|
| 版本 | `v0.1` |
| 日期 | `2026-11-01` |
| 状态 | 草稿，待评审 |
| Owner | 待指定 |
| 对应 | `docs/ROADMAP.md:892-902`（P7 插件与 MCP）；范围按用户 2026-11-01 的决定（D8）收窄（1.2） |
| 代码核对基线 | `a345b39`（含 PR #35）；源码行号以此为准，`docs/ROADMAP.md` 的行号以本稿所在 PR 修改后的版本为准 |
| Milestone | 一期：公开 `Extension`、MCP 客户端（只接工具，只用 Streamable HTTP）<br>以后按需：运行中更新工具列表、OAuth、MCP 的其他能力 |

> 缺口 ①（依赖与 TLS）已在本稿所在的 PR 中解决：rmcp 只支持 reqwest 0.13，与项目原来的 0.12 并存会让现有的 `wss://` 长连接 panic。项目的 reqwest 已先升级到 0.13，与 rmcp 共用一份，rustls 只保留一个加密后端（决策 D9）。

---

## 1. 背景与范围

P7 的目标是第三方能力不改内核即可接入。本稿定两件事：公开的 `Extension` 接口，以及作为它第一个实现的 MCP 客户端。MCP 服务器由外部服务实现，本项目只做客户端。

### 1.1 为什么做

| 诉求 | 今天的状况 |
|-|-|
| 接入外部团队提供的查询能力（“部署方向”中的“专用查询工具 / MCP”，`docs/ROADMAP.md:515`） | 只有内置工具（`src/tools/mod.rs:18-24`），新增工具要改代码、重新编译 |
| 以库的方式打包扩展 | SDK 可以自己往 `LoopSpec.tools` 与 `HookChain` 里加，但没有把工具、钩子、提示词段落打包的接口；“扩展只能用 `Extension` 阶段”留待 P7 实施（`docs/ROADMAP.md:868`） |
| 外部工具不能沿用内置工具的默认放行 | 非 bash 工具只受 `askTools` / `denyTools` 约束，其余兜底为 Allow（`src/cli.rs:611-632`） |

### 1.2 本期范围

- 公开 `Extension` trait（技术方案 5.5 节的 L1）：名称、工具、提示词段落、钩子。
- MCP 客户端：按 profile 中的配置连接外部 MCP 服务器，把选定的工具交给模型。调用经过现有的策略、审批、截断、脱敏与审计。
- 用户 2026-11-01 定的边界：只做客户端；只接工具；只支持 Streamable HTTP。

### 1.3 非本期范围

| 不做 | 原因 |
|-|-|
| MCP 服务器与领域工具（Kubernetes、Prometheus 查询等） | 由外部服务实现（用户决定） |
| stdio 传输 | 外部服务独立部署，Pod 内不启动其他进程（用户决定） |
| resources、prompts、sampling、elicitation、roots 等 | 只接工具（用户决定） |
| OAuth 授权流程 | 规范中授权是可选的（附录 9.1 第 8 条）；一期只支持静态 token（缺口 ③） |
| 运行中更新工具列表 | 工具集在组装时固定在 `LoopSpec.tools`；收到 `list_changed` 只记录（3.6） |
| 扩展注册命令 | P7 清单原先包括“命令”，但只有 chat 有命令，没有使用者。以后以带默认实现的方法加入，不破坏兼容（`docs/ROADMAP.md:897` 已注明） |
| 钩子进程（L2）、动态库、WASM、热加载 | 技术方案 5.5 节另行设计，或已排除 |

### 1.4 相关链接

| 资料 | 链接 |
|-|-|
| MCP 规范 2026-07-28（最新正式版） | github.com/modelcontextprotocol/modelcontextprotocol，`docs/specification/2026-07-28/`，release tag 提交 `5f5440bb` |
| MCP 规范 2025-11-25（上一版，有握手与会话） | 同一仓库 `docs/specification/2025-11-25/`，提交 `38c84e9f` |
| rmcp（官方 Rust SDK） | https://github.com/modelcontextprotocol/rust-sdk ，crates.io `rmcp` 3.8.0 |
| OpenAI 函数名约束 | github.com/openai/openai-openapi `openapi.yaml`，提交 `c2aecc2`，`FunctionObject.name` |
| 技术方案 5.5 节 | `docs/zx-agent-general-kernel-proposal.html` |

---

## 2. 整体设计

### 2.1 架构图

```
 profile 的 [[mcp]]：服务器地址、凭据变量、显式列出的工具
        │ 进程启动时，每个服务器连接一次
        ▼
 mcp::connect ── server/discover（旧服务器回退到 initialize）、tools/list ──▶ 外部 MCP 服务
        │  只保留配置中列出的工具，每个包成一个 McpTool               （Streamable HTTP）
        ▼
 McpExtension（实现 Extension）
        ▼
 cli::prepare（唯一组装入口）
   ├ 工具：内置工具 + 扩展工具；重名在发请求前拒绝
   ├ 策略：MCP 工具默认需要审批，allowTools 中的直接执行
   ├ 凭据：tokenEnv 从命令环境中删除，值登记脱敏
   └ 提示词：扩展的段落附在内置段落之后（MCP 不加段落）
        ▼
 run_loop：before_tool（策略、审批）→ McpTool::execute ── tools/call ──▶ 外部服务
                                                ◀── content / isError ──
           截断 → after_tool（扩展 → 脱敏 → 落盘）→ 写入会话

 serve：连接由进程持有，每个 IM 线程的会话共用同一组 McpTool。
```

### 2.2 建成之后怎么用

1. 平台团队部署一个 Prometheus 查询的 MCP 服务，地址 `https://prom-mcp.monitoring.svc/mcp`，提供 `query` 与 `query_range`，用 token 鉴权。
2. 运维在 profile 中加一段 `[[mcp]]`（3.2），列出这两个工具，把 `query` 放进 `allowTools`。
3. 在 chat 或 IM 中问“shop 的 5xx 从什么时候开始升高”。模型调用 `prom__query`，直接执行；调用 `prom__query_range` 时先审批。
4. 会话文件记录工具定义、每次调用的参数与结果；token 不出现在命令环境、会话与日志中。

### 2.3 方案选型

#### 2.3.1 MCP 客户端：采用 rmcp

| 对比项 | rmcp 3.8.0（采用） | 在项目的 reqwest 上自己实现 |
|-|-|-|
| 协议版本 | 支持 2024-11-05 至 2026-07-28。`Auto` 模式先用新协议，对方是旧服务器或 10 秒内不回应时回退到 `initialize`（`rmcp-3.8.0/src/service/client.rs:704-719,861-899`） | 2026-07-28 去掉了握手与会话（附录 9.1 第 1–3 条），新旧两套生命周期都要自己写 |
| 规范细节 | 已实现 `Mcp-Method`、`Mcp-Name` 与 `x-mcp-header`（`transport/common/mcp_headers.rs`）、分页、MRTR 多轮、会话过期后重新初始化、取消 | 逐项自己实现，并跟进规范 |
| 依赖 | 项目的 reqwest 已升级到 0.13，与 rmcp 共用（缺口 ①）；rmcp 再增加 15 个包（附录 9.2） | 不新增 |
| 授权 | D2 已授权 | — |

**结论：采用 rmcp。** 核心原因是用户要求本项目不做过多实现。2026-07-28 刚改掉握手与会话，规范仍在快速变化，跟进的成本应当留在官方 SDK 里。代价是 rmcp 再增加 15 个包；它需要的 reqwest 0.13 已先行升级（缺口 ①）。

#### 2.3.2 配置放在 profile

| 对比项 | profile（采用） | 单独的配置文件 | 命令行参数 |
|-|-|-|-|
| 与现有分工 | profile 决定工具和规则（`.claude/CLAUDE.md`“配置与文件内容分工”），run、chat、serve 都经 `cli::prepare` 读取 | 工具与规则分在两处 | 同左 |
| 写法 | TOML 表，可以写注释 | 同左 | 多字段的列表不便写 |
| 显式性 | run 的默认 profile 不含 MCP，要用必须显式 `--profile` | 要显式指定文件 | 显式 |

#### 2.3.3 连接：每个进程一份

serve 为每个 IM 线程调用一次 `cli::prepare`（`src/serve.rs:280-313`）。在 `prepare` 中连接，每个线程都会各连一次。规范要求 `tools/list` 的结果不随连接变化（附录 9.1 第 4 条）。所以启动时连接一次，由进程持有，传给每次 `prepare`。

---

## 3. 详细设计

### 3.1 Extension 接口

```rust
/// Tools, hooks and prompt sections registered together at assembly.
pub trait Extension: Send + Sync {
    /// Unique among extensions; names the hooks in hook_failed and in traces.
    fn name(&self) -> &str;
    fn tools(&self) -> Vec<Arc<dyn Tool>> { Vec::new() }
    /// (tag, text) pairs placed after the built-in sections of the system prompt.
    fn prompt_sections(&self) -> Vec<(String, String)> { Vec::new() }
    fn hooks(&self) -> Vec<Arc<dyn Hooks>> { Vec::new() }
}
```

| 项 | 规则 | 依据 |
|-|-|-|
| 位置 | `zx_agent::extensions`；内核模块 `agent` 不依赖它 | 场景策略不进内核 |
| 钩子阶段 | 只能是 `Extension`，声明其他阶段时组装报错 | A4 定稿：内置策略最先，脱敏与落盘在扩展之后（`docs/ROADMAP.md:853,868`） |
| 钩子名称 | 以 `Extension::name()` 注册（`HookOptions`，`src/agent/hooks.rs:370`） | A3 |
| 工具重名 | 与内置工具或其他扩展的工具重名时，组装报错 | A5；`LoopSpec::validate`（`src/agent/agent_loop.rs:47-71`） |
| 提示词段落 | 不能与内置段落同名（`environment`、`command_policy`、`project_context`、`available_skills`），附在内置段落之后 | 内置段落是程序提供的事实（`src/cli.rs:588-596`） |
| 组装 | `cli::prepare` 接收扩展列表并统一注册；SDK 用 `extensions::register` 注册工具与钩子，段落自行传给 `build_system_prompt`（`src/prompt.rs:15`） | A6 |

与技术方案草图（5.5 节）的差异：
- `hooks()` 返回列表而不是 `Option`：一个扩展可能有多个钩子。
- `prompt_sections` 不接收 `PromptEnv`：代码中没有这个类型，环境事实已在 `environment` 段落中。

MCP 工具的审批规则由 `cli::prepare` 以内置策略的身份加入（3.3），不经 `Extension` 接口。扩展的钩子只能处在 `Extension` 阶段，而 `before_tool` 取所有钩子中最严的结论（`src/agent/hooks.rs:609` 的测试），所以扩展放宽不了策略给出的审批或拒绝。

### 3.2 MCP 配置

```toml
[[mcp]]
name = "prom"                                 # 小写字母、数字与 -，作为工具名前缀
url = "https://prom-mcp.monitoring.svc/mcp"
tokenEnv = "PROM_MCP_TOKEN"                   # 可选：以 Authorization: Bearer 发送
tools = ["query", "query_range"]              # 必填：交给模型的工具，其余不暴露
allowTools = ["query"]                        # 可选：不经审批直接执行
timeoutSecs = 60                              # 可选：单次调用的上限，默认同 --tool-timeout-secs
```

| 规则 | 理由 |
|-|-|
| `name` 不重复；`tools` 非空；`allowTools` 是 `tools` 的子集 | 配置错误在启动时暴露 |
| `tools` 中的工具服务器没有提供时，启动失败 | 配置与服务器不一致时尽早发现 |
| 服务器提供、但配置中没列的工具不交给模型 | 信任精确到工具；没列出的工具描述也不进入上下文 |
| `extends` 时，子 profile 的 `[[mcp]]` 整体替换父 profile 的 | 与 `tools` 的覆盖方式相同；内置 profile 没有 `[[mcp]]` |
| `--tools` 只作用于内置工具；不新增命令行参数 | run 要用 MCP，必须显式 `--profile` |

### 3.3 策略与审批

| 情形 | 结论 |
|-|-|
| 默认 | 每次调用前审批：chat 在终端询问，serve 发审批卡片 |
| `allowTools` 中的工具 | 直接执行 |
| `rules.denyTools` 中的工具 | 拒绝，优先于 `allowTools`（多个策略取最严的结论） |
| run（无人值守） | 没有人审批，需要审批即拒绝（`NoHuman`，`src/interaction.rs:44-49`）；只有 `allowTools` 中的工具能执行 |
| 本会话内的授权 | 与 bash 相同 |

- 实现：`cli::prepare` 为不在 `allowTools` 中的 MCP 工具追加 `Rule { Tool(name) → Ask }`，与 `askTools` 的规则放在同一个 `RulePolicy` 中（`src/cli.rs:611-632`）。
- 不用 annotations 决定放行。规范：“clients **MUST** consider tool annotations to be untrusted unless they come from trusted servers”（附录 9.1 第 5 条）。是否放行由运维在配置中写明。
- `effects()` 一律为 `SideEffects`：每次调用前先写 `tool_call_started`，中断后恢复为“结果未知”，不重放（不变量 10）。
- `execution_mode()` 为 `Parallel`：同一回答中的多个调用可以并发。

### 3.4 凭据

- `tokenEnv` 在启动时读取，缺失或为空时报配置错误。
- 变量名加入 `hidden_env`，命令看不到；值登记脱敏，出现在工具输出中时被替换（`src/cli.rs:565-568,636-645`）。
- token 只以 `Authorization: Bearer` 发给该服务器，每个请求都带（规范要求，附录 9.1 第 8 条）；不进 URL、会话、事件与日志。
- rmcp 的传输配置类型派生了 `Debug`，会打印 token（`rmcp-3.8.0/src/transport/streamable_http_client.rs:2092,2113-2114`），代码中不打印它。
- `http://` 地址允许使用（集群内服务、服务网格中常见），启动行标出 token 以明文发送（缺口 ⑤）。

### 3.5 工具与调用

**命名**：模型看到的名字是 `<name>__<工具名>`。

- OpenAI 的函数名只允许 `a-z`、`A-Z`、`0-9`、下划线与短横线，最长 64（附录 9.3）。
- MCP 建议工具名为 1–128 个字符，可以含 `.`；聚合多个服务器时建议加服务器前缀（附录 9.1 第 5 条）。
- 所以：工具名中上述允许范围以外的字符替换为 `_`；替换后重名或超过 64 个字符时，启动报配置错误并列出该工具。调用服务器时用原名。`name` 不含 `_`，第一个 `__` 之前就是服务器名。

**定义**：`ToolDef { name, description, parameters: inputSchema }`。

- 描述原样交给模型，超过 4 KiB 时截断：描述随每个请求发送，且来自外部服务，可能含注入内容（技术方案风险表，`docs/zx-agent-general-kernel-proposal.html:461`）。
- 服务器返回的 `instructions` 不加入系统提示词。

**结果**：

| MCP 结果 | 交给模型的工具结果 |
|-|-|
| `content` 中的 text | 按顺序拼接 |
| image、audio | 一行说明（类型与大小），不发送二进制 |
| resource_link | 一行说明（URI） |
| 内嵌 resource | 文本资源取其文本，二进制只写说明 |
| 只有 `structuredContent`、没有 text | 序列化为 JSON 文本（规范建议服务器同时给出 text，附录 9.1 第 4 条） |
| `isError: true` | `is_error` 的工具结果，循环继续（不变量 2；规范要求把执行错误交给模型，附录 9.1 第 4 条） |
| JSON-RPC 错误、HTTP 失败、超时、取消 | 错误结果，写明服务器名与原因，不含 token |

**调用过程**：

- 参数必须是 JSON 对象（`validate_arguments`）；按 schema 的校验交给服务器。不校验 `structuredContent` 是否符合 `outputSchema`：规范对此是 SHOULD，项目没有 JSON Schema 校验的依赖。
- 截断：与 bash 相同，2000 行或 50 KiB（`src/tools/truncate.rs:5-7`，不变量 8）。
- 超时：rmcp 没有连接与请求超时（reqwest 0.13 默认不设），`McpTool` 给每次调用包一层 `tokio::time::timeout`。
- 取消：`ctx.cancel` 触发或运行被取消时，丢弃进行中的调用。rmcp 在丢弃时尽力发送 `notifications/cancelled`，并中止本地的请求（`service/client.rs:480-511`，`transport/worker.rs:268-284`）。新协议以断开响应流表示取消，旧协议要求发通知（附录 9.1 第 7 条），两者都覆盖。
- 审计：工具定义随 `request_config` 写入会话（`src/session/request.rs:14-20`），调用与结果照常写入。规范建议“Log tool usage for audit purposes”（附录 9.1 第 6 条）。

### 3.6 连接的生命周期

| 阶段 | 行为 |
|-|-|
| 启动 | 每个服务器用 `Auto` 模式建立连接：先发 `server/discover`，旧服务器回退到 `initialize`（协议版本 2025-11-25）；`list_all_tools` 取全部工具，核对配置。每个服务器 30 秒超时 |
| 启动失败 | 报配置错误，退出码 2；serve 也一样，由 Kubernetes 重启（缺口 ②） |
| 运行中 | 旧协议的会话过期（404）由 rmcp 重新初始化（`streamable_http_client.rs:2123-2134`）；`list_changed` 只记录，重启进程后生效 |
| 退出 | `close_with_timeout`：旧协议的会话发 DELETE，上限 5 秒 |
| 启动行 | stderr：服务器名、主机、协商的协议版本、交给模型的工具数 |

### 3.7 关键技术缺口

#### ① 依赖与 TLS：rmcp 与项目的 reqwest 版本不同【已解决：升级 reqwest，D9】

**问题**：rmcp 的 Streamable HTTP 传输只支持 reqwest 0.13（要求 `^0.13.2`，`rmcp-3.8.0/Cargo.toml:843-850`），项目原来锁定 0.12.28。Cargo 对 0.x 版本把第二位数字的变化视为不兼容，两个版本会同时编译进程序：

- rmcp 只接受 0.13 的 `reqwest::Client`，项目的 0.12 客户端不能交给它。
- 0.13 的 `rustls` 特性使用 aws-lc-rs 后端（`reqwest-0.13.5/Cargo.toml:169-173`），0.12 使用 ring，rustls 因此同时启用两个后端。

**证据**（两个后端同时启用的后果）：

- rustls 同时启用 `ring` 与 `aws_lc_rs` 时，`from_crate_features()` 返回 `None`（`rustls-0.23.46/src/crypto/mod.rs:265-286`）；没有安装进程级的后端时，`ClientConfig::builder()` 在 `crypto/mod.rs:249` panic。
- tokio-tungstenite 对 `wss://` 调用 `ClientConfig::builder()`（`tokio-tungstenite-0.30.0/src/tls.rs:126`），Slack 与 ⚑ D:5/CHINA FLAG: 飞书 的长连接都经过这里（`src/im/slack.rs:238`、`src/im/feishu.rs:419`）。
- 实测：同一段 `wss://` 连接代码，不加 rmcp 时正常报错 `Connection reset by peer`，加入后 panic（附录 9.4）。现有测试的假服务都用 `ws://`，发现不了这个问题。

> ⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书通道的出站长连接（远程访问）受此依赖变更影响] 本稿只评估技术风险，该平台的使用仍 Subject to Legal/Export Compliance approval。

| 方案 | reqwest | rustls 后端 | 要做的事 | 代价 |
|-|-|-|-|-|
| **a. 升级到 0.13，用它的 `rustls` 特性（采用）** | 0.13 一份 | aws-lc-rs | 无 | 构建时编译 aws-lc 的 C 代码；HTTPS 改用系统信任库 |
| b. 升级到 0.13，用 `rustls-no-provider` | 0.13 一份 | ring | 启动时安装 ring；`rustls` 加为直接依赖 | 嵌入本库的程序也必须先安装，否则创建 HTTP 客户端即 panic（`reqwest-0.13.5/src/async_impl/client.rs:721-723,2499-2507`） |
| c. 保留 0.12，rmcp 用 `reqwest-tls-no-provider` | 0.12 与 0.13 | ring | 启动时安装 ring；`rustls` 加为直接依赖 | 两份 reqwest；不安装时创建 MCP 客户端即 panic |
| d. 保留 0.12，rmcp 用 `reqwest` 特性 | 0.12 与 0.13 | ring 与 aws-lc-rs | 启动时安装其一；`rustls` 加为直接依赖 | 两份 reqwest，仍要编译 aws-lc；不安装时 `wss://` panic |
| e. 其他 | — | — | — | native-tls（第二套 TLS 栈）、不带 TLS（只能连 `http://`）、自己实现传输或整个协议（与“不做过多实现”相悖，2.3.1） |

**结论：采用 a，已在本稿所在的 PR 中实施（D9）。** 理由：

- 只有一份 HTTP 客户端。rustls 只有一个后端，进程默认的后端由 crate 特性确定，不需要任何初始化代码，嵌入本库的程序不受影响。
- 之后引入 rmcp（打开它的 `reqwest` 特性）时，rustls 的后端不变，只再增加 15 个包（附录 9.2）。
- 项目代码不用改：`Cargo.toml` 换版本与特性后，全部目标直接编译通过。

代价与行为变化：

- 构建需要 C 编译器（aws-lc-sys）。用项目的 Dockerfile 实测（`rust:1.88-slim-bookworm`，linux/arm64），不加任何包即可构建。
- HTTPS 的根证书改用系统信任库（rustls-platform-verifier，`reqwest-0.13.5/src/async_impl/client.rs:758-760`），不再内置。Linux 上一张系统证书都读不到时，创建 HTTP 客户端即失败（`rustls-platform-verifier-0.7.1/src/verification/others.rs:92-108`），连 `http://` 的本地模型也一样。镜像已安装 ca-certificates（`deploy/kubernetes/Dockerfile:21`）。两者都在镜像中实测过（附录 9.4）。
- 代理不变：仍读取 `HTTPS_PROXY`、`HTTP_PROXY`、`NO_PROXY`（`hyper-util-0.1.21/src/client/proxy/matcher.rs:238-249`）。
- 防回归：`tests/im_slack.rs` 的 `a_failed_tls_handshake_is_reported_and_retried` 让 Slack 通道连接一个握手失败的 `wss://` 地址，要求报告错误而不是 panic。给测试构建加入启用 `ring` 的依赖后，它在 `crypto/mod.rs:249` panic 而失败（已还原）。

#### ② 启动时 MCP 服务器不可用

**问题**：一个 MCP 服务器暂时不可用，会让 serve 启动失败，整个 IM 机器人不可用。

| 方案 | 说明 |
|-|-|
| a. 启动失败（建议） | 与其他配置错误一致；Kubernetes 按退避重启；问题立即可见 |
| b. 跳过该服务器继续启动 | serve 可用，但工具集与配置不一致，模型不知道少了工具；还需要后台重连与运行中更新工具，本期不做 |

**建议 a。** run 与 chat 同样失败退出。

#### ③ 凭据只支持静态 token

**问题**：规范中授权是可选的；采用时按 OAuth 2.1，token 放在 `Authorization` 头里。规范也允许双方另行约定认证方式（附录 9.1 第 8 条）。

**建议**：一期只支持从环境变量读取的静态 token。需要 OAuth 的服务器暂不支持，以后按需加入 `client_credentials`。

#### ④ 响应体没有大小上限

**问题**：rmcp 用 `response.bytes()` 读取 JSON 响应，没有上限（`transport/common/reqwest/streamable_http_client.rs:360`）。SSE 每个事件上限 16 MiB（`transport/common/client_side_sse.rs:18`）。3.5 的截断发生在结果交给模型之前，不限制内存。

**建议**：一期接受。服务器由运维显式配置；在 README 的“安全模型”中写明。

#### 其余缺口

| # | 项 | 处理 |
|-|-|-|
| ⑤ | 经 `http://` 发送 token | 允许（集群内服务、服务网格常见），启动行标出；评审可改为必须显式确认 |
| ⑥ | 工具描述与 schema 来自外部 | 只交出配置中列出的工具；描述限长；服务器的 `instructions` 不进提示词 |
| ⑦ | 工具列表的更新 | `list_changed` 只记录。rmcp 的工具缓存在出错时可能返回过期结果（`serve_stale_on_error` 默认开启，`service/client/cache.rs`）；这里只在启动时取一次，不受影响 |
| ⑧ | 旧服务器启动较慢 | `Auto` 模式对不回应 `server/discover` 的服务器最多等 10 秒才回退（`service/client.rs:719`） |

---

## 4. 接口与数据变更一览

| 变更 | 内容 |
|-|-|
| 公开 API | `extensions::Extension`、`extensions::register`；新模块 `mcp`：服务器配置类型，以及返回 `Extension` 实现的 `connect` |
| profile | 新增 `[[mcp]]`（3.2） |
| CLI | 无新参数 |
| 会话文件 | 格式不变；MCP 工具定义经 `request_config` 记录 |
| 事件 | 不变 |
| 退出码 | MCP 配置错误与启动时连接失败为 2 |
| 依赖 | 本稿所在的 PR：reqwest 升级到 0.13（D9，缺口 ①）。模块 B：引入 `rmcp` 3.8.0（D2 已授权） |

---

## 5. 测试验收

| 项 | 判据 | 怎么验 |
|-|-|-|
| 列出工具 | 只交出配置中的工具，名字为 `<name>__<工具>`；缺少的工具、替换后重名或超长时启动失败 | 本地假 MCP 服务：基于 `tests/common/http.rs` 手写，独立于 rmcp，分别按新旧两种协议应答 |
| 调用与结果 | 参数原样到达；text、`structuredContent`、非文本内容与 `isError` 的映射 | 同上 |
| 策略 | 默认审批；`allowTools` 直接执行；`denyTools` 优先；run 中未放行的被拒 | FauxProvider + 假服务 + 记录审批的 Interaction |
| 凭据 | 请求带 `Authorization`；bash 中看不到 `tokenEnv`；输出中的 token 被脱敏；错误信息不含 token | 同上，命令执行 `env` |
| 截断、超时与取消 | 超长结果被截断；挂起的服务超时后返回错误结果；取消后服务收到取消（旧协议的通知或新协议的断开） | 假服务挂起 |
| 崩溃恢复 | 中断的 MCP 调用恢复为“结果未知”，不重放 | 参照 `tests/crash_recovery.rs` |
| `wss://` 不受影响 | 引入 rmcp 后，`wss://` 连接返回连接错误而不是 panic | 新增回归测试：假 Slack 返回指向本地 TCP 服务的 `wss://` 地址，serve 记录 `failed` 而不退出；先确认它在未安装后端时失败 |
| Extension | 钩子声明非 `Extension` 阶段、工具重名、段落重名时报错 | 单元测试与集成测试 |
| 真实服务器 | 对一个开源的 Streamable HTTP MCP 服务器跑通列出与调用 | 本机启动；服务器未选定 |

---

## 6. 排期

| 模块 | 负责人 | 范围 | 依赖 |
|-|-|-|-|
| A. Extension 接口 | 待指定 | trait、`register`、`cli::prepare` 接入、阶段与重名检查 | 无 |
| B. MCP 客户端 | 待指定 | 引入 rmcp、配置、连接、`McpTool`、策略与凭据、假服务与测试 | A |
| C. 文档 | 待指定 | README、`docs/PROFILES.md`、部署说明、ROADMAP | B |

---

## 7. 后续规划

- OAuth（`client_credentials`）、运行中更新工具列表、stdio。
- “记住历史排障”：以 `Extension` 实现，`transform_context` 加外部存储（技术方案已规划，`docs/ROADMAP.md:754`）。
- 钩子进程（L2）。

---

## 8. 评审意见

| 日期 | 评审人 | 意见 | 回复 / 处理 |
|-|-|-|-|
| | | | |

---

## 9. 附录

### 9.1 MCP 规范原文

S 指 2026-07-28 版（`docs/specification/2026-07-28/`，提交 `5f5440bb26a62e2cf3440b92da5a667efa03b267`）；L 指 2025-11-25 版（`docs/specification/2025-11-25/`，提交 `38c84e9f93ad191d9eb26d92b945d17bd0efcaf3`）。仓库均为 github.com/modelcontextprotocol/modelcontextprotocol。已发布的版本：2024-11-05、2025-03-26、2025-06-18、2025-11-25、2026-07-28。

1. 版本：“There is no negotiation handshake. Every request carries its protocol version, and the server accepts or rejects each request independently”（S `basic/versioning.mdx:12-13`）。旧版：“the client **MUST** send a protocol version it supports”（L `basic/lifecycle.mdx:169-177`）。
2. Streamable HTTP：
   - “The client **MUST** include an `Accept` header listing both `application/json` and `text/event-stream`”（S `basic/transports/streamable-http.mdx:76-77`）。
   - “Every POST request to the MCP endpoint **MUST** include an `MCP-Protocol-Version` header.”（同文件 252-261）
   - “Remove protocol-level sessions and the `Mcp-Session-Id` header from the Streamable HTTP transport.”（S `changelog.mdx:12`）
   - 回退：“If the body is empty or is not a recognized modern JSON-RPC error, fall back to `initialize`”（S `streamable-http.mdx:654-665`）。
   - 旧版：“When a client receives HTTP 404 in response to a request containing an `MCP-Session-Id`, it **MUST** start a new session by sending a new `InitializeRequest`”（L `basic/transports.mdx:215-217`）。
3. 生命周期：“Make MCP stateless: remove the `initialize`/`notifications/initialized` handshake. Every request now carries its protocol version and client capabilities in `_meta`”（S `changelog.mdx:14`）。
4. Tools：
   - “**MUST NOT** vary per-connection or as a side effect of other requests on the connection”（S `server/tools.mdx:62-69`）。
   - “a tool that returns structured content SHOULD also return the serialized JSON in a TextContent block”（同文件 498-500）。
   - “Clients **SHOULD** validate structured results against this schema.”（同文件 514-515）
   - “Clients **SHOULD** provide tool execution errors to language models to enable self-correction.”（同文件 784-785）
5. 命名与 annotations：
   - “Tool names **SHOULD** be between 1 and 128 characters”；允许的字符为 “(A-Z, a-z), digits (0-9), underscore (\_), hyphen (-), and dot (.)”（S `server/tools.mdx:311-316`）。
   - “**SHOULD** implement a disambiguation strategy such as prefixing tool names with a server identifier.”（同文件 324-330）
   - “clients **MUST** consider tool annotations to be untrusted unless they come from trusted servers.”（同文件 305-306）
6. 安全：“there **SHOULD** always be a human in the loop with the ability to deny tool invocations”（S `server/tools.mdx:33-42`）；客户端 SHOULD “Show tool inputs to the user before calling the server”“Implement timeouts for tool calls”“Log tool usage for audit purposes”（同文件 795-803）。
7. 取消与超时：
   - “**Streamable HTTP**: Closing the SSE response stream is the cancellation signal. … No `notifications/cancelled` message is required or expected.”（S `basic/patterns/cancellation.mdx:39-41`）
   - 旧版：“To cancel, the client **SHOULD** explicitly send an MCP `CancelledNotification`.”（L `basic/transports.mdx:130-131`）
   - “Implementations **SHOULD** establish timeouts for all sent requests”（S `cancellation.mdx:47-50`）。
8. 授权：
   - “Authorization is **OPTIONAL** for MCP implementations.”（S `basic/authorization/index.mdx:18-22`）
   - “MCP client **MUST** use the Authorization request header … Note that authorization **MUST** be included in every HTTP request from client to server.”“Access tokens **MUST NOT** be included in the URI query string”（同文件 262-271）。
   - “clients and servers **MAY** negotiate their own custom authentication and authorization strategies.”（S `basic/index.mdx:228-229`）

### 9.2 rmcp 3.8.0 核对记录

源码位于 `~/.cargo/registry/src/index.crates.io-*/rmcp-3.8.0/`，下文路径相对于它。

| 项 | 结论 | 出处 |
|-|-|-|
| MSRV | `rust-version = "1.88"`，与项目一致 | crates.io 元数据 |
| 特性（缺口 ① 的方案 a） | `default-features = false`，`["client", "transport-streamable-http-client-reqwest", "reqwest"]`；`reqwest` 即 reqwest 0.13 的 `rustls` | `Cargo.toml:65-183` |
| HTTP 客户端 | reqwest `^0.13.2`（解析为 0.13.5），只为 0.13 的 `Client` 实现传输 | `Cargo.toml:843-850`；`src/transport/common/reqwest/streamable_http_client.rs:65` |
| 启动方式 | `Initialize`、`Discover`、`Auto`；`serve()` 默认 `Initialize` | `src/service/client.rs:704-717,755-771` |
| 协议版本 | 已知 2024-11-05 至 2026-07-28；最后一个带握手的是 2025-11-25 | `src/model.rs:150-200` |
| 凭据 | `auth_header(token)` 在每个请求上加 `Authorization: Bearer` | `src/transport/streamable_http_client.rs:2163-2172` |
| 超时 | 没有连接与请求超时，调用方自行限时 | `streamable_http_client.rs:2221-2237`；reqwest 默认值 |
| 取消 | 丢弃调用时尽力发送 `notifications/cancelled`，并中止本地请求 | `src/service/client.rs:480-511`；`src/transport/worker.rs:268-284` |
| 关闭 | `close()` 排空消息，旧协议的会话发 DELETE（上限 5 秒） | `src/service.rs:1080-1158`；`streamable_http_client.rs:1858-1907` |
| 新增的包（在升级后的项目上） | 15 个：rmcp、sse-stream、tokio-stream、chrono、num-traits、autocfg、tracing-attributes；其余是只在特定平台编译的 iana-time-zone、iana-time-zone-haiku、android_system_properties 与 5 个 windows-* | 临时工程的 Cargo.lock 与项目的对比 |

不引入 schemars、axum、oauth2 与子进程相关的包。

### 9.3 OpenAI 函数名

> The name of the function to be called. Must be a-z, A-Z, 0-9, or contain underscores and dashes, with a maximum length of 64.

出处：openai/openai-openapi，提交 `c2aecc2fbe1ddff3e7500a0eaa5c2bc4c19c5318`，`openapi.yaml:53439-53440`（`FunctionObject.name`）。

### 9.4 TLS 实测

**临时工程**（本机 macOS，2026-11-01）：复制项目的依赖与 Cargo.lock，按下表改动 reqwest 与 rmcp，运行同一段程序。本地 TCP 服务接受连接、2 秒后关闭，程序以 `tokio_tungstenite::connect_async("wss://127.0.0.1:<端口>/")` 连接它；加了 rmcp 的工程先创建 rmcp 的传输。方案字母同缺口 ①。

| 工程 | rustls 的特性 | 结果 |
|-|-|-|
| 项目原样（reqwest 0.12） | `ring,std,tls12` | `IO error: Connection reset by peer (os error 54)` |
| 方案 d，不安装后端 | `aws-lc-rs,aws_lc_rs,ring,std,tls12` | panic：`rustls-0.23.46/src/crypto/mod.rs:249:14`，“Could not automatically determine the process-level CryptoProvider from Rustls crate features.” |
| 方案 c，启动时安装 ring | `ring,std,tls12` | rmcp 的传输创建成功；`wss://` 返回 `Connection reset by peer` |
| 方案 c，不安装（对照） | `ring,std,tls12` | panic：`reqwest-0.13.5/src/async_impl/client.rs:2501:5`，“No rustls crypto provider is configured.” |
| **方案 a**，不安装任何后端 | `aws-lc-rs,aws_lc_rs,std,tls12` | rmcp 的传输与 reqwest 客户端都创建成功；`wss://` 返回 `Connection reset by peer` |

方案 a 与 c 的工程另用 `cargo +1.88.0 check` 检查通过。

**仓库**：升级后用 `tests/im_slack.rs` 的 `a_failed_tls_handshake_is_reported_and_retried` 验证（缺口 ①）。临时加入启用 `ring` 的 dev 依赖时，测试构建中 rustls 的特性为 `aws-lc-rs,aws_lc_rs,ring,std,tls12`，该测试在 `crypto/mod.rs:249` panic 而失败；还原后通过。

**镜像**（Docker 29.9.2，linux/arm64）：用 `deploy/kubernetes/Dockerfile` 构建，`rust:1.88-slim-bookworm` 中编译了 aws-lc-sys，release 构建 4 分 14 秒完成。在镜像中运行 `zx-agent run`：

| 情形 | 结果 |
|-|-|
| `--base-url https://example.com/v1` | 握手成功，服务器返回 `HTTP 405` |
| `--base-url http://127.0.0.1:9/v1` | 正常启动，请求失败在 TCP 连接：`Connection refused` |
| 删除 `/etc/ssl/certs` 与 `/usr/share/ca-certificates` 的内容后，同上一行 | 启动即失败：`zx-agent: builder error: unexpected error: No CA certificates were loaded from the system`，退出码 2 |

---

`p7-mcp-review.md` · v0.1 · 2026年11月
