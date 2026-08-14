# P0-01 Baseline Manifest（2202887）

> 生成日期：2026-08-14（执行 P0-01/P0-02 时记录）。
> 本文件是迁移的**可复现基线**：任何 Phase 开始前，先核对
> `git status`、`rustc/cargo` 与本文件；结果不一致时必须更新本文件，
> 不能把本文对环境的判断当成永远正确（见 `README.md` §1）。

## 1. 提交与工作树

| 项 | 值 |
|---|---|
| HEAD | `2202887355174aa49b4796b2d86178b9c1dff9ef`（`2202887 chore: 收尾清理——提交未完成功能、补齐文档、清偿 fmt/clippy 债务`） |
| 工作树 | 有未提交修改（见 §4），**未 commit、未 stash** |

## 2. 工具链与环境

| 项 | 值 |
|---|---|
| rustc | 1.97.1（`8bab26f4f`） |
| cargo | 1.97.1（`c980f4866`） |
| rust-toolchain.toml | pin 1.97.1（与 CI 一致） |
| OS | Windows |
| 终端/执行环境 | Git Bash（本 manifest 的验证命令均在 Git Bash 下执行） |
| edition | 2024，`rust-version = "1.97"` |

## 3. 构建与测试基线（P0-02 修复后）

命令（`--all-features`）：

| 检查 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo check --all-targets --all-features` | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings -D clippy::undocumented_unsafe_blocks` | PASS |
| `cargo test --all-targets --all-features --no-fail-fast` | **全绿**：36 个测试目标 result 行全部 `ok`，合计约 917 passed / 3 ignored / 0 failed |

修复前（`00-current-state-audit.md` §5 记录）：同一测试命令 **24 个失败**，
全部源于 `tests/fixtures/remote_server.rs` 找不到 `cygpath`
（agent_remote 2、remote_bash 5、remote_contract 6、remote_files 6、remote_traverse 5）。
修复后该 5 个 target 全部通过。3 个 ignored 为 real-API opt-in 测试
（无需凭据环境自动跳过）。

## 4. 工作树未提交修改（用户资产）

P0-02 修复前已存在（audit §6 记录，本轮未触碰、未回滚）：

```text
 M src/agent/tool_runtime.rs
 M src/app.rs
 M src/tui/model.rs
 M src/tui/model/model_tests.rs
 M src/tui/reducer.rs
 M src/tui/tests.rs
```

内容：request_input 卡片主行携带问题摘要、挂起提示块级去重等 UX 优化。
后续 Phase 若需触碰同一文件，先基于新提交重做审计。

## 5. 本轮 P0-02 修改（cygpath 隐式前提修复）

```text
 M tests/fixtures/remote_server.rs  新增 win_to_posix()（纯 Rust，cygpath -u 最小等价）；
                                    start_test_server() 改用它
 M tests/agent_remote.rs           cygpath -u → fixtures::remote_server::win_to_posix
 M tests/remote_bash.rs            （同上）
 M tests/remote_files.rs           （同上）
 M tests/remote_traverse.rs        （同上）
```

- 根因：fixture server 与各 remote 测试各自 `Command::new("cygpath")` 且无
  fallback（`.expect("cygpath")`），Windows 无 Git Bash 的 PATH 时 server 起不来，
  导致 5 个 target 共 24 个用例失败。
- 修复：server 与 client 改用**同一个**纯 Rust 转换（盘符→`/c`、UNC→`//`、
  POSIX 原样），绝对路径前缀匹配天然一致；不依赖外部工具。
- 验证：5 个 remote target 全绿；新增 4 个 `win_to_posix` 黄金对拍单测；
  与真实 `cygpath -u` 对拍（`C:\Users\foo\Temp\abc`→`/c/Users/foo/Temp/abc`、
  `D:/Work/x.rs`→`/d/Work/x.rs`、`C:\`→`/c/`）一致。
- 回滚：仅恢复 fixture adapter（git revert 该 5 个文件的 diff），
  不改 production remote 语义。
- 生产侧 `src/tool/command.rs` / `src/tool/mod.rs` 的 `cygpath -w` 已有
  fallback 降级，不在本轮范围。

## 6. 依赖树摘要（P0-06 输入）

`cargo tree -d` 主要重复项：

- `aead 0.5.2`（经 `ssh-key 0.6.7 → russh-keys 0.49.2`）与 `aead 0.6.1`（经 russh 栈）并存；
- `ssh-key 0.7.0-rc.11`（russh 0.62.6 栈）与 `ssh-key 0.6.7`（russh-keys）并存；
- `ed25519 3.0.0` / `ed25519-dalek 3.0.0` 与 `ssh-key 0.7.0-rc.11` 的 rc 版本并存。

佐证 audit `Low-1`（`russh-keys` 疑似直接未使用、重复加密栈）；
P0-06 用 `cargo machete` + 人工确认后决定删除，不能只凭扫描删。

## 7. 结构与 target 摘要

- target 共 41 个：lib `tpi`、bin `tpi`、39 个 integration test。
- src 约 48,000 行；测试约 14,000 行（`wc -l` 快照，非质量目标）。
- 高修改压力文件（audit §3 已列）：`src/app.rs` 2,910、`src/tui/mod.rs` 3,261、
  `src/agent/mod.rs` 1,756、`src/tool/edit.rs` 2,584。

## 8. 复现命令（另一台 CI 机器可直接执行）

```bash
git checkout 2202887355174aa49b4796b2d86178b9c1dff9ef
rustup toolchain install 1.97.1
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings -D clippy::undocumented_unsafe_blocks
cargo test --all-targets --all-features --no-fail-fast
```

环境缺失（无 Rust 1.97 / 无 Git Bash）与代码失败通过上述结果区分：
无 Git Bash 时 remote 测试**不再**失败（fixture 不依赖 cygpath）。
