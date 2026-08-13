# TPI Logical Shell Session 与 Remote Workspace 实施任务书

> 面向执行本任务的 Coding Agent
> 项目：TPI
> 本机平台：Windows
> 本地 Shell：Git Bash
> 远端目标：SSH 主机，主要考虑 Linux 开发环境
> 核心目标：统一解决本地 Shell 状态连续性与远程工程工作区能力。

---

# 0. 一句话任务

将 TPI 从：

```text
每条 bash 都是完全独立的一次性命令
```

升级为：

```text
Logical Shell Session
        +
Workspace Backend
        │
        ├── Local Workspace
        └── SSH Remote Workspace
```

最终模型仍只使用现有：

```text
read
list
search
glob
edit
write
bash
```

但：

```text
bash("cd src")
bash("pwd")
```

能够保持 `cwd`，

```text
bash("export FOO=bar")
bash("echo $FOO")
```

能够保持 exported environment，

并且同一套工具可以透明运行在：

```text
Local Workspace
```

或：

```text
SSH Remote Workspace
```

之上。

---

# 1. 先明确：本任务不是“实现真正持久 Bash”

不要将目标理解为：

```text
启动一个 bash.exe
然后永远不退出
```

本阶段明确不实现：

```text
Persistent Bash Process
PTY Shell
jobs / fg / bg
shell function persistence
alias persistence
interactive REPL
长期 open fd
后台 job 恢复
```

真正需要持久化的是：

```text
Shell 的用户可感知逻辑状态
```

第一阶段仅包括：

```text
cwd
exported environment
unset environment
```

底层每条命令仍然可以：

```text
fresh shell
+
独立进程生命周期
```

---

# 2. 核心架构原则

最终架构应接近：

```text
                         Agent
                           │
                           ▼
                     Tool Protocol
                           │
                           ▼
                    ActiveWorkspace
                           │
                 ┌─────────┴─────────┐
                 │                   │
          LocalWorkspace       RemoteWorkspace
                 │                   │
         ShellSessionState     ShellSessionState
                 │                   │
                 ▼                   ▼
        LocalShellExecutor     SshShellExecutor
                 │                   │
          process-host           SSH channel
                 │
          fresh Git Bash
```

最重要的设计原则：

> **持久的是 `ShellSessionState`，不是 Bash 进程。**

---

# 3. ShellSessionState 是公共能力

不得设计成：

```rust
struct RemoteWorkspace {
    persistent_cwd: ...
    persistent_env: ...
}
```

正确方向：

```rust
struct ShellSessionState {
    cwd: WorkspacePath,
    env_overlay: EnvOverlay,
}
```

然后：

```rust
struct LocalWorkspace {
    ...
    shell: ShellSessionState,
}

struct RemoteWorkspace {
    ...
    shell: ShellSessionState,
}
```

本地和远程必须共享同一套状态语义。

---

# 4. EnvOverlay

不要每次保存完整 process environment。

建议：

```rust
struct EnvOverlay {
    set: HashMap<String, String>,
    unset: HashSet<String>,
}
```

表示：

```text
相对于 Workspace 初始环境
用户修改了什么
```

例如：

```bash
export FOO=123
unset BAR
```

产生：

```text
set:
    FOO = 123

unset:
    BAR
```

优势：

```text
状态小
容易序列化
容易 diff
容易恢复
更容易处理 secret
```

---

# 5. bash Tool 保持唯一

严禁新增：

```text
persistent_bash
remote_bash
ssh_bash
local_bash
```

模型仍然只有：

```text
bash
```

语义定义为：

> Execute a shell command in the current workspace using the current logical shell state.

实际执行：

```text
bash(command)
      │
      ▼
ActiveWorkspace
      │
 ┌────┴────┐
 ▼         ▼
Local     Remote
```

Transport 不应泄漏给模型。

---

# 6. 第一阶段优先解决本地 Shell

整个开发顺序必须调整为：

