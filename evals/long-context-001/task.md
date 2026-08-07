src/lib.rs 有 600+ 行自动生成的函数。其中 `calculate` 的求和
范围有 off-by-one（1..n 应为 1..=n）。定位并修复。不要修改测试。
`cargo test` 全绿。