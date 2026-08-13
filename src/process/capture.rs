//! 状态捕获段的流式剥离（任务书 §22）。
//!
//! bash wrapper 在用户命令之后追加一段**高熵 nonce 包裹**的状态段：
//!
//! ```text
//! \n__TPI_CAPTURE_BEGIN_<nonce>__\n
//! <捕获内容：cwd / env>
//! \n__TPI_CAPTURE_END_<nonce>__\n
//! ```
//!
//! 这段是 control plane（命令执行后的真实 shell 状态），**不得**进入模型
//! 输出、artifact 或 UI。本模块在进程层做 framing 剥离——只识别标记，不
//! 理解内容（Data/Control 逻辑分离）；标记可能在任意帧边界被切开，因此是
//! 流式状态机。
//!
//! 高熵 nonce（uuid v7）保证用户命令自身输出与标记碰撞的概率可忽略。

/// 由 nonce 构造 begin/end 标记。
pub fn markers(nonce: &str) -> (Vec<u8>, Vec<u8>) {
    let begin = format!("\n__TPI_CAPTURE_BEGIN_{nonce}__\n").into_bytes();
    let end = format!("\n__TPI_CAPTURE_END_{nonce}__\n").into_bytes();
    (begin, end)
}

/// 流式剥离状态机：输入任意分块的 stdout 字节流，返回"用户数据"部分，
/// 命中捕获段的部分累积到内部 buffer，调用 [`CaptureScanner::take_capture`] 取出。
pub struct CaptureScanner {
    begin: Vec<u8>,
    end: Vec<u8>,
    /// 未定归属的 lookahead（可能跨帧的部分标记）。
    scan: Vec<u8>,
    in_capture: bool,
    capture: Vec<u8>,
}

impl CaptureScanner {
    pub fn new(nonce: &str) -> Self {
        let (begin, end) = markers(nonce);
        Self {
            begin,
            end,
            scan: Vec::new(),
            in_capture: false,
            capture: Vec::new(),
        }
    }

    /// 处理一段 stdout 字节，返回应转发给用户处理（模型/artifact/UI）的部分。
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut user = Vec::new();
        // `pending` 是待处理数据的所有权视图：初始借用入参，命中标记后
        // 切换到 `self.scan` 的尾部（owned），避免借用逃逸。
        let mut pending: std::borrow::Cow<'_, [u8]> = std::borrow::Cow::Borrowed(bytes);
        loop {
            if self.in_capture {
                self.scan.extend_from_slice(&pending);
                match find_subslice(&self.scan, &self.end) {
                    Some(pos) => {
                        self.capture.extend_from_slice(&self.scan[..pos]);
                        let after = self.scan[pos + self.end.len()..].to_vec();
                        self.scan.clear();
                        self.in_capture = false;
                        if after.is_empty() {
                            break;
                        }
                        pending = std::borrow::Cow::Owned(after);
                        // 落到下方 begin 扫描（end 之后理论上不再有捕获段，安全兜底）。
                    }
                    None => {
                        // 保留尾部可能跨帧的部分，其余进入捕获内容。
                        let keep = self.end.len().saturating_sub(1).min(self.scan.len());
                        let split = self.scan.len() - keep;
                        self.capture.extend_from_slice(&self.scan[..split]);
                        self.scan.drain(..split);
                        break;
                    }
                }
            } else {
                self.scan.extend_from_slice(&pending);
                match find_subslice(&self.scan, &self.begin) {
                    Some(pos) => {
                        user.extend_from_slice(&self.scan[..pos]);
                        let after = self.scan[pos + self.begin.len()..].to_vec();
                        self.scan.clear();
                        self.in_capture = true;
                        if after.is_empty() {
                            break;
                        }
                        pending = std::borrow::Cow::Owned(after);
                        // 继续循环进入 capture 分支（同一块内可能已含 end）。
                    }
                    None => {
                        // 保留尾部可能跨帧的部分，其余作为用户数据转发。
                        let keep = self.begin.len().saturating_sub(1).min(self.scan.len());
                        let split = self.scan.len() - keep;
                        user.extend_from_slice(&self.scan[..split]);
                        self.scan.drain(..split);
                        break;
                    }
                }
            }
        }
        user
    }

    /// 取出捕获内容（BEGIN..END 之间的原始字节）；未命中任何捕获段时为 `None`。
    pub fn take_capture(&mut self) -> Option<Vec<u8>> {
        if self.in_capture || self.capture.is_empty() {
            // 命令结束后仍处于捕获段内 → 截断/异常，捕获无效。
            return None;
        }
        Some(std::mem::take(&mut self.capture))
    }

    /// 命令结束：flush 滞留在 lookahead 中的用户数据（最后一段可能因跨帧
    /// 检测而滞留）。若此时仍处于未闭合捕获段内，捕获内容作废（截断），
    /// 滞留字节也丢弃——它们属于被截断的 control 数据，不是用户输出。
    pub fn finish(&mut self) -> Vec<u8> {
        if self.in_capture {
            self.capture.clear();
            self.scan.clear();
            Vec::new()
        } else {
            std::mem::take(&mut self.scan)
        }
    }
}

