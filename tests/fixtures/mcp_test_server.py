#!/usr/bin/env python3
"""MCP 测试 Server（README2 §30：echo/add/fail/sleep 四个工具）。

用 Python 标准库实现 MCP stdio transport（newline-delimited JSON-RPC 2.0），
不依赖 mcp 包，供集成测试可靠验证：正常调用 / 参数 / 错误 / timeout / crash。

运行: python mcp_test_server.py
"""

import json
import sys
import time


def send(msg):
    sys.stdout.write(json.dumps(msg, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def send_result(msg_id, result):
    send({"jsonrpc": "2.0", "id": msg_id, "result": result})


def send_error(msg_id, code, message):
    send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": message}})


def text_content(text, is_error=False):
    return {
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
    }


TOOLS = [
    {
        "name": "echo",
        "description": "原样返回传入的 text",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "add",
        "description": "计算 a + b",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"],
        },
    },
    {
        "name": "fail",
        "description": "总是返回工具错误",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "sleep",
        "description": "休眠指定毫秒后返回（测试 timeout）",
        "inputSchema": {
            "type": "object",
            "properties": {"ms": {"type": "number"}},
            "required": ["ms"],
        },
    },
]


def handle_call(name, arguments):
    if name == "echo":
        text = arguments.get("text", "")
        return text_content(f"echo: {text}")
    if name == "add":
        return text_content(str(arguments.get("a", 0) + arguments.get("b", 0)))
    if name == "fail":
        return text_content("工具执行失败（预期）", is_error=True)
    if name == "sleep":
        ms = int(arguments.get("ms", 0))
        time.sleep(ms / 1000.0)
        return text_content(f"slept {ms}ms")
    raise ValueError(f"未知工具: {name}")


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError as e:
            send_error(0, -32700, f"解析失败: {e}")
            continue
        method = msg.get("method")
        msg_id = msg.get("id")
        if method == "initialize":
            send_result(msg_id, {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mcp-test-server", "version": "1.0.0"},
            })
        elif method == "notifications/initialized":
            # 通知无响应。
            continue
        elif method == "tools/list":
            send_result(msg_id, {"tools": TOOLS})
        elif method == "tools/call":
            params = msg.get("params", {})
            name = params.get("name", "")
            arguments = params.get("arguments", {})
            try:
                result = handle_call(name, arguments)
                if result.get("isError"):
                    # isError 是 result 内容的一部分；MCP 语义：仍返回 result。
                    send_result(msg_id, result)
                else:
                    send_result(msg_id, result)
            except Exception as e:  # noqa: BLE001
                send_error(msg_id, -32602, str(e))
        elif method == "shutdown":
            send_result(msg_id, None)
        else:
            send_error(msg_id, -32601, f"未知方法: {method}")


if __name__ == "__main__":
    main()
