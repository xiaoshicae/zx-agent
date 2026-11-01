# 技术方案 · 常驻服务模式与 IM 接入

| 信息 | 内容 |
|-|-|
| 版本 | `v0.1` |
| 日期 | `2026-11-01` |
| 状态 | 草稿（待评审） |
| Owner | 待指定 |
| 对应 | `docs/ROADMAP.md:365`：P6“部署方向”中告警 Webhook 与 IM 机器人需要新的服务模式，另行评审；P9 的“由 webhook 或告警触发运行”“异步审批” |
| 代码核对基线 | `5518791`（含 PR #25）；文中行号以此为准 |
| Milestone | 一期：`zx-agent serve`、IM 接口、Slack（Socket Mode）、⚑ D:5/CHINA FLAG: 飞书（长连接）、IM 内审批<br>二期：告警 Webhook（如 Alertmanager） |

> ⚑ D:5/CHINA FLAG: 飞书（Feishu），开放平台域名 `open.feishu.cn`、`accounts.feishu.cn`。
>
> ⚠ COMPLIANCE REVIEW: [排障对话内容、命令输出与 IM 用户标识发往上述平台的服务器；出站长连接属于远程访问] 该平台部分的实施须经法务 / 出口合规审批（Subject to Legal/Export Compliance approval）。

---

## 1. 背景与范围

用户在 IM 里 @ Agent 排障，写操作也在 IM 里审批；Agent 以常驻服务的形式运行在 Pod 中。本稿先定 IM 接入的接口，首批实现 Slack 和 ⚑ D:5/CHINA FLAG: 飞书。

### 1.1 为什么做

| 诉求 | 今天的状况 |
|-|-|
| 在 IM 群里 @ Agent 排障 | 只能 `kubectl exec` 进 Pod 运行 chat（`deploy/kubernetes/README.md`） |
| 写操作在 IM 里审批 | 审批只有终端实现（chat）。`Interaction` 是异步 trait（`src/interaction.rs:36-41`），可以接 IM |
| 多人同时使用 | 每人一个 exec 进程；同一个会话文件同一时间只能有一个写入者（PR #21） |
| 常驻 Pod 不暴露 API key | 主进程是 tini 和 sleep，命令能从 `/proc/1/environ` 读到 key（`deploy/kubernetes/README.md:182`） |
| 告警触发排障 | 没有入口；定时巡检只能用 CronJob |

### 1.2 本期范围

- `zx-agent serve`：作为常驻 Pod 的主进程，接收 IM 消息、管理会话、转发审批。
- IM 接口：`im` 模块中的 trait 与类型（3.1）。两个实现：Slack（Socket Mode）、⚑ D:5/CHINA FLAG: 飞书（长连接）。
- 会话：一个 IM 线程对应一个会话，会话文件保存在存储卷上。
- 审批：IM 卡片上的按钮，只有名单中的审批人能批准。
- 访问控制、并发上限、去重与限流。

### 1.3 非本期范围

| 不做 | 原因 |
|-|-|
| 告警 Webhook | 需要 HTTP 服务（新依赖）和入口，放在二期 |
| 用 HTTP 回调接收 IM 事件 | 需要公网入口：Slack 的 Request URL，⚑ D:5/CHINA FLAG: 飞书要求“IPv4 公网地址”（附录 9.2）。集群内通常没有 |
| 其他 IM 平台 | 接口已留出，以后按需新增实现 |
| 多副本 | 会话文件单写者；⚑ D:5/CHINA FLAG: 飞书长连接部署多个客户端时，每个事件只投递给随机一个（缺口 ⑤） |
| 在 Kubernetes 之外运行 serve | 本方案靠 Pod 共享进程命名空间处理 PID 1（2.3.2） |
| 在 IM 中上传文件、流式更新的富文本 | 一期只发文本和审批卡片 |

### 1.4 相关链接

| 资料 | 链接 |
|-|-|
| Slack Socket Mode | https://docs.slack.dev/apis/events-api/using-socket-mode |
| Slack chat.postMessage | https://docs.slack.dev/reference/methods/chat.postMessage |
| Slack 交互 | https://docs.slack.dev/interactivity/handling-user-interaction |
| ⚑ D:5/CHINA FLAG: 飞书事件订阅方式 | https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/server-side-sdk/golang-sdk-guide/handle-events |
| ⚑ D:5/CHINA FLAG: 飞书 Go SDK（长连接实现） | https://github.com/larksuite/oapi-sdk-go （`ws` 包，分支 `v3_main`） |
| Kubernetes 共享进程命名空间 | https://kubernetes.io/docs/tasks/configure-pod-container/share-process-namespace/ |
| 常驻 Pod 部署 | `deploy/kubernetes/README.md` |

---

## 2. 整体设计

### 2.1 架构图