```text
Phase S0  审计当前 bash/process-host
       ↓
Phase S1  ShellSessionState
       ↓
Phase S2  Local cwd persistence
       ↓
Phase S3  Local env persistence
       ↓
Phase S4  Shell transaction semantics
       ↓
Phase W0  Workspace boundary
       ↓
Phase R0  SSH connection
       ↓
Phase R1  Remote bash
       ↓
Phase R2  Remote files
       ↓
Phase R3  Remote search/glob/list
       ↓
Phase R4  reconnect/session resume
```

也就是说：

> **SSH 一行代码都没写之前，本地 TPI 就应该已经拥有 cwd/env persistence。**

---

# 7. Phase S0：审计现有 bash 架构

编码前必须先完整阅读：

```text
bash tool
process-host
Job Object
timeout
cancellation
ToolOutcome
artifact
session
scheduler
cwd handling
environment handling
```

输出：

```text
SHELL_ARCH_AUDIT.md
```

至少回答：

1. 当前 `bash.cwd` 如何解释；
2. workspace root 在哪里进入执行链；
3. process-host 如何收到 cwd/env；
4. timeout 与 cancellation 如何杀进程树；
5. bash output 如何流式返回；
6. ToolOutcome 何时确认命令结束；
7. Session 当前是否保存任何 Shell state；
8. 哪些地方直接假设“下一条 bash 从 workspace root 开始”。

此阶段不允许改架构。

---

# 8. Phase S1：引入 ShellSessionState

添加统一状态对象。

逻辑上：

```rust
struct ShellSessionState {
    cwd: WorkspacePath,
    env_overlay: EnvOverlay,
    version: u64,
}
```

`version` 可选，但建议保留。

它便于：

```text
调试
状态事务
session replay
未来 effect tracking
```

初始：

```text
cwd = workspace root
env_overlay = empty
```

---

# 9. ShellSessionState 必须属于 Workspace

禁止：

```rust
static SHELL_STATE
```

或：

```rust
App {
    global_shell_state
}
```

因为未来可能：

```text
Local Workspace
GPU Workspace
Build Server Workspace
```

每个都必须拥有独立 cwd/env。

即：

```text
Workspace A:
cwd = src
FOO=A

Workspace B:
cwd = scripts
FOO=B
```

不能互相污染。

---

# 10. Phase S2：本地 cwd Persistence

最终真实行为：

```bash
bash("mkdir -p a/b")
bash("cd a/b")
bash("pwd")
```

必须得到：

```text
<workspace>/a/b
```

而不是 workspace root。

---

# 11. 不允许解析 command 字符串猜 cwd

禁止：

```rust
if command.starts_with("cd ") {
    ...
}
```

因为以下都合法：

```bash
cd foo && cd bar
cd "$(dirname "$FILE")"
source setup.sh
foo() { cd abc; }
```

状态必须来自：

> **Shell 实际执行后的最终状态。**

---

# 12. Shell command 生命周期

推荐逻辑：

```text
ShellSessionState(v7)
        │
        ▼
构造 fresh shell
        │
恢复 cwd/env
        │
执行用户 command
        │
捕获最终 cwd/env
        │
command 正常结束
        │
        ▼
commit ShellSessionState(v8)
```

这是一次：

> **Shell State Transaction**

---

# 13. Shell state commit 规则

必须明确。

### 正常结束

包括：

```text
exit 0
exit != 0
```

只要 Shell 确认正常结束：

```text
commit final cwd/env
```

例如：

```bash
cd foo
false
```

真实 Bash 执行完后 cwd 已经是 `foo`。

因此逻辑 Session 也应该保持：

```text
cwd = foo
```

---

# 14. 不正常结束

以下情况：

```text
timeout
user cancellation
process-host crash
connection interruption
shell state capture failure
```

不得提交未知的新状态。

保持：

```text
last confirmed ShellSessionState
```

即：

```text
v7
→ command
→ timeout
→ discard tentative v8
→ remain v7
```

---

