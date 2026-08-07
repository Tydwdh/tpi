inventory.py 的 apply_batch_snapshot 声称失败回滚，但实际有 bug：
快照只保存了 items 引用副本？检查并修复使 `python inventory_test.py` 通过。