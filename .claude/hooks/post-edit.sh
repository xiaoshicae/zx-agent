#!/usr/bin/env bash
# PostToolUse（Edit|Write）：
# 1. 对刚改过的 .rs 文件跑 rustfmt，格式由工具决定，不靠 Claude 记得。rustfmt 会顺带格式化这个文件里
#    用 `mod x;` 声明的子模块。格式化失败（比如代码还没写完）时不报错，留给 Stop hook 的 clippy 和 test 去报。
# 2. 记下本会话改过的 cargo 项目。stop-check.sh 只检查本会话改过的项目，纯问答和同时在跑的其他会话都不会触发。

input=$(cat)
file=$(jq -r '.tool_input.file_path // empty' <<<"$input")
session=$(jq -r '.session_id // empty' <<<"$input")

case "$file" in
    *.rs | */Cargo.toml) ;;
    *) exit 0 ;;
esac
[ -f "$file" ] || exit 0

# edition 与 Cargo.toml 保持一致（红线 5 不允许改 edition）
case "$file" in
    *.rs) rustfmt --edition 2024 "$file" >/dev/null 2>&1 ;;
esac

# 记下 workspace 的根目录，而不是离文件最近的 Cargo.toml：在成员 crate（crates/*）里跑 cargo test
# 只测那一个包，测不到根目录 tests/ 下的集成测试。cargo 读不了清单（比如正改到一半）时退回最近的 Cargo.toml。
dir=$(dirname "$file")
manifest=$(cd "$dir" && cargo locate-project --workspace --message-format plain 2>/dev/null)
if [ -n "$manifest" ]; then
    dir=$(dirname "$manifest")
else
    while [ "$dir" != / ] && [ ! -f "$dir/Cargo.toml" ]; do
        dir=$(dirname "$dir")
    done
fi
if [ -n "$session" ] && [ -f "$dir/Cargo.toml" ]; then
    marks="${TMPDIR:-/tmp}/zx-agent-stop-check"
    mkdir -p "$marks" && echo "$dir" >>"$marks/$session"
fi
exit 0
