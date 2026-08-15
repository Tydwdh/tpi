#!/usr/bin/env bash
# P7-01：依赖 DAG dry run——模拟物理 crate 拆分后的模块边界与循环检测。
#
# 目标边界（P7-02 拆 crate 顺序）：
#   core -> session -> capabilities -> agent -> TUI -> adapters/CLI
#
# 检查：每个边界层只依赖**前面的层**（无反向/循环引用）。
# 方法：提取每层文件的**真实 import**（`use crate::X`，排除注释行与 doc 链接
# `[crate::...]`），归入层后验证单向性。
# 注意：这是 dry run（启发式）；精确检查由 cargo 拆包后编译期保证。
# 连续两阶段无反向 import 才允许开始拆 crate。

set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

# 层 → 目录/文件映射（当前单 crate 内的模块近似）。
declare -A LAYER
LAYER[core]="crates/tpi-core/src"
LAYER[session]="crates/tpi-session/src"
LAYER[capabilities]="src/tool src/shell src/workspace src/process src/mcp"
LAYER[agent]="src/agent src/context src/provider"
LAYER[tui]="src/tui"
LAYER[adapters]="src/app.rs src/app src/web.rs src/doctor.rs src/eval src/auth src/clipboard src/main.rs"

# 层顺序（数字越小越底层）。
ORDER=(core session capabilities agent tui adapters)
declare -A RANK
for i in "${!ORDER[@]}"; do
  RANK[${ORDER[$i]}]=$i
done

# 目标层的模块前缀集合（用于判定引用是否指向该层）。
declare -A LAYER_PREFIX
LAYER_PREFIX[core]="ids|message|plan|util"
LAYER_PREFIX[session]="session"
LAYER_PREFIX[capabilities]="tool|shell|workspace|process|mcp"
LAYER_PREFIX[agent]="agent|context|provider"
LAYER_PREFIX[tui]="tui"
LAYER_PREFIX[adapters]="app|web|doctor|eval|auth|clipboard"

violations=0
# 对每层：提取真实 use 引用；凡指向 rank 更大的层（后面的层）即反向引用违规。
for layer in "${ORDER[@]}"; do
  files=""
  for pat in ${LAYER[$layer]}; do
    if [ -e "$pat" ] || [ -d "$pat" ]; then
      files="$files $pat"
    fi
  done
  refs=$(grep -rEh '^\s*(pub\s+)?use\s+crate::[a-z_]+' $files 2>/dev/null \
          | sed -E 's/.*use\s+crate::([a-z_]+).*/\1/' | sort -u)
  for other in "${ORDER[@]}"; do
    [ "$layer" = "$other" ] && continue
    if [ "${RANK[$other]}" -gt "${RANK[$layer]}" ]; then
      prefix="${LAYER_PREFIX[$other]}"
      hit=$(echo "$refs" | grep -E "^(($prefix))$" || true)
      if [ -n "$hit" ]; then
        echo "VIOLATION: $layer -> $other（反向引用）:"
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
