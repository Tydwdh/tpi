src/main.rs 的 find_index 有一个会在运行时 panic 的 unwrap。定位并
修复（保持 Option 语义）。`cargo build` 通过且运行输出两行（第二行 None）。