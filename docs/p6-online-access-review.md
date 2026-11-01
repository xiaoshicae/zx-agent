# 技术方案 · 在线排障的访问权限（P6）

| 信息 | 内容 |
|-|-|
| 版本 | `v0.1` |
| 日期 | `2026-11-01` |
| 状态 | 草稿（待评审） |
| Owner | 待指定 |
| 对应 PRD | 无（技术驱动）；对应 `docs/ROADMAP.md` P6 的最后两项 |
| Milestone | 一期：容器联网与 kubeconfig、chat 自动放行只读命令、凭据自动脱敏、Linux 上保护 agent 的环境变量、Kubernetes 部署说明<br>二期（按需）：按地址限制出网 |

---

## 1. 背景与范围

在线排障要访问 Kubernetes API、指标和日志系统。权限原则只有一句：**凭据决定 agent 能做什么，zx-agent 只决定什么时候问人。**

### 1.1 为什么做

| 诉求 | 今天的状况 |
|-|-|
| 在线排障要查询 Kubernetes、指标、日志 | 容器执行固定为 `--network none`（`src/executor/container.rs:475-476`），只能离线排障 |
| 权限要好懂、好配（2026-11-01 确认） | 命令规则有 `rules.allow`、`rules.deny`、`rules.askTools`、`rules.denyTools` 四项（`docs/PROFILES.md:55-58`），而且只做字面检查，不是安全边界（README“安全模型”） |
| chat 里只读排障不必每条都确认 | `assistant` profile 对每条 bash 命令都要求确认（`profiles/assistant.toml` 的 `askTools = ["bash"]`） |
| 无人值守不做写操作 | `troubleshoot` profile 没有命令规则（`profiles/troubleshoot.toml`），能不能写取决于命令拿到的凭据 |
| 凭据不进入模型上下文 | 模型 API key 已不进命令环境、不进容器；kubeconfig 和 ServiceAccount token 没有自动脱敏。同一用户的进程能读到另一进程的环境变量（实测 E1–E3，附录 9.2），而 Kubernetes 模式下命令与 agent 正是这种关系 |

### 1.2 本期范围

- 两种部署：带 Docker 的主机（容器执行，chat 与 run），以及 Kubernetes Job/Pod（run，本机执行）。
- 三类目标：Kubernetes API、指标查询（如 Prometheus）、日志系统（如 Loki、Elasticsearch）。
- 交付：权限模型、新增配置项、自动保护措施和验收方法。具体集群与地址由部署方配置（评审范围已确认）。

### 1.3 非本期范围

| 不做 | 原因 |
|-|-|
| 按地址限制出网（出网代理） | 2026-11-01 确认：容器联网用普通网络，网络限制交给部署环境。需要时作为二期，实测记录见附录 9.4 |
| 云厂商 API | 不在目标范围内 |
| 无人值守的写操作 | run 使用只读凭据，写请求由 API server 拒绝 |
| 按 URL 路径或 HTTP 方法控制 | 需要解密 TLS |
| OS 级沙箱 | 沿用技术方案 5.9 的划分，后续作为单独的执行器 |

### 1.4 相关链接

| 资料 | 链接 |
|-|-|
| 路线图 P6 | `docs/ROADMAP.md` |
| 技术方案 5.9 执行环境与安全 | `docs/zx-agent-general-kernel-proposal.html` |
| 现有安全模型与容器执行 | `README.md` 的“安全模型”“容器执行” |
| RBAC 预置角色 | https://kubernetes.io/docs/reference/access-authn-authz/rbac/ |
| NetworkPolicy | https://kubernetes.io/docs/concepts/services-networking/network-policies/ |
| ptrace 访问检查 | https://man7.org/linux/man-pages/man2/ptrace.2.html |

---

## 2. 整体设计

### 2.1 权限模型

| | chat（有人） | run（无人值守） |
|-|-|-|
| **凭据**：能做什么 | 用户提供。只读排障用 `view`；要做写操作时用有写权限的凭据，如自己的 kubeconfig | 只读：Kubernetes 预置的 `view` 角色 |
| **命令**：什么时候问人 | 只读命令直接执行，其余逐条确认（3.1） | 不变：除危险模式和 `rules.deny` 外都执行；写请求由 API server 按只读凭据拒绝 |
| **网络**：能连哪里 | 本机执行与 Kubernetes Pod：由部署环境决定（不变）。容器执行：默认无网络，`network = "bridge"` 后用普通网络（3.4） | 同左 |

