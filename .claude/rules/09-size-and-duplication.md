---
paths:
  - "**/*.rs"
---

# 09 尺寸与重复

函数超过 80 行 clippy 会报（`too_many_lines`，阈值在 `clippy.toml`）。这里是数字之外的判断。

## 尺寸

- **函数一屏**：目标 60 行以内，一个函数做一件事；写不出一句话文档的函数在做两件事。拆出来的函数按步骤命名（`load_profile`、`open_session`、`register_hooks`），不叫 `helper`、`do_x`。
- **可以带 `#[expect(clippy::too_many_lines, reason = "...")]` 的只有两种**：按变体展开、每个分支一两行的 match（`one arm per event variant`）；一张数据表（`one event of every variant`）。冻结的模块（chat）只做不改行为的整理，也用这条。
- **文件一个主题**：产线代码超过 600 行就按职责拆成子模块（`executor/container/{mod,config,scripts}.rs`），不按类型拆（没有 `types.rs` 加 `impls.rs`）。入口文件只分派（`main.rs`）。
- **状态用枚举**：两个 `Option`、`Option` 加 `bool`、`Result<Result<_>>` 表示的状态换成一个枚举（`Pending`、`Decision`、`Connection`），见规则 05。

## 重复

- **同样的 5 行以上出现第二次就提取**，第三次必须提取。提取出来的放在它所属那一层的 `mod.rs`：工具共用的在 `tools/mod.rs`（`parse_args`、`resolve`、`unless_cancelled`），平台共用的在 `zx_im::platform`（`Pacer`、`Cards`），测试共用的在 `tests/common`（`zx_agent()`、`stdout`）。
- **一处定义，别处引用**：常量、错误前缀（`fault`）、超时加上报（`Shared::bounded`）、结束事件（`StreamEvent::ending`）都只写一份。
- **不横向引用**：同一层的模块互不依赖（run、acp、sessions 不引用 chat，两个 IM 平台互不引用）；要共用就下沉到共同的下层。需要引用上层时，说明分层或接口该改了（CLAUDE.md 的"分层由 crate 边界保证"）。

## 提交前自查

对照 diff 回答：最长的函数多长；有没有新抄的第二份；新名字和仓库里的同义词一致吗；搬过的代码文档跟着走了吗；`pub` 是不是最小。PR 模板里有这几项。