```
  Slack / 飞书⚑
     ▲   │  出站 WebSocket：只需出网，不需要公网入口
     │   ▼
  ImChannel：SlackChannel / FeishuChannel
     ▲   │  Inbound：@ 消息、单聊消息、按钮点击
     │   ▼
     │  Router：按（平台, 聊天, 线程）找到会话；空闲则 prompt，运行中则插话，
     │   │      超出并发上限则排队；按钮点击交给 ImApproval
     │   ▼
     │  AgentSession × N：每个 IM 线程一个，会话文件在 /data/sessions
     │   │  需要审批时调用 Interaction::confirm
     │   ▼
     │  ImApproval：发审批卡片，等匹配的按钮点击
     │   │
     └───┘  send / update：处理中提示、最终回答、审批卡片；卡片结果按 ApprovalResolved 事件更新

  以上组件都在 zx-agent serve 进程内。它是容器的主进程，PID 1 是 pause（2.3.2）。
  “会话”指 AgentSession；“聊天”指 IM 的频道、群或单聊。
  图中“飞书⚑”与 FeishuChannel 均指 ⚑ D:5/CHINA FLAG: 飞书。
```

### 2.2 建成之后怎么用

1. 在 Slack 频道里发“@zx-agent shop 命名空间的 Pod 起不来，看看原因”。
2. Agent 在这条消息下开一个线程，回复“处理中”，然后执行只读命令（自动放行），最后在线程中给出结论。
3. 需要写操作（如 `kubectl rollout restart`）时，线程中出现一张卡片，列出命令和理由，带“批准一次”“拒绝”两个按钮。只有审批人点击才生效，结果更新到卡片上。
4. 在同一线程里追问，会接着同一个会话；不同线程互不影响，可以同时进行。

⚑ D:5/CHINA FLAG: 飞书群中的用法相同，线程对应群里的话题。

### 2.3 方案选型

#### 2.3.1 IM 事件怎么进来：采用出站长连接

| 对比项 | 出站长连接（采用） | HTTP 回调 |
|-|-|-|
| Slack | Socket Mode：“without exposing a public HTTP Request URL” | Request URL，需公网可达 |
| ⚑ D:5/CHINA FLAG: 飞书 | 长连接：“无需提供公网 IP 或域名”，但“你的服务器需要能够访问公网” | 请求地址“必须是 IPv4 公网地址” |
| 集群内部署 | 只需出网 | 需要 Ingress 与公网地址 |
| 代价 | 需要 WebSocket 客户端（新依赖，缺口 ①）。Slack 的 Socket Mode 应用不能上架 Slack Marketplace；⚑ D:5/CHINA FLAG: 飞书长连接只支持企业自建应用 | 需要 HTTP 服务（新依赖）、签名校验与公网暴露 |

**结论：采用出站长连接。** 核心原因是集群内通常没有公网入口。以上限制对内部自用的机器人没有影响。原文见附录 9.1、9.2。

#### 2.3.2 常驻 Pod 的主进程：采用 `zx-agent serve`，并共享 Pod 的进程命名空间

| 对比项 | serve 加 `shareProcessNamespace: true`（采用） | serve 直接作为 PID 1 | 维持 tini 加 sleep |
|-|-|-|-|
| API key | 只在 zx-agent 的环境中，它在 Linux 上不可 dump。PID 1 是 pause，`/proc/1/environ` 中没有容器的环境变量 | 只在 zx-agent 的环境中；PID 1 就是 zx-agent | tini 与 sleep 的环境中有 key，命令可读（`deploy/kubernetes/README.md:182`） |
| 孤儿进程 | pause 回收 | 没有进程回收，成为僵尸进程 | tini 回收 |
| 需要的代码 | 无 | zx-agent 自己承担 init 的职责 | 无 |

孤儿进程的来源：`LocalExecutor` 在命令正常结束后也会结束整个进程组（`src/executor/local.rs:89-92`），组内的后台进程在 bash 退出时已过继给 PID 1（zx-agent 没有设为 subreaper，仓库中没有 `PR_SET_CHILD_SUBREAPER`）。zx-agent 只等待自己启动的子进程：tokio 1.54.1 按子进程句柄调用 `try_wait`（tokio 源码 `src/process/unix/reap.rs:89`），其 `src/process/` 目录中没有 `waitpid` 调用。

本机实测（docker-desktop，Kubernetes v1.36.1，containerd 2.3.1）：两个 Pod 以 uid 10001 运行，主进程各启动一个 2 秒后退出的孤儿进程，容器设置一个环境变量作为标记。

| | 共享进程命名空间 | 不共享 |
|-|-|-|
| PID 1 | `pause` | 主进程 `sleep` |
| 孤儿进程退出后 | 已回收 | 僵尸进程（状态 `Z`，父进程为 1） |
| `/proc/1/environ` 中的标记 | 0 处 | 1 处 |

pause 的回收逻辑在其源码的 `sigreap` 中：`waitpid(-1, NULL, WNOHANG)`，注册为 SIGCHLD 的处理函数。Kubernetes 文档写明共享之后“The container process no longer has PID 1.”，并且同一 Pod 中各容器的进程互相可见；这里只有一个容器，没有影响。原文与实测输出见附录 9.3。

