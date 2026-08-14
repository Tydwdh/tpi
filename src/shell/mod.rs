//! Logical Shell Session（任务书 §3/§8/§9/§12-§14）。
//!
//! 持久的是 [`ShellSessionState`]，**不是 bash 进程**。底层每条命令仍然是
//! fresh shell + 独立进程生命周期 + Job Object 进程树取消（§11.5 不变量
//! 原样保留）；跨命令保留的是「用户可感知的逻辑状态」：cwd 与 exported env
//! overlay。
//!
//! 状态属于 Workspace（Local/Remote 各持有一份，任务书 §9），禁止 static
//! 全局或 App 级共享——未来多 workspace（GPU / build server）必须互不污染。

use std::collections::{HashMap, HashSet};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

/// env overlay：相对 Workspace 初始环境的用户修改（任务书 §4）。
///
/// - `set`：新增/覆盖的变量（`export FOO=123` → `FOO = "123"`）；
/// - `unset`：被删除的变量（`unset BAR` → `BAR`）。
///
/// 不保存完整 process environment，因此状态小、易 diff、易恢复，
/// 也更利于处理 secret（overlay 值不进 session 文件，§21）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvOverlay {
    pub set: HashMap<String, String>,
    pub unset: HashSet<String>,
}

/// 动态/每次 fresh shell 必然变化的变量（任务书 §20）：diff 时必须忽略，
/// 否则每轮都会把 `SHLVL`、`PWD` 之类当成用户修改写进 overlay。
///
/// `_` 是 bash 记住的最后一条命令路径；`BASHPID`/`PPID`/`SECONDS` 每进程不同；
/// `SHLVL` 随 shell 嵌套变化；`PWD`/`OLDPWD` 是 cwd 派生值（cwd 已单独跟踪）。
pub const DYNAMIC_ENV_VARS: &[&str] = &[
    "SHLVL", "PWD", "OLDPWD", "BASHPID", "PPID", "SECONDS", "RANDOM", "_",
];

fn is_dynamic(name: &str) -> bool {
    DYNAMIC_ENV_VARS
        .iter()
        .any(|key| key.eq_ignore_ascii_case(name))
}

/// 计算新 overlay：`diff(baseline, new)`（任务书 §20）。
///
/// - `new` 相对 `baseline` 新增/改值的 key → `set`；
/// - `baseline` 有而 `new` 没有的 key → `unset`；
/// - 动态变量一律忽略，不进入 overlay。
pub fn diff_env(baseline: &HashMap<String, String>, new: &HashMap<String, String>) -> EnvOverlay {
    let mut overlay = EnvOverlay::default();
    for (key, value) in new {
        if is_dynamic(key) {
            continue;
        }
        match baseline.get(key) {
            Some(base) if base == value => {}
            Some(_) => {
                overlay.set.insert(key.clone(), value.clone());
            }
            None => {
                overlay.set.insert(key.clone(), value.clone());
            }
        }
    }
    for key in baseline.keys() {
        if is_dynamic(key) {
            continue;
        }
        if !new.contains_key(key) {
            overlay.unset.insert(key.clone());
        }
    }
    overlay
}

/// 一次逻辑 shell 会话的持久状态（任务书 §8）。
///
/// `version` 每次成功 commit 递增，用于调试、状态事务与 session replay。
///
/// 序列化（session 持久化）只保留 `cwd` 与 `version`：
/// `env_overlay` 是用户修改（可能含 secret，§21 不落盘明文）；
/// `baseline` 是 Workspace 初始环境的完整快照（必然含 secret，更不落盘）。
/// 两者都是 runtime/session-memory 状态，恢复后重新捕获。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSessionState {
    /// 逻辑 cwd（初始 = workspace root；严格模式下不得逃出 root）。
    pub cwd: Utf8PathBuf,
    /// 用户 env 修改（§21：仅内存，不随 session 持久化）。
    #[serde(skip)]
    pub env_overlay: EnvOverlay,
    /// 已确认状态版本；每次 commit 递增（任务书 §8）。
    pub version: u64,
    /// Workspace 初始环境快照（§20；仅内存，不随 session 持久化）。
    /// `None` = 尚未捕获（首次 bash 执行前捕获）。
    #[serde(skip)]
    pub baseline: Option<HashMap<String, String>>,
}

