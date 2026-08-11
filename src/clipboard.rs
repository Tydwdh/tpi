//! 系统剪贴板（§用户诉求：Ctrl+C 复制选中文本）。
//!
//! 面向 Windows：OpenClipboard + EmptyClipboard + SetClipboardData(CF_UNICODETEXT)，
//! 用 windows-sys 的 Win32 剪贴板 API（不引入额外跨平台依赖）。
//!
//! 所有 Win32 FFI 都收束在本模块的安全包装函数内。

use std::ptr;

/// CF_UNICODETEXT（windows-sys 放在 Win32::System::Ole；值为 13）。
const CF_UNICODETEXT: u32 = 13;

/// 读取系统剪贴板文本（CF_UNICODETEXT）。
///
/// - `Ok(Some(text))`：剪贴板有 Unicode 文本；
/// - `Ok(None)`：无文本 / 剪贴板被占用 / 打开失败（粘贴尽力而为，不打断）；
/// - `Err`：读取过程出现硬错误（当前实现不会触发，保留语义完整性）。
///
/// §优化：粘贴快捷键直读剪贴板——不依赖终端 bracketed paste 支持，
/// 任何终端下 Ctrl+V 都能整段一次上屏。
pub fn read_text() -> std::io::Result<Option<String>> {
    if !open_clipboard() {
        return Ok(None);
    }
    let result = (|| {
        let h = get_clipboard_data(CF_UNICODETEXT);
        if h.is_null() {
            return Ok(None);
        }
        let lock = global_lock(h);
        if lock.is_null() {
            return Ok(None);
        }
        let mut len = 0usize;
        let ptr = lock.cast::<u16>();
        // SAFETY: GetClipboardData(CF_UNICODETEXT) 返回系统所有权的
        // NUL 结尾 UTF-16 内存；GlobalLock 保证持有锁期间该内存有效。
        // 扫描在字符串边界内停止，不会越界读取。
        unsafe {
            while *ptr.add(len) != 0 {
                len += 1;
            }
        }
        // SAFETY: 上述循环确认 ptr[0..len] 是已初始化的 UTF-16 单元。
        let wide = unsafe { std::slice::from_raw_parts(ptr, len) };
        let text = String::from_utf16_lossy(wide);
        global_unlock(h);
        Ok(Some(text))
    })();
    close_clipboard();
    result
}

/// 把 UTF-8 文本写入系统剪贴板（CF_UNICODETEXT）。
///
/// 失败（剪贴板被占用/打开失败）时静默返回 false——复制是尽力而为，
/// 不打断用户。
pub fn set_text(text: &str) -> bool {
    // 转 UTF-16（含结尾 NUL）。
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let Some(bytes) = wide.len().checked_mul(std::mem::size_of::<u16>()) else {
        return false;
    };

    // 在清空用户现有剪贴板前完成所有可能失败的本地分配与复制。
    let h = global_alloc(bytes);
    if h.is_null() {
        return false;
    }
    let lock = global_lock(h);
    if lock.is_null() {
        global_free(h);
        return false;
    }
    // SAFETY: `lock` points to the `bytes`-sized allocation above and `wide`
    // contains exactly `bytes` initialized bytes. The regions do not overlap.
    unsafe {
        ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), lock.cast::<u8>(), bytes);
    }
    global_unlock(h);

    if !open_clipboard() {
        global_free(h);
        return false;
    }
    let ok = (|| {
        if !empty_clipboard() {
            global_free(h);
            return false;
        }
        // GlobalAlloc(GMEM_MOVEABLE) 分配；仅 SetClipboardData 成功后所有权
        // 才转给系统。此前任一步失败都必须主动 GlobalFree。
        if set_clipboard_data(h).is_null() {
            global_free(h);
            return false;
        }
        true
    })();
    close_clipboard();
    ok
}

#[cfg(windows)]
fn open_clipboard() -> bool {
    use windows_sys::Win32::System::DataExchange::OpenClipboard;
    // SAFETY: a null owner window is explicitly supported by OpenClipboard.
    unsafe { OpenClipboard(ptr::null_mut()) != 0 }
}

#[cfg(windows)]
fn empty_clipboard() -> bool {
    use windows_sys::Win32::System::DataExchange::EmptyClipboard;
    // SAFETY: caller opens the clipboard before invoking this wrapper.
    unsafe { EmptyClipboard() != 0 }
}

#[cfg(windows)]
fn close_clipboard() -> bool {
    use windows_sys::Win32::System::DataExchange::CloseClipboard;
    // SAFETY: paired with a successful OpenClipboard call in set_text.
    unsafe { CloseClipboard() != 0 }
}

#[cfg(windows)]
fn global_alloc(bytes: usize) -> *mut std::ffi::c_void {
    use windows_sys::Win32::System::Memory::GlobalAlloc;
    const GMEM_MOVEABLE: u32 = 0x0002;
    // SAFETY: allocation size is derived from a live Vec and needs no alignment
    // stronger than the Win32 global-memory allocator provides.
    unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
}

#[cfg(windows)]
fn global_lock(h: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    use windows_sys::Win32::System::Memory::GlobalLock;
    // SAFETY: h is returned by global_alloc and remains owned by this module.
    unsafe { GlobalLock(h as _) }
}

#[cfg(windows)]
fn global_unlock(h: *mut std::ffi::c_void) {
    use windows_sys::Win32::System::Memory::GlobalUnlock;
    // SAFETY: h was successfully locked by global_lock above.
    let _ = unsafe { GlobalUnlock(h as _) };
}

#[cfg(windows)]
fn global_free(h: *mut std::ffi::c_void) {
    use windows_sys::Win32::Foundation::GlobalFree;
    // SAFETY: invoked only while this module still owns the allocation.
    let _ = unsafe { GlobalFree(h as _) };
}

#[cfg(windows)]
fn set_clipboard_data(h: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    use windows_sys::Win32::System::DataExchange::SetClipboardData;
    // SAFETY: clipboard is open and h is an unlocked GMEM_MOVEABLE allocation
    // containing a NUL-terminated UTF-16 string.
    unsafe { SetClipboardData(CF_UNICODETEXT, h as _) }
}

#[cfg(windows)]
fn get_clipboard_data(format: u32) -> *mut std::ffi::c_void {
    use windows_sys::Win32::System::DataExchange::GetClipboardData;
    // SAFETY: clipboard is open; GetClipboardData returns a system-owned
    // handle for the requested format (NULL when the format is absent).
    unsafe { GetClipboardData(format) }
}

// 非 Windows 平台（编译兜底；TPI 面向 Windows，此处仅保证可编译）。
#[cfg(not(windows))]
fn open_clipboard() -> bool {
    false
}
#[cfg(not(windows))]
fn empty_clipboard() -> bool {
    false
}
#[cfg(not(windows))]
fn close_clipboard() -> bool {
    false
}
#[cfg(not(windows))]
fn global_alloc(_bytes: usize) -> *mut std::ffi::c_void {
    ptr::null_mut()
}
#[cfg(not(windows))]
fn global_lock(_h: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    ptr::null_mut()
}
#[cfg(not(windows))]
fn global_unlock(_h: *mut std::ffi::c_void) {}
#[cfg(not(windows))]
fn global_free(_h: *mut std::ffi::c_void) {}
#[cfg(not(windows))]
fn set_clipboard_data(_h: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    ptr::null_mut()
}
#[cfg(not(windows))]
fn get_clipboard_data(_format: u32) -> *mut std::ffi::c_void {
    ptr::null_mut()
}
