# 错误处理要点（Rust）

- 库代码：`thiserror` 定义领域错误；不要用 `anyhow`（丢弃调用方上下文）。
- 二进制/胶水：`anyhow` 合理。
- 避免：`unwrap()`（改用 `?` / 显式 `ok_or` / 防御性分支）。
- `#[cfg(test)]` 内允许 `unwrap`。
- 对外 API 返回 `Result` 而非 `Option` 表示「可能失败」。
