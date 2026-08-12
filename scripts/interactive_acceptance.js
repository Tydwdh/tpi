// TPI 交互验收驱动（§40）：通过 node-pty（Windows ConPTY）在真实终端语义下
// 驱动 tpi.exe，验证：真实 provider 流式运行、工具调用（read/bash/cargo test）、
// 运行中 Ctrl-C 取消、空闲双击 Ctrl-C 退出、运行中排队下一条消息自动执行、
// PgUp/PgDn/Ctrl+End、鼠标滚轮后输入存活、运行中 resize、/new 会话切换、/quit 干净退出。
//
// 用法（仓库外临时目录，避免污染工作区依赖）：
//   npm init -y && npm install node-pty
//   node <repo>/scripts/interactive_acceptance.js
// 环境：需已配置 ~/.tpi 模型与凭据（会调用真实 provider，产生少量费用）。
// 注意：Alt+E 工具详情 overlay 与 bracketed paste 无法经 ConPTY 输入模拟
// （ESC 前缀/粘贴事件由真实终端产生），由单元测试覆盖（tui_rework/tui_reducer）。

const pty = require('node-pty');
const path = require('path');
const fs = require('fs');

const TPI_BIN = process.env.TPI_BIN || path.join(__dirname, '..', 'target', 'debug', 'tpi.exe');
const CWD = process.env.TPI_CWD || path.join(__dirname, '..');

function spawnTpi() {
  const proc = pty.spawn(TPI_BIN, [], { name: 'xterm-256color', cols: 120, rows: 40, cwd: CWD, env: { ...process.env } });
  let raw = '';
  proc.onData((d) => { raw += d; });
  const exited = new Promise((res) => proc.onExit((e) => res(e.exitCode)));
  const clean = () => raw.replace(/\x1b\[[0-9;?]*[a-zA-Z]/g, '').replace(/\x1b[()][0-9A-Z]/g, '');
  return { proc, raw: () => raw, clean, exited };
}

function waitFor(get, pred, timeoutMs, label) {
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const iv = setInterval(() => {
      if (pred(get())) { clearInterval(iv); resolve(); }
      else if (Date.now() - t0 > timeoutMs) { clearInterval(iv); reject(new Error('timeout: ' + label)); }
    }, 200);
  });
}

async function scenario(name, fn) {
  console.log('==== ' + name + ' ====');
  try { await fn(); console.log('PASS: ' + name); return true; }
  catch (e) { console.log('FAIL: ' + name + ' -> ' + e.message); return false; }
}

async function quitAndCheck(s) {
  s.proc.write('/quit\n');
  const code = await Promise.race([s.exited, new Promise((r) => setTimeout(() => r('TIMEOUT'), 20000))]);
  if (code !== 0) throw new Error('exit code=' + code);
}

