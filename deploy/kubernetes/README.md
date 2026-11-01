# 在 Kubernetes 中常驻部署 zx-agent

部署后可以这样使用：

- 运维人员用 `kubectl exec` 进入常驻 Pod，用 chat 排障。只读命令直接执行，其余命令逐条确认（见仓库 README 的“安全模型”）。
- CronJob 定时巡检，使用只读账号，结果写进 Pod 日志。
- kubectl 在 Pod 内运行，凭据来自 ServiceAccount，不需要另外提供 kubeconfig。

告警 Webhook、IM 机器人等触发方式需要服务模式，尚未实现，见 `docs/ROADMAP.md` P6 的“部署方向”。

## 目录内容

| 文件 | 内容 |
|-|-|
| `Dockerfile` | 镜像：zx-agent，以及 bash、kubectl、curl、jq、tini；以 10001 号非 root 用户运行 |
| `kustomization.yaml` | 命名空间、清单列表、镜像地址 |
| `serviceaccounts.yaml` | 两个账号：常驻 Pod 用 `zx-agent`，定时巡检用 `zx-agent-inspect` |
| `rbac.yaml` | 两个账号都绑定 Kubernetes 预置的只读角色 `view`（不含 Secret） |
| `operator.yaml` | 可选：常驻 Pod 的写权限，默认不启用 |
| `pvc.yaml` | 会话文件的存储卷，挂载在 `/data` |
| `deployment.yaml` | 常驻 Pod |
| `cronjob.yaml` | 定时巡检，默认每小时一次 |

## 部署

1. 在仓库根目录构建镜像，并推送到集群能拉取的镜像仓库：

   ```sh
   docker build -f deploy/kubernetes/Dockerfile -t <镜像仓库>/zx-agent:<标签> .
   docker push <镜像仓库>/zx-agent:<标签>
   ```

   然后把 `kustomization.yaml` 中 `images` 的 `newName`、`newTag` 改成这个镜像。每次构建都要用新标签，例如版本号或提交号。标签不是 `latest` 时，拉取策略默认是 `IfNotPresent`，重复使用同一标签，节点会继续用已缓存的旧镜像。验证时实际遇到过这种情况：Pod 内二进制的校验和与新镜像不同。

2. 创建命名空间和模型配置。API key 从文件读入，避免它留在 shell 历史里：

   ```sh
   kubectl create namespace zx-agent
   kubectl -n zx-agent create configmap zx-agent-model \
     --from-literal=model=<模型 ID> --from-literal=base-url=<OpenAI 兼容服务的地址>
   kubectl -n zx-agent create secret generic zx-agent-model --from-file=api-key=<存放 API key 的文件>
   ```

   `base-url` 不含 `/chat/completions`，例如 `https://api.openai.com/v1`。

3. 应用清单：

   ```sh
   kubectl apply -k deploy/kubernetes
   ```

   要换命名空间，改 `kustomization.yaml` 的 `namespace`，ClusterRoleBinding 中的账号会随之改变。`rbac.yaml` 与 `operator.yaml` 依赖 kustomize 填入命名空间，只能用 `apply -k` 应用。

4. 需要在 chat 中执行写操作时，取消 `kustomization.yaml` 中 `operator.yaml` 一行的注释，再执行一次第 3 步。

## 使用

对话（`-it` 分配终端；会话文件保存在存储卷上，下次用同一路径可以接着聊）：

```sh
kubectl -n zx-agent exec -it deploy/zx-agent -- zx-agent chat --session /data/<名字>.jsonl
```

同一个会话文件同一时间只能被一个 chat 或 run 打开。另一个人或任务再打开它时，会报告 `is in use` 并以退出码 2 结束。所以多人同时使用时，各自用不同的文件名。这项限制依赖文件系统的 `flock`：文件系统不支持时，打开会话会直接失败，不会退回到不加锁的写入（见 `docs/ROADMAP.md` 中 A1 的实施记录）。本次只在 ext4 的 local-path 卷上验证过。

单次任务：

```sh
kubectl -n zx-agent exec deploy/zx-agent -- zx-agent run --session /data/<名字>.jsonl "<任务>"
```

定时巡检：任务描述和执行时间在 `cronjob.yaml` 的 `args` 与 `schedule` 中修改。立即试跑一次，并查看结果：

```sh
kubectl -n zx-agent create job --from=cronjob/zx-agent-inspect inspect-now
kubectl -n zx-agent logs job/inspect-now
```

退出码的含义见仓库 README 的“run 退出码”一节。

## 权限

| 账号 | 用于 | 权限 |
|-|-|-|
| `zx-agent` | 常驻 Pod：chat 与单次任务 | `view`；启用 `operator.yaml` 后，另有下表的写权限 |
| `zx-agent-inspect` | 定时巡检 | `view` |

`operator.yaml` 只开放以下写操作。每一项都在验证集群上实测过，见“验证记录”：

| 命令 | 需要的权限 |
|-|-|
| `kubectl rollout restart` | `apps` 组 `deployments`、`statefulsets`、`daemonsets` 的 `patch` |
| `kubectl scale` | `apps` 组 `deployments/scale`、`statefulsets/scale` 的 `patch` |
| `kubectl delete pod` | `pods` 的 `delete` |