`view` 是 Kubernetes 预置的角色，不用自己写 Role。官方文档：它 “Allows read-only access to see most objects in a namespace”，并且 “does not allow viewing Secrets”。在 Kubernetes 仓库的角色测试数据里，它包含 `pods`、`pods/log`、`events`、`deployments` 的 get、list、watch，不包含 `secrets`（附录 9.1）。

### 2.2 建成之后怎么用

**带 Docker 的主机**：profile 里写这几行。

```toml
execution = "container"

[container]
image = "<带 bash 与 kubectl 的镜像>"
network = "bridge"
kubeconfig = "<只读 kubeconfig 的绝对路径>"
```

zx-agent 把 kubeconfig 只读挂载进容器，为命令设置 `KUBECONFIG`，并把文件中的 token 登记为脱敏对象。chat 中 `kubectl get pods -A | grep -v Running` 直接执行，`kubectl rollout restart deploy/api` 先请求确认。

**Kubernetes Job**：

1. 给 Job 的 ServiceAccount 绑定 `view`。要排查所有命名空间时改用 `kubectl create clusterrolebinding`，参数相同、不带 `--namespace`。两条命令都用 `--dry-run=client` 核对过生成的对象（附录 9.3，K2）。
   ```sh
   kubectl create rolebinding zx-agent-view --clusterrole=view \
     --serviceaccount=<SA 所在命名空间>:<SA 名称> --namespace=<被排查的命名空间>
   ```
2. API key 用 Secret 引用（`valueFrom.secretKeyRef`）注入，不写成 Job 清单里的字面值：`view` 能读取 Pod 定义，字面值会被命令读到。
3. 容器入口直接运行 zx-agent（exec 形式），不让持有 API key 的 shell 作为父进程留在容器里（3.3）。
4. Pod 不添加 `SYS_PTRACE` 能力（3.3）。
5. 镜像：发布产物的 Linux 版本只有 `x86_64-unknown-linux-gnu`（`.github/workflows/release.yml:67`），所以需要 x86_64、基于 glibc、带 bash 与 kubectl 的镜像。

### 2.3 方案选型

#### 2.3.1 写保护靠什么：采用凭据

| 对比项 | 方案一：凭据（采用） | 方案二：命令规则 |
|-|-|-|
| 做法 | 无人值守用只读凭据，写请求由 API server 拒绝；chat 的确认只决定什么时候问人 | profile 列出允许、需确认、拒绝的命令前缀，并要求先配白名单 |
| 是否强制 | 是，由 API server 检查 | 否，只做字面匹配（`src/policy.rs:30`：“A lexical match, not a shell parser or an execution sandbox.”） |
| 覆盖范围 | 凭据能访问的所有接口，包括 `curl` 发出的请求 | 只覆盖列出的命令写法 |
| 用户要理解的 | 一个只读账号 | 三张命令表，以及它们的优先级和管道规则 |

**结论：采用方案一。** 核心原因是只有它真正强制执行，而且用户要理解的概念最少；run 的命令规则保持现状，不新增限制。

⚠ **前提会变**：方案一假设 run 拿到的是只读凭据。部署方给了有写权限的凭据，run 就能写；zx-agent 不检查凭据的权限范围。

#### 2.3.2 容器怎样联网：采用普通网络（2026-11-01 确认）

| 对比项 | 普通网络（采用） | 只通列出的地址（二期备选） |
|-|-|-|
| 用户配置 | `network = "bridge"` | 列出服务地址 |
| 能否挡住数据外发 | 不能，交给部署环境 | 能（附录 9.4，N4） |
| 实现成本 | 一个配置项 | 出网代理边车；发布流程增加 Linux 静态构建 |

---

## 3. 详细设计

### 3.1 chat：只读命令自动放行