# 15. bash.cwd 参数语义必须重新定义清楚

当前 bash 有：

```text
cwd?
```

不能与 persistent cwd 混淆。

推荐：

### 未传 cwd

```text
使用 ShellSessionState.cwd
```

### 显式 cwd

```text
只对本次 invocation 生效
```

例如：

```text
session cwd = /project/src
```

调用：

```json
{
  "command": "cargo test",
  "cwd": "/project/tests"
}
```

本次从：

```text
/project/tests
```

执行。

完成以后：

```text
session cwd 仍为 /project/src
```

除非用户 command 自身执行了：

```bash
cd ...
```

---

# 16. One-shot cwd 与 shell cwd mutation 必须区分

这是重要 invariant：

```text
bash cwd parameter
=
execution override

cd command
=
shell state mutation
```

不要混为一个字段的不同写法。

---

# 17. cwd 边界

Logical cwd 默认不得逃出当前 Workspace root。

例如：

```bash
cd ../../../../
```

如果 TPI 当前使用 strict workspace sandbox：

必须保持这一语义。

如果用户主动开启 unrestricted/free mode：

才允许超出。

不要因为 Shell State 引入而绕过现有路径安全策略。

---

# 18. Phase S3：exported env persistence

必须支持：

```bash
export TPI_FOO=abc
```

之后：

```bash
printf '%s' "$TPI_FOO"
```

输出：

```text
abc
```

并支持：

```bash
unset TPI_FOO
```

之后变量消失。

---

# 19. 不允许通过 command parser 捕获 export

禁止：

```rust
parse("export FOO=...")
```

因为：

```bash
source env.sh
export FOO="$(pwd)"
set -a
...
```

都会改变 environment。

必须从 Shell 执行后的真实 environment 得到状态。

---

# 20. Env snapshot 不能无脑全部保存

必须识别两类环境：

```text
baseline environment
session overlay
```

运行前：

```text
baseline + overlay
```

运行后：

```text
new environment
```

计算：

```text
diff(baseline, new)
```

得到新的：

```text
EnvOverlay
```

不要让：

```text
SHLVL
PWD
OLDPWD
BASHPID
RANDOM
_
```

等动态变量污染持久状态。

需要定义：

```text
ignored/dynamic env variables
```

---

# 21. Secret Policy

环境里可能包含：

```text
API_KEY
TOKEN
PASSWORD
SECRET
AWS_*
GITHUB_TOKEN
```

第一版建议：

```text
cwd
→ durable session state

env overlay
→ runtime/session-memory only
```

不要默认把全部 env 明文写入 session 文件。

如果未来需要 durable env：

必须单独设计：

```text
redaction
DPAPI
encrypted store
```

当前任务不要扩展到这里。

---

# 22. Shell state capture protocol

Shell stdout/stderr 是用户数据。

不能直接使用易碰撞：

```text
__TPI_ENV__
__TPI_CWD__
```

然后从 stdout 搜字符串。

优先：

```text
独立 control channel
```

如果现有 process-host 不适合：

可使用：

```text
随机 command id
+
高熵 nonce
+
严格 framing
```

但必须：

```text
Data Plane
和
Control Plane
```

逻辑上分离。

---

# 23. 不要为了 cwd/env 引入 PTY

本地 Logical Shell Session 第一版：

```text
仍然 bash -c
```

不要增加：

```text
ConPTY
portable-pty
terminal emulation
prompt parser
```

这些解决的是 interactive process，不是 cwd/env persistence。

---

# 24. 保留现有 Job Object

这是硬约束。

本地每条命令仍必须拥有：

```text
独立可控进程树
```

超时：

```text
TerminateJobObject
```

取消：

```text
TerminateJobObject
```

不允许为了 Shell State 把所有 command 放进一个永久 Bash Job。

---

# 25. Local Shell Definition of Done

在进入 SSH 开发之前，以下必须全部通过：

```bash
cd src
pwd
```

保持 cwd。

