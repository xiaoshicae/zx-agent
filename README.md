# zx-agent

一个用 Rust 编写的通用 agent 内核：同一个内核支撑人机对话、无人值守运行和被其他程序嵌入。场景之间的差异通过 agent（提示词、工具、权限、上限）和插件表达，不改内核。分层方式、循环语义和会话文件格式沿用 [pi](https://github.com/earendil-works/pi)（commit `a92283056`），不同之处见[与 pi 的差异](docs/REFERENCE.md#与-pi-的差异)。

当前版本为 v0.1.0，已实现：

- 子命令：无人值守的 `run`，终端对话 `chat`（应急与脚本化入口），ACP agent `acp`（交互式界面由 ACP 客户端提供），IM 服务 `im`，列出会话的 `sessions`。
- OpenAI 兼容的流式 provider，带错误分类、自动重试与响应大小上限。
- 工具：`bash`、`read_file`、`edit_file`、`write_file`，只读的 `grep`、`find`、`ls`，按需读取 skill 的 `read_skill`，分页读取落盘结果的 `read_output`，以及 MCP 服务器的工具。
- 主目录中的 `config.toml`（环境）与 agent 文件（角色），项目 `AGENTS.md`；命令策略与 ask / allow / deny 权限；已知密钥脱敏；可选的容器执行。
- 只追加的会话记录（默认 JSONL 文件，可续跑），可选的自动与手动摘要，长工具结果落盘。
- 轮次、时间、token 与花费上限；JSONL 事件流与可选的 OpenTelemetry 追踪导出；扩展接口 `Extension`。

尚未实现：OpenAI 兼容以外的 provider（如 Anthropic），见[路线图](docs/ROADMAP.md)。

## 快速开始

需要 Rust 1.88 及以上与 C 编译器（aws-lc 在构建时编译）。HTTPS 使用系统的根证书：Linux 上没有安装 CA 证书（如 Debian 的 `ca-certificates`）时启动即报错 `No CA certificates were loaded from the system`，只连 `http://` 时也一样。例外是 IM 平台（Slack 与 ⚑ D:5/CHINA FLAG: 飞书）的 WebSocket 长连接：它只信任编译进程序的根证书（`tokio-tungstenite` 的 `rustls-tls-webpki-roots`），不读系统信任库；在使用私有 CA 的出口代理后面，REST 调用能通而长连接的握手可能失败。

```bash
cargo build --release
export OPENAI_API_KEY=...
./target/release/zx-agent run --model <模型 ID> "检查本机磁盘与内存使用情况，找出资源占用最高的进程"
```

模型与服务地址可以写进 `~/.zx-agent/config.toml`，见[配置说明](docs/CONFIGURATION.md)：

```toml
model = "<模型 ID>"

[provider]
baseUrl = "https://api.openai.com/v1"
apiKeyEnv = "OPENAI_API_KEY"
```

本地或私有部署的 OpenAI 兼容服务（vLLM、Ollama、llama.cpp server 等）用 `ZX_BASE_URL` 指定地址；找不到 API key 时请求不带认证头：

```bash
ZX_BASE_URL=http://127.0.0.1:11434/v1 ./target/release/zx-agent run --model <模型名> "排查 nginx 返回 502 的原因"
```

### 终端对话

```bash
zx-agent chat                          # 新对话，存入会话目录
zx-agent chat --resume                 # 续接本工作目录最近一次的对话
zx-agent chat --resume <ID 前缀>       # 续接 ID 以它开头的唯一会话
zx-agent sessions                      # 列出本工作目录的会话（可加 --cwd）
zx-agent chat --session ./chat.jsonl   # 指定会话文件
```

不给 `--session` 时，每次对话存为 `~/.zx-agent/sessions/--<工作目录>--/<UTC 时间>_<会话 ID>.jsonl`（目录名为去掉开头 `/`、其余 `/` 换成 `-` 的工作目录），新建的目录权限 0700、文件 0600；`--no-session` 只保存在内存。`--resume` 找不到会话时新建并提示，`--resume <ID 前缀>` 找不到或不唯一时以退出码 2 结束。`sessions` 每行一个会话：ID 前 8 个字符、最近活动时间、消息数、会话名称（没有名称时为第一条提示词的首行）。名称由 ACP 客户端的 `/name` 或 SDK 的 `Session::set_name` 设置，chat 没有起名命令。

输入与插话、`/` 命令、审批与退出码见[参考手册](docs/REFERENCE.md#chat)。

### ACP 客户端

```bash
zx-agent acp
```

`acp` 经 stdin 与 stdout 收发 [Agent Client Protocol](https://agentclientprotocol.com/) v1 的消息，由 ACP 客户端（Zed、JetBrains、Neovim、Emacs 的插件，或 Wisp 等终端客户端，见[客户端目录](https://agentclientprotocol.com/overview/clients)）启动：在客户端里把 agent 命令配置为上面这一行。参数与 `chat` 相同（`acp --help`），没有 `--session`、`--output` 与 `--color`；会话的工作目录由客户端给出，`--cwd` 在这里只是 `session/list` 没有带 `cwd` 时所列的目录。stdout 只走协议，诊断在 stderr。

会话、命令、回合、更新与退出的细节见[参考手册](docs/REFERENCE.md#acp)。

### 配置与 agent

配置都在主目录（`~/.zx-agent`，或 `ZX_AGENT_HOME`）中，都可以没有。`config.toml` 决定环境：模型、provider、执行环境、MCP 服务器、模型的单价与上下文窗口。agent 决定角色：说明、工具、权限、上限、项目上下文、skills 与摘要。`run` 使用内置的 `unattended`，`chat`、`im` 与 `acp` 使用内置的 `assistant`；主目录下的 `agents/unattended.md`、`agents/assistant.md` 替换它们。`chat` 默认加载 Git 根目录到工作目录的 `AGENTS.md`，`run` 默认不加载。每个键见 [CONFIGURATION.md](docs/CONFIGURATION.md)。

### 无人值守示例

可用于 cron、systemd timer、Kubernetes Job：

```bash
export ZX_AGENT_HOME=/etc/zx-agent-inspect
mkdir -p "$ZX_AGENT_HOME/agents"
cat > "$ZX_AGENT_HOME/agents/unattended.md" <<'EOF'
+++
extends = "unattended"

[permissions]
ask = ["bash"]
allow = ["bash(kubectl get)", "bash(kubectl describe)", "bash(kubectl logs)", "bash(grep)", "bash(tail)", "bash(head)"]
+++
EOF
zx-agent run \
  --model "$ZX_MODEL" --tools bash \
  --max-model-requests 20 --timeout-secs 600 \
  --session "/var/log/zx-agent/$(date +%Y%m%d-%H%M%S).jsonl" \
  --output jsonl \
  "命名空间 shop 中的 Pod 频繁重启，排查原因" > events.jsonl
echo "exit code: $?"
```

`allow` 之外的 bash 命令要审批，`run` 没有人审批，因此拒绝。在 Kubernetes 中常驻部署见 [deploy/kubernetes/README.md](deploy/kubernetes/README.md)。

### IM 服务

`zx-agent im` 作为常驻进程接入 IM：私聊整段是一个会话，频道里一个线程一个会话；写操作在 IM 里审批。支持 Slack（Socket Mode）与 ⚑ D:5/CHINA FLAG: 飞书（长连接），都只需出网。⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：对话内容、命令输出与用户 ID 发往该平台] 该平台的使用 Subject to Legal/Export Compliance approval。部署到 Kubernetes 见 `deploy/kubernetes/README.md` 的“IM 服务”。

```bash
export SLACK_APP_TOKEN=xapp-...   # app-level token，用于 Socket Mode
export SLACK_BOT_TOKEN=xoxb-...   # bot token，用于发消息
# 使用 ⚑ D:5/CHINA FLAG: 飞书时改为 FEISHU_APP_ID 与 FEISHU_APP_SECRET（企业自建应用的凭据）
zx-agent im --config im.toml --model <model> --max-model-requests 100 --timeout-secs 3600
```

内置的 assistant 不限模型请求数与时间，所以示例在命令行给每次运行加了上限：不加时，一次不收敛的运行会一直占着运行名额（`maxRunning`）。等待审批的时间也算在时间上限里。

`im.toml`（未知的键视为错误）：

```toml
[slack]
# appTokenEnv = "SLACK_APP_TOKEN"   # 存放 token 的环境变量名，此为默认值
# botTokenEnv = "SLACK_BOT_TOKEN"

# ⚑ D:5/CHINA FLAG: 飞书；两个平台可以同时配置。名单中写作 feishu:oc_…（群或私聊）与 feishu:ou_…（用户）
# [feishu]
# appIdEnv = "FEISHU_APP_ID"
# appSecretEnv = "FEISHU_APP_SECRET"

[access]
conversations = ["slack:C0123456789"]   # 只在这些频道或私聊中回答，其他地方只回一句“未授权”
approvers = ["slack:U0123456789"]       # 只有这些用户的点击能批准写操作

[limits]
maxRunning = 4              # 同时运行的会话数，默认 4，其余排队
idleSecs = 1800             # 会话空闲多久后释放其文件锁，默认 1800

[sessions]
dir = "/data/sessions"      # 会话文件在 <dir>/--<工作目录>--/ 下，头部的标签记录它属于哪个对话

# 可选：替换默认的英文文案（字段名见 Replies 与 CardLabels）
[replies]
working = "处理中…"
[cardLabels]
allowed = "已批准"
```

平台应用的设置、会话的划分、审批、日志、退出与崩溃之后的行为见[参考手册](docs/REFERENCE.md#im-服务)。其余参数与 `chat` 相同，`--max-model-requests` 等作用于每一次运行；没有 `--session`。

### MCP 工具

在 `config.toml` 中用 `[[mcp]]` 列出外部 MCP 服务器及其工具（只支持 Streamable HTTP，[配置项](docs/CONFIGURATION.md#mcp-服务器)），模型以 `<服务器名>__<工具名>` 调用。

- 只有列出的工具交给模型；agent 的 `permissions.allow` 列出的直接执行，其余每次都要审批（run 中因此拒绝）。服务器给出的 annotations 不影响放行。
- 每个进程启动时连接一次；连不上、token 变量没有值、服务器缺少列出的工具时以退出码 2 报错。成功时 stderr 每个服务器一行 `[mcp] <名字>: <工具>; protocol <版本>; <地址>`，经 `http://` 发送 token 时另加 `the token is sent unencrypted`。
- 先用 `server/discover` 握手，旧服务器回退到 `initialize`。结果中的文本原样交给模型，图片、音频与二进制资源只写一行说明，最多 2000 行或 50 KiB；进度通知作为工具进度显示。

### DeepSeek

⚑ D:5/CHINA FLAG: DeepSeek（官方文档页面没有写明公司所在地；据公开信息为中国公司，未核实）。

⚠ COMPLIANCE REVIEW: [对话内容、命令输出与工具结果会发往该服务的 API，属于远程访问与数据外发] 使用前须经法务 / 出口合规审批（Subject to Legal/Export Compliance approval）。

该服务的 API 兼容 OpenAI 格式（[官方文档](https://api-docs.deepseek.com/)），用现有的 provider 即可：

```toml
model = "deepseek-flash"

[provider]
baseUrl = "https://api.deepseek.com"
apiKeyEnv = "DEEPSEEK_API_KEY"
# 关闭思考模式：
# requestFields = { thinking = { type = "disabled" } }
```

思考模式默认开启，`requestFields` 中的 `reasoning_effort` 调整强度。回答中的 `reasoning_content` 记入会话并回传给模型（官方文档：带工具的请求必须回传，否则返回 400）。历史中混有不带推理内容的回答（如中途换过模型）时打开 `provider.fillMissingReasoning`：pi 记录过该服务对这种历史返回 “The `reasoning_content` in the thinking mode must be passed back to the API.”（pi #3668），本项目没有实测这种情况。

### 参数与退出码

`run` 用退出码表达结果：0 完成，1 失败，2 参数或配置错误，3 到 9 是各类上限、中止与被拒，见[参考手册](docs/REFERENCE.md#run-退出码)。命令行参数见[参考手册](docs/REFERENCE.md#命令行参数)，其余设置在 `config.toml` 与 agent 中，见[配置说明](docs/CONFIGURATION.md)。

## 架构

一个 Cargo workspace，9 个 crate，依赖只能自上而下，由编译器保证：

```
zx-cli      crates/zx-cli    命令行程序 zx-agent（run、chat、acp、im、sessions）、config.toml 与 agent、
                             OpenTelemetry 导出
  ├─ zx-im-slack   crates/zx-im-slack   Slack 平台
  ├─ zx-im-feishu  crates/zx-im-feishu  ⚑ D:5/CHINA FLAG: 飞书平台
  │    └─ zx-im    crates/zx-im         IM 平台接口与按会话分派的服务（依赖 zx-core）
  └─ zx-agent  src/lib.rs    SDK：把下面四个 crate 再导出成一个库，没有自己的代码
       ├─ zx-tools  crates/zx-tools  内置工具、执行环境、长结果落盘、命令与规则策略、续写与脱敏、MCP 客户端
       ├─ zx-core   crates/zx-core   循环、会话、运行层、共享会话、摘要、提示词、扩展接口
       │    └─ zx-ai  crates/zx-ai   消息类型、流式事件协议、provider 契约
       └─ zx-ai-openai  crates/zx-ai-openai  OpenAI 兼容 provider（只依赖 zx-ai）
```

命令行与 IM 是建在库之上的两种接入方式，不在库里：引入 `zx-agent` 不编译命令行解析、ACP、追踪导出与 IM 平台。只嵌入内核的程序不编译 HTTP 客户端。代码中没有场景、环境或平台文案语言的规则：场景是 agent 文件，Kubernetes 的规则在 `deploy/kubernetes/`。

一次运行：`Session::send` 取得会话并返回一个 `Run`，调用方读它，运行才推进；每一步先写入会话再发出事件；发给模型的上下文就是会话的投影；工具调用先逐个检查，再并发执行，按调用顺序写结果；失败的请求按策略重试，最后汇总为 `RunReport`。

循环语义、与 pi 的差异和系统提示词的组成见[参考手册](docs/REFERENCE.md#语义)。

## 安全模型

策略只是第一道防线，不是安全边界。生产环境以最小权限账号运行，放在容器或虚拟机中，必要时限制网络。命令策略与自动放行的条件、工具与密钥的边界、容器执行见 [docs/SECURITY.md](docs/SECURITY.md)。

## 作为库使用

`zx-agent` 库再导出各 crate：内核 `zx-core` 按域模块原样导出（`zx_agent::agent`、`zx_agent::tools`、`zx_agent::policy`、`zx_agent::hooks`、`zx_agent::transcript` 等），模型层在 `zx_agent::ai`，`zx-tools` 附带的实现在 `zx_agent::builtin`。库里没有接入方式：IM 服务在 `zx-im`（平台在 `zx-im-slack` 与 `zx-im-feishu`），`config.toml`、agent 文件与 OpenTelemetry 导出在命令行的 crate `zx-cli`（`zx_cli::config`、`zx_cli::otel`）。只需要内核时可以只依赖 `zx-core` 与 `zx-ai`，按需加上 `zx-ai-openai`。`zx-core` 按 JSON Schema 校验工具参数的部分是默认开启的 feature `json-schema`；写 `default-features = false` 时参数只由工具自己解析，内核的依赖从 103 个 crate 减到 40 个（含它自己）。各 crate 都不发布到 crates.io。

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

入口、会话、审批、事件、扩展点、存储与摘要见 [docs/LIBRARY.md](docs/LIBRARY.md)，接口与契约见 rustdoc：`cargo doc --open`。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | `config.toml` 与 agent 文件的键 |
| [docs/REFERENCE.md](docs/REFERENCE.md) | 命令行参数、退出码，chat、ACP 与 IM 的行为，循环语义与 pi 的差异，事件、追踪与会话文件的格式 |
| [docs/SECURITY.md](docs/SECURITY.md) | 安全模型与容器执行 |
| [docs/LIBRARY.md](docs/LIBRARY.md) | 作为库使用 |
| [docs/TESTS.md](docs/TESTS.md) | 每个测试文件覆盖什么 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 设计目标、已定决策与分阶段计划 |
| [deploy/kubernetes/README.md](deploy/kubernetes/README.md) | 在 Kubernetes 中部署 |

## 开发

```bash
cargo test                                   # 不需要网络和 API key；付费的真实 API 测试默认跳过
cargo clippy --all-targets -- -D warnings
cargo clippy -p zx-core --no-default-features --all-targets -- -D warnings   # 不带 json-schema 的内核
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

在仓库根目录运行时覆盖全部 9 个 crate；加 `-p <crate>` 只处理一个。集成测试分两处：只用库的在根目录的 `tests/`，驱动程序的在 `crates/zx-cli/tests/`，每个文件覆盖什么见 [docs/TESTS.md](docs/TESTS.md)。

`tests/real_api.rs` 会产生费用：每个测试都标为 `#[ignore]`，没有设置 `ZX_E2E_AUTHORIZED=1` 时在发出请求前失败，运行方式见该文件开头的注释。

## CI 与发布

`Rust` 工作流在 `main` 的 push、PR 和手动运行时检查格式、clippy、Rustdoc、actionlint 与 shellcheck，并在 Linux 与 macOS 上构建和测试，另用 Rust 1.88 编译和测试。

`Release` 工作流在推送 `v*` tag 时构建 release 草稿；tag 必须与 `Cargo.toml` 中 `[workspace.package]` 的版本一致。发布包含二进制、README、`docs/` 和 MIT 许可证，支持 `x86_64-unknown-linux-gnu` 与 `aarch64-apple-darwin`，附 `SHA256SUMS` 与 [GitHub 来源证明](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)。验证：

```bash
ZX_ARCHIVE=zx-agent-v0.1.0-aarch64-apple-darwin.tar.gz
gh attestation verify "$ZX_ARCHIVE" --repo xiaoshicae/zx-agent --bundle "$ZX_ARCHIVE.sigstore.json"
```

## 许可证

MIT，见 [LICENSE](LICENSE)。设计参考的 [pi](https://github.com/earendil-works/pi) 同样采用 MIT 许可证（Copyright (c) 2025 Mario Zechner）。

---

README.md · v0.1.0 · 2026年11月
