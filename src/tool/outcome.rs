//! 统一工具结果协议（文档 §8.2/§8.3）。
//!
//! 三类 payload 不能混用：
//! - `model_payload`：短、稳定、有预算；必须包含状态和下一步诊断所需字段。
//! - `display_payload`：富文本、diff、完整可展开输出，不自动进入模型上下文。
//! - `session_metadata`：原始参数摘要、资源、时间、统计，用于恢复与评测。
//!
//! 预期失败（未找到、stale、退出码非零）返回 [`ToolOutcome`]；
//! 只有工具实现自身崩溃等基础设施错误才返回 Rust `Err`（§8.2）。

use serde::{Deserialize, Serialize};

/// 工具终态（§3.2 不变量 3：每个 tool call 恰好产生一个终态结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Interrupted,
    Rejected,
}

/// 中断写工具时的副作用判定（§10.7：`effect=not_applied|committed|unknown`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    /// 副作用未发生（纯读工具或提交前崩溃且可证明未提交）。
    NotApplied,
    /// 副作用已发生（提交后崩溃）。
    Committed,
    /// 无法判定（M1 写工具；M3 起用 backup journal 精确判定）。
    Unknown,
}

impl std::fmt::Display for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::NotApplied => write!(f, "not_applied"),
            Effect::Committed => write!(f, "committed"),
            Effect::Unknown => write!(f, "unknown"),
        }
    }
}

/// opaque artifact 引用（§8.4：`@artifact/<session>/<id>`；M2 实现存储）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub session: String,
    pub id: String,
}

impl std::fmt::Display for ArtifactRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "@artifact/{}/{}", self.session, self.id)
    }
}

/// 模型可见的紧凑结果（§8.3 文本 envelope 的结构化形态）。
///
/// 不变量（§2.2）：退出码、stale 原因等判断下一步所需的状态
/// 必须在此可见，不能只在 UI metadata 中。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPayload {
    pub status: ToolStatus,
    pub program: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// 有界输出摘要（§8.4 默认预算）。
    pub output: String,
    /// 中断时的副作用判定（§10.7）；正常终态为 `None`。
    pub effect: Option<Effect>,
    pub artifact: Option<ArtifactRef>,
}

/// UI 展示用富结果（M5 完整化）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DisplayPayload {
    pub text: String,
}

/// 恢复与评测用元数据（M1 起由各工具填充）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolMetadata {
    pub tool: String,
    /// workspace-relative 目标路径（read/edit/write/run 的 cwd 等）。
    pub target: Option<String>,
    /// 实际解析/执行的程序（§11.2：记录实际选择）。
    pub program: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// 工具执行耗时（毫秒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTiming {
    pub duration_ms: u64,
}

/// 工具观察到的资源版本（§8.2：记录实际观察值，M3 完整化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceVersion {
    pub path: String,
    pub revision: String,
}

/// 工具结果的持久化形态（写入 session log 前序列化，§4.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToolOutcome {
    pub status: ToolStatus,
    pub model_payload: ModelPayload,
    pub session_metadata: ToolMetadata,
}

/// 统一工具结果（文档 §8.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub status: ToolStatus,
    pub model_payload: ModelPayload,
    pub display_payload: DisplayPayload,
    pub session_metadata: ToolMetadata,
    pub evidence: Vec<ArtifactRef>,
    pub observed_resources: Vec<ResourceVersion>,
    pub artifacts: Vec<ArtifactRef>,
    pub timing: ToolTiming,
}

impl ToolOutcome {
    /// 构造一个命令失败结果。
    ///
    /// M1：`exit_code` 必须进入 `model_payload`（§2.2：结构化退出状态不能只在 UI details）。
    pub fn command_failed(program: &str, exit_code: i32) -> Self {
        Self::failed(
            "run",
            ModelPayload {
                status: ToolStatus::Failed,
                program: Some(program.to_string()),
                exit_code: Some(exit_code),
                duration_ms: 0,
                output: format!("status: failed\nprogram: {program}\nexit_code: {exit_code}"),
                effect: None,
                artifact: None,
            },
        )
    }

    /// 失败结果（工具预期失败：§8.2 返回 ToolOutcome 而非 Err）。
    pub fn failed(tool: &str, model_payload: ModelPayload) -> Self {
        Self {
            status: model_payload.status,
            model_payload,
            display_payload: DisplayPayload::default(),
            session_metadata: ToolMetadata {
                tool: tool.to_string(),
                ..Default::default()
            },
            evidence: Vec::new(),
            observed_resources: Vec::new(),
            artifacts: Vec::new(),
            timing: ToolTiming { duration_ms: 0 },
        }
    }

    /// 成功结果。
    pub fn succeeded(tool: &str, output: String) -> Self {
        Self {
            status: ToolStatus::Succeeded,
            model_payload: ModelPayload {
                status: ToolStatus::Succeeded,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output,
                effect: None,
                artifact: None,
            },
            display_payload: DisplayPayload::default(),
            session_metadata: ToolMetadata {
                tool: tool.to_string(),
                ..Default::default()
            },
            evidence: Vec::new(),
            observed_resources: Vec::new(),
            artifacts: Vec::new(),
            timing: ToolTiming { duration_ms: 0 },
        }
    }

    pub fn with_metadata(mut self, metadata: ToolMetadata) -> Self {
        self.session_metadata = metadata;
        self
    }

    pub fn with_timing(mut self, duration_ms: u64) -> Self {
        self.timing.duration_ms = duration_ms;
        self.model_payload.duration_ms = duration_ms;
        self
    }

    /// 转持久化形态。
    pub fn into_stored(self) -> StoredToolOutcome {
        StoredToolOutcome {
            status: self.status,
            model_payload: self.model_payload,
            session_metadata: self.session_metadata,
        }
    }

    /// 模型可见文本 envelope（§8.3）。
    pub fn model_text(&self) -> String {
        self.model_payload.output.clone()
    }
}
