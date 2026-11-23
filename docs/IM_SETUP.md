# IM 接入指南

`zx-im` 把 agent 接入 Slack 与 ⚑ D:5/CHINA FLAG: 飞书。本文按顺序列出接入一个平台要做的事：在平台上建应用，开权限与事件，把凭据交给 `zx-im`，写 `im.toml`，把群和人加进名单，以及实测中遇到过的问题。每个键的说明见 [CONFIGURATION.md](CONFIGURATION.md#imtoml)，服务的行为见 [REFERENCE.md](REFERENCE.md#im-服务)，部署到 Kubernetes 见 [deploy/kubernetes/README.md](../deploy/kubernetes/README.md#im-服务)。

⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：对话内容、命令输出与用户 ID 发往该平台；审批人可以放行在 `zx-im` 所在机器上执行的命令] Subject to Legal/Export Compliance approval.

## 总览

两个平台都只需出网，不需要公网地址。

| 步骤 | Slack | ⚑ D:5/CHINA FLAG: 飞书 |
|---|---|---|
| 建应用 | 新建 Slack 应用 | 新建企业自建应用，启用机器人能力 |
| 连接方式 | Socket Mode | 长连接 |
| 权限与事件 | Bot Token Scopes、Event Subscriptions | 权限管理、事件与回调 |
| 生效 | 安装到工作区；改权限后重新安装 | 创建版本并发布；改设置后再发布 |
| 凭据（环境变量） | `SLACK_APP_TOKEN`、`SLACK_BOT_TOKEN` | `FEISHU_APP_ID`、`FEISHU_APP_SECRET` |
| 名单里的 ID | `slack:C…`（频道）、`slack:D…`（私聊）、`slack:U…`（用户） | `feishu:oc_…`（群或私聊）、`feishu:ou_…`（用户） |

## Slack

1. **建应用。** 在 <https://api.slack.com/apps> 新建应用（From scratch），选定工作区。
2. **Socket Mode。** 打开 Socket Mode，生成带 `connections:write` 的 app-level token（`xapp-` 开头），即 `SLACK_APP_TOKEN`。
3. **Interactivity。** 打开 Interactivity，审批卡片的按钮要用。
4. **权限。** 在 OAuth & Permissions 的 Bot Token Scopes 中添加：

   | 权限 | 必需 | 用途 |
   |---|---|---|
   | `app_mentions:read` | 是 | 收到 @ 机器人的消息（以设置页显示的名称为准） |
   | `im:history` | 是 | 收到私聊 |
   | `chat:write` | 是 | 发消息、改消息 |
   | `channels:history`、`groups:history` | 否 | 公开、私有频道：线程里不 @ 的回复也送来；读机器人第一次被 @ 的线程，以及频道里上次 @ 它之后的消息 |
   | `users:read` | 否 | 模型看到发言人的名字，没有时看到用户 ID |
   | `reactions:write` | 否 | 插话得到 👀 回执 |
   | `files:read` | 否 | 模型看到消息里的图片 |
   | `files:write` | 否 | 发文件：太长的回答作为文件发出，agent 用工具 `send_file` 发报告或图片；没有时长回答照旧分段发文字，工具报错 |

5. **事件。** 在 Event Subscriptions 中打开事件，订阅 bot 事件 `app_mention` 与 `message.im`；要线程里不 @ 的回复，再订阅 `message.channels`、`message.groups`（配合上表的 `channels:history`、`groups:history`）。
6. **私聊。** 在 App Home 的 Show Tabs 中打开 Messages Tab：Slack 文档说明，用户经这个页签与应用对话，它需要 `chat:write`、`im:history` 与事件 `message.im`。
7. **安装。** 在 Install App 中安装到工作区，复制 Bot User OAuth Token（`xoxb-` 开头），即 `SLACK_BOT_TOKEN`。以后增减权限，要重新安装才生效；实测中重新安装后 bot token 没有变。
8. **加进频道。** 在要用的频道里执行 `/invite @应用名`。据 Slack 文档，应用不在频道里时收不到 @ 它的事件，新应用也不能在没加入的公开频道发消息。

## ⚑ D:5/CHINA FLAG: 飞书