**结论：采用。** Deployment 设置 `shareProcessNamespace: true`，`command` 改为 `zx-agent serve`，不再经过 tini（3.6）。实测中的主进程是 `sleep`；zx-agent 自身的环境不可读，已在 PR #22 中验证（`deploy/kubernetes/README.md:223`）。

---

## 3. 详细设计

### 3.1 IM 接口

新增公开模块 `im`，与 `Provider`、`Tool`、`SessionLog` 一样作为扩展点：新平台只需实现 `ImChannel`。接口按两个平台都具备的能力来定，平台差异留在实现内部。`async_trait` 与 `tokio_util` 的 `CancellationToken` 都是现有依赖，`AgentSession::prompt_with_cancel` 也使用后者。

```rust
/// 一个 IM 平台。实现负责连接、重连、确认平台的投递、去重与限流；
/// serve 只看到下面这些类型。
#[async_trait]
pub trait ImChannel: Send + Sync {
    /// 用于配置、日志和会话路径的名字，例如 "slack"。
    fn platform(&self) -> &str;

    /// 连接平台，把收到的事件送进 `inbound`，直到 `cancel` 被触发。
    /// 断线后自行重连；只在被取消或遇到配置错误（例如凭据无效）时返回。
    async fn run(&self, inbound: mpsc::Sender<Inbound>, cancel: &CancellationToken) -> Result<(), ImError>;

    /// 发一条消息；`to.thread` 有值时回复在该线程中。
    async fn send(&self, to: &Destination, message: &Outbound) -> Result<MessageRef, ImError>;

    /// 替换已发出的消息，例如审批卡片决定之后改为结果。
    async fn update(&self, posted: &MessageRef, message: &Outbound) -> Result<(), ImError>;
}

/// 消息发往哪里。
pub struct Destination {
    pub conversation: ConversationId,
    pub thread: Option<ThreadId>,
}

/// 平台送来的事件。
pub enum Inbound {
    /// 发给机器人的消息：在频道或群里 @ 机器人，或者单聊。
    Message { id: MessageId, at: Destination, sender: UserId, text: String },
    /// 审批卡片上的按钮被点击。
    Decision { at: Destination, user: UserId, approval: ApprovalId, choice: Choice },
}

/// serve 要发出的内容。
pub enum Outbound {
    /// 纯文本，可以含代码块。
    Text(String),
    /// 带“批准一次”“拒绝”两个按钮的卡片。
    ApprovalRequest { id: ApprovalId, tool: String, summary: String, reason: String },
    /// 审批结束后替换卡片：结果，以及点击按钮的人（超时、取消时没有）。
    ApprovalResolved { id: ApprovalId, user: Option<UserId>, result: ApprovalResult },
}

pub enum Choice { AllowOnce, Deny }
```

`ConversationId`（聊天：频道、群或单聊）、`ThreadId`、`UserId`、`MessageId`、`ApprovalId` 都是包一层 `String` 的新类型。`MessageRef` 由实现定义内容，例如 Slack 的 channel 与 ts。`ApprovalResult` 是现有的公开类型（`src/agent/events.rs:113`），与 `ApprovalResolved` 事件共用。

实现必须遵守的约定：

| 约定 | 原因 |
|-|-|
| 先确认，后处理：收到平台投递后立即确认（Slack 回 `envelope_id`，⚑ D:5/CHINA FLAG: 飞书回响应帧），再把事件送进 `inbound` | 两个平台都要求 3 秒内处理完一次投递，否则重投（附录 9.1、9.2）。⚑ D:5/CHINA FLAG: 飞书 Go SDK 等处理函数返回后才写响应帧，本实现不沿用 |
| 同一条消息只送出一次 `Inbound`，用有上限的“最近消息 ID”集合去重 | ⚑ D:5/CHINA FLAG: 飞书原文：“特殊情况下可能会收到重复的推送，如有幂等需求请使用 message_id 去重”；超时重投也会造成重复 |
| 只送出 @ 机器人的消息和单聊消息；机器人自己发的消息不送出 | 避免回复自己，也避免误读群里的闲聊 |
| 超出平台单条上限的文本由实现拆分：`send` 拆成多条；`update` 替换第一段，其余作为后续回复 | serve 不需要知道各平台的上限 |
| `send`、`update` 遵守平台限频，实现内部排队 | Slack 每个频道约每秒 1 条；⚑ D:5/CHINA FLAG: 飞书同一群“机器人共享 5 QPS”（附录 9.1、9.2） |
| 凭据只从环境变量读取 | 与模型 API key 一致（3.6） |

### 3.2 消息路由与会话

