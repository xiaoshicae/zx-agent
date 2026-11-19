# 安全模型

策略只是第一道防线，不是安全边界。生产环境以最小权限账号运行，放在容器或虚拟机中，必要时限制网络。

## 命令策略

命令策略（`CommandPolicy`）只检查 bash，只负责拒绝，没有命令白名单；其余命令是否询问见[权限](CONFIGURATION.md#权限)。它拒绝的命令：

- 含 `--no-preserve-root`、`dd if=`、`:(){`、`> /dev/sd` 或 `> /dev/nvme`（转小写、合并空白后按子串比较）；
- 含 `rm -rf /` 或 `rm -fr /`，且 `/` 就是整个路径，即其后是命令结尾、空白、`*`、引号或 `;`、`&`、`|`、`)`、`` ` ``；`rm -rf /tmp/build` 可以运行；
- 任意一个词（按文件名比较，如 `/sbin/reboot`）是 `shutdown`、`reboot`、`poweroff`、`halt`、`mkfs`（含 `mkfs.<类型>`），或是 `init 0`、`init 6`、`telinit 0`、`telinit 6`。只有 `grep`、`last`、`journalctl`、`cat`、`echo` 等只读程序的参数不算；有选项能运行别的程序的不算只读程序：`rg`（`--pre`）、`man`（`-P`）、`sort`（`--compress-program`）；
- 含 agent 的 `permissions.deny` 中 `bash(<文本>)` 的文本。

这些规则容易绕过，只起减速作用。

## 自动放行

自动放行（`permissions.allow` 的 `bash(<前缀>)`）只决定是否询问。满足以下全部条件的命令不经审批：

- 是由清单中的前缀组成的管道；前缀按词比较，只合并空白，区分大小写：在区分大小写的文件系统上 `HEAD` 与 `head` 是两个程序，前缀 `head` 不放行 `HEAD`；
- 只由字母、数字、空格、制表符和 `- _ . / : = , @ + | ^ ~ !` 组成：引号、`\`、`$`、通配符、括号、`#`、`;`、`&`、`` ` ``、`>`、`<` 与换行都改为询问，因为它们能让 shell 执行的内容与字面不同；
- 不含 URL（`://`），也不含 `permissions.allowExcept` 的字词（不分大小写；内置 assistant 为 `secret`）；
- 程序在 `permissions.allowOptions` 中列出了选项时，只带这些选项。

检查不含任何具体程序的知识。把 kubectl 之类的程序加进 `allow` 时，要同时列出它可带的选项，否则任何选项都不会触发询问；`deploy/kubernetes/agents/assistant.md` 只放行 kubectl 的只读选项，`--server`、`--kubeconfig`、`--token`、`--raw` 等都要询问。选项按命令的第一个词查找：程序只经 `sudo kubectl` 或带路径的前缀放行时，为它列出的选项不起作用，这样的配置在启动时报错。自动放行的命令能读到什么取决于账号与凭据，读到的内容会进入模型上下文。

## 工具

- bash 命令结束、超时或被取消时，终止其进程组；用 `setsid` 等方式脱离进程组的进程可能继续运行，需要保证时用容器执行。
- `read_file`、`grep`、`find`、`ls` 不经命令检查，可以读进程能读的任何文件；`edit_file`、`write_file` 可以写进程能写的任何普通文件，包括工作目录之外的。需要限制时不启用它们，或用容器执行。内置的 `unattended` 与 Kubernetes 的 agent 不启用写文件与搜索工具。
- 同一进程中各会话的 `edit_file` 与 `write_file` 对同一个文件逐个修改，不协调其他进程或经 bash 的写入。
- 输出都有界：`read_file` 单文件至多 10 MiB，每次至多 2000 行或 50 KiB；搜索工具有条数与 60 秒超时上限。
- `read_output` 只读落盘目录中以输出 ID 命名的普通文件，不跟随符号链接；落盘目录在工作目录之下时，从工作目录到它的每一级都不能是符号链接，否则既不读也不写。落盘目录默认在工作目录下，容器中的命令也能写它，而 `read_output` 在本机读取。
- MCP 服务器与工具都由使用者显式列出；工具描述与结果来自外部服务，可能含注入内容。工具一律按有副作用处理；交给模型的结果有上限，响应体本身没有（SSE 每个事件至多 16 MiB）。

## 密钥

- 以下凭据只从环境变量读取：API key（`provider.apiKeyEnv`）、MCP 的 `tokenEnv`、zx-im 的平台凭据（Slack 的两个 token，⚑ D:5/CHINA FLAG: 飞书的 App Secret）与 OpenTelemetry 的请求头变量（`OTEL_EXPORTER_OTLP_HEADERS` 等）。在本机执行的命令看不到这些变量；工具输出在写入会话、事件和回传模型之前，把它们的完整字面值替换为 `[REDACTED]`。这两项由 `zx-app` 的装配设置；作为库使用时要嵌入方自己设置（`LocalExecutor::hidden_env` 与 `RedactSecrets`，见 [LIBRARY.md](LIBRARY.md)）。
- 其他凭据文件用 `config.toml` 的 `secretFiles` 或 `ZX_SECRET_FILES` 登记（如 Kubernetes ServiceAccount 的 token），程序本身不预设路径。文件每次过滤前重新读取，轮换下来的旧值保留最近 32 个继续过滤。短于 8 个字符的值照常过滤，但所有相同的文本都会被替换，启动时在 stderr 以 `[secret] <文件>: …` 提示。过滤器出错时丢弃原始输出，返回错误结果。
- 过滤不识别未知、编码或拆分后的密钥，也不处理用户输入、模型输出、工具参数、项目上下文与说明。这些内容仍可能进入模型请求、会话和事件，需要控制会话文件的访问权限；chat 默认把会话写到主目录下，不想留下记录时用 `--no-session`。
- 本机执行时，工具与 agent 是同一系统用户，仍可经其他途径读到凭据（如 Linux 的 `/proc/<pid>/environ`）；其他环境变量（如云厂商凭证）bash 仍能读到，需要时在启动前清理。隔离要靠容器执行或不同账号。
- 项目上下文会发给模型服务方；在不可信仓库中用 `discover = false` 的 agent（发现规则见[项目上下文](CONFIGURATION.md#项目上下文)）。

## 容器执行

容器执行（`execution = "container"`，键见[配置说明](CONFIGURATION.md#容器执行)）时，bash、文件读写与搜索工具在 Docker 容器中执行；read_skill 与 read_output 仍读取本机文件。容器是沙箱：限制放在容器的外墙上，容器内默认给命令该用户能有的全部权限。

- 每个会话一个容器，打开会话时创建（chat 的 `/new` 换一个新容器；acp 与 zx-im 同时打开几个会话就有几个容器），随会话关闭或进程结束而删除，被 SIGKILL 时也一样。容器带 `zx-agent.session=<会话 ID>` 标签。启动时检查命令不以 root 运行、镜像有 bash 等程序、容器用户能读（可写时能写）工作目录；不满足、镜像不在本地或 Docker 不可用时以退出码 2 结束。
- 容器内默认：工作目录以读写方式挂载在同一路径，改动直接作用于宿主机；容器自身的文件可写；加入 `bridge` 网络，可以联网。严格模式为 `network = "none"`、`workspace = "readOnly"`、`root = "readOnly"`。
- 外墙：只挂载工作目录，不加 `--privileged`，不挂 Docker socket，不用宿主机的网络与 PID 命名空间，去掉全部 capability 并设 `no-new-privileges`，限制进程数与内存，命令不以 root 运行。命令不继承 agent 的环境变量，API key 不进入容器。
- 同一容器内的调用逐个执行；每条命令前后终止命令用户的全部进程，脱离进程组的后台进程也不例外。
- 审批照常；容器执行时 agent 可以设 `permissions.approveAll = true` 不再询问，拒绝规则仍然生效。宿主注册的执行环境声明自己隔离命令（`ExecutorFactory::isolates`）时也可以，隔离是否成立由宿主负责。
- 可写又联网时，挂进容器的内容（代码、`.git` 中的远端地址等）可能被发送出去。工作目录整体对容器可见，所以会话文件必须在工作目录之外，否则启动时报配置错误（在主目录中运行 chat 时改用另一个 `ZX_AGENT_HOME` 或 `--no-session`）。
- 容器中没有 agent 的环境变量，也只看得到工作目录，所以需要凭据的在线访问（如集群 API）不经容器执行。

---

SECURITY.md · v0.8 · 2026年11月