```bash
export FOO=abc
echo "$FOO"
```

保持变量。

```bash
unset FOO
echo "${FOO-unset}"
```

正确 unset。

```bash
cd src
false
```

cwd 仍应更新。

```bash
cd src
sleep 1000
```

timeout 后 ShellState 不进入未知状态。

Ctrl-C：

```text
整个当前 process tree 被杀
下一条 bash 仍正常
last confirmed cwd/env 可继续使用
```

---

# 26. Phase W0：Workspace Boundary

只有 Local Shell 稳定后，才开始 Remote Workspace。

目标：

```text
Tool Protocol
     │
ActiveWorkspace
     │
 ┌───┴───┐
Local   Remote
```

---

# 27. 不要一上来设计巨大 Workspace trait

禁止：

```text
VFS
20 个 async trait
plugin workspace backend
distributed filesystem abstraction
```

先找真正变化的 seam：

```text
filesystem access
command execution
workspace identity
shell state
```

如果 enum 足够：

优先 enum。

---

# 28. 建议的逻辑模型

例如：

```rust
enum Workspace {
    Local(LocalWorkspace),
    Remote(RemoteWorkspace),
}
```

其中：

```rust
struct LocalWorkspace {
    root: PathBuf,
    shell: ShellSessionState,
}

struct RemoteWorkspace {
    host: RemoteHost,
    root: RemotePath,
    shell: ShellSessionState,
    connection: RemoteConnection,
}
```

具体类型可根据代码调整。

---

# 29. 一个 Agent Session 默认只有一个 Active Workspace

第一版：

```text
Local
或者
Remote
```

不要允许一次 Tool Call 里：

```text
host=A
```

下一次：

```text
host=B
```

Host 不应该成为：

```text
read/edit/bash
```

的常规模型参数。

---

# 30. Workspace Identity

必须能表示：

```text
local:C:\project
```

以及：

```text
ssh:gpu:/home/dev/project
```

但模型的大多数 Tool 调用不需要重复这个信息。

---

# 31. Phase R0：SSH Connection Layer

Remote 第一版只实现 transport primitive：

```text
connect
disconnect
reconnect
exec
file read
file write
```

此阶段不要马上接 Tool。

先单独验证 SSH subsystem。

---

# 32. 技术选型

实现者应比较：

```text
russh
ssh2 / libssh2
system OpenSSH
```

重点评价：

```text
Windows support
SSH config compatibility
known_hosts
ssh-agent
key authentication
password authentication
SFTP
async
maintenance
deployment complexity
```

不要因为“纯 Rust”就自动选择某个库。

---

# 33. 优先复用用户现有 SSH 配置

目标体验：

用户已经有：

```sshconfig
Host gpu
    HostName 192.168.1.10
    User dev
    IdentityFile ~/.ssh/id_ed25519
```

TPI：

```text
/connect gpu
```

即可。

不要第一版要求用户重新维护：

```text
tpi_remote.toml
```

里的重复凭据。

---

# 34. Host Key Verification

必须做。

禁止：

```text
accept every host key
```

未知 host：

```text
必须由用户确认
```

Agent 不得自己选择：

```text
yes, trust this host
```

这是用户安全边界，不属于模型决策。

---

# 35. Phase R1：Remote bash

现有：

```text
bash
```

开始根据：

```text
ActiveWorkspace
```

分发：

```text
Local
→ LocalShellExecutor

Remote
→ SshShellExecutor
```

Tool schema 不变。

---

# 36. Remote 也必须共享 ShellSessionState

必须做到：

```bash
bash("cd scripts")
bash("pwd")
```

远端第二条得到：

```text
/home/dev/project/scripts
```

以及：

```bash
bash("export CUDA_VISIBLE_DEVICES=1")
bash("python train.py")
```

第二条自动带：

```text
CUDA_VISIBLE_DEVICES=1
```

这套逻辑不能重新写一份 SSH-specific implementation。

---

# 37. SSH Connection 不等于 Shell Session

