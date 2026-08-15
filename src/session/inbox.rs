//! P8-01：next-step/next-turn inbox——排队输入（pending messages）。
//!
//! - [`Inbox`]：有界输入队列（运行中输入排队，run 完成后作为下一条消息）；
//! - receipt：每次 push 返回序号（可确认已入队）；
//! - queue limits：超限拒绝（不无限堆积）；
//! - cancel release：cancel 时清空（未消费输入释放）；
//! - durable claim：run 开始时 claim（取走全部；claim 后新输入继续排队）。
//!
//! 先 fake Agent state tests（本模块单测）；真实 pending_message 迁移到 Inbox
//! 在 agent 集成时完成（app 的 pending_message 语义保留，Inbox 是它的组件化）。

/// 队列上限（§：不无限堆积）。
pub const MAX_INBOX_CAPACITY: usize = 8;

/// inbox 条目（receipt = push 序号）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxEntry {
    pub receipt: u64,
    pub message: String,
}

/// 有界输入队列。
#[derive(Debug, Clone, Default)]
pub struct Inbox {
    entries: std::collections::VecDeque<InboxEntry>,
    next_receipt: u64,
    capacity: usize,
}

impl Inbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: std::collections::VecDeque::new(),
            next_receipt: 0,
            capacity: capacity.max(1),
        }
    }

    /// push：返回 receipt（Err = 队列满，拒绝）。
    pub fn push(&mut self, message: String) -> Result<u64, String> {
        if self.entries.len() >= self.capacity {
            return Err(format!(
                "inbox 已满（{}/{}）",
                self.entries.len(),
                self.capacity
            ));
        }
        let receipt = self.next_receipt;
        self.next_receipt += 1;
        self.entries.push_back(InboxEntry { receipt, message });
        Ok(receipt)
    }

    /// claim：取走全部条目（run 开始时消费；返回条目列表，队列清空）。
    pub fn claim(&mut self) -> Vec<InboxEntry> {
        std::mem::take(&mut self.entries).into_iter().collect()
    }

    /// cancel release：清空全部（未消费输入释放）。
    pub fn release_all(&mut self) -> Vec<InboxEntry> {
        self.claim()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// receipt 单调（push 返回序号，可确认入队）。
    #[test]
    fn push_returns_monotonic_receipt() {
        let mut inbox = Inbox::new(8);
        assert_eq!(inbox.push("a".into()), Ok(0));
        assert_eq!(inbox.push("b".into()), Ok(1));
        assert_eq!(inbox.len(), 2);
    }

    /// queue limits：满时拒绝（不无限堆积）。
    #[test]
    fn queue_limit_rejects_when_full() {
        let mut inbox = Inbox::new(2);
        inbox.push("a".into()).unwrap();
        inbox.push("b".into()).unwrap();
        let err = inbox.push("c".into()).expect_err("满时拒绝");
        assert!(err.contains("已满"), "{err}");
    }

    /// durable claim：run 开始时取走全部；claim 后新输入继续排队。
    #[test]
    fn claim_takes_all_and_new_input_queues() {
        let mut inbox = Inbox::new(8);
        inbox.push("a".into()).unwrap();
        inbox.push("b".into()).unwrap();
        let claimed = inbox.claim();
        assert_eq!(claimed.len(), 2);
        assert!(inbox.is_empty());
        // claim 后新输入排队（receipt 继续单调）。
        let receipt = inbox.push("c".into()).unwrap();
        assert_eq!(receipt, 2);
        assert_eq!(inbox.len(), 1);
    }

    /// cancel release：清空未消费输入。
    #[test]
    fn release_all_clears_pending() {
        let mut inbox = Inbox::new(8);
        inbox.push("a".into()).unwrap();
        let released = inbox.release_all();
        assert_eq!(released.len(), 1);
        assert!(inbox.is_empty());
    }
}
