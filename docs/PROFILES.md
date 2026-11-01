# 场景配置、项目上下文与 skills

`run` 默认使用 `troubleshoot`，`chat` 默认使用 `assistant`。两个内置 profile 的 TOML 和说明文件位于 `profiles/`，构建时嵌入二进制，发布后的程序无需额外查找安装目录。

```bash
zx-agent run --model "$ZX_MODEL" --profile troubleshoot "检查磁盘使用情况"
zx-agent chat --model "$ZX_MODEL" --profile assistant
# 从仓库根目录使用完整的只读代码评审示例
zx-agent run --model "$ZX_MODEL" --profile examples/profiles/review.toml "评审 src/profile/ 的实现"
```

## 自定义 profile

`--profile` 接受内置名称或文件路径。文件相对启动目录解析；其中 `instructions`、`context.files`、`skills.path` 均相对 TOML 文件所在目录解析，与 `--cwd` 无关。示例中的全部文件已提交在 [`examples/profiles/`](../examples/profiles/)。

```toml
name = "code-review"
extends = "assistant"
instructions = "review.md"
tools = ["read_file", "read_skill"]
execution = "local"

[rules]
denyTools = ["bash"]

[limits]
maxTurns = 12
timeoutSecs = 300

[extensions]
redaction = true
continueOnLength = 0

[context]
discover = true
files = ["review-context.md"]

[[skills]]
name = "rust-review"
description = "Review Rust ownership, async cancellation, errors and test coverage."
path = "skills/rust-review/SKILL.md"
```

| 配置 | 语义 |
|---|---|
| `name` | 场景名称，省略时继承基线 |
| `extends` | 可选的 `troubleshoot` 或 `assistant`；省略时使用当前前端的默认 profile；不支持文件链式继承 |
| `instructions` | 替换场景说明的 UTF-8 文件；CLI `--system-prompt-file` 优先 |
| `tools` | 替换启用列表，支持 `bash`、`read_file`、`read_skill`；`[]` 禁用全部工具 |
| `execution` | 当前只接受 `local`；容器执行留到 P6，填写未实现后端时报错 |
| `rules.allow` | bash 命令前缀白名单，沿用原管道检查；空列表表示只检查 deny |
| `rules.deny` | 追加 bash 命令子串拒绝规则，原内置拒绝规则始终保留 |
| `rules.askTools` | 替换需审批的工具名列表；run 通过 NoHuman 明确拒绝，chat 交给终端审批 |
| `rules.denyTools` | 追加被拒绝的工具名；启用该工具或批准调用都不能覆盖拒绝 |
| `limits.maxTurns` | 每轮模型请求数；`"unlimited"` 不限，`0` 不请求模型 |
| `limits.timeoutSecs` | 每轮总时间上限，`0` 关闭 |
| `extensions.continueOnLength` | 输出达到 token 上限后的续写次数；每次消耗一次模型请求预算 |
| `extensions.redaction` | 仅接受 `true` 或省略；CLI 的已知认证 key 脱敏不可关闭 |
| `context.discover` | 是否自动发现项目 AGENTS.md |
| `context.files` | 追加显式上下文文件 |
| `skills` | 显式登记 name、description、path 的目录；正文按需读取 |

未提供的值继承基线。叠加顺序为 **内置基线 < profile 文件 < 显式命令行参数**；即使 CLI 值恰好等于旧默认值，也按显式覆盖处理。`--allow-cmd` 替换文件的 allow；`--deny-cmd` 追加到文件和内置 deny，不能清除拒绝规则。重复的 `--context-file` 追加文件。

连接地址、模型、认证环境变量名、传输与工具超时、输出模式和会话路径仍使用原 CLI 选项。TOML 不接受这些键，也不接受任意扩展名称；未知字段、错误类型、无效路径或不支持的配置均在请求模型前以退出码 2 报错。

`run` 始终是无人值守前端，`chat` 始终保留 bash 的终端审批；`--profile` 不改变前端。例如 `run --profile assistant` 遇到 bash 的 Ask 会拒绝，`chat --profile troubleshoot` 仍会询问 shell 审批。

## 项目上下文

默认从最近 Git 根目录到工具 cwd，依次加载各层 `AGENTS.md`。Git worktree 的 `.git` 文件也被识别为边界；没有 Git 根目录时只检查 cwd。不会隐式读取用户主目录里的其他 AI 配置。

```bash
zx-agent run --model "$ZX_MODEL" --context-file ./extra-context.md "分析当前项目"
zx-agent chat --model "$ZX_MODEL" --no-context
```

`--context-file` 相对启动目录解析，即使工具 cwd 通过 `--cwd` 改到别处。`--no-context` 同时关闭自动发现和 profile 中的显式上下文文件，不能与 `--context-file` 同时使用。相同 canonical 路径只加载一次。

上下文追加到生成的 `project_context` 提示词段落，沿用现有系统提示词持久化和更新机制。只读普通 UTF-8 文件，每个文件最多 64 KiB；上下文总量包含路径标题和分隔符，最多 256 KiB。超限报错，不静默截断指令。

## 按需加载 skills

P4 使用 profile 中显式登记的目录，不自动扫描全局 skills，也不解析 SKILL.md 的 YAML frontmatter。已有 SKILL.md 可以直接作为正文，名称和简介以 TOML 为准；skill 里的脚本不会自动执行。

启动只检查登记项及文件元数据，并向 `available_skills` 加入目录。正文只有模型调用 `read_skill` 时才读取，作为普通工具结果先持久化、再发给模型：

```text
profile 的目录信息 → 系统提示词 → 模型选择 read_skill(name)
                                      ↓
                              参数校验 → 策略 / 审批
                                      ↓
                         按需读取正文 → 脱敏 → 审计 → 模型
```

每个目录最多 64 项，name 使用 1–64 个小写字母、数字或连字符，description 为 1–1024 字节。正文文件最多 64 KiB；单次工具输出沿用 2000 行、50 KiB 的截断限制。输出被分页时返回下一次调用的 UTF-8 字节 `offset`，可继续读取，包括一条超长行的剩余部分。

`read_skill` 仅接受已登记名称和可选 offset，不能通过参数指定任意路径。内置 profile 预留该工具；目录为空时不注册。`--tools` 或 profile 的 tools 列表未启用它时，工具和目录均不加入请求。skill 调用与其他工具使用同一条校验、钩子、审批、取消、脱敏和审计流水线。

项目上下文与 skill 正文是提示词和工具数据，不能修改已组装的工具或规则。认证 key 的脱敏边界仍见 [README 安全模型](../README.md#安全模型)；上下文和说明属于输入，不在工具输出过滤范围内。