1. **建应用。** 在开发者后台（<https://open.feishu.cn/app>）创建企业自建应用，启用机器人能力。
2. **权限。** 在权限管理中开通：

   | 权限 | 必需 | 用途 |
   |---|---|---|
   | `im:message` | 是 | 发送与编辑消息、更新卡片、下载消息里的图片（实测只开这一项就够） |
   | `im:message.group_at_msg:readonly` | 是 | 收到群里 @ 机器人的消息 |
   | `im:message.p2p_msg:readonly` | 是 | 收到私聊 |
   | `im:message.group_msg` | 否 | 话题里不 @ 的回复也送来；读机器人第一次被 @ 的话题，以及群里上次 @ 它之后的消息（官方文档写明以应用身份读群消息要这项权限）。开通后应用能读到所在群的全部消息 |
   | `im:chat.members:read` | 否 | `zx-im` 不用；只为管理员列出群成员的 open_id（见下文“取得 ID”） |
   | `im:resource` 或 `im:resource:upload` | 否 | 发文件（单个至多 30 MB）：太长的回答作为文件发出，agent 用工具 `send_file` 发报告或图片；没有时长回答照旧分段发文字，工具报错 |

   ⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：`im:message.group_msg` 读取群内全部消息与历史消息] Subject to Legal/Export Compliance approval.

   ⚠ COMPLIANCE REVIEW: [⚑ D:5/CHINA FLAG: 飞书：`im:resource` 向该平台上传 agent 产出的文件（数据）] Subject to Legal/Export Compliance approval.
3. **凭据。** 在凭证与基础信息中取得 App ID 与 App Secret，即 `FEISHU_APP_ID` 与 `FEISHU_APP_SECRET`。
4. **先启动 `zx-im`。** 按下文“im.toml 与启动”写好 `[feishu]` 段并启动。官方文档要求保存长连接设置时长连接在线，所以这一步要在下一步之前。
5. **事件与回调。** 在事件与回调中：
   - 事件的订阅方式选“使用长连接接收事件”，添加事件 `im.message.receive_v1`（接收消息）。
   - 回调的订阅方式同样选长连接，添加回调 `card.action.trigger`（卡片回传交互），不要选旧版的“消息卡片回传交互（旧）”。
6. **发布。** 在版本管理与发布中创建版本，可用范围要包含会用机器人的人，然后发布。以后改权限、事件或回调，都要再创建版本并发布才生效；实测中只开了权限、没加事件时，消息到不了 `zx-im`。
7. **加进群。** 在群设置中把机器人加进要用的群。

## im.toml 与启动

最小的 `im.toml`（两个平台可以同时配置；名单中的 ID 见下一节）：

```toml
[slack]
# [feishu]

[access]
conversations = ["slack:C0123456789"]   # 只在这些频道、群或私聊中回答，其他地方只回 replies.unauthorized
approvers = ["slack:U0123456789"]       # 能批准或拒绝的人；为空时审批只能超时

[sessions]
dir = "/data/sessions"
```

第一次接入还不知道 ID 时，可以先写 `conversations = []` 启动：此时每条要机器人回答的消息都被拒绝，日志里正好留下要填的 ID（见下一节），填好后让它重读名单（见本节末尾）。

凭据只放在环境变量（Kubernetes 中为 Secret）里，不写进 `im.toml`；`zx-im` 执行的命令看不到存放密钥的变量（⚑ D:5/CHINA FLAG: 飞书的 App ID 不是密钥，不隐藏）。启动：

```bash
export SLACK_APP_TOKEN=xapp-...   # 使用 ⚑ D:5/CHINA FLAG: 飞书时改为 FEISHU_APP_ID 与 FEISHU_APP_SECRET
export SLACK_BOT_TOKEN=xoxb-...
zx-im --config im.toml --model <model> --max-model-requests 100 --timeout-secs 3600
```