1. **会话键**：由平台、聊天（频道、群或单聊）和线程三者组成。频道里一条新的 @ 消息还没有线程，就以它为根回复，开出线程：Slack 用原消息的 `ts` 作为 `thread_ts`；⚑ D:5/CHINA FLAG: 飞书用回复接口并设置 `reply_in_thread`。之后线程里的消息进入同一个会话。单聊按线程处理，规则相同。
2. **会话文件**：`/data/sessions/<平台>/<聊天>/<线程>.jsonl`。`/data` 是现有部署包的存储卷（`deploy/kubernetes/deployment.yaml:58`、`deploy/kubernetes/README.md:189`）。各段 ID 先编码再用作文件名，避免出现 `/` 等字符。格式不变。
3. **空闲回收**：会话一段时间（默认 30 分钟）没有消息后，释放内存和文件锁。再来消息时用 `SessionStore::open`（`src/session.rs:293`）接着用。
4. **运行中又来消息**：同一会话的新消息作为插话（`AgentSession::steer`，`src/agent_session.rs:129`）。运行结束的瞬间到达的插话会留到下一次运行（缺口 ③）。不同会话可以并行，但最多同时运行 `maxRunning` 个（默认 4）；超出时回复“排队中”，按到达顺序执行。
5. **访问控制**：只在配置列出的聊天中响应，单聊也按聊天 ID 列出；名单外的聊天只回复一句“未授权”。
6. **回复方式**：开始运行时在线程中回复“处理中”，结束后把这条消息更新为最终回答，免得刷屏。工具调用默认不逐条发送。
7. **取消**：在线程中发送 `/cancel`，调用 `AgentSession::abort`（`src/agent_session.rs:139`）。
8. **场景配置**：沿用 assistant profile，所以只读命令自动放行，其余命令走 IM 审批（3.3）。

### 3.3 审批

`ImApproval` 实现 `Interaction`。卡片的发出与收尾分在两处：

| 步骤 | 由谁做 | 做法 |
|-|-|-|
| 发卡片、等决定 | `ImApproval::confirm` | 在会话线程中发出 `Outbound::ApprovalRequest`，等 `ApprovalId` 相同的 `Inbound::Decision`。点击者不在审批人名单中时回复一句提示，决定不生效。返回 `Approval::AllowOnce` 或 `Approval::Deny` |
| 卡片改为结果 | serve 的事件订阅 | 收到 `ApprovalResolved` 事件后，按 `tool_call_id` 找到卡片，用 `Outbound::ApprovalResolved` 替换 |

收尾不放在 `confirm` 里，原因如下：

- 运行层用 `LoopConfig::interaction_timeout` 与运行的取消令牌包住 `confirm`（`src/agent/agent_loop.rs:684-688`）。
- 超时或取消时，`confirm` 的 future 被直接丢弃（`src/agent/hooks.rs:292-305`），没有机会再更新卡片。
- `ApprovalResolved` 对每种结局都会发出：批准、拒绝、超时、取消、宿主出错（`src/agent/agent_loop.rs:693-712`）。

其他规则：

- **超时**：`LoopConfig::interaction_timeout`（`src/agent/agent_loop.rs:61`）默认 120 秒（`src/interaction.rs:11`）。serve 按配置 `approvalTimeoutSecs` 设置，默认 600 秒。超时、取消与宿主出错都按拒绝处理，这是运行层现有的约定（`src/interaction.rs:31-34`）。
- **授权范围**：只提供“批准一次”和“拒绝”，不提供会话范围的授权（`Approval::AllowSession`）。
- **审批人身份**：`Approval` 与 `ApprovalResolved` 都不含审批人（缺口 ④）。

### 3.4 Slack 实现

| 方面 | 做法 |
|-|-|
| 入站 | 用 app-level token（`xapp-`，放在 Authorization 头中）调用 `apps.connections.open` 取得 WSS 地址，连接后先收到 `hello`。之后每个 envelope 都立即回复 `{"envelope_id": …}`。<br>`app_mention`（频道中 @）和 `message.im`（单聊，`channel_type` 为 `im`）转成 `Inbound::Message`；`interactive` 类型中的 `block_actions`（按钮）转成 `Inbound::Decision` |
| 重连 | 收到 `disconnect`（`refresh_requested`、`warning`、`link_disabled`）后，重新调用 `apps.connections.open`。文档说明连接“once every few hours”会刷新一次 |
| 出站 | `chat.postMessage` 发消息，用 bot token（`xoxb-`），需要 `chat:write`；`thread_ts` 用于回复线程。`chat.update` 用于替换卡片 |
| 应用配置 | 打开 Socket Mode 和 Interactivity。权限包括：app_mentions 的读权限（文档两处写法不同，见附录 9.1）、`im:history`、`chat:write`；订阅 `app_mention` 与 `message.im` |
| 限制 | 最多 10 个连接；每个频道约每秒 1 条消息 |

### 3.5 ⚑ D:5/CHINA FLAG: 飞书实现

> ⚠ COMPLIANCE REVIEW: [对话内容、命令输出、用户 ID 发往该平台；出站长连接] 本节内容须经法务 / 出口合规审批后实施（Subject to Legal/Export Compliance approval）。