Remote Workspace：

```text
Logical State
    root
    cwd
    env

Runtime Transport
    SSH connection
```

必须分离。

SSH socket 断开：

```text
connection lost
```

不代表：

```text
cwd/env/session 消失
```

重连后继续恢复 logical state。

---

# 38. Remote command 每次仍独立

第一版推荐：

```text
persistent SSH transport connection
+
fresh exec channel per command
```

而不是：

```text
persistent interactive Bash
```

即：

```text
TCP connection 可以复用
shell process 不必复用
```

---

# 39. Remote cancellation

SSH v1 不得伪装拥有和 Windows Job Object 一样的 guarantee。

Local：

```text
strong process-tree cancellation
```

Remote v1：

```text
best-effort cancellation
```

如果无法证明 descendants 已全部死亡：

必须诚实记录。

以后 Remote ProcessHost 再增强。

---

# 40. Remote command interruption

例如：

```text
python script.py
```

已经发送远程执行，

然后：

```text
network disconnect
```

不得自动认为：

```text
command failed and nothing happened
```

真实状态应：

```text
Effect::Unknown
```

或者现有等价语义。

更不能自动 replay。

---

# 41. Phase R2：Remote file tools

接入：

```text
read
edit
write
```

必须保持本地完全相同的 semantic contract。

特别是：

```text
revision-bound
stale rejection
atomic batch
diff
```

不能因为远端用 SFTP 就退化。

---

# 42. Revision 必须 transport-independent

如果本地和远端文件 bytes 完全相同：

最好得到相同 content revision。

Revision 表示：

```text
内容身份
```

而不是：

```text
某种本地文件句柄
```

---

# 43. Remote read

模型输出格式继续：

```text
[revision=...]

lines X-Y of N

1: ...
2: ...
```

不能因为 Remote transport 改变 ModelPayload contract。

---

# 44. Remote edit

推荐：

```text
read current remote bytes
↓
verify revision
↓
apply replacements locally/in-memory
↓
upload temp
↓
atomic rename if filesystem supports
```

如果远程 FS 无法提供某项 guarantee：

明确降低保证。

不要伪造 atomicity。

---

# 45. Phase R3：Remote list/search/glob

不要机械复用本地 traversal。

远端大量小文件走 SFTP：

```text
N 个文件
→ N 次 RTT
```

会非常慢。

可以根据工具分别选择最佳 transport。

---

# 46. Remote search

优先：

```text
remote rg
```

如果可用。

但必须先：

```text
capability detect
```

不能默认服务器一定有。

Fallback 可以：

```text
grep
```

或其它受控实现。

---

# 47. Remote glob/list

允许：

```text
remote find/helper
```

或：

```text
SFTP traversal
```

内部实现不同没关系。

模型看到的 ToolOutcome 必须一致。

---

# 48. Transport Complexity 不得泄漏给模型

模型不能需要决定：

```text
这次用 SFTP
还是 rg
还是 find
还是 remote helper
```

这是 Harness Mechanism。

模型只决定：

```text
我要 read
我要 search
我要 bash
```

---

# 49. Phase R4：Session Resume

Session 应保存：

```text
workspace kind
remote host identity
remote workspace root
logical cwd
```

env durability 根据前面的 secret policy。

不要保存：

```text
socket
channel
PID
```

resume：

```text
load logical Remote Workspace
↓
connection = disconnected
↓
reconnect on demand
```

---

# 50. Connection State

建议明确：

```rust
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}
```

这是 runtime state。

---

# 51. TUI

用户必须始终能明确知道当前执行位置。

Local：

```text
LOCAL  C:\company\project
```

Remote：

```text
SSH  gpu:/home/dev/project
```

不要只显示：

```text
/home/dev/project
```

---

# 52. Remote 状态不能污染 Transcript

例如：

```text
SSH reconnecting
SSH connected
```

优先：

```text
Status Bar
Live Activity
Toast
```