1. **配置**：profile 新增 `rules.autoApprove`，CLI 新增 `--auto-approve-cmd`（可重复）。`assistant` profile 内置一份清单（第 3 点）。
2. **判断**：同时满足下表四个条件才自动放行，否则按原有方式请求确认。拒绝规则仍然优先。

| 条件 | 说明 |
|-|-|
| 管道的每一段都以清单中的前缀开头 | 沿用命令白名单的检查：合并空白、转小写后逐段比较（`src/policy.rs:169-177`、`src/policy.rs:231-232`） |
| 不含 shell 语法 | 沿用白名单禁止的 `;`、`&`、`` ` ``、`$(`、`>`、`<` 和换行 |
| 不含 `secret` | 读取 Secret 要确认 |
| 不含 URL（`://`）和 kubectl 的连接参数 | 连接参数指 `--server`、`--kubeconfig`、`--token`、`--insecure-skip-tls-verify`，以及含字母 `s` 的单横线短参数（覆盖 `-s` 的各种写法）。实测 `--server` 指向其他 HTTPS 地址时，kubectl 把 kubeconfig 中的 token 发给了该地址（附录 9.3，K1） |

3. **内置清单**：`kubectl get`、`kubectl describe`、`kubectl logs`、`kubectl top`、`kubectl events`、`kubectl explain`、`kubectl api-resources`、`kubectl version`，以及 `grep`、`head`、`tail`、`wc`、`jq`。入选标准：不能执行其他程序、不能写文件。因此不收录：
   - `awk`：`system()` 能执行命令。
   - `sed`：GNU sed 的 `e` 命令能执行命令，`w` 命令能写文件（附录 9.3，K3）。
   - `sort`：`--compress-program` 会运行指定程序，`-o` 能写文件（K3）。
   - `uniq`：第二个参数是输出文件（K3）。
   - `find`：`-exec`、`-delete`。
   - `xargs`：运行其他命令。
   - `tee`：写文件。
   - `curl`：能发送任意请求。
4. **实现位置**：`RulePolicy` 现在按 Deny → Ask → Allow 的顺序判断（`src/policy.rs:67-68`）。自动放行放在 Deny 之后、Ask 之前。
5. **影响范围**：只作用于需要确认的调用。run 本来就不对 bash 要求确认（内置 profile 的规则从空值开始，`src/profile/mod.rs:165`；`troubleshoot` 也没有设置），所以不受影响。
6. **与 `rules.allow` 的区别**：`allow` 是“只能运行这些”，清单外的命令被拒绝；`autoApprove` 是“这些不用问”，清单外的命令仍可确认后执行。

### 3.2 凭据

| 部署 | 提供方式 | zx-agent 自动做的 |
|-|-|-|
| 容器执行 | `[container] kubeconfig` | 只读挂载在同一路径；为命令设置 `KUBECONFIG`；把文件中的 token 等字面值登记脱敏（重新读取的时机见缺口 ①） |
| Kubernetes Job/Pod | ServiceAccount 绑定 `view`（2.2） | 读取 ServiceAccount 的 token 文件并登记脱敏（同上） |
| 本机执行 | 命令使用当前环境中的 kubeconfig（不变） | 不读取、不登记（缺口 ①） |

chat 使用有写权限的凭据时，自动放行的命令能读到该凭据可读的其他数据，如 ConfigMap、Pod 定义和日志。这些内容会进入模型上下文，并发送给模型服务方。

### 3.3 Linux 上保护 agent 的环境变量

Kubernetes 模式下，命令与 agent 在同一容器中以同一用户运行。实测（单次观测，附录 9.2）：

- 同一用户的进程能读到另一进程的 `/proc/<pid>/environ`（E1–E3）。
- 目标进程调用 `prctl(PR_SET_DUMPABLE, 0)` 后，读取返回 Permission denied（E1–E3）。
- 读取方带 `CAP_SYS_PTRACE` 时，仍能读到（E4）。

与手册一致：`/proc/<pid>/environ` 的读取受 ptrace 访问检查控制（proc_pid_environ(5)）；目标进程不可 dump、且调用方没有 `CAP_SYS_PTRACE` 时拒绝访问（ptrace(2) “Ptrace access mode checking”第 4 步）。

设计：

1. Linux 上，agent 启动时对自身调用 `prctl(PR_SET_DUMPABLE, 0)`。`libc` 已是 Unix 依赖（`Cargo.toml:34-35`），不新增依赖；需要一处 `unsafe` 调用，已有先例（`src/executor/local.rs:29`）。
2. 副作用：这个属性同时决定进程是否产生 core dump、能否被 `PTRACE_ATTACH`（PR_SET_DUMPABLE(2const)）。调试 agent 需要 `CAP_SYS_PTRACE`。
3. 部署要求：Pod 不添加 `SYS_PTRACE`；入口直接运行 zx-agent。其他持有 API key 的进程（例如作为父进程的 shell）不受这项保护。
4. 本机执行时，启动 agent 的 shell 通常也持有 key，保护不完整；README“安全模型”中的说明不变。

### 3.4 容器联网

1. **配置**：`[container] network` / `--container-network`，默认 `none`。`bridge` 是 Docker 的默认网络；也可以填部署方预先建好的 Docker 网络名，例如带出网限制的网络。不接受 `host` 和 `container:<名字>`：它们让命令共享宿主机或其他容器的网络。
2. **保持不变**：只读根文件系统、去掉全部 capability、非 root、进程数与内存上限、API key 不进容器（README“容器执行”）。
3. **选择普通网络所接受的风险**：
   - 模型如果被日志等内容诱导，可能把读到的数据发往外部，包括凭据本身。run 不限制命令，可以直接读出 token 再发出去。
   - 命令能访问宿主机能访问的地址。实测在 Docker Desktop 上，默认网络中的容器能访问宿主机 `127.0.0.1` 上的服务（附录 9.4，N3 对照）；原生 Linux 和云主机元数据服务的情况没有验证。
4. **减轻方式**：凭据只读、短期；在部署环境限制出网（防火墙，或第 1 点的预建网络）；需要更强的保证时做二期的地址限制。Kubernetes 中可用 NetworkPolicy，官方文档提醒：“A default deny-all egress policy also blocks DNS traffic”；集群还必须使用支持 NetworkPolicy 的网络插件。

### 3.5 关键技术缺口

以下各项都不阻塞实施。

#### ① 脱敏覆盖不到运行时才出现的 token

`RedactSecrets` 只过滤登记过的完整字面值（README“安全模型”）。以下三种情况不在登记范围内：

- kubeconfig 的 exec 插件在运行时才取得 token，文件里没有。
- ServiceAccount 的 token 文件可能被 kubelet 更新（未验证）。
- 本机执行时，zx-agent 不读取用户的 kubeconfig。

| # | 选项 | 说明 |
|-|-|-|
| a) | 只登记启动时文件中的静态值 | 最简单 |
| b) | 在 a) 的基础上，每次脱敏前重新读取已登记的文件 | 覆盖文件被更新的情况；文件很小，成本低 |
| c) | 调用 exec 插件取得 token 再登记 | 每次运行多一次凭据请求 |