| 方面 | 做法 |
|-|-|
| 入站 | 长连接，只支持企业自建应用。<br>1. 用 App ID 与 App Secret 调用 `POST {域名}/callback/ws/endpoint`，取得 WebSocket 地址与客户端配置。<br>2. 连接后收发二进制帧，格式为 `pbbp2.Frame`：SeqID、LogID、service、method、headers、payload 等。<br>3. 心跳是 ping/pong，默认 2 分钟一次，以服务端下发的间隔为准。<br>4. 数据帧的 payload 是事件 JSON，收到后立即回响应帧（状态码 200）。<br>5. `im.message.receive_v1` 转成 `Inbound::Message`：群聊中是否只收到 @ 机器人的消息，取决于申请的权限；卡片按钮回调 `card.action.trigger` 转成 `Inbound::Decision` |
| 协议依据 | 官方文档只说明长连接由 SDK 提供，没有公开协议。本实现参照 Go SDK `ws` 包（`v3_main`）的源码，实施时在记录中固定所参照的提交（缺口 ②） |
| 卡片回调 | 新版卡片回传交互支持长连接；旧版“消息卡片回传交互”只能用回调地址（附录 9.2），所以用新版卡片 |
| 出站 | 发消息：`POST /open-apis/im/v1/messages`，`receive_id_type=chat_id`。回复：`POST /open-apis/im/v1/messages/:message_id/reply`，设置 `reply_in_thread`。更新卡片的接口实施前核对 |
| 凭据 | `tenant_access_token` 从 `https://accounts.feishu.cn/oauth/v3/token` 取得（`client_credentials`、App ID、App Secret），有效期最长 2 小时，到期前刷新。旧接口已标为历史版本 |
| 限频与去重 | 接口限频为 1000 次/分钟、50 次/秒，同一群 5 QPS；按 `message_id` 去重 |

### 3.6 配置与部署

`zx-agent serve --config <文件>` 读取 TOML 配置。凭据仍只从环境变量读取：

```toml
[slack]
appTokenEnv = "SLACK_APP_TOKEN"     # xapp-，Socket Mode
botTokenEnv = "SLACK_BOT_TOKEN"     # xoxb-，发消息

# ⚑ D:5/CHINA FLAG: 飞书（须经法务 / 出口合规审批）
[feishu]
appIdEnv = "FEISHU_APP_ID"
appSecretEnv = "FEISHU_APP_SECRET"
domain = "https://open.feishu.cn"

# 下面的 feishu: 前缀同样指 ⚑ D:5/CHINA FLAG: 飞书
[access]
conversations = ["slack:C0123456789", "feishu:oc_xxx"]   # 在哪些频道、群或单聊中响应
approvers = ["slack:U0123456789", "feishu:ou_xxx"]       # 谁能批准写操作

[limits]
maxRunning = 4              # 同时运行的会话数
approvalTimeoutSecs = 600   # 审批等待时长
idleSecs = 1800             # 会话空闲多久后释放
```

| 项 | 做法 |
|-|-|
| 凭据保护 | IM 凭据所在的环境变量与模型 API key 一样处理：加入 bash 子进程要删除的变量（`BashTool::hidden_env`，`src/tools/bash.rs:26`），其值与 API key 一起交给 `RedactSecrets::new`（`src/extensions.rs:64`）脱敏 |
| Deployment | `shareProcessNamespace: true`；`command` 改为 `["zx-agent", "serve", "--config", "/etc/zx-agent/serve.toml"]`，不再经过 tini（2.3.2）；配置放在 ConfigMap 并挂载到 `/etc/zx-agent`，IM 凭据放在 Secret，以环境变量注入；单副本，沿用 `Recreate`（`deploy/kubernetes/deployment.yaml:12-13`） |
| 停止 | 收到 SIGTERM 后停止接收新消息、取消进行中的运行、关闭连接后退出。现有 CLI 已处理 SIGTERM（`src/main.rs:187`、`src/chat/signals.rs:27`） |
| 出网 | 需要访问 Slack 与 ⚑ D:5/CHINA FLAG: 飞书的服务。WebSocket 连接到哪些主机，文档没有写，实施时实测（缺口 ⑥） |
| 原有用法 | `kubectl exec` 运行 chat、run，以及 CronJob 不变 |

### 3.7 关键技术缺口

#### ① WebSocket 客户端是新依赖【已定：a)】

标准库没有 WebSocket；锁定的 reqwest 0.12.28 也没有 WebSocket 相关的 feature（其 `Cargo.toml` 中没有 websocket 字样）。

| # | 选项 | 说明 |
|-|-|-|
| a) | `tokio-tungstenite` 0.30.0，关掉默认 feature，只开 `connect` 与 `rustls-tls-webpki-roots` | 见下表 |
| b) | 自己实现 RFC 6455 握手与分帧（tokio + rustls） | 不新增依赖，但握手要用 SHA-1，分帧、掩码、ping/pong、关闭握手都要自己写和测 |

选项 a) 按 `.claude/rules/08-dependencies-msrv.md` 要求的四点核对（2026-11-01，在仓库副本中实测，未改动仓库的 `Cargo.toml`、`Cargo.lock`）：

