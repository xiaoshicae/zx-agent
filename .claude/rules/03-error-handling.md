---
paths:
  - "**/*.rs"
---

# 03 错误处理

| 层 | 错误类型 |
|---|---|
| 应用边界：构造配置、打开文件、子命令 | `anyhow::Result`，加 `.with_context()` |
| 库内部，调用方需要区分错误种类的 | 用 `thiserror` 派生的错误枚举，每个变体写 `#[error("...")]` |
| crate 内部、调用方不区分种类的（如会话记录失败） | `anyhow::Result`，加 `.context()` |
| 工具执行失败（`Tool::execute`） | `Result<ToolOutput, String>`，字符串写给模型看 |
| provider 失败 | 不返回 `Err`（不变量 1） |

- **给模型看的错误要能据此行动**：写清原因和替代做法，如 read_file 遇到目录时返回 "… is a directory; use bash `ls` to list it"。
- **错误枚举的底层原因**，要么拼进 `Display`（`#[error]` 里写 `{0}`），要么从 `source()` 返回（`#[source]` 或 `#[from]`），只选其一，否则错误链里会重复。
- **anyhow 的错误**在写进报告、事件或输出的那一处按 `{:#}` 格式化一次：`{}` 只有最外层的 context。
- **转换错误时保留原因**：写 `.map_err(|e| format!("cannot read {}: {e}", path.display()))`，不写 `.map_err(|_| "read failed")`。
- **错误信息**写明操作对象（路径、URL、条目 id），不写入密钥。`Display` 小写开头、结尾不加句号；给模型的工具结果是完整的句子。
- **`std::sync::Mutex` 加锁**一律用 `zx_util::sync::lock(&mutex)`（zx-core 与 zx-builtin 里是 `crate::sync::lock`），不直接写 `.lock()`；什么时候不能用它见 `crates/zx-util/src/sync.rs`。
- **不调用 `std::process::exit`**：`main` 返回 `ExitCode`，退出码在各程序里：`crates/zx-cli/src/process.rs` 与 `crates/zx-im/src/program.rs`。

## 不吞错误

生产代码中对 `Result` 用下面的写法会悄悄丢掉错误，每处都要在旁边注释为什么可以丢弃（`.claude/scripts/self-check.sh` 会列出生产代码中的 `let _ =`）：`let _ = fallible();`、`.ok()`、`.unwrap_or_default()`、`.unwrap_or(x)`、没有 else 的 `if let Ok(x) = ...`。例子：`crates/zx-ai/src/provider.rs` 的 `send_event`（接收端关闭后不再需要事件）。
