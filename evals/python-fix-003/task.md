counter.py 的 top_words 词频统计错误：`counts.get(word, 1)` 导致
每个词至少计 1 次。修复使 `python counter_test.py` 全部断言通过。