不要每次都往 conversation transcript 插 System Message。

---

# 53. Agent Context

模型应该知道当前 Workspace identity，例如：

```text
Current workspace:
SSH gpu:/home/dev/project

Shell cwd:
scripts
```

但不要每个 ToolOutcome 都重复。

Workspace state 属于 Harness Context。

---

# 54. Tool description 进一步抽象

例如 `bash` 应逐步从：

```text
Run a command through Git Bash...
```

演进为更抽象：

```text
Run a shell command in the current workspace.
```

Transport-specific 细节只在必要时告诉模型。

但第一版不要为了 Remote 立即大改所有 prompt。

先保证行为正确。

---

# 55. Remote Artifact

Remote command 的 stdout/stderr 完整输出仍然可以：

```text
下载/流到 Local TPI
↓
保存为本地 artifact
```

不要第一版做：

```text
distributed artifact store
```

---

# 56. Remote Helper 什么时候才需要

只有真实遇到这些瓶颈：

```text
strict process-tree cancellation
search 太慢
大量 remote file operations
大 artifact
断线后的执行状态确认
```

才考虑：

```text
Remote tpi-process-host
```

---

# 57. Remote ProcessHost 未来架构

未来可能：

```text
Local TPI
    │
   SSH
    │
Remote tpi-process-host
    │
    ├── exec
    ├── cancel
    ├── read
    ├── edit
    ├── search
    └── artifact
```

建议按需临时部署：

```text
检测版本
↓
上传 binary
↓
启动
```

而不是要求用户安装 daemon。

但不属于 Remote v1。

---

# 58. 本阶段禁止

严格禁止顺手实现：

```text
Persistent Bash PID
PTY
OSC command protocol
shell job control
alias/function persistence
REPL
multi-host Agent
remote daemon mandatory
MCP
ACP
Subagent
Plugin Architecture
distributed filesystem
```

除非发现没有这些就无法满足已定义需求，并提供明确证据。

---

# 59. 本地测试矩阵

必须包含：

### cwd

```bash
pwd
cd src
pwd
cd ..
pwd
```

### env

```bash
export FOO=123
echo "$FOO"
unset FOO
echo "${FOO-unset}"
```

### failure

```bash
cd src && false
```

确认最终 cwd 语义。

### timeout

```bash
export FOO=bad
sleep 1000
```

timeout 后确认 ShellState commit 行为符合设计。

### cancel

长命令 Ctrl-C：

```text
当前进程树死亡
TPI 可继续使用
ShellSessionState 不损坏
```

---

# 60. Remote 测试矩阵

至少：

```text
connect
pwd
cd
export
env
disconnect
reconnect
```

以及：

```text
read
write
edit
search
glob
bash
```

---

# 61. Remote stale edit

必须测试：

```text
TPI read → R1

外部 SSH 修改文件 → R2

TPI edit(R1)
```

结果必须：

```text
stale_revision
```

---

# 62. 网络 Fault Injection

必须测试：

```text
command 发送前断网
command 执行期间断网
upload 中断
rename 前断网
SSH server restart
authentication failure
host key changed
```

每一种都必须明确：

```text
NotApplied
Committed
Unknown
```

或现有等价状态。

---

# 63. 真正的 Agent End-to-End 测试

不要只测 SSH API。

构造远程 Workspace：

```text
project/
    logs/
        input.log
```

用户：

```text
在远程机器上写一个 Python 脚本，
统计 logs/input.log 中每种错误的数量，
输出 result.csv，并运行验证。
```

期望 Agent：

```text
list
read
write analyze.py
bash python analyze.py
read result.csv

必要时：
edit
bash
```

禁止 Agent trajectory 出现：

```text
ssh ...
scp ...
```

说明 Remote abstraction 真正成功。

---

# 64. 第二个 Agent 测试

远程放一个有 Bug 的 Python/Rust 项目。

用户：

```text
帮我修复项目，让测试通过。
```

Agent 应自然：