| 项 | 结果 |
|-|-|
| 维护状况 | MIT 许可；最新版 0.30.0 发布于 2026-07-11，此前 0.29.0 发布于 2026-03-17；crates.io 下载量约 3.4 亿 |
| 传递依赖 | `Cargo.lock` 新增 11 个包：`tokio-tungstenite`、`tungstenite`、`sha1`、`digest`、`block-buffer`、`crypto-common`、`hybrid-array`、`typenum`、`const-oid`、`data-encoding`、`webpki-roots` 0.26.11。`rustls`、`tokio-rustls` 沿用现有版本。新增一处重复版本：`webpki-roots` 0.26.11 与现有的 1.0.11 |
| 默认 feature | 可以关掉；默认是 `connect` 与 `handshake` |
| MSRV | 声明 `rust-version = 1.85`；加入后 `cargo +1.88.0 check --lib` 通过 |

**已定（2026-11-01）：a)。** 用户同意新增 `tokio-tungstenite`（红线 5），记为 `docs/ROADMAP.md` 的决策 D7，实施 B、C 时引入。⚑ D:5/CHINA FLAG: 飞书帧所用的 protobuf 只有一种消息、9 个字段，手写编解码即可，不引入 `prost`。

#### ② ⚑ D:5/CHINA FLAG: 飞书长连接协议没有官方文档

只能参照 Go SDK 的源码，协议变化时可能失效。

| # | 选项 | 说明 |
|-|-|-|
| a) | 按 Go SDK 实现，固定参照的提交，用录制的帧做测试 | 平台改动时需要跟进 |
| b) | 改用 HTTP 回调 | 有官方文档，但需要公网入口 |

**建议 a)**；集群有公网入口时，可以把 b) 作为备选。

#### ③ 运行结束瞬间到达的插话

运行层在每个回合末检查插话队列（`src/agent/agent_loop.rs:545`），在下一回合开始时取出（`src/agent/agent_loop.rs:383-384`）；运行停止时，队列中的输入留给下一次运行（`src/agent/agent_loop.rs:370-371`）。插话若恰好在最后一次检查之后、运行结束之前到达，要等线程里再来一条消息才会处理。`AgentSession` 目前没有查询或取出队列的公开方法。

| # | 选项 | 说明 |
|-|-|-|
| a) | 给 `AgentSession` 增加取出未处理输入的方法；serve 在每次运行结束后调用，非空就作为下一次 prompt | 新增公开方法，不影响现有调用方 |
| b) | serve 不用插话：运行中到达的消息由 serve 排队，运行结束后作为下一次 prompt | 不改内核，但运行中的消息要等本次运行结束才被看到 |

**建议 a)**，方法名与语义在实施 A 时定稿。

#### ④ 审批人身份没有进入审计

`Interaction::confirm` 的返回值 `Approval` 只有 `AllowOnce`、`AllowSession`、`Deny`（`src/interaction.rs:25-29`），`ApprovalResolved` 事件也没有审批人字段（`src/agent/events.rs:245-252`）。IM 中的审批人可能是名单里的任何一人，需要记录是谁批准的。

| # | 选项 | 说明 |
|-|-|-|
| a) | serve 自己记录：每次决定写一行结构化日志（会话键、`tool_call_id`、审批人、结果、时间），卡片上也显示审批人 | 不改公开类型；记录不在会话文件中，靠 `tool_call_id` 与会话、事件对应 |
| b) | 在 `Approval` 或 `ApprovalResolved` 中加入审批人 | 记录进入事件流；但二者都是公开类型，`Approval` 的 JSON 格式刚在 PR #25 中确定（`src/interaction.rs:22-24`），加字段会影响现有实现与匹配 |

**建议 a)**；需要把审批人写进会话文件时，再按 b) 单独评审。

#### 其余缺口

| # | 项 | 处理 |
|-|-|-|
| ⑤ | 只能单副本 | 会话文件单写者；⚑ D:5/CHINA FLAG: 飞书多个客户端时每个事件随机给其中一个（附录 9.2）。多副本需要外部会话存储（`SessionLog` 可替换）与分布式协调。一期接受单副本 |
| ⑥ | 出网主机清单 | 两个平台的文档都没有列出 WebSocket 主机。实施时实测，并写进部署说明，供 NetworkPolicy 使用 |
| ⑦ | 合规 | ⚑ D:5/CHINA FLAG: 飞书部分须经法务 / 出口合规审批后实施（Subject to Legal/Export Compliance approval） |

---

## 4. 接口与数据变更一览