**建议 b)**，同时使用短期 token 限制泄露后的影响。

#### ② 自动放行仍是字面检查

清单中的程序可能有没想到的能力，也可能出现新的绕过写法（`src/policy.rs:30`）。被绕过时，一条命令会不经确认就执行；chat 使用有写权限的凭据时，这条命令可能是写操作。

| # | 选项 | 说明 |
|-|-|-|
| a) | 清单保持短小，按入选标准逐个评审，每条排除规则都有测试 | 好用；依赖评审质量 |
| b) | 只放行 kubectl 的只读子命令，不放行文本工具 | 更保守；`kubectl … \| grep` 也要确认，体验变差 |

**建议 a)**；风险高的集群，chat 也用 `view` 凭据。

#### ③ 容器内能否解析内部域名

容器使用 Docker 的 DNS 配置。公司内部域名、VPN 后的地址能否在容器里解析，没有验证。**建议**验收时在目标环境实测；不行时再评估是否增加 DNS 配置项。

#### ④ Kubernetes 模式未在真实集群验证

以下三项都没有实测：

- Pod 内的 kubectl 是否自动使用 ServiceAccount。
- 实际集群中 `view` 的权限：聚合角色可能与测试数据不同。
- `prctl` 在 Pod 内的效果。

**建议**列入第 5 节的验收项，由部署方在测试集群上验证。

