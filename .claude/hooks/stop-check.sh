#!/usr/bin/env bash
# Stop：本会话用 Edit / Write 改过 Rust 代码或 Cargo.toml 时，结束前跑 CLAUDE.md「命令」里的格式、lint、测试检查。
# 改动由 post-edit.sh 按会话记录，所以纯问答、以及同时在跑的其他会话的改动都不会触发。
# - 第一次不通过：拦下（decision=block），把输出的最后部分交给 Claude 去修。
# - 已经拦过一次（stop_hook_active=true）仍不通过：放行，用 systemMessage 提示用户，并清掉记录。
#   这样用户本来就要保留失败时（比如先写复现测试）不会被反复拦截；之后再改代码会重新检查。

input=$(cat)
session=$(jq -r '.session_id // empty' <<<"$input")
already_blocked=$(jq -r '.stop_hook_active // false' <<<"$input")
mark="${TMPDIR:-/tmp}/zx-agent-stop-check/$session"
[ -n "$session" ] && [ -f "$mark" ] || exit 0

log=$(mktemp)
trap 'rm -f "$log"' EXIT

check() {
    "$@" >"$log" 2>&1 && return 0
    local output
    output=$(tail -n 60 "$log" | tail -c 6000)
    if [ "$already_blocked" = true ]; then
        rm -f "$mark"
        jq -n --arg m "结束前的检查仍未通过（${dir}：$*），请查看。" '{systemMessage: $m}'
    else
        jq -n --arg r "$* 没有通过（${dir}）。修好后再结束；如果用户明确要求保留这个失败（例如先写复现测试），说明原因后直接结束。输出的最后部分：
$output" '{decision: "block", reason: $r}'
    fi
    exit 0
}

while IFS= read -r dir; do
    cd "$dir" || continue
    check cargo fmt --check
    check cargo clippy -q --all-targets -- -D warnings
    check cargo test -q
done < <(sort -u "$mark")
rm -f "$mark"
exit 0