| 用途 | 位置 | 新增 / 改动 |
|-|-|-|
| 常驻服务 | CLI `zx-agent serve --config <文件>` | 新增子命令 |
| 服务配置 | TOML：`[slack]`、`[feishu]`（⚑ D:5/CHINA FLAG: 飞书）、`[access]`、`[limits]` | 新增（3.6） |
| IM 扩展点 | 公开模块 `im`：`ImChannel` 与相关类型 | 新增（3.1） |
| 依赖 | `tokio-tungstenite` 0.30（`docs/ROADMAP.md` 决策 D7） | 新增（缺口 ①，实施 B、C 时引入） |
| 插话 | `AgentSession` 取出未处理输入的方法 | 新增（缺口 ③，待定） |
| 会话文件 | `/data/sessions/<平台>/<聊天>/<线程>.jsonl` | 新的存放位置，格式不变 |
| 审批记录 | serve 的结构化日志 | 新增（缺口 ④）；事件格式不变 |
| 部署 | Deployment 运行 serve，`shareProcessNamespace: true`；新增配置 ConfigMap 与凭据 Secret | 改动（2.3.2、3.6） |

---

## 5. 测试验收

| 项 | 判据 | 怎么验 |
|-|-|-|
| 路由与会话 | @ 消息在线程中得到回答；同一线程追问接着同一会话；不同线程并行；超出上限时排队；名单外的聊天被拒绝；运行结束瞬间到达的插话不滞留 | 用内存实现的 `ImChannel` 驱动 serve，模型用 FauxProvider |
| 审批 | 卡片发出；审批人批准后才执行；非审批人点击无效；批准、拒绝、超时、取消四种结局都把卡片改为对应结果；审批记录含审批人 | 同上 |
| Slack | envelope 立即确认；正确解析 `app_mention`、`message.im` 与 `block_actions`；收到 `disconnect` 后重连；`chat.postMessage`、`chat.update` 的请求内容正确 | 本地假 Socket Mode 服务（WebSocket）加假 Web API |
| ⚑ D:5/CHINA FLAG: 飞书 | 帧编解码（含 ping/pong）；事件立即确认；重复投递只处理一次；token 到期前刷新；发送与回复的请求内容正确 | 本地假 endpoint 加假 WebSocket |
| 部署与安全 | PID 1 是 pause；命令读不到 serve 进程的环境，`/proc/1/environ` 中没有 key；后台命令留下的进程被回收，没有僵尸进程；IM 凭据不在 bash 环境中，读出的值被脱敏；SIGTERM 后正常退出 | 本机集群（docker-desktop） |
| 真实平台 | 在测试用 Slack 工作区和 ⚑ D:5/CHINA FLAG: 飞书租户中各跑通一次：@、回答、审批 | 需要提供测试应用的凭据；模型可以用假服务，不产生费用 |

---

## 6. 排期

| 功能模块 | 负责人 | 范围 | 依赖关系 |
|-|-|-|-|
| A. IM 接口与 serve 骨架 | 待定 | `im` 模块、路由、会话管理、审批中转、`AgentSession` 新方法（缺口 ③）；用内存实现测试 | 无新依赖，可以先做 |
| B. Slack | 待定 | Socket Mode、发消息、卡片 | 依赖 A；引入 `tokio-tungstenite`（缺口 ①，已定） |
| C. ⚑ D:5/CHINA FLAG: 飞书 | 待定 | 长连接、token、发送与回复、卡片 | 依赖 A；引入 `tokio-tungstenite`；须经合规审批 |
| D. 部署与验收 | 待定 | Deployment 改为运行 serve 并共享进程命名空间、配置与凭据、文档；真实平台验收 | 依赖 B 或 C |
| 二期：告警 Webhook | 待定 | HTTP 服务、Alertmanager 格式、结果发到 IM | 需要 HTTP 服务依赖 |

```
A ──┬──▶ B（Slack）───┬──▶ D
    └──▶ C（飞书⚑）───┘
缺口 ① 已定（2026-11-01），不再阻塞 B、C
图中“飞书⚑”即 ⚑ D:5/CHINA FLAG: 飞书。
```

---

## 7. 后续规划

- 二期：告警 Webhook，告警触发的排障结果发到 IM。
- 其他 IM 平台：只需实现 `ImChannel`。
- 多副本：外部会话存储加分布式协调。
- 在 Kubernetes 之外运行 serve 时的 PID 1 处理。
- 在 IM 中流式展示进度。

---

## 8. 评审意见

| 日期 | 评审人 | 意见 | 回复 / 处理 |
|-|-|-|-|
| | | | |

---

## 9. 附录

2026-11-01 读取，均为官方文档或官方源码原文。

### 9.1 Slack

