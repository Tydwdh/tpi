#!/usr/bin/env python3
"""示例 skill 脚本：查询 Bevy Player 实体（示意）。

Skill 的 scripts/ 由 Agent 通过已有 bash 工具调用（README2 §23：
能组合现有 primitive，就不要创造新的 primitive）。
"""

import json
import sys

def main():
    query = {"type": "query", "target": "Player", "component": "Transform"}
    print(json.dumps(query, ensure_ascii=False))

if __name__ == "__main__":
    main()
