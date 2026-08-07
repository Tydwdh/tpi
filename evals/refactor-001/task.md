refactor：average_a 与 average_b 完全重复。提取一个 `stats(scores) -> (f64, i32)`
返回 (均值, 最大值)，让两个函数都调用它。`cargo test` 保持全绿。