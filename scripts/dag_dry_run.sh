#!/usr/bin/env bash
# P7-01：依赖 DAG dry run——模拟物理 crate 拆分后的模块边界与循环检测。
#
# 目标边界（P7-02 拆 crate 顺序）：
#   core -> session -> capabilities -> agent -> TUI -> adapters/CLI
#
# 检查：每个边界层只依赖**前面的层**（无反向/循环引用）。
# 方法：grep 每层的 `crate::` 引用，归入层后验证单向性。
# 注意：这是 dry run（启发式，不含 type 级精确性）；精确检查由 cargo 拆包后
# 编译期保证。连续两个阶段无反向 import 才允许开始拆 crate。

set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

# 层 → 目录映射（当前单 crate 内的模块近似）。
declare -A LAYER
LAYER[core]="src/ids.rs src/message.rs src/util.rs src/error.rs"
LAYER[session]="src/session src/conversation.rs 2>/dev/null"
LAYER[capabilities]="src/tool src/shell src/workspace src/process src/mcp"
LAYER[agent]="src/agent src/context src/provider"
LAYER[tui]="src/tui"
LAYER[adapters]="src/app src/web src/doctor src/eval src/auth src/clipboard"

# 层顺序（数字越小越底层）。
ORDER=(core session capabilities agent tui adapters)
declare -A RANK
for i in "${!ORDER[@]}"; do
  RANK[${ORDER[$i]}]=$i
done

violations=0
# 对每层：找它引用了哪些层的模块（反向引用 = 违规）。
for layer in "${ORDER[@]}"; do
  files=""
  for pat in ${LAYER[$layer]}; do
    # 去掉 2>/dev/null 字样
    pat=${pat%% *}
    if [ -e "$pat" ] || [ -d "$pat" ]; then
      files="$files $pat"
    fi
  done
  for other in "${ORDER[@]}"; do
    [ "$layer" = "$other" ] && continue
    # 引用其他层（other）但 other 在本层之后（rank 更大）= 反向引用违规。
    if [ "${RANK[$other]}" -gt "${RANK[$layer]}" ]; then
      count=$(grep -rn "crate::" $files 2>/dev/null | grep -c "crate::${other#?}" 2>/dev/null || true)
      # 更精确：引用目标层的代表性模块路径。
      count=$(grep -rEoh "crate::[a-z_]+" $files 2>/dev/null | grep -cE "crate::(${other#?}|.*)" || true)
      # 简化：检查本层文件里是否出现后面层的 crate:: 模块引用。
      later_mods=""
      case "$other" in
        core) later_mods="ids|message|util";;
        session) later_mods="session";;
        capabilities) later_mods="tool|shell|workspace|process|mcp";;
        agent) later_mods="agent|context|provider";;
        tui) later_mods="tui";;
        adapters) later_mods="app|web|doctor";;
      esac
      hit=$(grep -rEoh "crate::($later_mods)\b" $files 2>/dev/null | sort -u | head -5)
      if [ -n "$hit" ]; then
        echo "VIOLATION: $layer -> $other (反向引用):"
        echo "$hit" | sed 's/^/    /'
        violations=$((violations+1))
      fi
    fi
  done
done

if [ "$violations" -eq 0 ]; then
  echo "DAG dry run: OK（无反向模块引用；连续两阶段无反向即可开始拆 crate）"
else
  echo "DAG dry run: $violations 处反向引用（拆 crate 前需消除）"
  exit 1
fi