| 主题 | 原文 | 来源 |
|-|-|-|
| Socket Mode | “Socket Mode allows your app to use the Events API and interactive features—without exposing a public HTTP Request URL.” | using-socket-mode |
| 连接 | “The URL is created at runtime by calling the `apps.connections.open` method, and it refreshes regularly.” | 同上 |
| 确认 | “Your app still needs to acknowledge receiving _each event_ so that Slack knows whether to retry.” | 同上 |
| 刷新 | “No matter what, you'll need to handle connection refreshes once every few hours.” | 同上 |
| 连接数 | “Socket Mode allows your app to maintain _up to 10_ open WebSocket connections at the same time.” | 同上 |
| 上架限制 | “Apps using Socket Mode are _not_ currently allowed in the public Slack Marketplace.” | 同上 |
| 交互 | “When you toggle Socket Mode on, you'll **only** receive events and interactive payloads over your WebSocket connections—not over HTTP.” | 同上 |
| 时限 | “Your app should respond to the event request with an HTTP 2xx within three seconds.” | events-api |
| 线程 | `thread_ts`：“Provide another message's `ts` value to make this message a reply.” | chat.postMessage |
| 限频 | “It will generally allow an app to post 1 message per second to a specific channel.” | 同上 |
| 单聊 | “Subscribe to message.im events to receive messages directed to your bot user in direct message conversations.” | app_mention、message.im |
| 权限写法 | app_mention 页写作 `app_mentions.read`，Socket Mode 的 manifest 示例写作 `app_mentions:read`；以应用 OAuth 设置中显示的为准 | 同上 |

### 9.2 ⚑ D:5/CHINA FLAG: 飞书

> ⚠ COMPLIANCE REVIEW: [该平台的资料] 仅供内部评审。

| 主题 | 原文 | 来源 |
|-|-|-|
| 长连接 | “与开放平台建立一条 WebSocket 全双工通道（你的服务器需要能够访问公网）。”“无需提供公网 IP 或域名、无需使用内网穿透工具” | handle-events |
| 回调地址 | “该请求地址必须是 IPv4 公网地址。” | 事件订阅：发送至开发者服务器 |
| 应用类型 | “长连接模式仅支持企业自建应用。” | handle-events |
| 集群模式 | “如果同一应用部署了多个客户端（client），那么只有其中随机一个客户端会收到消息。” | 同上 |
| 时限 | “长连接模式下接收到消息后，需要在 3 秒内处理完成且不抛出异常，否则会触发超时重推机制。” | 同上 |
| 去重 | “特殊情况下可能会收到重复的推送，如有幂等需求请使用 message_id 去重，不要依赖 event_id” | im.message.receive_v1 |
| 卡片回调 | “消息卡片回传交互（旧）回调不支持使用长连接接收回调订阅方式” | handle-callback |
| 限频 | “1000 次/分钟、50 次/秒”；“向同一群组发送消息的限频为群内机器人共享 5 QPS” | im/v1/message/create |
| token | “`tenant_access_token` 的有效期最长为 2 小时。”；旧接口“本接口已成为历史版本，不推荐使用。” | tenant_access_token |
| 长连接端点 | `requestURL := strings.TrimRight(c.domain, "/") + GenEndpointUri`，`GenEndpointUri = "/callback/ws/endpoint"`，请求体含 `AppID`、`AppSecret` | Go SDK `ws/client_transport.go`、`ws/const.go` |
| 帧格式 | `type Frame struct { SeqID uint64; LogID uint64; Service int32; Method int32; Headers []Header; PayloadEncoding string; PayloadType string; Payload []byte; LogIDNew string }`（protobuf 字段 1–9） | Go SDK `ws/pbbp2.pb.go` |
| 心跳 | 默认 `pingInterval: 2 * time.Minute`，以服务端下发的间隔为准 | Go SDK `ws/client.go` |
| 确认时机 | Go SDK 在处理函数返回后，才写入状态码为 `http.StatusOK` 的响应帧 | Go SDK `ws/client_message.go` |

### 9.3 Kubernetes 与 pause

| 主题 | 原文 | 来源 |
|-|-|-|
| PID 1 | “The container process no longer has PID 1.” | `kubernetes/website` 仓库 `content/en/docs/tasks/configure-pod-container/share-process-namespace.md:115`（提交 `585c1c60`） |
| 进程可见 | “Processes are visible to other containers in the pod. This includes all information visible in `/proc`, such as passwords that were passed as arguments or environment variables. These are protected only by regular Unix permissions.” | 同上，第 121–123 行 |
| 回收 | `static void sigreap(int signo) {` `while (waitpid(-1, NULL, WNOHANG) > 0)`，并以 `sigaction(SIGCHLD, …sigreap…)` 注册 | `kubernetes/kubernetes` 仓库 `build/pause/linux/pause.c:37-38`、`:59`（该文件最近一次提交为 `82ac28cc`） |

实测输出（2026-11-01，docker-desktop，Kubernetes v1.36.1；两个 Pod 的主进程均为 `sh -c '(sleep 2 &); exec sleep 3600'`，容器设置环境变量 `PROBE_SECRET`；输出为 `/proc/<pid>/stat` 的前四列）：

```
===== shared（shareProcessNamespace: true）
1 (pause) S 0
19 (sh) S 0
7 (sleep) S 0
/proc/1/environ lines with PROBE_SECRET: 0
===== isolated（未设置）
1 (sleep) S 0
12 (sleep) Z 1
13 (sh) S 0
/proc/1/environ lines with PROBE_SECRET: 1
```

两个 Pod 中的 `sh` 都是执行检查的 `kubectl exec` 进程。

---

`serve-im-review.md` · v0.1 · 2026年11月
