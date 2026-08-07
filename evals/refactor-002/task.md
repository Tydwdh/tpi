refactor：pricing.py 里的阈值（100/500）与税率（0.13）是散落的魔法数字。
提取为模块级常量 DISCOUNT_LOW/DISCOUNT_HIGH/TAX_RATE 并使用。
`python pricing_test.py` 保持通过。