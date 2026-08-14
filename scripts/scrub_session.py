#!/usr/bin/env python3
"""P0-03：从真实 session 采样并脱敏，生成 session golden corpus fixture。

用法：
  python scripts/scrub_session.py --src <真实.jsonl> --dst <corpus/xxx.jsonl>

脱敏规则（确定性：同一输入 → 同一输出）：
- 保留 envelope 结构：schema/seq/event_id/timestamp/session_id/run_id/type。
  这些是随机标识符与时间戳，不含用户隐私；保留它们使 corpus 可验证
  envelope 语义（seq 递增、session_id 一致、event_id 唯一）。
- payload 字符串字段按敏感度替换为占位符：
  - content/text/output/command/arguments/description/body 等 → "REDACTED_<TYPE>_<n>"
  - 含路径语义的字段（path/target/target_path/temp_path/backup_path/root 等）
    → "/workspace/<rel>"
  - **多行字符串（含 \\n）一律替换**——代码块/命令/长文本几乎必然是内容
    （P0-03 泄露根因：bash 工具把完整命令写进 recovery.target_path）
  - 其余标量（status/exit_code/行数/时长/token 数）保留（不敏感，且是语义的一部分）
- 不记录：绝对路径、用户名、API key、真实代码/文本内容。

产物随 fixture 一起进 git（脚本本身可复现脱敏），原始文件绝不动。
"""
import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

# payload 中会被替换的字符串字段名（按语义分组）
TEXT_FIELDS = {
    "content", "text", "output", "command", "arguments", "description",
    "body", "tail", "message", "prompt", "summary", "answer", "error_detail",
    # target_path 在 bash 工具里承载的是**完整命令**（P0-03 泄露根因），
    # 必须整体替换，不能按路径折叠。
    "target_path",
}
# 含路径语义的字段：temp_path/backup_path 是 edit/write 的恢复素材路径。
PATH_FIELDS = {
    "path", "target", "temp_path", "backup_path",
    "workspace_root", "artifacts_root", "cwd", "dir", "file", "root",
    "known_hosts_path",
}

_counter = {"n": 0}


def _redact_text(field: str) -> str:
    _counter["n"] += 1
    return f"REDACTED_{field.upper()}_{_counter['n']}"


def _redact_path(field: str, value: str) -> str:
    # 保持路径结构（相对部分）但去掉盘符/用户目录/绝对前缀 → 只留文件名/相对形状。
    v = value.replace("\\", "/")
    parts = [p for p in v.split("/") if p and p not in (".", "..")]
    if not parts:
        return "/workspace"
    # 保留最后 2 段（文件名 + 父目录名），其余折叠
    keep = parts[-2:] if len(parts) >= 2 else parts
    return "/workspace/" + "/".join(keep)


def _scrub_value(field: str, value, depth: int = 0) -> object:
    """递归脱敏一个 payload 值。标量/数字/布尔保留，字符串按字段名处理。"""
    if depth > 6:
        return _redact_text(field)
    if isinstance(value, str):
        # 多行字符串几乎必然是代码/命令/长文本 → 一律替换（P0-03 泄露根因：
        # bash 工具把完整命令写进 recovery.target_path）。
        if "\n" in value:
            return _redact_text(field)
        if field in PATH_FIELDS:
            return _redact_path(field, value)
        if field in TEXT_FIELDS:
            return _redact_text(field)
        # 其他字符串：若是疑似路径/URL/用户名，脱敏；否则保留（如状态名、reason）。
        if "\\" in value or re.match(r"^[A-Za-z]:[\\/]", value) or value.startswith("/"):
            return _redact_path(field, value)
        return value
    if isinstance(value, list):
        return [_scrub_value(field, item, depth + 1) for item in value]
    if isinstance(value, dict):
        return {k: _scrub_value(k, v, depth + 1) for k, v in value.items()}
    return value


def scrub_line(line: str) -> str:
    ev = json.loads(line)
    payload = ev.get("payload")
    if isinstance(payload, dict):
        ev["payload"] = _scrub_value("payload", payload)
    return json.dumps(ev, ensure_ascii=False, separators=(",", ":"))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", required=True, help="真实 session jsonl")
    ap.add_argument("--dst", required=True, help="脱敏输出路径")
    args = ap.parse_args()

    src = Path(args.src)
    dst = Path(args.dst)
    dst.parent.mkdir(parents=True, exist_ok=True)

    _counter["n"] = 0
    with open(src, "r", encoding="utf-8") as fin, open(dst, "w", encoding="utf-8") as fout:
        for line in fin:
            line = line.rstrip("\n")
            if not line.strip():
                continue
            fout.write(scrub_line(line) + "\n")

    digest = hashlib.sha256(dst.read_bytes()).hexdigest()[:16]
    n_lines = sum(1 for _ in dst.open(encoding="utf-8"))
    print(f"scrubbed -> {dst}  ({n_lines} lines, sha256[:16]={digest})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
