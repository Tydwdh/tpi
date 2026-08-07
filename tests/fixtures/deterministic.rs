//! 确定性时钟与 ID 生成器（M0 交付项，§21）。
//!
//! 测试必须可复现：不使用真实时间与随机源。
//! 生产实现按 §5.2 改用 UUIDv7 与 `time`，测试保持本生成器。

use tpi::ids::{RequestId, ToolCallId};

/// 确定性 ID 生成器：进程内递增。
///
/// M0 交付项（§21）；消费方是 M1 的 agent loop 测试。
#[expect(dead_code, reason = "M0 交付项，M1 起由 agent loop 测试消费")]
#[derive(Debug, Default)]
pub struct DeterministicIds {
    next: u64,
}

#[expect(dead_code, reason = "M0 交付项，M1 起由 agent loop 测试消费")]
impl DeterministicIds {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_id(&mut self) -> RequestId {
        self.next += 1;
        RequestId::from_u128(self.next as u128)
    }

    pub fn tool_call_id(&mut self) -> ToolCallId {
        self.next += 1;
        ToolCallId::from_u128(self.next as u128)
    }
}

/// 确定性单调时钟：固定起点、显式推进。
///
/// M0 交付项（§21）；消费方是 M1 的 agent loop 测试。
#[expect(dead_code, reason = "M0 交付项，M1 起由 agent loop 测试消费")]
#[derive(Debug, Default)]
pub struct DeterministicClock {
    millis: u64,
}

#[expect(dead_code, reason = "M0 交付项，M1 起由 agent loop 测试消费")]
impl DeterministicClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&mut self, millis: u64) {
        self.millis += millis;
    }

    pub fn now_millis(&self) -> u64 {
        self.millis
    }
}
