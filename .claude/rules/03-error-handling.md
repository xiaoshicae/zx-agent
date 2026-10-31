---
paths:
  - "**/*.rs"
---

# 03 错误处理

## 错误分层

| 层 | 错误类型 | 原因 |
|---|---|---|
| 应用边界：`main.rs`、构造配置、打开文件 | `anyhow::Result`，加 `.with_context()` | 最终给人看，需要完整的上下文链 |
| 库内部，调用方需要区分错误种类的 | 自定义错误枚举（引入 `thiserror` 要先征得用户同意） | 调用方可以用 `match` 区分 |
| 工具执行失败（`Tool::execute`） | `Result<ToolOutput, String>` | 这段字符串写给模型看 |
| provider 失败 | 不返回 `Err`，编码成最终的 `StreamEvent::Error` | 不变量 1 |

- **给模型看的错误要能据此行动**：写清原因和替代做法。例如 read_file 遇到目录时返回 "… is a directory; use bash `ls` to list it"，而不是只写 "error"。
- **错误枚举的底层原因**，要么拼进 `Display`，要么从 `source()` 返回，只选其一，否则打印错误链时会重复。
- **转换错误时保留原因**：写 `.map_err(|e| format!("cannot read {}: {e}", path.display()))`，不写 `.map_err(|_| "read failed")`。
- **错误信息**写明操作对象（路径、URL、条目 id），不写入密钥。`Display` 小写开头、结尾不加句号；给模型的工具结果是完整的句子，沿用 pi 的风格。
- **加锁**一律用 `crate::sync::lock(&mutex)`，不直接写 `.lock()`。为什么可以忽略锁中毒、什么时候不能用它，见 `src/sync.rs`。
- `std::process::exit` 只出现在 `main.rs`。

## 不吞错误

下面的写法会悄悄丢掉错误，每次使用都要在旁边写注释说明为什么可以丢弃。`.claude/scripts/self-check.sh` 会列出所有 `let _ =`。

| 写法 | 丢掉了什么 |
|---|---|
| `let _ = fallible();` | 整个错误 |
| `.ok()` | 错误信息（变成 `None`） |
| `.unwrap_or_default()` / `.unwrap_or(x)` | 失败这件事本身，看起来像成功了 |
| 没有 else 的 `if let Ok(x) = ...` | 失败分支 |

例子：`src/ai/provider.rs` 的 `send_event` 忽略发送错误，因为接收端已关闭说明没人再需要这个事件。