/// 在 `haystack` 中查找 `needle` 首次出现的位置。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个测试 capture 字节流：用户输出 + 捕获段（随机分块）。
    fn stream_with_capture(nonce: &str, capture: &str, chunk: usize) -> Vec<u8> {
        let (begin, end) = markers(nonce);
        let mut full = Vec::new();
        full.extend_from_slice(b"user line 1\n");
        full.extend_from_slice(&begin);
        full.extend_from_slice(capture.as_bytes());
        full.extend_from_slice(&end);
        full.extend_from_slice(b"user line 2\n");
        // 按 chunk 分块打散，模拟 pump 帧边界。
        let mut out = Vec::new();
        for block in full.chunks(chunk) {
            out.extend_from_slice(block);
        }
        out
    }

    #[test]
    fn captures_and_strips_across_chunk_boundaries() {
        for chunk in 1..=64 {
            let nonce = "abc123def456abc123def456abc123def4";
            let capture = "C:\\proj\\src";
            let stream = stream_with_capture(nonce, capture, chunk);
            let mut scanner = CaptureScanner::new(nonce);
            let mut user = Vec::new();
            for block in stream.chunks(7) {
                user.extend_from_slice(&scanner.feed(block));
            }
            user.extend_from_slice(&scanner.finish());
            assert_eq!(user, b"user line 1\nuser line 2\n", "chunk={chunk}");
            assert_eq!(
                String::from_utf8(scanner.take_capture().unwrap()).unwrap(),
                capture,
                "chunk={chunk}"
            );
        }
    }

    #[test]
    fn no_capture_marker_yields_none() {
        let mut scanner = CaptureScanner::new("nonce123");
        let mut user = scanner.feed(b"plain output\n");
        user.extend_from_slice(&scanner.finish());
        assert_eq!(user, b"plain output\n");
        assert!(scanner.take_capture().is_none());
    }

    #[test]
    fn truncated_capture_is_rejected() {
        // 命令输出在 END 标记之前结束（被截断/异常终止）→ 捕获无效。
        let nonce = "nonce456";
        let (begin, _) = markers(nonce);
        let mut scanner = CaptureScanner::new(nonce);
        let mut buf = begin.clone();
        buf.extend_from_slice(b"partial");
        let user = scanner.feed(&buf);
        assert_eq!(user, b"");
        assert!(scanner.take_capture().is_none(), "未闭合的捕获段必须丢弃");
    }

    #[test]
    fn marker_split_exactly_at_feed_boundary() {
        let nonce = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (begin, end) = markers(nonce);
        let mut scanner = CaptureScanner::new(nonce);
        let mut full = Vec::new();
        full.extend_from_slice(&begin);
        full.extend_from_slice(b"CWD");
        full.extend_from_slice(&end);
        // 逐字节喂入，标记必然被切开。
        let mut user = Vec::new();
        for b in full {
            user.extend_from_slice(&scanner.feed(&[b]));
        }
        assert_eq!(user, b"");
        assert_eq!(scanner.take_capture().unwrap(), b"CWD");
    }
}