```text
search
read
edit
bash test
修复
再验证
```

和 Local workflow 没有显著区别。

---

# 65. Eval 指标

建议记录：

```text
task success
tool calls
turns
tokens
remote round trips
SSH reconnect count
search latency
file transfer bytes
stale edit count
unknown effect count
command cancellation latency
```

这样以后决定：

```text
是否值得 Remote ProcessHost
```

就有真实数据，而不是猜测。

---

# 66. Definition of Done：Logical Shell

* [ ] Local cwd 持久
* [ ] Local exported env 持久
* [ ] unset 持久
* [ ] 每命令仍独立 process tree
* [ ] timeout 不破坏已确认状态
* [ ] cancellation 不破坏已确认状态
* [ ] bash.cwd one-shot override 语义明确
* [ ] Shell state 属于 Workspace
* [ ] 没有 Persistent Bash
* [ ] 没有 PTY

---

# 67. Definition of Done：Remote Workspace

* [ ] SSH connection
* [ ] known_hosts / host key verification
* [ ] Remote root
* [ ] Remote bash
* [ ] cwd persistence
* [ ] env persistence
* [ ] read
* [ ] list
* [ ] search
* [ ] glob
* [ ] edit
* [ ] write
* [ ] revision/stale detection
* [ ] session resume
* [ ] reconnect
* [ ] effect unknown 正确表达
* [ ] TUI 清楚显示远程 identity
* [ ] Agent 不需要直接使用 ssh/scp

---

# 68. 架构验收

最终应该能够画成：

```text
                         TPI Agent
                            │
                            ▼
                       Tool Protocol
                            │
                            ▼
                     ActiveWorkspace
                            │
              ┌─────────────┴─────────────┐
              │                           │
      LocalWorkspace                RemoteWorkspace
              │                           │
      ShellSessionState             ShellSessionState
       cwd + env                     cwd + env
              │                           │
    LocalShellExecutor             SshShellExecutor
              │                           │
       process-host                  SSH transport
              │
       Windows Job Object
```

而不应该变成：

```text
local bash
persistent bash
ssh bash
remote bash
remote read
remote write
scp
PTY shell
```

这种能力碎片化系统。

---

# 69. 设计哲学

整个实现遵守：

### Mechanism 下沉

模型决定：

```text
要读文件
要改文件
要运行命令
```

Harness 决定：

```text
本地还是 SSH
如何恢复 cwd/env
如何传输
如何 revision
如何 reconnect
```

### Information Hiding

模型不需要知道：

```text
SSH channel
SFTP
process-host
Job Object
env capture protocol
```

### Define Errors Out of Existence

尽量通过类型和状态机避免：

```text
本地/远程 shell state 混用
未知状态被当成成功
stale edit 覆盖
断网自动重复副作用命令
host key 自动信任
```

### 持久语义，不持久脆弱进程

```text
cwd/env
```

属于 durable/logical state。

```text
PID/socket/channel/bash process
```

属于 runtime resource。

不要混为一体。

---

# 70. 实施纪律

执行本任务的 Agent：

1. 先审计，再修改；
2. 先实现 Local Logical Shell；
3. Local 全绿后才引入 SSH；
4. 不允许大爆炸重构；
5. 不增加重复 Tool；
6. 不降低现有 Job Object guarantee；
7. 不降低 revision-bound edit guarantee；
8. Unknown 就明确 Unknown；
9. Remote 无法提供 Local guarantee 时明确记录差异；
10. 每阶段增加 regression；
11. 最后必须跑真实 Agent 任务，而不仅是 primitive test。

---

# 71. 最终用户体验

本地：

```text
User:
进入 scripts，然后设置代理，以后都使用这个代理。

Agent:
bash("cd scripts")
bash("export HTTPS_PROXY=...")

后续：
bash("python task.py")
```

无需重复 cwd/env。

远程：