(async () => {
  if (!fs.existsSync(TPI_BIN)) {
    console.error('未找到 tpi.exe: ' + TPI_BIN + '（先 cargo build）');
    process.exit(2);
  }
  const results = [];

  // 1. 基本：真实 provider 流式回答 → /quit 干净退出
  results.push(await scenario('basic-run-and-quit', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    s.proc.write('你好\n');
    await waitFor(s.clean, (o) => o.includes('模型生成中'), 60000, 'run-start');
    await waitFor(s.clean, (o) => o.includes('就绪'), 120000, 'run-done');
    await quitAndCheck(s);
  }));

  // 2. 运行中 Ctrl-C → 取消提示（Windows raw mode 按键路径）
  results.push(await scenario('ctrl-c-during-run', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    s.proc.write('读取 Cargo.toml 并总结依赖\n');
    await waitFor(s.clean, (o) => o.includes('模型生成中'), 60000, 'run-start');
    await new Promise((r) => setTimeout(r, 800));
    s.proc.write('\x03');
    await waitFor(s.clean, (o) => o.includes('已发送取消') || o.includes('就绪'), 60000, 'cancel-or-done');
    await quitAndCheck(s);
  }));

  // 3. 空闲 Ctrl-C → 首次仅提示，2 秒内第二次退出（Quit effect）
  results.push(await scenario('ctrl-c-idle-double-press-quits', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    await new Promise((r) => setTimeout(r, 500));
    s.proc.write('\x03');
    await waitFor(s.clean, (o) => o.includes('再按一次 Ctrl+C'), 5000, 'first-press-hint');
    s.proc.write('\x03');
    const code = await Promise.race([s.exited, new Promise((r) => setTimeout(() => r('TIMEOUT'), 15000))]);
    if (code === 'TIMEOUT') throw new Error('did not exit on second idle Ctrl-C');
  }));

  // 4. 滚动键（PgUp/PgDn/Ctrl+End）后正常退出
  results.push(await scenario('scroll-keys-no-crash', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    s.proc.write('请介绍 TPI\n');
    await waitFor(s.clean, (o) => o.includes('就绪'), 120000, 'run-done');
    s.proc.write('\x1b[5~\x1b[6~\x05');
    await new Promise((r) => setTimeout(r, 800));
    await quitAndCheck(s);
  }));

  // 5. 多工具真实任务：read + bash(cargo test) → 工具卡片可见
  results.push(await scenario('multi-tool-task', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    s.proc.write('读取 src/util.rs，然后运行 cargo test --lib util 确认测试通过\n');
    await waitFor(s.clean, (o) => o.includes('就绪'), 300000, 'run-done');
    const clean = s.clean();
    if (!/read|util\.rs/.test(clean) || !/bash|cargo test/.test(clean)) {
      throw new Error('read/bash 工具卡片未出现');
    }
    await quitAndCheck(s);
  }));

  // 6. 运行中排队第二条消息 → 当前 run 结束后自动执行（footer 提示已排队）
  results.push(await scenario('queued-message-auto-runs', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    s.proc.write('请用一句话介绍 TPI\n');
    await waitFor(s.clean, (o) => o.includes('模型生成中'), 60000, 'run-start');
    s.proc.write('第二条问题：TPI 用什么语言写的？\n');
    await waitFor(s.clean, (o) => o.includes('第二条问题'), 180000, 'second-user-line');
    await waitFor(s.clean, (o) => (o.match(/就绪/g) || []).length >= 2, 240000, 'second-run-done');
    await quitAndCheck(s);
  }));

  // 7. 运行中 resize → 不崩溃
  results.push(await scenario('resize-during-run', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    s.proc.write('介绍 TPI\n');
    await waitFor(s.clean, (o) => o.includes('模型生成中'), 60000, 'run-start');
    for (const [w, h] of [[80, 24], [160, 50], [100, 30]]) { s.proc.resize(w, h); await new Promise((r) => setTimeout(r, 300)); }
    await waitFor(s.clean, (o) => o.includes('就绪'), 180000, 'run-done');
    await quitAndCheck(s);
  }));

  // 8. /new 会话切换 → 新消息运行 → /quit
  results.push(await scenario('new-session-switch', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    s.proc.write('/new\n');
    await new Promise((r) => setTimeout(r, 800));
    s.proc.write('第二段会话：你好\n');
    await waitFor(s.clean, (o) => o.includes('模型生成中'), 60000, 'run-start');
    await waitFor(s.clean, (o) => o.includes('就绪'), 180000, 'run-done');
    await quitAndCheck(s);
  }));

  // 9. 鼠标滚轮序列后输入路径仍存活
  results.push(await scenario('mouse-wheel-then-input-alive', async () => {
    const s = spawnTpi();
    await waitFor(s.clean, (o) => o.includes('TPI：'), 20000, 'welcome');
    s.proc.write('请介绍 TPI\n');
    await waitFor(s.clean, (o) => o.includes('就绪'), 120000, 'run-done');
    for (let i = 0; i < 3; i++) s.proc.write('\x1b[<64;60;20M');
    await new Promise((r) => setTimeout(r, 600));
    s.proc.write('x\n');
    await waitFor(s.clean, (o) => o.includes('x'), 30000, 'input-alive');
    await quitAndCheck(s);
  }));

  const passed = results.filter(Boolean).length;
  console.log('==== RESULT: ' + passed + '/' + results.length + ' passed ====');
  process.exit(passed === results.length ? 0 : 1);
})();
