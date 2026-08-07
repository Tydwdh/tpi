#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""TPI Eval Harness 任务生成器（一次性工具）。

生成 evals/<task-id>/{task.md, expected.toml, repo/}，每个 repo 独立
git 仓库并提交初始 commit（可重置现场）。运行：python scripts/gen_evals.py
"""
import io, os, subprocess, sys

ROOT = os.path.join(os.path.dirname(__file__), "..", "evals")
os.makedirs(ROOT, exist_ok=True)

def w(path, content):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with io.open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(content)

def git(repo, *args):
    subprocess.run(["git", *args], cwd=repo, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

def make_task(task_id, suite, title, files, task_md, verify, base_commit=None, timeout=900):
    d = os.path.join(ROOT, task_id)
    repo = os.path.join(d, "repo")
    os.makedirs(repo, exist_ok=True)
    for rel, content in files.items():
        w(os.path.join(repo, rel), content)
    git(repo, "init", "-q", "-b", "main")
    git(repo, "add", ".")
    git(repo, "-c", "user.name=eval", "-c", "user.email=eval@local",
        "commit", "-q", "-m", "init")
    w(os.path.join(d, "task.md"), task_md)
    toml = [f'name = "{task_id}"', f'suite = "{suite}"',
            f'title = "{title}"', f'timeout_sec = {timeout}']
    if base_commit:
        toml.append(f'base_commit = "{base_commit}"')
    toml.append("")
    for v in verify:
        toml.append("[[verify]]")
        toml.append(v)
    w(os.path.join(d, "expected.toml"), "\n".join(toml) + "\n")
    print(f"  {task_id} [{suite}] {title}")

# 通用 verify 片段

def bash_ok(cmd, out=None, err=None, exit_code=0):
    lines = [f'type = "bash"', f'command = {cmd!r}']
    if exit_code is not None:
        lines.append(f"expect_exit = {exit_code}")
    if out:
        # TOML 数组（serde 期望 Vec<String>）
        lines.append(f"expect_stdout_contains = [{out!r}]")
    if err:
        lines.append(f"expect_stderr_contains = [{err!r}]")
    return "\n".join(lines)

def file_exists(path):
    return "\n".join([f'type = "file_exists"', f'path = {path!r}'])

def file_contains(path, contains):
    return "\n".join([f'type = "file_contains"', f'path = {path!r}',
                      f'contains = {contains!r}'])

print("生成 Eval 任务...")

# ============ core：Rust 修复 ============
make_task(
    "rust-fix-001", "core",
    "修复借用检查错误（Rust 编译失败）",
    {
        "src/main.rs": '''// 目标：让 `cargo build` 通过。
// 当前编译器报借用/移动错误。不要改变程序行为，只修编译问题。
fn main() {
    let mut data = vec![1, 2, 3];
    let first = &data[0];
    data.push(4); // 这里与上面的不可变借用冲突
    println!("first={} len={}", first, data.len());
}
''',
        "Cargo.toml": '[package]\nname = "rust_fix_001"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/main.rs 无法编译（借用检查错误）。修复它，使 `cargo build` 通过。\n不要改变程序输出语义（first=1 len=4）。",
    [bash_ok("cargo build", err="Finished", exit_code=0),
     bash_ok("./target/debug/rust_fix_001.exe", out="first=1 len=4")],
)

make_task(
    "rust-fix-002", "core",
    "修复二分查找 off-by-one（测试失败）",
    {
        "src/lib.rs": '''// 目标：`cargo test` 全绿。
pub fn binary_search(sorted: &[i32], target: i32) -> Option<usize> {
    let mut lo = 0;
    let mut hi = sorted.len(); // 左闭右开
    while lo < hi {
        let mid = (lo + hi) / 2;
        // bug：比较方向反了（> 应为 <），导致查找失败
        if sorted[mid] > target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if sorted.get(lo) == Some(&target) { Some(lo) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_first_element() { assert_eq!(binary_search(&[1, 3, 5, 7], 1), Some(0)); }
    #[test]
    fn finds_last_element() { assert_eq!(binary_search(&[1, 3, 5, 7], 7), Some(3)); }
    #[test]
    fn finds_middle() { assert_eq!(binary_search(&[1, 3, 5, 7, 9], 5), Some(2)); }
    #[test]
    fn missing_reports_none() { assert_eq!(binary_search(&[1, 3, 5], 4), None); }
    #[test]
    fn empty_slice() { assert_eq!(binary_search(&[], 1), None); }
}
''',
        "Cargo.toml": '[package]\nname = "rust_fix_002"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/lib.rs 的 binary_search 有 bug：某些测试失败。修复算法使全部测试通过。\n不要修改测试代码。",
    [bash_ok("cargo test", out="test result: ok")],
)

make_task(
    "rust-fix-003", "core",
    "修复 fizzbuzz 边界错误（测试失败）",
    {
        "src/lib.rs": '''// 目标：`cargo test` 全绿。
// 返回 1..=n 的 fizzbuzz 序列：3 的倍数 "Fizz"，5 的倍数 "Buzz"，同时 "FizzBuzz"。
pub fn fizzbuzz(n: u32) -> Vec<String> {
    (1..n).map(|i| {
        if i % 15 == 0 { "FizzBuzz".to_string() }
        else if i % 3 == 0 { "Fizz".to_string() }
        else if i % 5 == 0 { "Buzz".to_string() }
        else { i.to_string() }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn includes_n() { assert_eq!(fizzbuzz(1), vec!["1"]); }
    #[test]
    fn fizz_at_3() { assert_eq!(fizzbuzz(3), vec!["1", "2", "Fizz"]); }
    #[test]
    fn buzz_at_5() { assert_eq!(fizzbuzz(5), vec!["1", "2", "Fizz", "4", "Buzz"]); }
    #[test]
    fn fizzbuzz_at_15() {
        let v = fizzbuzz(15);
        assert_eq!(v.len(), 15);
        assert_eq!(v[14], "FizzBuzz");
    }
}
''',
        "Cargo.toml": '[package]\nname = "rust_fix_003"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/lib.rs 的 fizzbuzz 序列少了一项（区间边界错误）。修复使全部测试通过。\n不要修改测试代码。",
    [bash_ok("cargo test", out="test result: ok")],
)

make_task(
    "rust-fix-004", "core",
    "修复 String 类型错误（编译+测试）",
    {
        "src/lib.rs": '''// 目标：`cargo build` 与 `cargo test` 都通过。
// 把一句话按词翻转：如 "a b c" -> "c b a"。
pub fn reverse_words(sentence: &str) -> String {
    let words: Vec<&str> = sentence.split_whitespace().collect();
    let mut out = String::new();
    for word in words.iter().rev() {
        out.push_str(word);
        out.push_str(" "); // 末尾多一个空格（可能是问题点之一）
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reverses_three_words() { assert_eq!(reverse_words("a b c"), "c b a"); }
    #[test]
    fn single_word_no_trailing_space() { assert_eq!(reverse_words("hello"), "hello"); }
    #[test]
    fn empty_sentence() { assert_eq!(reverse_words(""), ""); }
}
''',
        "Cargo.toml": '[package]\nname = "rust_fix_004"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/lib.rs 有两个问题：末尾多余空格导致测试失败；另外该函数在\n某个调用点有类型错误（把 &str 当 String 用）。修复使全部测试通过。\n不要修改测试代码。",
    [bash_ok("cargo test", out="test result: ok")],
)

make_task(
    "rust-fix-005", "core",
    "修复并发求和 bug（结果不稳定）",
    {
        "src/main.rs": '''// 目标：`cargo build` 通过且程序稳定输出 1000000。
// 多线程累加 0..1000 每段 1000 次，结果应是 1000000，
// 但当前输出不稳定（数据竞争）。
use std::thread;

fn main() {
    let mut total = 0u64;
    let handles: Vec<_> = (0..1000).map(|_| {
        thread::spawn(move || {
            let mut local = 0u64;
            for i in 0..1000u64 { local += i; }
            local
        })
    }).collect();
    for h in handles {
        total += h.join().unwrap();
    }
    println!("total={total}");
}
''',
        "Cargo.toml": '[package]\nname = "rust_fix_005"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/main.rs 声称多线程累加但结果与预期不符（每段 0..1000 求和\n应该是 499500*1000 = 499500000）。检查逻辑：任务描述要求输出 total=499500000。\n修复计算逻辑使输出正确。",
    [bash_ok("cargo build", out="Finished"),
     bash_ok("./target/debug/rust_fix_005.exe", out="total=499500000")],
)

# ============ core：Python 修复 ============
make_task(
    "python-fix-001", "core",
    "修复排序 bug（Python）",
    {
        "sorting.py": '''# 目标：python sorting_test.py 通过。
# 把 (name, score) 按 score 降序排列，分数相同按 name 升序。
def rank(entries):
    return sorted(entries, key=lambda e: e[1], reverse=True)
''',
        "sorting_test.py": '''from sorting import rank

def check():
    data = [("bob", 3), ("alice", 3), ("carol", 5)]
    out = rank(data)
    assert out[0] == ("carol", 5), out
    # 分数相同必须按 name 升序（当前实现没做第二键）
    assert out[1] == ("alice", 3), out
    assert out[2] == ("bob", 3), out
    assert rank([]) == []
    print("all tests passed")

check()
''',
    },
    "sorting.py 的 rank() 排序结果不稳定：分数相同时没有按 name 升序。\n修复使 `python sorting_test.py` 全部断言通过。",
    [bash_ok("python sorting_test.py", out="all tests passed")],
)

make_task(
    "python-fix-002", "core",
    "修复吞异常 bug（Python）",
    {
        "parser.py": '''# 目标：python parser_test.py 通过。
# 把 "k=v;k=v" 解析为 dict；非法键值对应报错而不是被吞掉。
def parse(s):
    result = {}
    for part in s.split(";"):
        try:
            k, v = part.split("=", 1)
            result[k.strip()] = v.strip()
        except ValueError:
            pass  # bug：静默吞掉非法片段
    return result
''',
        "parser_test.py": '''from parser import parse

def check():
    assert parse("a=1;b=2") == {"a": "1", "b": "2"}
    try:
        parse("a=1;broken")
        raise AssertionError("非法片段必须抛 ValueError")
    except ValueError:
        pass
    assert parse(" a = 1 ; b = 2 ") == {"a": "1", "b": "2"}
    print("all tests passed")

check()
''',
    },
    "parser.py 的 parse() 静默吞掉非法片段（如 \"broken\"），与接口契约\n（非法输入抛 ValueError）不符。修复使 `python parser_test.py` 通过。",
    [bash_ok("python parser_test.py", out="all tests passed")],
)

make_task(
    "python-fix-003", "core",
    "修复 dict.get 默认值 bug（Python）",
    {
        "counter.py": '''# 目标：python counter_test.py 通过。
# 统计词频，返回出现次数 >= threshold 的词（按次数降序）。
def top_words(text, threshold=1):
    counts = {}
    for word in text.split():
        counts[word] = counts.get(word, 1)  # bug：默认值应为 0 再加 1
    return sorted(
        (w for w, c in counts.items() if c >= threshold),
        key=lambda w: (-counts[w], w),
    )
''',
        "counter_test.py": '''from counter import top_words

def check():
    assert top_words("a b a c a b") == ["a", "b", "c"]
    assert top_words("a b a c a b", threshold=2) == ["a", "b"]
    assert top_words("x x y") == ["x", "y"]  # y 出现 1 次也应列出（threshold=1）
    print("all tests passed")

check()
''',
    },
    "counter.py 的 top_words 词频统计错误：`counts.get(word, 1)` 导致\n每个词至少计 1 次。修复使 `python counter_test.py` 全部断言通过。",
    [bash_ok("python counter_test.py", out="all tests passed")],
)

make_task(
    "python-fix-004", "core",
    "修复正则捕获 bug（Python）",
    {
        "extract.py": '''# 目标：python extract_test.py 通过。
# 从日志行提取 "key=value"（value 可能含空格，用引号包裹）。
import re

def extract(line):
    # bug：引号内的空格被截断
    m = re.search(r'key=(\\S+)', line)
    return m.group(1) if m else None
''',
        "extract_test.py": '''from extract import extract

def check():
    assert extract("key=hello") == "hello"
    assert extract("key=\\"two words\\"") == "two words"
    assert extract("no value here") is None
    assert extract("a=1 key=last") == "last"
    print("all tests passed")

check()
''',
    },
    "extract.py 的 extract() 无法处理引号包裹的含空格 value。修复正则\n使 `python extract_test.py` 全部断言通过。",
    [bash_ok("python extract_test.py", out="all tests passed")],
)

# ============ core：JS 修复 ============
make_task(
    "js-fix-001", "core",
    "修复数组去重 bug（JavaScript）",
    {
        "dedup.js": '''// 目标：node dedup_test.js 通过。
// 按 id 字段去重，保留首次出现。
function dedup(items) {
  const seen = new Set();
  return items.filter((item) => {
    if (!seen.has(item.id)) {
      seen.add(item);
      return true;
    }
    return false;
  });
}

module.exports = { dedup };
''',
        "dedup_test.js": '''const { dedup } = require("./dedup");

function check() {
  const out = dedup([{ id: 1 }, { id: 2 }, { id: 1 }, { id: 3 }]);
  if (out.length !== 3) throw new Error("expected 3 unique, got " + out.length);
  if (out[0].id !== 1 || out[1].id !== 2 || out[2].id !== 3)
    throw new Error("unexpected order");
  console.log("all tests passed");
}

check();
''',
    },
    "dedup.js 的去重逻辑有 bug：Set 存入的是整个对象而不是 id。修复使\n`node dedup_test.js` 通过。",
    [bash_ok("node dedup_test.js", out="all tests passed")],
)

make_task(
    "js-fix-002", "core",
    "修复异步时序 bug（JavaScript）",
    {
        "fetch_order.js": '''// 目标：node fetch_order_test.js 通过。
// 并行请求，但要求按请求顺序输出（先发出的先输出）。
async function ordered(items) {
  const results = [];
  for (const item of items) {
    // bug：不等待完成就把结果按完成顺序推入；且函数立即返回（空数组）
    fetch(item).then((r) => results.push(r));
  }
  return results;
}

async function fetch(item) {
  await new Promise((res) => setTimeout(res, item.delay));
  return item.value;
}

module.exports = { ordered, fetch };
''',
        "fetch_order_test.js": '''const { ordered } = require("./fetch_order");

async function check() {
  const items = [
    { value: "first", delay: 30 },
    { value: "second", delay: 5 },
    { value: "third", delay: 10 },
  ];
  const out = await ordered(items);
  if (out.join(",") !== "first,second,third")
    throw new Error("顺序错误: " + out.join(","));
  console.log("all tests passed");
}

check().catch((e) => { console.error(e); process.exit(1); });
''',
    },
    "fetch_order.js 的 ordered() 串行 await 导致慢请求阻塞快请求，\n输出顺序仍正确但总耗时是串行的。任务：修复为并行发起全部请求但**按请求顺序**\n返回结果（Promise.all 保序）。使 `node fetch_order_test.js` 通过。",
    [bash_ok("node fetch_order_test.js", out="all tests passed")],
)

# ============ core：Shell ============
make_task(
    "shell-fix-001", "core",
    "修复 bash 参数解析 bug",
    {
        "greet.sh": '''#!/usr/bin/env bash
# 目标：bash greet.sh 通过；未加引号的 "$@" 会拆词。
# 用法：greet.sh <name...>  输出 "Hello, <name1> <name2>!"
names=$@
# bug：无引号展开会折叠连续空格（IFS 拆词）
echo Hello, $names!
''',
        "greet_test.sh": '''#!/usr/bin/env bash
# 双空格：未加引号的 $@ 会被 IFS 拆词并折叠空格
out=$(bash greet.sh "Alice  Smith")
[ "$out" = "Hello, Alice  Smith!" ] || { echo "FAIL: $out"; exit 1; }
echo "all tests passed"
''',
    },
    "greet.sh 的参数拼接没有引号保护：带空格的名字会被拆成多个词。\n修复使 `bash greet_test.sh` 通过（\"Alice  Smith\" 的双空格必须保留）。",
    [bash_ok("bash greet_test.sh", out="all tests passed")],
)

# ============ feature：实现类 ============
make_task(
    "feature-001", "core",
    "实现 dedup 函数（Rust，测试先行）",
    {
        "src/lib.rs": '''// 目标：`cargo test` 全绿。
// 实现 `dedup`：保留首次出现的元素（保持顺序）。
pub fn dedup(items: &[u32]) -> Vec<u32> {
    todo!("实现 dedup")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removes_duplicates_keeping_first_order() {
        assert_eq!(dedup(&[1, 2, 1, 3, 2]), vec![1, 2, 3]);
    }
    #[test]
    fn empty_input() { assert_eq!(dedup(&[]), Vec::<u32>::new()); }
    #[test]
    fn all_duplicates() { assert_eq!(dedup(&[7, 7, 7]), vec![7]); }
}
''',
        "Cargo.toml": '[package]\nname = "feature_001"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/lib.rs 的 `dedup` 未实现（todo!）。实现它：保留首次出现的元素、\n保持顺序。不要修改测试。使 `cargo test` 全绿。",
    [bash_ok("cargo test", out="test result: ok")],
)

make_task(
    "feature-002", "core",
    "实现 is_palindrome（Rust，测试先行）",
    {
        "src/lib.rs": '''// 目标：`cargo test` 全绿。
// 实现 `is_palindrome`：忽略大小写与空白，判断是否回文。
pub fn is_palindrome(text: &str) -> bool {
    todo!("实现 is_palindrome")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simple() { assert!(is_palindrome("racecar")); }
    #[test]
    fn mixed_case_and_spaces() { assert!(is_palindrome("A man a plan a canal Panama")); }
    #[test]
    fn not_palindrome() { assert!(!is_palindrome("hello")); }
    #[test]
    fn empty() { assert!(is_palindrome("")); }
}
''',
        "Cargo.toml": '[package]\nname = "feature_002"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/lib.rs 的 `is_palindrome` 未实现（todo!）。实现：忽略大小写与非\n字母数字字符。不要修改测试。使 `cargo test` 全绿。",
    [bash_ok("cargo test", out="test result: ok")],
)

make_task(
    "feature-003", "core",
    "实现 parse_duration（Python，测试先行）",
    {
        "duration.py": '''# 目标：python duration_test.py 通过。
# 解析 "1h30m" / "45m" / "30s" 为秒数；非法输入抛 ValueError。
def parse_duration(text):
    raise NotImplementedError("实现 parse_duration")
''',
        "duration_test.py": '''from duration import parse_duration

def check():
    assert parse_duration("1h30m") == 5400
    assert parse_duration("45m") == 2700
    assert parse_duration("30s") == 30
    assert parse_duration("2h") == 7200
    assert parse_duration("1h1m1s") == 3661
    try:
        parse_duration("abc")
        raise AssertionError("非法输入必须抛 ValueError")
    except ValueError:
        pass
    print("all tests passed")

check()
''',
    },
    "duration.py 的 parse_duration 未实现。实现：支持 h/m/s 组合，\n顺序固定 h→m→s；非法输入抛 ValueError。使 `python duration_test.py` 通过。",
    [bash_ok("python duration_test.py", out="all tests passed")],
)

make_task(
    "feature-004", "core",
    "实现 top_k（Python，测试先行）",
    {
        "topk.py": '''# 目标：python topk_test.py 通过。
# 返回频率最高的 k 个元素（频率相同按首次出现顺序）。
def top_k(items, k):
    raise NotImplementedError("实现 top_k")
''',
        "topk_test.py": '''from topk import top_k

def check():
    assert top_k(["a", "b", "a", "c", "a", "b"], 2) == ["a", "b"]
    assert top_k([1, 1, 2, 3, 3, 3], 1) == [3]
    assert top_k(["x"], 5) == ["x"]
    assert top_k([], 2) == []
    print("all tests passed")

check()
''',
    },
    "topk.py 的 top_k 未实现。实现：按频率降序取前 k 个（频率相同按\n首次出现顺序；k 大于元素数时返回全部）。使 `python topk_test.py` 通过。",
    [bash_ok("python topk_test.py", out="all tests passed")],
)

make_task(
    "feature-005", "core",
    "实现 debounce（JavaScript，测试先行）",
    {
        "debounce.js": '''// 目标：node debounce_test.js 通过。
// 实现 debounce：连续调用时只执行最后一次（等待 waitMs 后）。
function debounce(fn, waitMs) {
  // 实现 debounce
}

module.exports = { debounce };
''',
        "debounce_test.js": '''const { debounce } = require("./debounce");

async function check() {
  let count = 0;
  const d = debounce(() => { count++; }, 30);
  d(); d(); d();
  await new Promise((res) => setTimeout(res, 10));
  d();
  await new Promise((res) => setTimeout(res, 60));
  if (count !== 1) throw new Error("expected 1 call, got " + count);
  console.log("all tests passed");
}

check().catch((e) => { console.error(e); process.exit(1); });
''',
    },
    "debounce.js 的 debounce 未实现。实现：waitMs 内的连续调用只执行\n最后一次。使 `node debounce_test.js` 通过。",
    [bash_ok("node debounce_test.js", out="all tests passed")],
)

# ============ refactor：重构类 ============
make_task(
    "refactor-001", "core",
    "消除重复代码（Rust）",
    {
        "src/lib.rs": '''// 目标：`cargo test` 全绿 + 新增一个 `stats` 辅助函数。
// 两处统计逻辑完全重复（均值+最大），提取为共用函数。
pub fn average_a(scores: &[i32]) -> f64 {
    let sum: i32 = scores.iter().sum();
    let max = scores.iter().max().unwrap_or(&0);
    let _ = max;
    sum as f64 / scores.len() as f64
}

pub fn average_b(scores: &[i32]) -> f64 {
    let sum: i32 = scores.iter().sum();
    let max = scores.iter().max().unwrap_or(&0);
    let _ = max;
    sum as f64 / scores.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn averages_agree() {
        assert_eq!(average_a(&[1, 2, 3]), 2.0);
        assert_eq!(average_b(&[1, 2, 3]), 2.0);
    }
}
''',
        "Cargo.toml": '[package]\nname = "refactor_001"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "refactor：average_a 与 average_b 完全重复。提取一个 `stats(scores) -> (f64, i32)`\n返回 (均值, 最大值)，让两个函数都调用它。`cargo test` 保持全绿。",
    [bash_ok("cargo test", out="test result: ok"),
     file_contains("src/lib.rs", "fn stats")],
)

make_task(
    "refactor-002", "core",
    "消除魔法数字重复（Python）",
    {
        "pricing.py": '''# 目标：python pricing_test.py 通过 + 引入常量。
# 价格计算：满 100 打 9 折，满 500 打 8 折（含税 0.13）。
def checkout(amounts):
    total = sum(amounts)
    if total >= 100 and total < 500:
        total = total * 0.9
    elif total >= 500:
        total = total * 0.8
    return total * 1.13
''',
        "pricing_test.py": '''from pricing import checkout

def check():
    assert abs(checkout([50, 50]) - 101.7) < 1e-9
    assert abs(checkout([100, 100, 100, 100, 100]) - 452.0) < 1e-9
    assert abs(checkout([10]) - 11.3) < 1e-9
    print("all tests passed")

check()
''',
    },
    "refactor：pricing.py 里的阈值（100/500）与税率（0.13）是散落的魔法数字。\n提取为模块级常量 DISCOUNT_LOW/DISCOUNT_HIGH/TAX_RATE 并使用。\n`python pricing_test.py` 保持通过。",
    [bash_ok("python pricing_test.py", out="all tests passed"),
     file_contains("pricing.py", "TAX_RATE")],
)

make_task(
    "refactor-003", "core",
    "拆分大函数（Python）",
    {
        "report.py": '''# 目标：python report_test.py 通过 + 拆出 helpers。
# 生成报告：标题行 + 每行 "name: value" + 汇总行。
def render(entries, title):
    lines = []
    lines.append("=" * len(title))
    lines.append(title)
    lines.append("=" * len(title))
    total = 0
    for name, value in entries:
        lines.append(f"{name}: {value}")
        total += value
    lines.append("---")
    lines.append(f"total: {total}")
    return "\\n".join(lines)
''',
        "report_test.py": '''from report import render

def check():
    out = render([("a", 1), ("b", 2)], "T")
    assert out.split("\\n")[0] == "="
    assert out.split("\\n")[1] == "T"
    assert out.split("\\n")[3] == "a: 1"
    assert out.endswith("total: 3")
    print("all tests passed")

check()
''',
    },
    "refactor：report.py 的 render() 一个函数做了三件事（标题、行渲染、\n汇总）。拆成 `header(title)`、`render_line(name, value)`、`footer(total)`\n三个辅助函数并由 render() 调用。`python report_test.py` 保持通过。",
    [bash_ok("python report_test.py", out="all tests passed"),
     file_contains("report.py", "def render_line")],
)

# ============ search：代码理解 ============
make_task(
    "search-001", "core",
    "在大文件中定位并修复 panic 行（Rust）",
    {
        "src/main.rs": '''// 目标：修复 panic；`cargo build` 通过。
// 下面有一个明显会在运行时 panic 的 unwrap（本任务只需修复它，
// 使代码在不 panic 的前提下保持行为：找不到时返回 None 语义）。
fn find_index(items: &[&str], target: &str) -> Option<usize> {
    let mut result = None;
    for (i, item) in items.iter().enumerate() {
        if *item == target {
            result = Some(i);
            break;
        }
    }
    // 历史遗留：此处曾用 unwrap 在未找到时 panic，现已改为 Option，
    // 但下面这一行仍会 panic：
    let idx = result.unwrap();
    Some(idx)
}

fn main() {
    let items = ["rust", "go", "python"];
    println!("{:?}", find_index(&items, "rust"));
    // 未找到不应崩溃：
    println!("{:?}", find_index(&items, "java"));
}
''',
        "Cargo.toml": '[package]\nname = "search_001"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/main.rs 的 find_index 有一个会在运行时 panic 的 unwrap。定位并\n修复（保持 Option 语义）。`cargo build` 通过且运行输出两行（第二行 None）。",
    [bash_ok("cargo build", out="Finished"),
     bash_ok("./target/debug/search_001.exe", out="Some(0)"),
     bash_ok("./target/debug/search_001.exe", out="None")],
)

make_task(
    "search-002", "core",
    "在长文件里找到隐藏 bug（Python）",
    {
        "inventory.py": '''# 目标：python inventory_test.py 通过。
# 库存管理：入库/出库/查询。文件较长，bug 藏在某处。

def make_store():
    return {"items": {}, "log": []}

def add(store, name, qty):
    if qty <= 0:
        raise ValueError("qty must be positive")
    store["items"][name] = store["items"].get(name, 0) + qty
    store["log"].append(("add", name, qty))

def remove(store, name, qty):
    if qty <= 0:
        raise ValueError("qty must be positive")
    current = store["items"].get(name, 0)
    if current < qty:
        raise ValueError("insufficient stock")
    store["items"][name] = current - qty
    store["log"].append(("remove", name, qty))

def query(store, name):
    return store["items"].get(name, 0)

def total(store):
    return sum(store["items"].values())

def apply_batch(store, ops):
    for op in ops:
        kind, name, qty = op
        if kind == "add":
            add(store, name, qty)
        elif kind == "remove":
            remove(store, name, qty)
        else:
            raise ValueError("unknown op")

def apply_batch_snapshot(store, ops):
    # 与 apply_batch 相同但要求失败时回滚（不留下部分修改）
    snapshot = store["items"]  # bug：引用拷贝，回滚时指向已被修改的同一 dict
    try:
        apply_batch(store, ops)
    except ValueError:
        store["items"] = snapshot
        raise
''',
        "inventory_test.py": '''from inventory import (
    make_store, add, remove, query, total, apply_batch_snapshot,
)

def check():
    s = make_store()
    add(s, "apple", 5)
    add(s, "apple", 3)
    assert query(s, "apple") == 8
    remove(s, "apple", 2)
    assert query(s, "apple") == 6
    assert total(s) == 6
    # 批量失败必须回滚（包括 add 的部分）
    s2 = make_store()
    add(s2, "pear", 2)
    try:
        apply_batch_snapshot(s2, [("add", "kiwi", 1), ("remove", "pear", 99)])
        raise AssertionError("必须抛 ValueError")
    except ValueError:
        pass
    assert query(s2, "kiwi") == 0, "add 的部分修改必须回滚"
    assert query(s2, "pear") == 2
    print("all tests passed")

check()
''',
    },
    "inventory.py 的 apply_batch_snapshot 声称失败回滚，但实际有 bug：\n快照只保存了 items 引用副本？检查并修复使 `python inventory_test.py` 通过。",
    [bash_ok("python inventory_test.py", out="all tests passed")],
)

# ============ search：C 阅读（无编译器，纯阅读） ============
make_task(
    "c-read-001", "core",
    "阅读 C 代码定位越界（无编译器）",
    {
        "buffer.c": '''// 目标：找出越界写并修复（本机无 gcc，不编译；按注释修复即可）。
// 下面的代码把 src 复制到 dst，但存在一个越界写。
#include <stdio.h>
#include <string.h>

void copy_tag(char *dst, size_t dst_cap, const char *src) {
    size_t len = strlen(src);
    if (len >= dst_cap) {
        len = dst_cap; // 这里等于 dst_cap 时，下面写 dst[len] 越界 1 字节
    }
    memcpy(dst, src, len);
    dst[len] = '\\0';
}

int main(void) {
    char buf[4];
    copy_tag(buf, sizeof(buf), "toolong");
    printf("%s\\n", buf);
    return 0;
}
''',
    },
    "buffer.c 的 copy_tag 有 1 字节越界写（len == dst_cap 时 dst[len]）。\n修复它（dst 必须保留 1 字节给 '\\0'）。不需要编译，直接修改源码——\n把修复后的条件写在代码里。",
    [file_contains("buffer.c", "dst_cap - 1")],
)

# ============ long-context ============
LONG_LINES = []
LONG_LINES.append("// 目标：在下方大文件中找到 `calculate` 函数并修复其返回值错误。")
LONG_LINES.append("// 文件是自动生成的数学工具库，大部分函数是对的，只有一个错。")
LONG_LINES.append("// 修复后运行 `cargo test`（测试在文件末尾）。")
LONG_LINES.append("")
for i in range(600):
    LONG_LINES.append(f"pub fn helper_{i:03}(x: i64) -> i64 {{ x * {i + 1} + {i % 7} }}")
LONG_LINES.append("")
LONG_LINES.append("// 计算 1+2+...+n 的和（当前实现有 off-by-one）。")
LONG_LINES.append("pub fn calculate(n: i64) -> i64 {")
LONG_LINES.append("    (1..n).sum() // 应为 1..=n")
LONG_LINES.append("}")
LONG_LINES.append("")
LONG_LINES.append("#[cfg(test)]")
LONG_LINES.append("mod tests {")
LONG_LINES.append("    use super::*;")
LONG_LINES.append("    #[test]")
LONG_LINES.append("    fn sum_of_first_three() { assert_eq!(calculate(3), 6); }")
LONG_LINES.append("    #[test]")
LONG_LINES.append("    fn sum_of_first_ten() { assert_eq!(calculate(10), 55); }")
LONG_LINES.append("    #[test]")
LONG_LINES.append("    fn zero() { assert_eq!(calculate(0), 0); }")
LONG_LINES.append("}")
make_task(
    "long-context-001", "long",
    "长上下文：600 行文件里修复一个 off-by-one",
    {
        "src/lib.rs": "\n".join(LONG_LINES) + "\n",
        "Cargo.toml": '[package]\nname = "long_context_001"\nversion = "0.1.0"\nedition = "2021"\n',
    },
    "src/lib.rs 有 600+ 行自动生成的函数。其中 `calculate` 的求和\n范围有 off-by-one（1..n 应为 1..=n）。定位并修复。不要修改测试。\n`cargo test` 全绿。",
    [bash_ok("cargo test", out="test result: ok")],
)

# ============ git 操作 ============
make_task(
    "git-task-001", "core",
    "恢复被破坏的 git 工作区",
    {
        "README.md": "# Demo Project\n\nThis is a demo repo used for the git eval task.\n",
        "src/main.py": "def main():\n    print(\"hello\")\n\nif __name__ == \"__main__\":\n    main()\n",
    },
    "当前 repo 工作区被破坏：`src/main.py` 被删除、`README.md` 被改成\n了空文件、还有一个未跟踪的 `scratch.txt`。任务：把工作区恢复到\n最近一次提交的状态（已提交内容恢复、未跟踪文件清理），然后运行\n`python src/main.py` 应输出 hello。",
    [bash_ok("python src/main.py", out="hello"),
     file_contains("README.md", "Demo Project"),
     bash_ok("git status --porcelain", exit_code=0)],
)
# git-task-001 特判：repo 两个 commit——A（完好，make_task 的 init）+ B（破坏现场）。
# eval 前 reset 到 B，agent 需恢复到 A（验证 git status 干净）。
_git_repo = os.path.join(ROOT, "git-task-001", "repo")
_commit_b = subprocess.run(
    ["git", "rev-parse", "HEAD"], cwd=_git_repo, capture_output=True, text=True
).stdout.strip()
os.remove(os.path.join(_git_repo, "src", "main.py"))
with io.open(os.path.join(_git_repo, "README.md"), "w", encoding="utf-8") as f:
    f.write("")
with io.open(os.path.join(_git_repo, "scratch.txt"), "w", encoding="utf-8") as f:
    f.write("scratch\n")
git(_git_repo, "add", "-A")
git(_git_repo, "-c", "user.name=eval", "-c", "user.email=eval@local",
    "commit", "-q", "-m", "broken")
with io.open(os.path.join(ROOT, "git-task-001", "expected.toml"), "a", encoding="utf-8") as f:
    f.write(f'base_commit = "{_commit_b}"\n')

print(f"\n共生成 {len([d for d in os.listdir(ROOT) if os.path.isdir(os.path.join(ROOT, d))])} 个任务")