---

## 4. 接口与数据变更一览

| 用途 | 位置 | 新增 / 改动 |
|-|-|-|
| 容器联网 | profile `[container] network`；CLI `--container-network` | 新增；默认 `none` |
| kubeconfig | profile `[container] kubeconfig`；CLI `--container-kubeconfig` | 新增；只读挂载并设置 `KUBECONFIG` |
| 自动放行 | profile `rules.autoApprove`；CLI `--auto-approve-cmd` | 新增；`assistant` profile 内置清单 |
| 现有命令与工具规则 | `rules.allow`、`rules.deny`、`rules.askTools`、`rules.denyTools` | 无改动 |
| 环境变量保护、凭据登记 | — | 自动进行，无配置项 |

会话文件与事件格式：无变更。自动放行与人工确认的结论目前都不发事件；ROADMAP“P7 前置”草案的 B3 计划增加 `ToolDecision` 事件，实施时自动放行应带上命中的清单前缀作为理由。

评审通过后同步更新的文档：README 的“安全模型”与“容器执行”、`docs/PROFILES.md`、ROADMAP P6、技术方案 5.9。5.9 现在写的是查询远端 API“需要受限网络访问和相应凭据”，要改为网络限制由部署环境提供。

---

## 5. 测试验收

| 项 | 判据 | 怎么验 |
|-|-|-|
| 离线行为不变 | 未配置 `network` 时仍为 `--network none` | `tests/container.rs` 现有测试 |
| 容器联网 | `network = "bridge"` 时能访问外部地址；`host`、`container:<名字>` 报配置错误 | 真实 Docker 测试，CI 的 Linux 任务与本机都运行 |
| kubeconfig | 容器内 kubectl 使用挂载的文件；向挂载点写入失败；文件中的 token 不出现在会话与模型请求中 | 真实 Docker 测试与集成测试 |
| 自动放行 | 清单内命令不经确认直接执行；以下命令都要求确认：含 `secret`、URL、连接参数或 shell 语法的命令，带多余空白的变体，管道后段不在清单中的命令；run 的行为不变 | 单元测试与 chat 命令行测试 |
| 环境变量保护 | Linux 上同一用户的命令读取 agent 的 `/proc/<pid>/environ` 失败 | CI 的 Linux 任务上的集成测试 |
| Kubernetes 模式 | 绑定 `view` 后 `kubectl get`、`kubectl logs` 可用，`kubectl get secret` 和写操作被拒绝；ServiceAccount 的 token 不出现在会话中 | 部署方在测试集群人工验收，记录到 ROADMAP |
| 真实目标 | 部署方配置好地址后，跑通一次 `kubectl get`、一次指标查询、一次日志查询 | 部署方人工验收 |

---

## 6. 排期

| 功能模块 | 负责人 | 范围 | 依赖关系 |
|-|-|-|-|
| 自动放行 | 待定 | `rules.autoApprove`、`--auto-approve-cmd`、内置清单 | 独立 |
| 容器联网与 kubeconfig | 待定 | `network`、`kubeconfig`、`KUBECONFIG`、凭据登记 | 独立 |
| 环境变量保护 | 待定 | `prctl`、ServiceAccount token 登记 | 独立 |
| Kubernetes 部署说明 | 待定 | README 中的部署步骤（2.2） | 依赖以上三项 |
| 真实环境验收 | 部署方 | 第 5 节最后两项 | 依赖以上全部 |

前三项互相独立，可以拆成三个 PR 依次合入。

---

## 7. 后续规划

- 二期：按地址限制出网。附录 9.4 的出网代理边车方案在 Docker Desktop 上实测可行（单次观测）。
- 本机执行时登记用户 kubeconfig 中的 token（缺口 ①）。
- 云厂商 API 等新的目标类别，按本稿格式另行评审。

---

## 8. 评审意见

| 日期 | 评审人 | 意见 | 回复 / 处理 |
|-|-|-|-|
| | | | |

---

## 9. 附录