改了 `[access]`（`conversations` 与 `approvers`）不必重启：向 `zx-im` 进程发 SIGHUP（如 `kill -HUP "$(pidof zx-im)"`），它重读 `im.toml`，日志记一条 `reloaded`；文件有错时保留原名单，日志记一条 `failed` 写明原因。其他段（含 `[[chats]]`）的改动要重启才生效。Kubernetes 中的做法见 [deploy/kubernetes/README.md](../deploy/kubernetes/README.md#im-服务)。

## 取得 ID

名单写的是“平台:ID”。最省事的做法是从日志取：不在名单里的聊天发来要机器人回答的消息，会收到 `replies.unauthorized` 的回复（默认为 “Not authorized: …”，不调用模型），日志的 `unauthorized` 行写明平台、聊天（`conversation`）与发送人（`user`）。

- **加一个群或频道：** 请人在里面 @ 机器人一次，取 `conversation`。
- **加一个审批人：** 请这个人私聊机器人一句，取 `user`；要让这个人私聊使用机器人，同一行的 `conversation` 就是私聊的 ID，也加进 `conversations`。日志行没有姓名，一次只请一个人发，按时间对上人。
- **审批人要在机器人发卡片的群里**，才能看到并点击卡片；私聊里的卡片只有提问的人看得到。

⚑ D:5/CHINA FLAG: 飞书的 open_id（`ou_…`）按应用区分，同一个人在不同应用里的 open_id 不同，要用本应用的。日志之外还有三种取法：

- **API 调试台：** 在 <https://open.feishu.cn/api-explorer> 选本应用，打开“发送消息”接口，`user_id_type` 选 `open_id`，用“快速复制 open_id”复制成员的 ID。不需要额外权限，适合只加几个人。
- **列群成员：** `GET /open-apis/im/v1/chats/:chat_id/members?member_id_type=open_id` 返回成员的姓名与 open_id；需要 `im:chat.members:read` 等读群成员的权限之一，机器人要在群里。适合一次加一批。
- **按邮箱查：** `POST /open-apis/contact/v3/users/batch_get_id` 由邮箱查 open_id；需要 `contact:user.id:readonly`，并且应用的通讯录数据权限要包含这些人。官方文档写明要用用户的邮箱，企业邮箱查不到。这会读取员工通讯录数据，不建议作为常规做法。

## 验证

1. 在名单里的群或频道 @ 机器人：先出现状态消息（`replies.working`），随后换成回答。默认文案是英文，中文译文见 `deploy/kubernetes/im/im.example.toml`。
2. 私聊机器人（私聊要在名单里）。
3. 让它运行一条要审批的命令：出现审批卡片，审批人点允许的按钮（`cardLabels.allowOnce`，默认 “Allow once”）后执行。
4. 在名单外的群 @ 机器人：收到 `replies.unauthorized` 的回复，日志多一行 `unauthorized`。

## 常见问题

### Slack

| 现象 | 原因与处理 |
|---|---|
| 频道里 @ 机器人没反应 | 机器人不在频道里：`/invite @应用名` |
| 线程里不 @ 机器人的回复没有回应 | 缺事件 `message.channels`、`message.groups`，或缺权限 `channels:history`、`groups:history` |
| 模型看不到图片，日志有下载失败 | 缺 `files:read`；添加后重新安装应用 |
| 长回答分成多条文字发出，日志的 `failed` 行说发不出 `answer.md` | Slack 缺 `files:write`（错误为 `missing_scope`）、⚑ D:5/CHINA FLAG: 飞书缺 `im:resource`；添加后（Slack 要重新安装应用）就作为文件发出 |

### ⚑ D:5/CHINA FLAG: 飞书

| 现象 | 原因与处理 |
|---|---|
| @ 机器人没反应，日志里也没有收到消息 | 没有添加事件 `im.message.receive_v1`，或添加后没有发布版本 |
| 保存长连接的订阅方式失败 | `zx-im` 不在线；先启动它再保存 |
| 话题里不 @ 机器人的回复没有回应 | 没开 `im:message.group_msg` |
| 日志的 `failed` 行有错误码 230027 | 读话题或群的消息缺 `im:message.group_msg`（实测列话题消息时是这个错误码）；不开也照常回答，只是读不到之前的消息 |

### 两个平台

| 现象 | 原因与处理 |
|---|---|
| 机器人回复 “Not authorized…” | 聊天不在 `conversations` 中：从 `unauthorized` 日志行取 ID 加入，再发 SIGHUP 或重启 |
| 点审批按钮后提示只有名单里的人能批 | 点的人不在 `approvers` 中 |
| 审批卡片一直没人能点 | `approvers` 为空时审批只能超时 |

---

IM_SETUP.md · v0.3 · 2026年11月
