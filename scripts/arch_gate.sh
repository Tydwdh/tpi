#!/usr/bin/env bash
# P0-07 架构依赖 gate v1（docs/refactor/08-migration-roadmap.md P0-07）。
#
# 禁止的反向/越界引用（重构期防线，防止边界在迁移完成前回流）：
#   R1: tui -> app   —— src/tui/ 不得引用 crate::app
#   R2: agent -> tui —— src/agent/ 不得引用 crate::tui
#   R3: 新增 global_registry() 调用 —— 注册表必须逐步迁移到 composition
#       root 注入（P4-02），禁止新增进程级全局注册表依赖点
#
# 既有违规以"精确 allowlist"登记（路径|特征子串）：每消除一项即删除
# 对应 allowlist 行，绝不允许只增不减。新增未登记引用 → 非零退出。
#
# 用法：bash scripts/arch_gate.sh（CI 中由 .github/workflows/ci.yml 调用）
set -u

fail=0
repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"

# check <rule> <dirs> <regex> <allowlist...>  —— dirs 空格分隔，allowlist 每项 "路径|needle"
check() {
    local rule="$1" dirs="$2" regex="$3"
    shift 3
    local allowed=("$@")
    while IFS=: read -r file line rest; do
        [ -z "$file" ] && continue
        # Windows 原生 rg 输出反斜杠路径，统一为正斜杠再与 allowlist 比较。
        local fnorm="${file//\\//}"
        local ok=0
        for a in "${allowed[@]}"; do
            local af="${a%%|*}" an="${a#*|}"
            if [ "$fnorm" = "$af" ] && printf '%s' "$rest" | grep -qF "$an"; then
                ok=1
                break
            fi
        done
        if [ "$ok" -eq 0 ]; then
            echo "GATE VIOLATION [$rule]: $fnorm:$line: $rest"
            fail=1
        fi
    done < <(rg -n "$regex" $dirs --glob '*.rs' 2>/dev/null)
}

# R1: TUI 不得反向引用 app。已清零（P1-04 把 preview_lines_to_body 纯投影
#     移入 src/tui/model.rs），allowlist 为空：任何 `crate::app` 引用都违规。
check "R1:tui->app" "src/tui" "(use crate::app|crate::app::)"

# R2: agent 不得引用 TUI。已清零（P1 Exit gate：测试 fake config 构造收敛到
#     config::test_config，tui 依赖集中在 config 模块），allowlist 为空。
check "R2:agent->tui" "src/agent" "(use crate::tui|crate::tui::)"

# R3: global_registry() 已整体移除（P4 gate：registry 由 composition root
#     注入；工具、测试各自持有独立实例）。规则删除（P0-07：allowlist 只减不增）。

if [ "$fail" -ne 0 ]; then
    echo "arch-gate: FAILED —— 以上为未登记的违规引用；新代码禁止引入，" \
        "消除既有违规后删除对应 allowlist 行（P0-07：allowlist 只减不增）。"
    exit 1
fi
echo "arch-gate: OK（无未登记的跨边界引用）"
