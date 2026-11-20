---
paths:
  - "**/*.rs"
---

# 04 异步与并发（tokio）

- **不阻塞 async 线程**：用 `tokio::time::sleep`、`tokio::process::Command`；CPU 密集的计算和同步库调用放进 `spawn_blocking`（如 `crates/zx-builtin/src/executor/local.rs` 写文件、`crates/zx-builtin/src/redact.rs` 读密钥文件）。少量、快速的同步 I/O（如启动时读取配置）可以接受，但要注释说明。
- **所有外部等待都设超时**，超时的错误写明在等什么、等了多久（如 provider 的 `idle_timeout`、整次运行的 `RunLimits::timeout`）。
- **取消**：用 `CancellationToken` 传递，耗时的操作在 `select!` 里同时监听 `cancel.cancelled()`。没被选中的分支会被直接丢弃：需要保留的进度放进独立任务和共享缓冲区（`crates/zx-builtin/src/executor/capture.rs` 的 `Capture::spawn`）；一旦开始就必须做完的，放进不随调用被丢弃的任务（`Executor::write_file`）。
- **`tokio::spawn`**：只在任务要比调用者活得久，或需要多核并行时使用；只是让几件事并发进行，用 `join_all` 或 `select!`，还能直接借用数据（`crates/zx-core/src/runner/step_loop/tools.rs` 的 `parallel`）。spawn 出的任务要被 `.await` 或 `abort()`，否则注释说明为什么可以独立运行。
- **流式事件用有界 channel**：provider 使用 `ai::event_channel()`，异步等待发送，接收端关闭后停止生产。确需无界 channel 时，必须有可验证的数据总量上限并说明来源。
- **trait object**：在任务之间共享的 trait 声明 `Send + Sync` 父 trait；async trait 统一用 `async_trait`，不和原生的 async fn in trait 混用。
