parser.py 的 parse() 静默吞掉非法片段（如 "broken"），与接口契约
（非法输入抛 ValueError）不符。修复使 `python parser_test.py` 通过。