### 9.1 `view` 角色的依据

- RBAC 文档 “User-facing roles”：“Allows read-only access to see most objects in a namespace. It does not allow viewing roles or role bindings.” 以及 “This role does not allow viewing Secrets, since reading the contents of Secrets enables access to ServiceAccount credentials in the namespace, which would allow API access as any ServiceAccount in the namespace (a form of privilege escalation).”
- Kubernetes 仓库 `plugin/pkg/auth/authorizer/rbac/bootstrappolicy/testdata/cluster-roles.yaml`（`master` 分支，2026-11-01 读取）：`view` 通过 `aggregationRule` 汇集带 `rbac.authorization.k8s.io/aggregate-to-view: "true"` 标签的角色；其中 `system:aggregate-to-view` 对 `pods`、`pods/log`、`events`、`deployments` 授予 get、list、watch，所有规则都不含 `secrets`。这是测试数据；实际集群中可能有其他带同一标签的角色被聚合进 `view`。

### 9.2 环境变量读取实测（2026-11-01，Docker Desktop 29.9.2，arm64，`python:3.12-alpine`，单次观测）

同一容器内用 shell 启动两个带 `ZX_PROBE_SECRET` 环境变量的 Python 进程，其中一个先调用 `prctl(PR_SET_DUMPABLE, 0)`；再由同一用户的另一个进程读取它们的 `/proc/<pid>/environ`。

| # | 运行身份 | 普通进程 | 设置为不可 dump 的进程 |
|-|-|-|-|
| E1 | root，Docker 默认 capability | 能读到 | Permission denied |
| E2 | uid 1000，Docker 默认 capability | 能读到 | Permission denied |
| E3 | uid 1000，`--cap-drop ALL`、`no-new-privileges` | 能读到 | Permission denied |
| E4（对照） | root，`--cap-add SYS_PTRACE` | 能读到 | 能读到 |

### 9.3 命令行为实测（2026-11-01，单次观测）

| # | 实验 | 结果 |
|-|-|-|
| K1 | 本机 kubectl v1.36.1 使用一份假 kubeconfig（token 为假值），执行 `kubectl get pods --server=<本机监听地址>` | `http://` 地址：请求不带 `Authorization`。`https://` 地址加 `--insecure-skip-tls-verify=true`：请求带 `Authorization: Bearer <kubeconfig 中的 token>`。不加该参数时，自签证书被拒绝 |
| K2 | `kubectl create rolebinding …` 与 `kubectl create clusterrolebinding …`，均加 `--dry-run=client -o yaml` | 分别生成引用 ClusterRole `view`、主体为指定 ServiceAccount 的 RoleBinding 与 ClusterRoleBinding |
| K3 | `debian:bookworm-slim` 中的 GNU sed、sort、uniq | `sed "e echo …"` 执行了命令；`sed "w /tmp/…"` 写入了文件；`sort --help` 列出 `--compress-program=PROG`；`uniq --help` 的用法为 `[INPUT [OUTPUT]]` |

### 9.4 出网限制实测（2026-11-01，Docker Desktop 29.9.2，单次观测；供二期参考）

工作镜像为一个 Ubuntu 系镜像；代理为实验用的 Python CONNECT 代理，白名单只有 `example.com:443`。

| # | 实验 | 结果 |
|-|-|-|
| N1 | 只连 `--internal` 网络的容器直接访问 `https://example.com` | 失败：域名解析失败 |
| N2 | 默认网络的容器访问 `https://example.com`（对照） | 200 |
| N3 | 只连内部网络的容器访问宿主机 `127.0.0.1:18765` 上的服务 | `host.docker.internal` 无法解析；经网关 `172.19.0.1` 被拒绝连接。对照：默认网络经 `host.docker.internal` 访问，200 |
| N4 | 代理边车同时连默认网络与内部网络，工作容器只连内部网络 | 经代理访问 `example.com` 200；访问未列出的 `www.wikipedia.org` 得到 403；不经代理直连失败；代理日志依次记录 `ALLOW example.com:443` 与 `DENY CONNECT www.wikipedia.org:443` |

---

`p6-online-access-review.md` · v0.1 · 2026年11月
