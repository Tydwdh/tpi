# Session Golden Corpus（P0-03）

来自用户真实 `~/.tpi/sessions` 的**脱敏副本**，用于验证 session reader
在重构期间保持行为不变（P2 拆 session 存储、P10 迁移的回归网）。

## 文件

| fixture | 来源 session | 行数 | 覆盖 lifecycle |
|---|---|---|---|
| `001_tool_loop.jsonl` | 019ff96f | 321 | 多 run、tool 循环、plan_replaced |
| `002_stream_interrupted.jsonl` | 019fe22e | 36 | assistant_attempt_interrupted |
| `003_awaiting_input.jsonl` | 01a00078 | 526 | user_input_requested + interrupted |
| `004_compaction_segment.jsonl` | 019ffaba（段） | 56 | compaction_committed（完整 run） |
| `005_corrupt_tail.jsonl` | 019fe22e（合成） | 37 | 尾部未完成行（reader 截断） |
| `006_corrupt_middle.jsonl` | 019fe22e（合成） | 37 | 中间坏行（reader 报 InvalidData） |

完整元数据（来源、行数、blake3）见 `manifest.json`。

## 脱敏规则（确定性：同一输入 → 同一输出）

由 `scripts/scrub_session.py` 生成，规则：

1. **保留 envelope 结构**：schema/seq/event_id/timestamp/session_id/run_id/type
   （随机标识符与时间戳，非隐私；保留用于验证 seq 递增/session 一致/event_id 唯一）。
2. **payload 字符串按字段替换**：
   - 文本类字段（content/text/output/command/arguments/prompt/…）→ `REDACTED_<FIELD>_<n>`
   - 路径类字段（path/target/temp_path/backup_path/root/…）→ `/workspace/<rel>`（折叠绝对前缀）
   - **多行字符串（含换行）一律替换**——代码/命令/长文本几乎必然是内容
     （P0-03 发现的泄露根因：bash 工具把完整命令写进 `recovery.target_path`）
3. **保留标量语义**：status/exit_code/行数/时长/token 数/reason/模型名（非隐私）。
4. **不记录**：绝对路径、用户名、API key、真实代码/文本。

原始文件从未被修改；fixture 可随时用 `scripts/scrub_session.py` 重新生成。

## 使用

```bash
cargo test --test session_golden
```

测试断言：真实 fixture 完整 replay（事件数 = manifest 行数）、seq 严格递增、
特殊 lifecycle 可还原、corrupt tail 截断、corrupt middle 报错、blake3 与 manifest 一致。

## 来源与 hash

`manifest.json` 记录每个 fixture 的 `src_session`（真实 session 短 id）、
`src_lines`（源行数）、`fixture_lines`、`blake3`。任何 fixture 被手改
→ `manifest_hashes_are_stable` 失败。
