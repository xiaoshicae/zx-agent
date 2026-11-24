# 参考手册

本手册说明 `zx-agent` 各子命令与 `zx-im` 的参数、行为、语义与输出格式。配置键及其默认值见 [CONFIGURATION.md](CONFIGURATION.md)，安全边界见 [SECURITY.md](SECURITY.md)，作为库使用见 [LIBRARY.md](LIBRARY.md)。

## 命令行参数

`run`、`chat`、`acp` 与 `zx-im` 都有 `--model`、`--cwd`、`--tools` 与四个预算参数；`sessions` 只有 `--cwd`，`zx-im` 另有必填的 `--config`。其余参数只适用于标出的子命令。默认值为“agent”的取自所用的 agent（内置 agent 的取值见 [CONFIGURATION.md](CONFIGURATION.md#内置-agent)），显式传入时覆盖。上限与预算作用于每一次运行，chat、acp 与 zx-im 中即每次提问。

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--model` | `ZX_MODEL`，再取 `config.toml` 的 `model` | 模型 ID；都没有时以退出码 2 结束 |
| `--tools` | agent | 逗号分隔，替换工具集合 |
| `--cwd` | 当前目录 | 新会话的工作目录，也是 `sessions` 与 `--resume` 列出会话时所看的目录。续接的会话在它开始时的目录工作：续接时显式给出另一个目录，或原目录已不存在，都以退出码 2 结束。`acp` 中会话的目录由客户端给出，`--cwd` 只是 `session/list` 没有带 `cwd` 时所列的目录 |
| `--max-model-requests` | agent | 模型请求次数上限，失败的尝试也计入；0 时不请求模型 |
| `--timeout-secs` | agent | 时间上限（秒），0 不限 |
| `--token-budget` | agent | 输入加输出 token 上限，0 不限；`run` 用完时以 7 结束 |
| `--cost-budget` | agent | 花费上限，需要模型的单价，0 不限；`run` 用完时以 8 结束 |
| `<提示词>` | 无 | 仅 `run`：任务描述，`-` 表示从 stdin 读取。以 `@` 开头的参数是附在提问上的图片文件（如 `@shot.png`，PNG、JPEG、GIF 或 WebP，相对路径按启动时所在的目录，至多 10 MiB），按模型 API 的上限适配；有图片时可以不给任务描述。读不了、不是图片或适配不了的文件、多于一个任务描述、既没有文本也没有图片，都以退出码 2 结束；`--continue` 不收 `@` 参数 |
| `--fail-on-denied` | 关闭 | 仅 `run`：有工具调用被拒时以 9 代替 0 |
| `--dangerously-skip-approvals` | 关闭 | 仅 `run`、`chat`、`acp`：这次运行不询问任何审批，同 agent 的 `permissions.approveAll`，`ask` 列出的工具也不问；拒绝规则与命令的拒绝列表照旧。与 `approveAll` 不同，不要求隔离命令的执行环境：命令在本机执行时以当前用户的权限执行，模型读到的内容可以借提示注入执行任何命令，只在沙箱中使用。以 root 运行且执行环境不隔离时以退出码 2 结束。启动时在 stderr 打印一行 `[danger]`。这是命令行设置权限的唯一例外（D47） |
| `--session` | `run`：只在内存；`chat`：会话目录中的新文件 | 仅 `run` 与 `chat`：会话文件，不存在则创建，存在则继续 |
| `--no-session` | 关闭 | 仅 `chat` 与 `acp`：不写会话文件 |
| `--resume [ID 前缀]` | 关闭 | 仅 `chat`，不能与 `--session` 同用 |
| `--continue` | 关闭 | 仅 `run`，须给出已存在的 `--session`，不能再给提示词：不加新输入，让模型接着回答（如崩溃之后）。会话以失败或被取消的回答结尾时先省略该回答；以完成的回答结尾且没有排队的消息时以 1 结束、不发请求 |
| `--output` | `text` | 仅 `run` 与 `chat`。`text`：进度与诊断在 stderr，最终回答在 stdout；`run` 没有得到回答时（失败、超时、被中止，或模型还要请求时预算用完）stdout 为空，最后的文字留在会话与 `jsonl` 的 `run_end` 里，截断的回答见退出码 6；`jsonl`：每个事件一行 JSON，输出到 stdout |
| `--deltas` | 关闭 | 仅 `jsonl`：同时输出流式的 `message_update` |
| `--color` | `auto` | 仅 `chat`：`auto` 在交互终端上着色（设了 `NO_COLOR` 或 `TERM=dumb` 时不着色），`always`、`never` |

## run 退出码

| 退出码 | 含义 |
|---|---|
| 0 | 完成：模型给出了最终回答 |
| 1 | 失败，如模型请求出错且重试无效（哪些失败重试见[模型请求失败](#模型请求失败)）、会话文件写入失败 |
| 2 | 参数或配置错误 |
| 3 | 达到 `--max-model-requests` 上限：模型还要请求，次数已用完 |
| 4 | 达到 `--timeout-secs` 时间上限 |
| 5 | 被 SIGINT 或 SIGTERM 中止。中止后的收尾（如导出追踪）期间再收到一次信号时，进程立即被该信号结束，没有退出码 |
| 6 | 截断：最终回答达到输出 token 上限，自动续写（agent 的 `continueOnLength`）的次数用完后仍未完成；续写被截断的回答所需的请求超出预算时，按该预算的退出码（3、7、8）结束。已生成的部分照常输出 |
| 7 | 达到 `--token-budget` 预算：用量已达预算，模型还要继续 |
| 8 | 达到 `--cost-budget` 预算：花费已达预算，模型还要继续 |
| 9 | 只在给出 `--fail-on-denied` 时出现：模型给出了最终回答，但至少一次工具调用被策略或审批拒绝。不加这个参数时这种情况仍是 0，被拒的次数在报告的 `deniedCalls` 与文本模式最后一行的 `denied_calls=` 中 |

## chat

输入与输出：

- 直接输入问题开始下一轮。在终端中，任务运行期间输入的文本作为插话，在下一次模型请求前进入上下文；任务结束时没被读到的插话作为下一轮提问发送，Ctrl+C 取消任务时丢弃。一次粘贴的多行合为一条消息（以 `/` 开头的命令只占一行；非 Unix 平台不合并）。管道输入逐轮、逐行消费。
- 单行（以及合并后的一条消息）最多 1 MiB；超限或无效 UTF-8 的行整行丢弃并提示，对话继续。
- stdout 只有模型的回答，其余在 stderr：启动信息、每个工具调用一行 `[tool] bash: <命令> -> ok 12ms` 及结果的前 3 行（`edit_file` 显示补丁的前 20 行），每轮一行 `[turn] 3 request(s) · 30 in · 15 out · 4.4s`。默认只在交互终端上着色，见 `--color`。`--output jsonl` 每轮输出事件与一个 `run_end`。
- 这是逐行 REPL，没有行编辑：方向键等控制序列会原样作为输入发送。
- 会话：不给 `--session` 时，每次对话存为主目录下的 `sessions/--<工作目录>--/<UTC 时间>_<会话 ID>.jsonl`（目录名为去掉开头 `/`、其余 `/` 换成 `-` 的工作目录），Unix 上新建的目录权限 0700、文件 0600；`--no-session` 只保存在内存。`--resume` 找不到会话时新建并提示，`--resume <ID 前缀>` 找不到或不唯一时以退出码 2 结束。`zx-agent sessions` 每行一个会话：ID 前 8 个字符、最近活动时间、消息数、会话名称（没有名称时为第一条提示词的首行；名称由 ACP 的 `/name` 或 SDK 的 `Session::set_name` 设置）。

| 输入 | 行为 |
|---|---|
| `/help` | 查看命令，含扩展注册的命令 |
| `/model`、`/model <id>` | 查看模型，或空闲时切换同一 provider 的模型，保留历史 |
| `/reasoning`、`/reasoning <级别>` | 查看或空闲时修改推理强度（`off`、`minimal`、`low`、`medium`、`high`、`xhigh`；`default` 交给服务端） |
| `/new` | 开新会话，清空历史和授权，重新加载 agent 与项目说明；运行中先取消任务。旧会话文件保留 |
| `/compact` | 请模型压缩旧上下文，原始记录保留；没有可压缩范围时不发请求 |
| `/continue` | 不加新输入，让模型接着回答，规则同 `run --continue` |
| `/status` | 模型、agent、会话文件、工作目录，以及本会话累计的轮数、请求数与 token 用量 |
| `/<扩展命令> [参数]` | 扩展注册的命令，内置命令优先；运行中也立即执行 |
| `/quit` | 取消当前任务并退出 |
| Ctrl+C | 取消当前任务或审批，保留会话；空闲时 2 秒内连按两次退出 |
| EOF（Ctrl+D）、SIGTERM | EOF 结束会话，管道中的任务先完成；SIGTERM 取消并退出 |

需要审批的调用显示工具名、参数（长文本参数至多显示 60 行，并注明其余行数）与要审批的原因，等待选择；agent 的 `permissions.allow` 列出的只读命令直接执行，规则见[安全模型](SECURITY.md)：

```text
[approval 1] bash: ls -la
  the agent requires tool approval
  1/y allow once · 2 allow identical calls in this conversation · 3/n deny · 4 discuss
[approval] Choose 1, 2, 3 or 4:
```

1 只允许这一次；2 本会话允许工具名与参数完全相同的调用；3 拒绝；4 不执行，把输入的话交给当前任务。空输入不授权，超时（`approvalTimeoutSecs`）不执行。

退出码：`/quit`、EOF 与空闲时连按两次 Ctrl+C 为 0，参数或配置错误、读取终端失败为 2，SIGTERM 为 5。单轮的模型或预算错误报告后继续对话。会话记录写不进去时会话自行关闭，chat 从存储重新打开它；会话只在内存中（`--no-session`）或重新打开失败时以 1 结束。

## ACP

- 传输：经 stdin 与 stdout 收发 ACP v1 的 JSON-RPC 消息；stdout 只走协议，诊断在 stderr。
- 会话：`session/new` 建会话，`cwd` 由客户端给出，会话此后一直在这个目录工作，续接后也一样；每个会话一个文件，放在 chat 为该工作目录使用的会话目录中（`--no-session` 时只在内存）。`session/load` 续接并把整段对话回放给客户端（摘要替换掉的较早部分也回放，提问与工具结果中的图片以图片块回放），`session/resume` 续接但不回放，两者只认在所给 `cwd` 开始的会话；`session/list` 列出会话（标题为会话名称，没有名称时为第一条提问），读不出的会话文件在 stderr 各报一行。`session/close` 取消会话正在进行的回合，并把会话移出内存：会话文件随之关闭（有回合在跑时，等它结束），别的进程就能打开它；文件保留，可以再 `session/load` 或 `session/resume`。关闭本进程没有持有的会话也算成功。`initialize` 总会声明 `promptCapabilities.image` 与 `sessionCapabilities.close`（关闭也释放内存）；`loadSession` 与 `sessionCapabilities` 的 `list`、`resume` 只在保存会话文件（不加 `--no-session`）时声明。客户端不发 `session/close` 时，会话留在内存中、文件保持锁定，直到进程退出。`session/fork` 未实现。客户端给的 `mcpServers` 被忽略，MCP 服务器只来自 `config.toml`。
- 命令：建会话、载入或续接成功后，应答之后以 `available_commands_update` 声明 `/name` 与 `/compact`。`/name <名称>` 给会话起名，不发给模型：以 `session_info_update` 通知新标题，回合以 `end_turn` 结束。名称只取一行（换行等控制字符换成空格、去掉首尾空白）；没有给名称时以 JSON-RPC 错误（`-32602`）应答，回合进行中也以错误应答。`/compact` 请模型摘要较早的对话，自成一个回合，可以用 `session/cancel` 取消：进度以 `compaction_update` 通知，完成时以 `end_turn` 结束，没有可摘要的部分时不发请求、发一条 `notice` 后同样以 `end_turn` 结束，取消为 `cancelled`，失败以 JSON-RPC 错误应答；带了输入时以 `-32602` 应答。`/named` 等其他文本仍是普通提问。
- 回合：`session/prompt` 的回答结束时应答 `stopReason`：完成为 `end_turn`，截断与 token、成本预算用尽为 `max_tokens`，请求次数用尽为 `max_turn_requests`（回答被截断时也是），取消为 `cancelled`；出错或超时以 JSON-RPC 错误应答。`session/cancel` 取消当前回合，紧跟在 `session/prompt` 之后发出的也生效。提问取其中的文本与图片：`resource_link` 以一行 `[resource <URI> (<名称>)]` 交给模型；图片按模型 API 的上限适配后随提问发给模型，适配不了时以 JSON-RPC 错误（`-32602`）应答并说明是第几张、为什么；音频与内嵌资源没有声明支持，收到时略去。既没有文本也没有图片的提问、带图片的命令（如 `/compact`）同样以 `-32602` 应答。以上被拒的提问不记入会话，也不发请求。工具结果中的图片在工具调用的内容中以图片块显示。
- 配置项：建会话、载入与续接的应答带 `configOptions`，都是单选：`model`（类别 `model`），可选 `[[models]]` 列出的模型与会话正在用的模型；`reasoning`（类别 `thought_level`），可选 `default`（请求不带推理设置，由服务决定）与 `off` 到 `xhigh`。会话正在回合中时（如载入本进程正在运行的会话）应答不带配置项。`session/set_config_option` 在会话空闲时修改，对之后的提问生效，应答带全部配置项的当前值，与 chat 的 `/model`、`/reasoning` 相同：换模型不换 provider，也不清空对话。不在可选值中的值、未知的配置项、不是单选值的值与回合进行中的修改都以 `-32602` 应答，不做改动。
- 更新：回答与推理流式发送，工具调用发 `tool_call` 及其状态（`read_file`、`edit_file`、`write_file` 带所操作文件的位置；其他工具的 `path` 可能是目录，客户端会把位置当文件打开，所以不带），`write_file` 附 diff、`edit_file` 附补丁。审批经 `session/request_permission`（允许一次、本会话允许相同调用、拒绝），请求中的调用以要审批的原因为内容（调用结束后换成结果），等待时间同 chat。
- 用量：会话所用模型在 `[[models]]` 有 `contextWindow` 时，以 `usage_update` 报告上下文的 token 数与窗口大小：每个报告了用量的回答之后，取它的输入加输出 token；建会话、载入或续接之后（在命令之后）、每个回合结束时与换模型之后，取最近一次测得的用量加上之后消息的估算，与自动摘要判断阈值的估算相同。模型没有窗口时不发。不带花费：协议要求写明币种，而 `[[models]]` 的单价不记币种。
- 退出：客户端关闭 stdin 时退出；收到 SIGINT 或 SIGTERM 时先取消进行中的回合，等它们结束（被中止的回答记入会话）再退出，这时回合的应答不保证送达客户端。两种情况退出码都是 0。等待期间再收到一次信号时，进程立即被该信号结束。

## IM 服务

IM 服务是程序 `zx-im`，命令见 [README](../README.md#im-服务)，`im.toml` 见 [CONFIGURATION.md](CONFIGURATION.md#imtoml)，接入一个平台的步骤见 [IM_SETUP.md](IM_SETUP.md)。

| 项 | 说明 |
|---|---|
| Slack 应用 | 打开 Socket Mode 与 Interactivity；订阅 bot 事件 `app_mention` 与 `message.im`。bot token 需要读取 @ 消息的权限（`app_mentions:read`，以应用设置页显示的为准）、`im:history` 与 `chat:write`；app-level token 带 `connections:write`。可选：订阅 `message.channels`、`message.groups`（权限 `channels:history`、`groups:history`）后线程里不 @ 的回复也会送来，机器人也能读取它第一次被 @ 的线程（Slack 从最早的消息起分页返回，所以逐页读到末尾再取最近的：每页 200 条、至多 5 页；线程更长或后面的页读不到时，取已读到的部分里最近的，并记一条 `failed` 日志），以及频道里上次 @ 它之后的消息（`conversations.history`，从这次 @ 往前读一页 200 条，Slack 先返回最近的）；`users:read` 让模型看到发言人的名字；`reactions:write` 让插话得到 👀 回执；`files:read` 让模型看到消息里的图片（见下文“图片”）。线程里其他机器人发的消息（如告警）与别人的发言一样只记进会话、随线程读入，不回答，也不由它开始对话；正文为空时取附件的 `pretext`、`title`、`text`，都没有时取 `fallback`，不读 `blocks`；没有用户身份的集成以 bot ID 为发言人，名字经 `bots.info` 取得（同样需要 `users:read`）。Marketplace 之外商业分发的应用读取线程与频道历史受 Slack 的限频（每分钟 1 次、每次 15 条），自建的内部应用不受影响 |
| 飞书应用 | 企业自建应用；事件与回调都选长连接，保存这两项设置时要有长连接在线（官方文档的要求），所以先启动 `zx-im` 再保存；订阅事件 `im.message.receive_v1` 与回调 `card.action.trigger`；开通发送消息（`im:message` 一项就覆盖发送、编辑消息、更新卡片与下载图片）、读取 @ 机器人消息（`im:message.group_at_msg:readonly`）与私聊消息（`im:message.p2p_msg:readonly`）的权限；改过这些设置后创建并发布新版本（官方文档对机器人能力与回调写明发布后生效）。可选：开通“获取群组中所有消息”（`im:message.group_msg`）后，话题里不 @ 的回复也会送来，机器人也能读取话题与群的历史（见下文“会话”；官方文档写明以应用身份读群里的消息要这项权限）。没开时照常回答，只是读不到之前的消息，并记一条 `failed` 日志。群的历史：先取这次 @ 的消息得到它的时间，再按时间倒序列出群里在它之前的一页 50 条，列出的只有话题的根消息，没有话题里的回复。话题的历史：先取根消息得到话题 ID，再倒序列出话题里的一页 50 条。读入文本与富文本消息，其他机器人发的也读入，本应用发的不读；图片、卡片等消息不读入。认出 @ 本机器人的历史消息要用它的 open_id，经 `/open-apis/bot/v3/info` 取得（官方文档写明不需要权限，要启用机器人能力）。⚠ COMPLIANCE REVIEW: [飞书：读取群内全部消息与历史消息] Subject to Legal/Export Compliance approval。收文本、图片与富文本（`post`）消息；下载消息里的图片经“获取消息中的资源文件”接口，据官方文档需要 `im:message`、`im:message:readonly`、`im:message.history:readonly` 之一（只开 `im:message` 时在真实租户验证过）。⚠ COMPLIANCE REVIEW: [飞书：下载用户发送的图片（数据）] Subject to Legal/Export Compliance approval。名字与回执未实现（模型看到的是 open_id，它按应用区分：同一个人在不同应用里的 open_id 不同） |
| 平台限流 | 平台要求等待时按它说的秒数等待后重试：一次调用至多重试 3 次、合计至多等 20 秒，短于服务对每次平台调用的 30 秒时限。要求等得更久时这次调用立即失败，原因写明被限流与要求等待的秒数（如发不出回答时的 `failed` 日志）；要求等待的时间读不出时按 1 秒算，并另记一条带 `platform` 的 `failed` |
| 网络 | 只需出网。只能经 HTTP 代理出网时写 config.toml 的 `[proxy]`（见 [CONFIGURATION.md](CONFIGURATION.md#代理)）：REST 调用与长连接都经它，长连接经代理的 `CONNECT` 隧道；没写时 REST 调用读环境变量中的代理，长连接直连。REST 调用信任系统的根证书；WebSocket 长连接只信任编译进程序的根证书，在解开 TLS 的出口代理（私有 CA）后面，REST 调用能通而长连接的握手可能失败 |
| 会话 | 私聊直接发送。单独发送 `/new` 立即开新会话，之前的保留；此后会话被释放或进程重启，接上的也是这个新会话，它不再读入线程里已有的消息。频道（飞书为群）里 @ 机器人，回答在这条消息的线程里；刚被回答的人接着说（不 @ 别人、期间没有别人发言）不必再 @；别人在线程里的话只记进会话，被 @ 时一并参考；会话已释放时，等有运行名额再打开会话写入；对话还没有会话时（如只发过 `/cancel`）不记录，下次被 @ 建会话时随线程读入。在别人发起的线程里第一次被 @ 时，先读入线程里这次 @ 之前的消息（最近的至多 50 条），再建会话。在频道里直接 @ 机器人、开始一个新线程时，先读入频道里上次 @ 它之后、这次之前的消息（最近的至多 50 条），再建会话：只读频道里的，不读线程里的回复；其他机器人的消息也读入，机器人自己的不读。“上次 @”是频道里上一条 @ 机器人的消息，以平台上的消息为准，不另存游标，所以重启不影响，停机期间的 @ 也算；读不到时（如缺权限）照常回答，记一条 `failed` 日志。群聊中交给模型的每条消息前加发言人的名字；机器人回答的消息，其发送者在平台上的用户 ID 作为这条输入的 actor 记入会话，并交给策略与钩子（排队等待运行期间收到的几条合成一条输入，取最后一条的发送者）。没 @ 机器人的频道消息收到时不理会，留到频道里下一次 @ 机器人时按上文读入；机器人没参与过的线程一概不理会；会话释放（`idleSecs`）或重启之后，接话要再 @ 一次 |
| 图片 | 机器人要回答的消息带图片（PNG、JPEG、GIF、WebP）时，在交给模型之前向平台下载：每条消息至多 4 张，每个文件至多 20 MiB，每次下载受出站超时限制，再缩到模型接口的上限内（与 read 工具相同）。下载失败、文件过大或不是这几种图片时，消息照常交给模型，正文末尾另起一行写明 `[image <名字> could not be used: <原因>]`；下载失败还记一条 `failed` 日志。其他文件不交给模型。只记进会话、不回答的消息不下载图片，正文末尾写 `[image <名字> not shown]`；读入的线程与频道历史不带图片。backlog 只记图片的引用（Slack 的私有 URL、飞书的 `image_key`），重启后处理留下的消息时再下载。图片不计入 `maxPendingBytes`，只受每条 4 张的限制。Slack 的 bot token 只发往 Slack 的文件主机（`files.slack.com`）或 `apiBase` 的主机，指向别处的文件不下载；带 `file_share` 子类型的消息照常送来（依据 Slack 文档，没有在真实工作区验证） |
| 运行中 | 要回答的消息作为插话进入运行；任务结束时没读到的插话作为下一次运行的提问，与其他会话一样排队等运行名额。单独发送 `/cancel` 取消本会话的任务并丢弃没读到的插话，取消完成前收到的消息在任务结束后回答；`/new` 先取消任务再开新会话，取消完成前收到的消息在新会话里回答。单独发送 `/compact` 请模型摘要较早的对话：与一次任务一样占运行名额、显示“处理中”，可被 `/cancel` 取消，结束时说明已摘要还是没有可摘要的；只在本会话空闲且有运行名额时进行，否则回复 `replies.busy`，稍后再发；还没有会话的对话不为它新建会话。开始时发“处理中”，结束时替换为回答；替换失败时把回答另发一条（替换超时而平台其实已改好时，回答会出现两次）。模型答完后、运行因插话接着请求模型时，这个回答先替换“处理中”，再发一条新的“处理中”，所以一次运行可以有几个回答。运行中发过审批卡片时，模型的回答另发一条，放在卡片之后（发不出时仍替换“处理中”），“处理中”改为 `replies.answerBelow`；取消、超时等没有回答的结局仍替换“处理中” |
| 容量 | 达到 `[limits]` 的上限时，消息不被接纳并记一条 `failed` 日志，写明碰到的是哪一个（以 `ServerConfig` 的字段名，如 `max_pending_messages`，即 `maxPendingMessages`；收件箱满另说明）；机器人要回答的消息另回复 `replies.overloaded`（在路由处被拒的只认 @ 机器人的消息），只记进会话的话不回复。一条消息从被接纳到处理完都计入它所在会话的数量与字节配额，补入发送者名称后再检查一次字节。`/cancel`、`/new`、`/compact` 与审批点击不占文本配额，达到上限时仍可处理，但每个会话的收件箱另有 32 条的上限。只有插入运行、或在运行被 `/cancel`、`/new` 取消期间留给下一次运行的消息才发送接收回执。打开会话（排队中的新会话，或为写入闲聊重新打开的会话）都在取得 `maxRunning` 的名额后进行；`/new` 仍立即创建新会话 |
| 每天的用量 | `[limits]` 的 `dailyTokens`、`dailyCost` 限整个服务，`[[chats]]` 的同名键限一个聊天，按 UTC 的一天计。每次运行与 `/compact` 结束时，按它报告的用量计入当天，失败的尝试与摘要请求都算。用完后，要开始新工作的提问与 `/compact` 只回复 `replies.dailyLimit`，不调用模型，提问随之丢弃；插进进行中运行的消息照常处理；触发不开线程，只记一条 `failed`。正在进行的运行照常结束，所以一天的用量可能超出上限，超出的部分受 agent 单次预算的约束。会话存在目录时用量只在内存中计数，重启后当天从零计；存在 Postgres 时每次计入都写进库里，启动时读入当天的接着计，写不进时记一条 `failed`，本进程照常计数 |
| 主动发起 | 不等人发消息也能开始对话。`im.toml` 的 `[[schedules]]` 每隔 `everySecs` 秒（至少 60，第一次在启动后一个间隔）在 `chat` 发一条 `title` 开出线程，在线程里以 `schedule:<名字>` 的身份问 `prompt`；宿主自己的程序经 `Program::on_triggers` 给出的触发（直接用库时是 `Server::triggers`）同样处理。此后回答、审批卡片与接话都与 @ 开始的对话相同：模型看到的提问前加来源的名字，来源不是平台的用户，不查名字；在线程里接着问要 @ 机器人。触发只能用于名单里的频道或群，否则什么也不发，只记一条 `failed`；开线程的消息发不出时同样只记 `failed`。触发计入 `maxConversations`，不进 backlog：停机时还没开出线程的会丢掉，停机期间到点的不补 |
| 数据范围 | 会话只保存授权聊天里机器人参与的对话（私聊，或它被 @ 过的线程）中的消息，其余在路由处丢弃。平台送来的每条消息在路由之前都先写入 backlog（见“崩溃与重启”），丢弃或处理完即删除，只留下 24 小时内以消息 ID 为名的空标记文件（存在 Postgres 时是清空了内容的一行）；进程恰在这期间崩溃时，记录留到下次启动处理完为止。线程中任何人的话都会进入模型的上下文（Slack 上也包括其他机器人发的消息），写操作仍要审批 |
| 发文件 | 比 `[limits]` 的 `longAnswerChars` 更长的回答作为 Markdown 文件 `answer.md` 发出，状态换成 `replies.answerAttached`；平台发不出时（如应用缺发文件的权限）照旧分段发文字，并记一条 `failed`。每个会话有工具 `send_file`，不由 agent 的 `tools` 启用：发模型写的内容（`content`，须给 `name`），或工作目录下的一个文件（`path`，至多 20 MiB，原样发出、不经脱敏）；发不出时工具结果写明原因。飞书的图片也作为文件发出 |
| 审批 | 使用 assistant agent，`im.toml` 的 `[[chats]]` 给某个聊天指定了 agent 时用那个：自动放行的只读命令直接执行，其余发审批卡片，只有审批人点击才生效（聊天在 `[[chats]]` 有自己的 `approvers` 时是他们，否则是 `access.approvers`）；超时（`approvalTimeoutSecs`）与取消也显示在卡片上。卡片只有“允许一次”与“拒绝”，没有“本会话允许相同调用”：点按钮的审批人不一定是发起对话的人，长期放行写进 agent 文件的 `permissions.allow` |
| 凭据 | Slack 的两个 token 与 飞书的 App Secret 默认引用 `SLACK_APP_TOKEN`、`SLACK_BOT_TOKEN`、`FEISHU_APP_SECRET`，也可以在 `im.toml` 里写别的变量或直接写值；它们引用的变量，连同宿主自己的程序加的平台给出的凭据（`BuiltChannel::credentials`），在命令的环境中删除，输出中出现其值时脱敏；`[sessions]` 的 `postgresUrl` 也是凭据，启动行不写连接串 |
| 会话文件 | 每个对话（私聊，或群里的一个线程）的会话在头部带三个标签：`im.platform`、`im.conversation` 与 `im.thread`（私聊为空）。服务启动时读取会话头部，记下每个对话最近创建的会话，续接时打开它；存储里已没有这个会话时（如文件被删除），记一条 `failed` 日志并像首次一样新建会话；`/new` 用同样的标签立即再建一个，此后续接新的，之前的保留 |
| 日志 | stdout 每行一个 JSON：`approval`（聊天、线程、工具调用、审批人、结果、审批结束的时间）、`released`、`unauthorized`（不在名单里的聊天发来要机器人回答的消息，已回复 `replies.unauthorized`：平台、聊天、线程与发送人，名单要写的 ID 可以从这里取）、`triggered`（触发开出了线程：平台、聊天、线程与来源）、`reloaded`（收到 SIGHUP 后换上了 `im.toml` 的名单：聊天数与审批人数；文件读不了或检查不过时是一条 `failed`，名单不变）、`run`（一次运行结束：平台、聊天、线程，`work` 为 `prompt` 或 `compaction`（`/compact` 的摘要），`outcome`，预算用完时另有 `budget` 与 `truncated`，`modelRequests`，`inputTokens`、`outputTokens`、`cacheReadTokens`、`cacheWriteTokens`，`cost`（没有单价时为 `null`），`durationMs`，供日志系统统计运行指标）、`failed`。平台继续运行时遇到的问题也是带 `platform` 的 `failed`：连接失败后的重试、连接中途断开（写明原因；平台自己关闭或要求换连接的不记）或静默（两倍 ping 间隔加余量收不到任何帧）后重新打开、读不了的帧。对话内容在会话文件中 |
| 退出 | SIGINT 或 SIGTERM 时取消进行中的任务，状态改为 `stopped` 的文案；还没写入会话的消息（等运行名额的、运行没读到的插话、收尾期间收到的）不再处理，也不回复，留在 backlog 里等下次启动；正在建立的平台连接随即放弃，`/new` 正在新建的会话也是，这条 `/new` 留到下次启动。收尾期间再收到一次信号时，进程立即被该信号结束。退出码：正常为 0，配置错误与被拒的凭据为 2，平台连接意外结束或失去主的身份（见“主备”）为 1；错误写到 stderr，以 `zx-im:` 开头 |
| 崩溃与重启 | 收到的消息先写入会话目录下的 backlog（`<dir>/.backlog`，每条一个文件；Unix 上服务新建的会话目录、`.running` 与 backlog 的目录只有属主能进入，文件只有属主能读），再确认投递；写入会话、已回复（如命令、拒绝）或有意丢弃（`/cancel`、`/new`、容量不足、对话还没有会话时别人的话）之后才删除。写不进 backlog 的消息不确认投递，等平台重投，并记一条带 `platform` 的 `failed` 日志。确认发不出去时（连接已断开）消息照常处理，连接随后重建；平台再投递这条消息时会被认出，不会处理两次。启动时先按到达顺序处理上个进程留下的消息，再接收新消息；backlog 里有读不出的文件（如被改坏）时，这次启动不处理留下的任何消息，只记一条 `failed` 日志，文件原样留到下次启动，记录里的消息读不出时只留下这一条，并记一条日志；上个进程遗留的“处理中”改为 `interrupted` 的文案。处理完的消息 24 小时内再被投递时会被认出并丢弃。崩溃或停止恰好发生在写入会话与删除记录之间时，这条消息会再处理一次。只防进程崩溃与被杀，不防断电（写入后不 fsync） |
| 存在 Postgres | `[sessions]` 写 `postgresUrl` 时，会话、backlog、“处理中”的记录与每天的用量存进 Postgres 里 `zx_im_` 开头的表，而不是会话目录下的文件，上文“崩溃与重启”所说的行为照旧，写入以库的提交为准。启动时连接并建好缺的表（几个进程同时启动时依次进行），表的版本记在 `zx_im_schema`，与程序的不同时拒绝启动。所有存储共用一条连接，断开后下一条语句重连；一条语句（含连接）30 秒内没完成即算失败。会话的每条记录以会话与序号为主键：同一会话有第二个写入者时，它的追加失败，会话不会分叉。backlog 里处理完的消息留下清空了内容的一行，24 小时后在之后的某次处理完消息时删除；每天的用量每个聊天每天一行，不删除 |
| 主备 | 会话存在 Postgres 时，几个 `zx-im` 可以用同一个 `im.toml` 同时运行，一个进程也照此先当选再服务。每个进程装配好之后在库里争抢一把锁（锁的键在建表时随机取，共用一个库、各用一个 schema 的几套部署互不影响）：拿到的当选为主，在 stderr 写 `[im] leading, term <n>`，再连平台、开始服务；其余写 `[im] another replica leads; this one waits to take over`，每 2 秒再试，期间不连平台，SIGINT 与 SIGTERM 让它以退出码 0 结束，SIGHUP 照样重读名单，接手时用它。锁随持有它的连接而去：主的进程结束时随即放出；主所在的节点或网络出了问题时，库按服务端的 TCP keepalive 在半分钟左右放出。接手的主把任期号加一，此后上一任的写入都被拒绝：会话、backlog、“处理中”的记录与用量的每条写入都在同一条语句里核对任期，新的任期要等上一任进行中的写入结束才开始。接手按启动的流程，这几步在拿到锁之后才做（预先读会读到主还在改的状态），所以接手的耗时是锁放出的时间加上它们：读会话头部，替换遗留的“处理中”，处理 backlog 里留下的消息，对话在原会话里接着。主每 5 秒在持锁的连接上读一次任期；连接断开、5 秒没有回应或任期变了，就算失去了主的身份：像收到 SIGTERM 一样停止服务，stderr 写 `zx-im: lost the lead: <原因>`，以退出码 1 结束，由编排系统重启为备 |

## 语义

### 与 pi 一致的关键语义

1. provider 不抛错：失败编码为 `stopReason` 为 `error` 或 `aborted` 的消息。
2. 模型的回复里没有工具调用时，循环结束。
3. 工具调用先逐个检查，再并发执行，按调用顺序写结果；有一个工具是 `Sequential` 时整批串行。
4. 工具失败（找不到、参数无效、被拒绝、执行报错、panic）都变成工具结果，循环继续。
5. 回复因 `length` 截断时，不执行其中的工具调用。
6. 引导消息在每一步结束后插入，后续消息在本来要停下时插入。
7. 发给模型的上下文以会话投影为准；重试时用 `context_edit` 省略失败的回答，不重复用户消息。
8. 系统提示词是对话的一部分：第一条 system 消息记录完整提示词，之后只追加变化的段落。

### 与 pi 的差异

| 项目 | zx-agent |
|---|---|
| 上限 | 有模型请求数、时间、token 与花费上限 |
| 工具确认 | 宿主审批；`run` 无人时拒绝 |
| 工具放行 | 由独立的接口 `ToolPolicy` 判定（放行、拒绝、询问），所有策略取最严的结论；钩子只观察与改写，不拦截调用（pi 在 `beforeToolCall` 钩子里拦截） |
| 输出截断 | 按 agent 的 `continueOnLength` 自动续写，仍未完成时运行以截断结束（见 [run 退出码](#run-退出码) 6） |
| 会话写入失败 | 立即中止运行，之后不再写入；会话随即关闭，再次打开时从存储读回 |
| 会话文件 | 启动时立即创建；chat 默认存入会话目录，`--resume` 续接；复制会话与切换分支只在 SDK |
| 会话名称 | 不能清除；chat 没有起名命令；客户端改标题的 `session/set_title` 在协议中仍不稳定，未实现 |
| 队列取法 | 两个队列共用一种取法（`QueueMode`），默认 `All`；pi 两个队列各设一种，默认 `one-at-a-time` |
| 扩展的消息与状态 | 运行中排队，下一回合开始或运行结束时写入；消息只有文本；请求只声明当前活跃的工具 |
| 环境信息 | 会话打开时写入系统提示词 |
| 项目上下文文件 | 每层目录只认 `AGENTS.md`；pi 每层取 `AGENTS.override.md`、`AGENTS.md`、`CLAUDE.md` 等候选名中找到的第一个。其他文件由 agent 的 `context.files` 列出 |
| 续接会话的目录 | 记录的目录已不存在时报错（见 `--cwd`），不询问是否改在当前目录继续 |
| 参数校验 | 严格按 JSON Schema，不转换类型；改写只在工具的 `prepare_arguments` 中 |
| 中断的工具调用 | 有副作用的调用执行前先记录开始；恢复时补写“结果未知”或“未执行”，不重放 |
| 工具结果 | 文本 `content` 与图片 `images`，另有不发给模型的 `details` 与 `usage`；没有 `structuredContent`、`runToolCall` |
| 图片 | 用户消息与工具结果的图片在文本旁的 `images` 里，不与文本穿插；pi 的内容是文本块与图片块的数组 |
| 失败分类 | provider 判断失败的种类，运行只按种类决定重试或摘要（见[模型请求失败](#模型请求失败)）；pi 按错误文本判断 |
| 上下文过长 | 只按错误识别（HTTP 413、错误码、通用措辞），不按服务推断 |
| 摘要 | 每次重新读取原始记录，不输入上一份摘要；每条事实引用来源条目。宿主要自己做摘要时实现独立的接口 `Summarizer`，钩子不提供摘要 |
| 续跑 | 以失败或被取消的回答结尾时，先省略该回答再续跑 |
| `prepare_request` | 只能替换模型与推理强度，不能改上下文 |
| 推理写法 | OpenAI 兼容服务由 `provider.reasoningFormat` 在三种中显式选择；Anthropic 写成自适应思考加 `output_config.effort` |
| 模型元数据 | 只有 `[[models]]` 给出的上下文窗口、单价与能否看图（`input`），没有内置模型表 |
| 界面 | 没有 TUI，交互界面交给 ACP 客户端 |

### 图片

- 用户消息与工具结果可以带图片。提问的图片来自 ACP 客户端、`run` 的 `@文件` 与 IM 附件；工具结果的图片来自 `read_file` 读到的 PNG、JPEG、GIF、WebP 文件与 MCP 工具的结果。
- 图片进入会话前按 pi 的上限适配：每边不超过 2000 像素、base64 不超过 4.5 MiB 的原样保留；更大的缩小后重新编码，先试 PNG，再按质量 80、85、70、55、40 试 JPEG，仍放不下就每次缩到四分之三。解不开的文件在这里被拒，不进会话；解码一张至多用 256 MiB 内存。`read_file` 读到缩小过的图片时，结果写明原尺寸与换算比例。
- 只有 `[[models]]` 的 `input` 含 `"image"` 的模型收到图片；发给其他模型时，provider 把一条消息的图片换成一句说明（用户消息为 `(image omitted: model does not support images)`，工具结果为 `(tool image omitted: model does not support images)`），会话里仍存原图。
- 用户消息的图片在文本之前，工具结果的图片在文本之后。OpenAI 兼容服务的工具消息只收文本，图片在这批结果之后以一条用户消息发出（开头一行 `Attached image(s) from tool result:`）；Anthropic 不收失败结果里的图片，它们放在这一轮全部结果之后，前面一行写明来自哪次调用。
- 估算上下文大小时，每张图片按 4800 字节（约 1200 token）计，与数据大小无关；摘要请求只读到图片的类型，读不到内容。
- 图片不经脱敏（见 [SECURITY.md](SECURITY.md#密钥)），以 base64 存进会话文件，也在事件的消息里。

### 模型请求失败

provider 判断每次失败的种类，记在失败回答的 `providerError` 里：`kind` 是种类，`status`（HTTP 状态）、`code`（服务端的错误码）与 `retryAfterMs`（服务端要求等待的毫秒数）在服务端给出时才有。运行只按 `kind` 决定接下来做什么：

| `kind` | 失败 | 之后 |
|---|---|---|
| `rate_limited` | 限频：HTTP 429 或限频的错误码、措辞 | 重试 |
| `unavailable` | 服务过载、出错或暂时不可用：HTTP 5xx 或相应的错误码、措辞 | 重试 |
| `transport` | 连接失败或中途断开，包括流结束时没有结束原因（如网关或代理截断了流） | 重试 |
| `timeout` | 连接、等待响应头或流中下一段数据超时（后两者的时限为 `provider.idleTimeoutSecs`），或 HTTP 408 与相应的错误码、措辞 | 重试 |
| `context_overflow` | 上下文过长：HTTP 413、相应的错误码、通用措辞，或指明上下文过长的结束原因（如 `model_context_window_exceeded`） | 开启自动摘要时摘要一次再试，否则失败 |
| `access_denied` | 凭据、权限或配额被拒：HTTP 401、403 或相应的错误码、措辞；Anthropic 的月度花费上限也是 429，`error.details.error_code` 为 `enforced_spend_limit_reached`，算作此类 | 失败 |
| `api_key` | 取不到 API key，或 key 不能作为请求头的值（如含换行），请求没有发出 | 失败 |
| `refused` | 服务端以正常结束、截断与调用工具以外的原因结束回答（OpenAI 兼容服务的 `finish_reason` 如 `content_filter`，Anthropic 的 `stop_reason` 如 `refusal`），`code` 为该取值 | 失败 |
| `protocol` | 响应不合协议，如无法解析的数据、要求流式时整个回答以 JSON 返回 | 失败 |
| `resource_limit` | 超出流式响应的上限：每次请求 16 MiB、每个 SSE 事件 1 MiB | 失败 |
| `other` | 其他失败，如 HTTP 400 | 失败 |

HTTP 错误按状态归类，错误码表明上下文过长或凭据、配额问题时以错误码为准；状态不能说明时（如 400），措辞表明上下文过长也算 `context_overflow`。2xx 响应体或流中报出的错误按错误码归类，错误码不认识或没有时按措辞。重试至多 `provider.maxRetries` 次，遵守服务端的 `Retry-After`；它要求等的超过 60 秒时不再重试，失败原因写明它要求等多久（同 pi）。连接或读流失败的原因里，服务地址只写到 origin（协议、主机与端口），路径与查询可能带凭据，不写进会话。

### 系统提示词的组成

| 部分 | 内容 | 何时出现 |
|---|---|---|
| 说明 | agent 的说明 | 总是 |
| `<approvals>` | 哪些工具调用要审批，由实际生效的权限生成，agent 的说明里不必写：有人回答时（chat、acp、zx-im）列出要人批准的工具（agent 有 bash 时含 bash）与可以免审批的命令前缀；没人回答时（run）列出会被拒绝的工具；跳过审批（`--dangerously-skip-approvals` 或 `permissions.approveAll`）时说明不询问 | 有工具要审批或跳过审批时 |
| `<environment>` | 操作系统与版本、CPU 架构、会话打开的时间（UTC）与时区、工作目录；容器执行时描述容器 | 总是 |
| `<project_context>` | 自动发现的 `AGENTS.md` 与 agent 列出的文件 | 有项目说明时 |
| `<available_skills>` | skill 的名称、描述与正文文件路径 | 启用 `read_skill` 且登记了 skill 时 |
| `<conversation>` | 对话经聊天平台进行：群聊中每条消息前有发言人的名字；一次任务只发出最终回答，工具调用之前的文字与工具输出都不发，所以最终回答要自成一体 | `zx-im` 的会话 |

环境信息通过读文件获得，不执行命令；主机名和用户名不写入，因为提示词会发给模型服务方。恢复会话时只追加有变化的段落，发送方式见 `provider.systemUpdates`。

## 输出格式

### 事件（`--output jsonl`）

每行一个事件，带信封字段：`schemaVersion`（当前为 7）、`seq`（运行内从 1 递增）、`atMs`、`sessionId`、`runId` 与 `type`。新增事件类型或字段时 `schemaVersion` 不变，读取方跳过不认识的 `type` 与字段；删除、改名或改变含义时才提高。一次运行以 `run_start` 开始，以 `run_end`（`report`）结束：

```json
{"schemaVersion":7,"seq":12,"atMs":1761700000000,"sessionId":"…","runId":"…","type":"run_end","report":{"outcome":"completed","finalText":"## Summary ...","modelRequests":3,"retries":0,"usage":{"input":1200,"output":300,"cacheRead":0,"totalTokens":1500},"sessionId":"…"}}
```

预算用完时，`report` 的 `outcome` 为 `budget_exhausted`，另带 `budget`（`model_request_limit`、`token_budget` 或 `cost_budget`）与 `truncated`（这次运行最后的回答是否在输出 token 上限处被截断）；`attempt_end` 的 `outcome` 也是这样。文本模式的最后一行写作 `outcome=budget_exhausted budget=token_budget truncated=true`。

其余事件：`attempt_start` / `attempt_end`（每次尝试）、`step_start` / `step_end`、`request_start`、`message_start` / `message_update`（需 `--deltas`）/ `message_end`、`tool_execution_start` / `tool_execution_update` / `tool_execution_end`、`tool_decision`（策略的结论 `decision`：`allow`、`deny` 或 `ask`，后两者带 `reason`）、`approval_requested` / `approval_resolved`、`retry_scheduled`、`context_edited`、`context_overflow`、`compaction_start` / `compaction_end` / `compaction_failed`、`custom_entry`、`hook_failed`、`tool_call_recovered`。chat 中手动 `/compact` 另输出一行不带信封字段的 `compaction_report`。`attempt_start` 与 `request_start` 的 `attempt` 是第几次尝试（首次为 1，重试与上下文溢出恢复都会再开一次尝试）；`retry_scheduled` 的 `retry` 是第几次重试（首次为 1，至多 `maxRetries`）。事件包含用户输入、工具参数、脱敏后的工具输出与模型输出。

`report.modelRequests` 含摘要请求；`report.usage` 只含模型的用量，失败的尝试也计入，被取消的请求记为 0；配置了单价时有 `totalCost`。工具报告的花费在 `report.toolUsage`，被拒的调用数在 `report.deniedCalls`。

### OpenTelemetry 追踪

run、chat、acp 与 zx-im 在设置了 `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`（完整地址）或 `OTEL_EXPORTER_OTLP_ENDPOINT`（后接 `/v1/traces`）时，用 OTLP/HTTP 导出追踪；都没设时不建立任何连接。其余标准变量由 OpenTelemetry SDK 读取。导出经 config.toml 的 `[proxy]`，没写时经环境变量中的代理（见 [CONFIGURATION.md](CONFIGURATION.md#代理)）；收集端在集群内时把它写进 `noProxy`。服务名（`service.name`）依次取 `OTEL_SERVICE_NAME`、`OTEL_RESOURCE_ATTRIBUTES` 中的 `service.name`，都没有时为程序名：`zx-agent` 或 `zx-im`。只导出 span，不导出指标，需要时由收集端从 span 推导。

一次运行是一条 trace：`run` 之下有 `attempt`、`step`、`request`、`tool <工具名>` 与 `compaction`，属性带运行 ID（`zx.run.id`）、模型、结果（预算用完时另有 `zx.budget` 与 `zx.truncated`）、用量与成本。只含元数据，不含输入、输出、参数与错误文本。`run` 在 `TRACEPARENT`（及 `TRACESTATE`）为合法值时接续调用方的 trace。导出经有界队列，满时丢弃，退出时最多等待 5 秒；一次运行的结束被丢弃时，它的 span 在同一会话开始下一次运行时结束（没有结果属性），最迟在退出时结束。命令看不到可能带认证头的 `OTEL_EXPORTER_OTLP_HEADERS`、`OTEL_EXPORTER_OTLP_TRACES_HEADERS` 与 `OTEL_EXPORTER_OTLP_METRICS_HEADERS`，工具输出中出现它们的值时脱敏。

### 会话文件（`--session`）

第一行是头部，之后每行一个条目，条目之间通过 `parentId` 连成一棵树：

```json
{"type":"session","version":15,"id":"…","timestamp":1761700000000,"cwd":"/srv","model":"…"}
{"type":"message","id":"a1b2c3d4","parentId":"f0e1d2c3","timestamp":…,"message":{"role":"user","content":"…","timestamp":…}}
{"type":"context_edit","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":…,"targetId":"b2c3d4e5","replacement":null}
```

- 只追加：修正上下文靠追加 `context_edit`，原始记录保留；`context_edit` 不能指向 system 或摘要消息。
- 同一文件同一时间只有一个写入者（`flock`）；被占用时 `run` 与 `chat` 以退出码 2 结束。Unix 以外的平台不加锁。
- 崩溃留下的未写完的尾记录在下次打开时略过并在 stderr 报告，在追加下一条记录之前截掉；其他损坏报错，文件保持原样。一次写入失败、超时或被中断后不再写入，会话随即关闭；再次打开时读回已写入的记录。
- 只读写版本 15，其他版本的文件拒绝打开，不做升级。zx-im 启动时建立的对话索引跳过其他版本的会话（存在 Postgres 里的也一样）：这样的对话下次被回答时像第一次一样建新会话，旧会话留在存储中。
- 用户消息与工具结果可以带 `images`：每项是 `mimeType` 与 base64 的 `data`，没有图片时不写这个字段。`request_config` 记下请求的模型能否看图（`takesImages`，不能时不写），还原的请求与当时发出的相同。
- 用户消息的记录可以带 `actor`：这条输入是谁发的，由接入方给出（如 IM 的用户 ID）；没有给出时不写这个字段，其他消息不能带。它不发给模型。
- 头部的可选字段：`labels`（宿主给会话的标签，如租户或所属的对话，创建后不变；列表可以按标签筛选）与 `parentSession`（复制分支得到的会话记录来源会话的 ID）。
- 每次模型请求前记录 `request_started`（配置变化时先记 `request_config`），可还原当时的请求；它的 `source.kind` 与 `request_start` 事件的 `kind` 一样是 `task`（任务请求）或 `summary`（摘要请求）。不记录 API key、URL 与 HTTP 头。
- 有副作用的工具调用执行前记录 `tool_call_started`。被取消的运行为整批调用都写下结果：还没开始的记为未执行；执行中被打断的，有副作用的工具说明它可能已改变状态，只读工具说明它没有改变任何东西。运行没能写下结果时（进程崩溃或退出、宿主丢弃了 `Run`、会话写入失败），以及失败或被取消的回答里的调用，由下一次运行补写错误结果，不重放，并发出 `tool_call_recovered`：`started`（结果未知，先核对再重试）、`not_started`（未执行）、`read_only`（只读工具，可重新调用）、`start_unknown`（当前未配置该工具，按可能已改变状态处理）。回到会话树上较早的位置后，结果留在另一分支上的调用同样补写：在那个分支上已经开始的记为 `started`。复制分支得到的会话保留来源会话里这些调用的开始记录（包括记在另一分支上的），补写时同样记为 `started`。
- 摘要记录为 `compaction_started`、`compaction` 或 `compaction_failed`。`firstKeptEntryId` 是保留部分的第一条记录；边界落在最近一轮之内时（一次提问里的工具调用就超过了阈值）`compaction_started` 另有 `keptInputEntryId`，即那一轮的提问：摘要读取它，它也原样留在上下文里，排在摘要之前。`compaction_started` 写明摘要由谁产出：内核自己请求模型时是 `request`（可据此还原请求），宿主的摘要器产出时是 `summarizer`（它的名字；它怎样得到摘要不在会话里，也没有对应的 `request_started`）。摘要的事实必须引用来源条目；引用只证明出处可定位，不证明陈述正确或没有信息损失。
- 扩展的记录：`custom`（状态，不进入模型上下文）与 `custom_message`（发给模型的消息，投影为一条用户消息）。
- 会话名称：`session_info`（`name`），以文件中最后一条为准，与分支无关，不进入模型上下文；`name` 只有一行、没有首尾空白、不为空，否则文件打开时报错。复制分支时随分支原样复制；起名也会更新列表的“最近活动”。

---

REFERENCE.md · v0.42 · 2026年11月