在 chat 中，这些命令不在自动放行清单里，每次执行前都要人工确认。定时巡检的账号没有写权限。

## 安全说明

- **常驻 Pod 中的 API key**：Pod 的主进程是 tini 和 sleep，它们的环境变量里有 API key。同一用户的命令可以从 `/proc/1/environ` 读到它。
  - 读到的 key 会被脱敏，不会进入模型上下文。
  - 被诱导的命令仍可能把 key 发往外部。在 chat 中，这类命令需要人工确认。
  - 能 `kubectl exec` 进这个 Pod 的人，本来就能读到 key。
  - 服务模式作为主进程之后，可以去掉这条限制。
- **定时巡检**：Pod 的主进程就是 zx-agent。它在 Linux 上把自身设为不可 dump，命令读不到它的环境。
- 不要给容器添加 `SYS_PTRACE` 能力，否则命令可以读取 agent 的环境变量。
- 会话文件在 `/data` 中。命令与 agent 以同一用户运行，能够修改这些文件，这是本机执行的限制。
- Pod 能访问哪些网络地址，由集群决定。需要限制时使用 NetworkPolicy。注意默认拒绝出网也会挡住 DNS，需要单独放行集群 DNS。

## 清理

```sh
kubectl delete -k deploy/kubernetes
kubectl -n zx-agent delete configmap zx-agent-model
kubectl -n zx-agent delete secret zx-agent-model
kubectl delete namespace zx-agent
```

`kubectl delete -k` 会删除清单中的对象，包括集群级的 ClusterRoleBinding 和存储卷声明。会话文件是否随之删除，取决于 StorageClass 的回收策略：`Delete` 时删除，`Retain` 时保留在卷上。可以用 `kubectl get storageclass` 查看。

## 验证记录

**环境**（2026-11-01，结果均为单次观测）：

- 集群：本机 Docker Desktop 自带的 Kubernetes v1.36.1，2 个 arm64 节点；默认 StorageClass 为 local-path（ext4），回收策略 `Delete`。
- 代码：完整验证基于 `dd43cce`。rebase 到包含 PR #21（会话文件单写者）的 `5c20655` 之后，换用新标签的镜像复验了以下几项：会话锁、脱敏、`/proc` 保护、chat 审批、定时巡检。
- 清单：通过一个 overlay 把命名空间换成 `zx-agent-e2e`、镜像换成本地构建的 `zx-agent:e2e`，其余清单原样使用。
- 镜像：没有推送到镜像仓库，这个集群可以直接使用本地镜像。
- 模型：按脚本应答的假服务，不产生费用。

| 项 | 结果 |
|-|-|
| 镜像 | 构建成功：zx-agent 0.1.0、kubectl v1.36.1（校验和检查通过）、jq 1.6。以 uid 10001 运行，PID 1 是 tini |
| 只读账号 | Pod 内 `kubectl get pods` 成功；读 Secret、`rollout restart` 都被拒绝（Forbidden） |
| run | `kubectl exec … zx-agent run` 执行 `kubectl get pods` 并回答，退出码 0，会话文件写在 `/data` |
| chat | 通过 `-i` 管道和 `-it` 终端两种方式运行。`kubectl get` 直接执行；`rollout restart` 先请求审批，未启用 `operator.yaml` 时，批准后被拒绝 |
| 写权限 | 启用 `operator.yaml` 后，下列操作在审批或直接执行后都成功：`rollout restart`（Deployment、StatefulSet、DaemonSet）、`scale`（Deployment、StatefulSet）、`delete pod`。`kubectl -v=6` 显示 `scale` 请求的是 `…/scale` 子资源 |
| 定时巡检 | `create job --from=cronjob` 试跑完成，`kubectl logs` 显示回答。巡检账号用 `auth can-i` 检查：`list pods` 为 yes，`patch deployments`、`delete pods` 为 no；在巡检 Pod 中执行写命令被拒绝 |
| 脱敏与 `/proc` | 命令读出 ServiceAccount token 和 PID 1 环境中的 API key，工具结果里都是 `[REDACTED]`；假模型收到的 8 个请求中不含 token，也不含 API key。命令读 agent 自身的 `/proc/<pid>/environ` 时得到 `Permission denied`。巡检 Pod 的 PID 1 是 zx-agent，读它的环境同样被拒绝 |
| 会话文件锁（PR #21） | chat 运行期间持有会话文件的 `flock`。另一个 `kubectl exec` 中对同一文件的 run 报告 `is in use`，退出码 2；chat 结束后，同一文件可以再次使用 |
| 清理 | `delete -k` 删除了全部对象，包括集群级的角色和绑定；从 CronJob 创建的 Job 被一并回收；卷被删除；删除命名空间后没有残留 |

未验证：x86_64 镜像与其他集群（如 GKE）、推送到镜像仓库、真实模型、多集群。

---

`deploy/kubernetes/README.md` · v0.1 · 2026年11月
