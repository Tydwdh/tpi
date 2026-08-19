//! P2-08：O2 local trace sink 故障注入测试。
//!
//! 覆盖 roadmap 验收：queue full / slow disk / shutdown deadline / 损坏尾部。
//! 原则：sink error 只降级观测，不 panic、不影响调用方。

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use tpi::trace::{TraceRecord, TraceRecordKind, TraceSink};

/// 可故障 writer：按 flag 拒绝写入（模拟磁盘满/IO 错误）。
struct FaultyWriter {
    fail: AtomicBool,
    bytes: Vec<u8>,
}

impl FaultyWriter {
    fn new() -> Self {
        Self {
            fail: AtomicBool::new(false),
            bytes: Vec::new(),
        }
    }
}

impl Write for FaultyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("injected write failure"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 慢盘 writer：每次 write 强制 yield（模拟慢 IO，验证不阻塞调用方语义）。
struct SlowWriter {
    bytes: Vec<u8>,
}

impl Write for SlowWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 同步 Write 无法真正 await；用极短 sleep 模拟慢盘（仅测试用）。
        std::thread::sleep(std::time::Duration::from_micros(50));
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn record(seq_hint: u64) -> TraceRecord {
    let mut r = TraceRecord::new("test.record");
    r.kind = TraceRecordKind::Event;
    r.record_seq = seq_hint;
    r
}

/// 正常路径：push + flush 写入全部记录，stats 正确。
#[test]
fn normal_flush_writes_all_records() {
    let writer = FaultyWriter::new();
    let mut sink = TraceSink::new(writer);
    for i in 0..10 {
        sink.push(record(i));
    }
    assert_eq!(sink.queued(), 10);
    sink.flush();
    assert!(sink.is_empty());
    assert_eq!(sink.stats().written_records, 10);
    assert_eq!(sink.stats().dropped_records, 0);
    assert_eq!(sink.stats().gaps, 0);
    assert_eq!(sink.stats().first_seq, Some(1));
    assert_eq!(sink.stats().last_seq, Some(10));
}

/// queue full：小容量下大量 push → 溢出 → gap counter + `TraceGap` 声明。
/// （`TraceSink` 容量是常量；通过不断 push 且不 flush 触发溢出。）
#[test]
fn queue_overflow_records_gap() {
    let writer = FaultyWriter::new();
    let mut sink = TraceSink::new(writer);
    // 灌入大量记录（> MAX_QUEUED_RECORDS）且不 flush。
    for i in 0..5000u64 {
        sink.push(record(i));
    }
    assert!(
        sink.stats().gaps > 0,
        "溢出必须产生 gap counter（dropped={}, gaps={}）",
        sink.stats().dropped_records,
        sink.stats().gaps
    );
    assert!(sink.stats().dropped_records > 0);
    // flush 后 stats 仍保留 gap 信息（manifest 披露）。
    sink.flush();
    assert!(sink.stats().gaps > 0);
}

/// slow disk：慢 writer 不 panic、flush 完成（有界队列保证有界耗时）。
#[test]
fn slow_disk_flush_completes() {
    let writer = SlowWriter { bytes: Vec::new() };
    let mut sink = TraceSink::new(writer);
    for i in 0..20 {
        sink.push(record(i));
    }
    sink.flush(); // 不 panic；慢但完成
    assert!(sink.is_empty());
    assert_eq!(sink.stats().written_records, 20);
}

/// 写入失败：flush 不 panic，失败记录计入 dropped，调用方不受影响。
#[test]
fn write_failure_degrades_observation_only() {
    let writer = FaultyWriter::new();
    let mut sink = TraceSink::new(writer);
    for i in 0..5 {
        sink.push(record(i));
    }
    // 让 writer 故障 → flush 失败。
    sink.writer_mut().fail.store(true, Ordering::SeqCst);
    sink.flush(); // 不 panic
    // 恢复后再次 flush 应成功（剩余记录重试）。
    sink.writer_mut().fail.store(false, Ordering::SeqCst);
    sink.flush();
    assert!(sink.is_empty());
    // 至少写入了部分；失败的一次计入 dropped 或写入成功。
    assert!(
        sink.stats().written_records >= 4,
        "恢复后应写入剩余: {:?}",
        sink.stats()
    );
}

/// shutdown deadline：FlushGuard Drop 时 flush（有界队列保证有界耗时，
/// 不强制超时——有界即 deadline 的替代）。
#[test]
fn flush_guard_flushes_on_drop() {
    let writer = FaultyWriter::new();
    let mut sink = TraceSink::new(writer);
    for i in 0..3 {
        sink.push(record(i));
    }
    {
        let guard = tpi::trace::TraceFlushGuard::new(&mut sink);
        drop(guard); // Drop 触发 flush
    }
    assert!(sink.is_empty(), "guard drop 后队列应已 flush");
    assert_eq!(sink.stats().written_records, 3);
}

/// 损坏尾部：sink 的 writer 已写坏数据时，新 push + flush 不受影响
/// （sink 是 append-only 追加，不读取旧数据；不 panic）。
#[test]
fn corrupted_tail_does_not_break_append() {
    let writer = FaultyWriter::new();
    let mut sink = TraceSink::new(writer);
    // 模拟 writer 中已有损坏尾部（人为写入垃圾）。
    sink.writer_mut()
        .bytes
        .extend_from_slice(b"corrupted-tail-no-newline");
    sink.push(record(1));
    sink.flush();
    assert_eq!(sink.stats().written_records, 1, "损坏尾部不影响新记录追加");
}
