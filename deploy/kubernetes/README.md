# 在 Kubernetes 中常驻部署 zx-agent

运维人员 `kubectl exec` 进常驻 Pod 用 chat 排障；CronJob 每小时巡检。kubectl 的凭据来自 ServiceAccount，token 经 `ZX_SECRET_FILES` 登记脱敏。

## 目录内容

- `Dockerfile`：zx-agent、IM 服务 zx-im 与 bash、kubectl、curl、jq、tini，以 10001 号非 root 用户运行。
- `agents/`：生成 ConfigMap `zx-agent-agents`，挂载到主目录的 `agents/`，替换同名内置 agent。`assistant.md`（chat 与 zx-im）加入只读 kubectl 命令及其选项；`unattended.md`（巡检与单次任务）要求排障报告的固定小节。
- `serviceaccounts.yaml`、`rbac.yaml`：常驻 Pod 的 `zx-agent` 与巡检的 `zx-agent-inspect`，都绑定预置的只读角色 `view`（不含 Secret）与 `zx-agent-cluster-view`（节点、持久卷、存储类）；`operator.yaml` 是可选的写权限，按命名空间授予，单独应用。
- `pvc.yaml` 是 `/data` 的卷；`im/` 与 `multicluster/` 是可选组件。

## 部署

1. 构建并推送镜像，改 `kustomization.yaml` 中 `images` 的 `newName`、`newTag`。每次用新标签，否则节点用缓存的旧镜像（`IfNotPresent`）。

   ```sh
   docker build -f deploy/kubernetes/Dockerfile -t <镜像仓库>/zx-agent:<标签> .
   docker push <镜像仓库>/zx-agent:<标签>
   ```

2. 创建命名空间与模型配置（`base-url` 不含 `/chat/completions`，如 `https://api.openai.com/v1`）：

   ```sh
   kubectl create namespace zx-agent
   kubectl -n zx-agent create configmap zx-agent-model \
     --from-literal=model=<模型 ID> --from-literal=base-url=<OpenAI 兼容服务的地址>
   kubectl -n zx-agent create secret generic zx-agent-model --from-file=api-key=<存放 API key 的文件>
   ```

3. 执行 `kubectl apply -k deploy/kubernetes`。`rbac.yaml` 由 kustomize 填入命名空间，只能用 `apply -k`；换命名空间时改 `kustomization.yaml` 的 `namespace`，以及 `operator.yaml` 中 `subjects` 的 `namespace`。
4. 需要写操作时，在 `operator.yaml` 中为每个允许修改的命名空间写一个 RoleBinding（文件中的一个以 `default` 为例），再执行 `kubectl apply -f deploy/kubernetes/operator.yaml`。它不在 `kustomization.yaml` 中：kustomize 会把所有 RoleBinding 改到 `zx-agent` 命名空间。

## 使用

```sh
kubectl -n zx-agent exec -it deploy/zx-agent -- zx-agent chat --session /data/<名字>.jsonl
kubectl -n zx-agent exec deploy/zx-agent -- zx-agent run --session /data/<名字>.jsonl "<任务>"
kubectl -n zx-agent create job --from=cronjob/zx-agent-inspect inspect-now   # 立即巡检一次
kubectl -n zx-agent logs job/inspect-now
```