```text
User:
连接 gpu 的 /home/dev/project，
帮我写一个 Python 脚本分析 data，然后运行到正确。

TPI:
Active Workspace = SSH gpu:/home/dev/project

Agent:
list("data")
write("analyze.py")
bash("python analyze.py")
edit("analyze.py")
bash("python analyze.py")
read("result.csv")
```

模型完全不用生成：

```text
ssh
scp
cd prefix
export prefix
```

---

# 72. 最终判断标准

如果实现完成以后，用户仍然需要反复告诉 Agent：

```text
“记得你现在在远程”
“记得 cd 到那个目录”
“记得带上代理”
“这是远程文件，不是本地文件”
```

则 abstraction 失败。

正确状态应该是：

> **Agent 只需要思考软件工程任务，Workspace 和 Shell 连续性由 Harness 自己保证。**

---

# 73. 最终任务定义

> **第一步，把现有本地 `bash` 升级成具有持久 cwd/env 的 Logical Shell Session，同时保留每命令独立进程树和 Job Object。第二步，把 Local/Remote 统一为 Active Workspace，让 SSH Remote Workspace 复用完全相同的 ShellSessionState 和现有 Tool DSL。Remote v1 不实现真正持久 Bash、PTY 或 job control；核心目标是让本地与远程工程任务在 Agent 看来拥有同一种操作语义。**

---

# 74. MCP Client 与 Agent Skills（README2 实施完成）

> 本段记录 MCP 与 Skills 扩展的**使用方式**（架构设计见 `docs/mcp-skills-design.md`）。

## MCP 配置

在 `~/.tpi/config.toml` 添加：

```toml
[mcp.servers.<name>]
command = "server 可执行文件"
args = []
enabled = true
timeout_ms = 30000

[mcp.servers.<name>.env]
FOO = "bar"
```

启动 TPI 后自动 spawn server → initialize → tools/list，工具以
`mcp::<server>::<tool>` 注册；`/mcp` 查看状态（Server/Status/Tools）。

完整示例见 `examples/mcp-servers.toml`。

## Skill 安装与目录

Skill 目录优先级：**project > user > builtin**（同名覆盖，记录来源）：

```text
<workspace>/.agent/skills/<skill>/SKILL.md     # 项目级
~/.tpi/skills/<skill>/SKILL.md                 # 用户级
```

标准 `SKILL.md` 格式（YAML frontmatter）：

```markdown
---
name: skill-name
description: 一句话说明
---
<body：使用说明/工作流/知识>
```

示例 skill 见 `examples/skills/`（hello-skill / rust-review / bevy-debug）。
安装：把 skill 目录复制到 `<workspace>/.agent/skills/` 或 `~/.tpi/skills/`。

## 工作方式（Progressive Disclosure）

1. 启动只加载 skill 的 name/description（metadata-only），注入 system prompt
   `[Available skills]` 列表；
2. 模型匹配任务后调用内置工具 `activate_skill(name)` → 返回完整 SKILL.md
   + references/scripts 清单；
3. references 按需读取（`<skill>/references/*`）。

## 调试命令

```text
/mcp           # MCP server 状态页
/mcp list      # 等价 /mcp（状态表）
/mcp tools     # 展示各 server 工具（并入状态页）
/mcp restart   # 重启 server（卸载工具 → 杀进程 → 重新启动）
```

MCP 调用日志（debug）：`RUST_LOG=tpi=debug` 查看 server/tool/duration/status。

## 已知限制（README2 §6-§7 明确不实现）

- MCP：Resources / Prompts / Streamable HTTP / OAuth / Tasks / Notifications /
  Sampling 未实现（YAGNI，README2 §7/§27 Phase 6）；
- Skills：`allowed-tools` 权限不强制（README2 §24，V1 只 parse 保存 metadata）；
- MCP 工具在 scheduler 中统一按 WorkspaceUnknown（保守串行）执行；
- 工具经 ToolSelector 按上下文选择（builtin 始终保留，MCP 按关键词匹配，
  总量上限 32）——未选中 MCP 工具本轮不可调用。

