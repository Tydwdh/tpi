//! RuntimeHandle：前端拿到的句柄（web_desktop.md §七）。
//!
//! 两个基本能力：
//! - `command(cmd)`：提交命令，同步返回 `CommandAck`（Accepted / Rejected）；
//!   命令的异步副作用通过事件流可见。
//! - `subscribe()`：订阅事件流（`EventEnvelope`）。
//!
//! 句柄可 clone、跨线程、跨 await 安全。

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};
use tpi_core::ids::RequestId;
use tpi_protocol::{ClientCommand, CommandAck, EventEnvelope};

use crate::service::RuntimeTask;

/// 提交给 runtime 执行的"挂号命令"：命令本身 + 结果 oneshot。
pub(crate) struct PendingCommand {
    pub command: ClientCommand,
    pub request_id: RequestId,
    pub reply: oneshot::Sender<CommandAck>,
}

/// Runtime 句柄。所有前端共享同一个 runtime，通过该句柄交互。
///
/// - 廉价 clone（Arc 内部）；
/// - `command()` 可并发调用，内部串行排队；
/// - `subscribe()` 返回的 receiver 可被丢弃（不影响其他订阅者）。
#[derive(Clone)]
pub struct RuntimeHandle {
    inner: Arc<Inner>,
}

struct Inner {
    cmd_tx: mpsc::Sender<PendingCommand>,
    event_tx: broadcast::Sender<EventEnvelope>,
    /// 最近一次的事件 seq（断线重连 / 新订阅者补全用）。
    /// 短临界区同步锁：runtime 单任务线程写、前端只读，无 await。
    last_seq: Arc<std::sync::Mutex<u64>>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl RuntimeHandle {
    /// 创建 runtime 句柄（内部启动 runtime 任务，返回 handle + join handle）。
    pub fn new<P>(task: RuntimeTask<P>) -> (Self, tokio::task::JoinHandle<()>)
    where
        P: tpi_agent::provider::Provider + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PendingCommand>(64);
        let (event_tx, _) = broadcast::channel(crate::EVENT_BROADCAST_CAPACITY);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let last_seq = Arc::new(std::sync::Mutex::new(0u64));
        let inner = Arc::new(Inner {
            cmd_tx,
            event_tx: event_tx.clone(),
            last_seq: last_seq.clone(),
            shutdown: shutdown.clone(),
        });
        let handle = Self { inner };
        let join = tokio::spawn(task.run(cmd_rx, event_tx, shutdown, last_seq));
        (handle, join)
    }

    /// 提交命令。返回的 Ack 表示命令是否被接受并进入执行队列（或被同步拒绝）。
    ///
    /// 命令的实际副作用（run 开始 / 工具事件 等）随后通过事件流送达。
    pub async fn command(&self, command: ClientCommand) -> Result<CommandAck, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request_id = RequestId::new_v7();
        let pending = PendingCommand {
            command,
            request_id,
            reply: reply_tx,
        };
        self.inner
            .cmd_tx
            .send(pending)
            .await
            .map_err(|_| "runtime 已停止".to_string())?;
        reply_rx
            .await
            .map_err(|_| "runtime 已停止（未回复）".to_string())
    }

    /// 订阅事件流（broadcast receiver）。
    ///
    /// - 新订阅者从**订阅之后**的事件开始；如果需要历史，可通过
    ///   `last_seq()` + 从 session store 重建（断线重连）。
    /// - 若消费速度跟不上，收到 `RecvError::Lagged` 时需要补全（客户端负责）。
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.inner.event_tx.subscribe()
    }

    /// 当前最新事件 seq（断线重连游标）。
    pub async fn last_seq(&self) -> u64 {
        match self.inner.last_seq.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => **poisoned.get_ref(),
        }
    }

    /// 请求优雅关闭 runtime。
    pub async fn shutdown(&self) -> Result<(), String> {
        self.inner.shutdown.cancel();
        Ok(())
    }
}