- 会话文件在卷上，用同一路径可以接着聊。一个文件同一时间只能被一个 chat 或 run 打开，否则报告 `is in use` 并以退出码 2 结束。锁依赖 `flock`，文件系统不支持时打开会话直接失败。
- 巡检的任务与时间在 `cronjob.yaml` 的 `args` 与 `schedule` 中修改；退出码见 [run 退出码](../../docs/REFERENCE.md#run-退出码)。
- 巡检的会话文件写在卷 `zx-agent-inspect-sessions` 的 `sessions/<Pod 名>.jsonl`，中断后可据此 `run --continue`；每次巡检开始前删除 30 天前的文件（`cronjob.yaml` 中 `prune-sessions` 的 `-mtime`），因为卷写满后会话写不进去，巡检会全部失败。读取要在巡检不运行时用挂载这块卷的临时 Pod。
- 巡检报告在 Pod 日志中，随 Job 保留一天（`ttlSecondsAfterFinished` 与两个 `JobsHistoryLimit`）；巡检失败没有通知，要由集群的监控发现失败的 Job。
- `run`（巡检，以及在常驻 Pod 中执行的单次任务）用的 unattended agent 没有 `[permissions]`：除[内置的拒绝规则](../../docs/SECURITY.md#命令策略)外，命令都直接执行，不会等待审批。有命令被拒时退出码仍可能是 0；在 `cronjob.yaml` 的 `command` 中加 `--fail-on-denied` 后以退出码 9 结束。

## 多集群

kubeconfig 用 `tokenFile` 引用 Secret 中各集群的 token，本身不含密钥；token 目录登记为脱敏对象。

1. 在每个远端集群创建只读账号（`view`）与长期 token，取出 token 与 base64 编码的 CA：

   ```sh
   kubectl apply -f deploy/kubernetes/multicluster/remote.yaml
   kubectl -n zx-agent-remote get secret zx-agent-token -o jsonpath='{.data.token}' | base64 -d > <集群名>.token
   kubectl -n zx-agent-remote get secret zx-agent-token -o jsonpath='{.data.ca\.crt}'
   ```

2. 写 kubeconfig。`local` 沿用 Pod 自己的账号，必须保留；每个远端集群加一组，地址须能从 Pod 访问：

   ```yaml
   apiVersion: v1
   kind: Config
   clusters:
     - {name: local, cluster: {server: "https://kubernetes.default.svc", certificate-authority: /var/run/secrets/kubernetes.io/serviceaccount/ca.crt}}
     - {name: <集群名>, cluster: {server: "https://<远端 API server 的地址>", certificate-authority-data: <第 1 步输出的 CA>}}
   users:
     - {name: local, user: {tokenFile: /var/run/secrets/kubernetes.io/serviceaccount/token}}
     - {name: <集群名>, user: {tokenFile: /var/run/secrets/zx-agent/clusters/<集群名>}}
   contexts:
     - {name: local, context: {cluster: local, user: local}}
     - {name: <集群名>, context: {cluster: <集群名>, user: <集群名>}}
   current-context: local
   ```

3. 创建两个对象，取消 `kustomization.yaml` 中 `components` 两行的注释，再 `apply -k`。新增集群时重新生成这两个对象并 `apply`（`--dry-run=client -o yaml | kubectl apply -f -`），约 1 分钟后生效，不需要重启 Pod。

   ```sh
   kubectl -n zx-agent create secret generic zx-agent-clusters --save-config --from-file=<集群名>=<集群名>.token
   kubectl -n zx-agent create configmap zx-agent-kubeconfig --save-config --from-file=kubeconfig=<kubeconfig 文件>
   ```

命令用 `--context <集群名>` 选择集群，放在子命令之后，在 chat 与 zx-im 中仍不经审批。远端 token 不过期，更换时重建远端的 `zx-agent-token` 再更新 Secret；远端账号只读。不再访问某个集群时，在该集群执行 `kubectl delete -f deploy/kubernetes/multicluster/remote.yaml --ignore-not-found`。

## IM 服务

启用 `im` 组件后，主进程是 IM 服务 `zx-im`，在 IM 中 @ 机器人排障、审批写操作（用法见 [README 的“IM 服务”](../../README.md#im-服务)）。Pod 共享进程命名空间，PID 1 是 pause，回收命令留下的进程。assistant agent 不限请求数与时间，所以 `im/deployment-patch.yaml` 的命令行给每次运行加了上限：`--max-model-requests 100` 与 `--timeout-secs 3600`（等待审批的时间也算在内）；要改上限就改这两个参数。

1. 创建 Secret `zx-agent-im`，只写用到的平台（⚑ D:5/CHINA FLAG: 飞书用 `feishu-app-id` 与 `feishu-app-secret` 两个键，Subject to Legal/Export Compliance approval）：`kubectl -n zx-agent create secret generic zx-agent-im --from-literal=slack-app-token=xapp-... --from-literal=slack-bot-token=xoxb-...`。
2. 由 `im/im.example.toml` 复制出 `im.toml`，填好聊天与审批人：`kubectl -n zx-agent create configmap zx-agent-im-config --from-file=im.toml`。
3. 取消 `kustomization.yaml` 中 `components:` 与 `- im` 的注释，再 `apply -k`。

修改配置或凭据后执行 `kubectl -n zx-agent rollout restart deploy/zx-agent`。日志格式见 [REFERENCE 的“IM 服务”](../../docs/REFERENCE.md#im-服务)，对话在 `/data/sessions`，收到而还没处理完的消息在 `/data/sessions/.backlog`，重启后接着处理。出网：Slack 需要 `slack.com` 及其 WebSocket 主机，⚑ D:5/CHINA FLAG: 飞书需要 `open.feishu.cn`、`accounts.feishu.cn` 及长连接主机；WebSocket 主机由平台运行时下发，没有实测。按域名限制出网要用出口代理。

## 权限

两个账号都只能读：`view` 覆盖各命名空间中除 Secret 外的多数对象，`zx-agent-cluster-view` 补上 `view` 不含的节点、持久卷与存储类。应用 `operator.yaml` 后，`zx-agent` 在其中写了 RoleBinding 的命名空间里另有下列写权限：chat 与 zx-im 中这些命令每次都要人工确认；在常驻 Pod 中执行的 `run` 不询问，会直接执行（见“使用”）。

| 命令 | 需要的权限 |
|-|-|
| `kubectl rollout restart` | `apps` 组 `deployments`、`statefulsets`、`daemonsets` 的 `patch` |
| `kubectl scale` | `apps` 组 `deployments/scale`、`statefulsets/scale` 的 `patch` |
| `kubectl delete pod` | `pods` 的 `delete` |

RBAC 不能把 `patch` 限制为重启：能 `patch` 工作负载就能改它的镜像、启动命令与 ServiceAccount。所以写权限按命名空间授予，不要包含 `kube-system` 与 zx-agent 自己的命名空间。

## 安全说明

- 主进程 tini 的环境里有 API key（启用 `im` 时在 zx-im 进程的环境里，连同 IM 凭据；巡检的主进程就是 zx-agent），同一用户的命令可以从 `/proc` 读到。读到的值被脱敏，不进入模型上下文；被诱导的命令仍可能把它发往外部（chat 与 zx-im 中要确认，`run` 不询问）。
- 命令与 agent 以同一用户运行，能修改 `/data` 中的会话文件。
- 用 NetworkPolicy 默认拒绝出网时，要单独放行集群 DNS。所有对象与 Pod 都带标签 `app.kubernetes.io/part-of: zx-agent`，可用于选择。
- `/tmp` 是上限 1Gi 的 `emptyDir`，超出后 Pod 被驱逐；常驻 Pod 被驱逐后由 Deployment 重建，进行中的 chat 会中断。

## 清理

```sh
kubectl delete -k deploy/kubernetes
kubectl -n zx-agent delete configmap zx-agent-model
kubectl -n zx-agent delete secret zx-agent-model
kubectl delete namespace zx-agent
```

应用过 `operator.yaml` 时，先执行 `kubectl delete -f deploy/kubernetes/operator.yaml --ignore-not-found`。启用 IM 时另删 `zx-agent-im-config` 与 `zx-agent-im`；启用多集群时另删 `zx-agent-kubeconfig` 与 `zx-agent-clusters`，并在每个远端集群删除 `remote.yaml`。会话文件是否随存储卷声明删除，取决于 StorageClass 的回收策略。

## 验证记录

见 [`docs/ROADMAP.md`](../../docs/ROADMAP.md) 第 6 节。

---

`deploy/kubernetes/README.md` · v0.9 · 2026年11月
