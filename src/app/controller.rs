//! P3-02：`AppController`——application use case 的语义宿主。
//!
//! - 接收 ports（`AppServices`：config/session/provider/cancel/mcp）+ [`UiIntent`]，
//!   返回 `Vec<AppEffect>`；**不引用 Crossterm/Ratatui**；
//! - 本阶段先迁 `start run` / `cancel run` 两个 use case；后续逐迁
//!   resume/session/input answer/config commands；
//! - 验收：fake runtime/session/platform 的 integration tests（本文件测试 +
//!   `tests/app_controller.rs`）。
//!
//! 设计：controller 是**同步决策 + async 副作用边界**——`handle` 同步返回
//! 意图对应的 effect 列表；run 启动/取消的具体 async 执行由调用方（surface
//! adapter）经 `run_async` 驱动（避免 controller 持有 runtime 细节）。

use crate::app::intent::{AppCommand, AppEffect, UiIntent};
use crate::provider::Provider;
use tokio_util::sync::CancellationToken;

/// controller 持有的 ports（P1-06 AppServices 的只读视图 + cancel 控制）。
pub struct AppController<P: Provider> {
    pub services: crate::app::AppServices<P>,
}

impl<P: Provider> AppController<P> {
    pub fn new(services: crate::app::AppServices<P>) -> Self {
        Self { services }
    }

    /// 处理一个 surface 意图，返回需要执行的 effects。
    /// 同步决策：不做 IO/await；副作用经 AppEffect 返回由 adapter 执行。
    pub fn handle(&mut self, intent: UiIntent) -> Result<Vec<AppEffect>, String> {
        let mut effects = Vec::new();
        match intent.command {
            AppCommand::Quit => {
                // 退出：先请求渲染（关闭动画/spinner），surface 收到后退出。
                effects.push(AppEffect::Draw);
            }
            AppCommand::CancelRun => {
                // cancel run：取消当前 run 的 token（若在跑）。
                if let Some(cancel) = self
                    .services
                    .current_cancel
                    .lock()
                    .map_err(|e| format!("cancel lock poisoned: {e}"))?
                    .as_ref()
                {
                    cancel.cancel();
                }
                effects.push(AppEffect::Notify("已取消当前 run".into()));
            }
            AppCommand::StartNewSession => {
                // 重置会话（下一轮输入创建新 session）。
                self.services.conversation.reset();
                effects.push(AppEffect::Draw);
            }
            AppCommand::ToggleSidebar => {
                // 视图意图（TUI 侧处理）；此处只标记渲染。
                effects.push(AppEffect::Draw);
            }
            AppCommand::ToggleReasoning => {
                effects.push(AppEffect::Draw);
            }
            AppCommand::OpenModal { name } => {
                // 命令处理器（P3-03 registry）填充 body；此处先交给 surface。
                effects.push(AppEffect::Notify(format!("打开 {name}")));
            }
            AppCommand::CompactNow | AppCommand::RetryLast => {
                // 由 surface adapter 触发对应 run 路径（本阶段交还调用方）。
                effects.push(AppEffect::Notify("手动压缩/重试由 run 路径处理".into()));
            }
            AppCommand::OpenSearch | AppCommand::OpenLastTool | AppCommand::OpenFailedTool => {
                effects.push(AppEffect::Draw);
            }
            AppCommand::SubmitInput(_)
            | AppCommand::OpenSession(_)
            | AppCommand::RequestInputAnswer(_, _)
            | AppCommand::Paste(_) => {
                // 输入类：由 surface adapter 进入 run 路径（run_async）。
                effects.push(AppEffect::Draw);
            }
        }
        Ok(effects)
    }

    /// 获取当前 cancel token（供 surface adapter 挂载 run）。
    pub fn take_cancel(&self) -> Arc<Mutex<Option<CancellationToken>>> {
        self.services.current_cancel.clone()
    }
}

use std::sync::{Arc, Mutex};
