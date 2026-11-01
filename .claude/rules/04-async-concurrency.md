---
paths:
  - "**/*.rs"
---

# 04 异步与并发（tokio）

持有 `std::sync::Mutex` 的 guard 跨过 `.await`，clippy 默认会报，这里不再重复。

- **不阻塞 async 线程**：用 `tokio::time::sleep`、`tokio::process::Command`；CPU 密集的计算和同步库调用放进 `spawn_blocking`。少量、快速的同步 I/O 可以接受，但要注释说明，例如会话文件追加一行（`src/session.rs`）、启动时读取配置。
- **所有外部等待都设超时**：无人值守时没人能中断卡住的程序。超时的错误写明在等什么、等了多久。已有的超时：provider 的 `connect_timeout` 与 `idle_timeout`、bash 的 `timeout_secs`、整次运行的 `Limits::timeout`、读取子进程输出的 `PIPE_DRAIN_TIMEOUT`。
- **取消**：用 `CancellationToken` 传递，耗时的操作在 `select!` 里同时监听 `cancel.cancelled()`。注意取消安全：没被选中的分支会被直接丢弃，做到一半的工作随之丢失。需要保留的进度放进独立任务和共享缓冲区，参照 `src/executor/capture.rs` 读取子进程输出的写法（`spawn_reader`）。
- **`tokio::spawn`**：只在任务要比调用者活得久，或需要多核并行时使用。只是让几件事并发进行，用 `join_all` 或 `select!` 就够了，还可以直接借用数据（参照 `src/agent/agent_loop.rs` 的 `parallel`）。spawn 出的任务要被 `.await` 或 `abort()`，否则要注释说明为什么可以独立运行。数据直接 `async move` 进去，不要先包 `Arc` 再 clone。
- **流式事件用有界 channel**：provider 使用 `ai::provider::event_channel()`，异步等待发送，接收端关闭后停止生产。确需无界 channel 时，必须有可验证的数据总量上限并说明来源。
- **trait object**：在任务之间共享的 trait 声明 `Send + Sync` 父 trait；async trait 统一用 `async_trait`，不和原生的 async fn in trait 混用。
