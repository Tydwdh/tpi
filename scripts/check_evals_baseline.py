#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""校验 Eval 任务基线（一次性工具，不花钱）。

对每个任务：
1. 在初始（未修复）状态下跑 expected.toml 的 bash 断言，
   要求至少一个 FAIL（任务确实需要修复）；
2. 输出每个任务的初始状态断言结果表。

用法：python scripts/check_evals_baseline.py
"""
import io, os, re, subprocess, sys, tomllib

ROOT = os.path.join(os.path.dirname(__file__), "..", "evals")

def run(cmd, cwd):
    try:
        p = subprocess.run(["bash", "-lc", cmd], cwd=cwd, capture_output=True,
                           text=True, timeout=120)
        return p.returncode, p.stdout or "", p.stderr or ""
    except subprocess.TimeoutExpired:
        return -1, "", "timeout"

def main():
    bad = []
    total = 0
    for task_id in sorted(os.listdir(ROOT)):
        d = os.path.join(ROOT, task_id)
        expected_path = os.path.join(d, "expected.toml")
        repo = os.path.join(d, "repo")
        if not os.path.isfile(expected_path):
            continue
        with io.open(expected_path, "rb") as f:
            expected = tomllib.load(f)
        total += 1
        # 先重置 repo（生成器已提交初始状态；重置以防现场被改）
        target = expected.get("base_commit", "HEAD")
        subprocess.run(["git", "reset", "--hard", "-q", target], cwd=repo, check=True,
                       stdout=subprocess.DEVNULL)
        subprocess.run(["git", "clean", "-fdx", "-q"], cwd=repo, check=True,
                       stdout=subprocess.DEVNULL)
        results = []
        for v in expected.get("verify", []):
            if v.get("type") != "bash":
                # 文件断言：按存在/包含判断
                path = os.path.join(repo, v["path"])
                if v["type"] == "file_exists":
                    passed = os.path.exists(path)
                else:
                    passed = False
                    try:
                        with io.open(path, encoding="utf-8") as f:
                            passed = v["contains"] in f.read()
                    except OSError:
                        passed = False
                results.append(("FILE", v["type"], passed))
                continue
            code, out, err = run(v["command"], repo)
            ok = True
            detail = []
            if v.get("expect_exit", 0) != code:
                ok = False
                detail.append(f"exit {code}")
            for needle in v.get("expect_stdout_contains", []):
                if needle not in out:
                    ok = False
                    detail.append(f"stdout!~{needle!r}")
            for needle in v.get("expect_stderr_contains", []):
                if needle not in err:
                    ok = False
                    detail.append(f"stderr!~{needle!r}")
            results.append((v["command"], "; ".join(detail) if detail else "", ok))
        fails = [r for r in results if not r[2]]
        status = "OK " if fails else "BAD"
        print(f"[{status}] {task_id}: {len(results)} 断言, 初始失败 {len(fails)}")
        if not fails:
            bad.append(task_id)
    print(f"\n共 {total} 个任务；初始状态断言全部通过（任务无修复必要）: {bad or '无'}")
    if bad:
        sys.exit(1)

main()