impl ShellSessionState {
    /// 初始状态：cwd = workspace root，overlay 为空，version 0。
    pub fn new(workspace_root: Utf8PathBuf) -> Self {
        Self {
            cwd: workspace_root,
            env_overlay: EnvOverlay::default(),
            version: 0,
            baseline: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_starts_at_workspace_root_with_empty_overlay() {
        let state = ShellSessionState::new(Utf8PathBuf::from("C:/proj"));
        assert_eq!(state.cwd.as_str(), "C:/proj");
        assert!(state.env_overlay.set.is_empty());
        assert!(state.env_overlay.unset.is_empty());
        assert_eq!(state.version, 0);
        assert!(state.baseline.is_none());
    }

    #[test]
    fn overlay_roundtrips_through_serialization() {
        let mut overlay = EnvOverlay::default();
        overlay.set.insert("FOO".into(), "abc".into());
        overlay.unset.insert("BAR".into());
        let json = serde_json::to_string(&overlay).unwrap();
        let back: EnvOverlay = serde_json::from_str(&json).unwrap();
        assert_eq!(back, overlay);
        assert_eq!(back.set["FOO"], "abc");
        assert!(back.unset.contains("BAR"));
    }

    /// §21：session 序列化只保留 cwd + version；env_overlay 与 baseline
    /// （可能含 secret）不落盘。
    #[test]
    fn state_serialization_excludes_env_data() {
        let mut state = ShellSessionState::new(Utf8PathBuf::from("C:/proj"));
        state
            .env_overlay
            .set
            .insert("HTTPS_PROXY".into(), "http://p:8080".into());
        state.env_overlay.unset.insert("SECRET".into());
        state.baseline = Some(HashMap::from([("API_KEY".into(), "sk-123".into())]));
        state.version = 3;
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("HTTPS_PROXY"), "{json}");
        assert!(!json.contains("API_KEY"), "secret 不得序列化: {json}");
        assert!(!json.contains("SECRET"), "{json}");

        let back: ShellSessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cwd, state.cwd);
        assert_eq!(back.version, 3);
        assert!(back.env_overlay.set.is_empty(), "overlay 不持久化");
        assert!(back.baseline.is_none(), "baseline 不持久化");
    }

    /// §20：diff 忽略动态变量；新增/改值 → set；消失 → unset。
    #[test]
    fn diff_env_computes_overlay_correctly() {
        let baseline = HashMap::from([
            ("FOO".into(), "1".into()),
            ("BAR".into(), "keep".into()),
            ("BAZ".into(), "gone".into()),
            ("SHLVL".into(), "1".into()),
        ]);
        let new = HashMap::from([
            ("FOO".into(), "2".into()),    // 改值 → set
            ("BAR".into(), "keep".into()), // 不变 → 忽略
            ("QUX".into(), "new".into()),  // 新增 → set
            ("SHLVL".into(), "2".into()),  // 动态 → 忽略
            ("PWD".into(), "/x".into()),   // 动态 → 忽略
        ]);
        let overlay = diff_env(&baseline, &new);
        assert_eq!(overlay.set["FOO"], "2");
        assert_eq!(overlay.set["QUX"], "new");
        assert!(overlay.unset.contains("BAZ"), "消失 → unset");
        assert!(!overlay.set.contains_key("BAR"));
        assert!(!overlay.set.contains_key("SHLVL"), "动态变量不得进 overlay");
        assert!(!overlay.set.contains_key("PWD"));
    }

    /// overlay 注入后再次 diff 必须幂等（set/unset 稳定，不抖动）。
    #[test]
    fn diff_env_is_idempotent_under_reinjection() {
        let baseline = HashMap::from([("A".into(), "1".into()), ("C".into(), "old".into())]);
        // 注入 overlay 后的环境：A 被覆盖为 2，B 新增，C 被 unset。
        let injected = HashMap::from([("A".into(), "2".into()), ("B".into(), "x".into())]);
        let overlay = diff_env(&baseline, &injected);
        assert_eq!(overlay.set["A"], "2");
        assert_eq!(overlay.set["B"], "x");
        assert!(overlay.unset.contains("C"));

        // 再次用同一 overlay 注入后的环境做 diff，结果相同（无抖动）。
        let again = diff_env(&baseline, &injected);
        assert_eq!(again, overlay);
    }
}
