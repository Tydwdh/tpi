//! 系统剪贴板（§用户诉求：Ctrl+C 复制选中文本）。
//!
//! 面向 Windows：OpenClipboard + EmptyClipboard + SetClipboardData(CF_UNICODETEXT)，
//! 用 windows-sys 的 Win32 剪贴板 API（不引入额外跨平台依赖）。
//!
//! `unsafe_op_in_unsafe_fn`：FFI 封装函数体内显式 unsafe 块（Rust 2024 约定）。

#![allow(unsafe_op_in_unsafe_fn)]

use std::ptr;

/// CF_UNICODETEXT（windows-sys 放在 Win32::System::Ole；值为 13）。
const CF_UNICODETEXT: u32 = 13;

/// 把 UTF-8 文本写入系统剪贴板（CF_UNICODETEXT）。
///
/// 失败（剪贴板被占用/打开失败）时静默返回 false——复制是尽力而为，
/// 不打断用户。
pub fn set_text(text: &str) -> bool {
    // 转 UTF-16（含结尾 NUL）。
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if !open_clipboard() {
            return false;
        }
        let ok = (|| {
            if !empty_clipboard() {
                return false;
            }
            // GlobalAlloc(GMEM_MOVEABLE) 分配；成功且 SetClipboardData 成功后
            // 由剪贴板接管。失败路径不释放（windows-sys 无 GlobalFree；单个
            // 失败泄漏可接受，且 SetClipboardData 几乎总是成功）。
            let bytes = wide.len() * 2;
            let h = global_alloc(bytes);
            if h.is_null() {
                return false;
            }
            let lock = global_lock(h);
            if lock.is_null() {
                return false;
            }
            ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, lock as *mut u8, bytes);
            global_unlock(h);
            !set_clipboard_data(h).is_null()
        })();
        close_clipboard();
        ok
    }
}

#[cfg(windows)]
unsafe fn open_clipboard() -> bool {
    use windows_sys::Win32::System::DataExchange::OpenClipboard;
    OpenClipboard(ptr::null_mut()) != 0
}

#[cfg(windows)]
unsafe fn empty_clipboard() -> bool {
    use windows_sys::Win32::System::DataExchange::EmptyClipboard;
    EmptyClipboard() != 0
}

#[cfg(windows)]
unsafe fn close_clipboard() -> bool {
    use windows_sys::Win32::System::DataExchange::CloseClipboard;
    CloseClipboard() != 0
}

#[cfg(windows)]
unsafe fn global_alloc(bytes: usize) -> *mut std::ffi::c_void {
    use windows_sys::Win32::System::Memory::GlobalAlloc;
    const GMEM_MOVEABLE: u32 = 0x0002;
    GlobalAlloc(GMEM_MOVEABLE, bytes)
}

#[cfg(windows)]
unsafe fn global_lock(h: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    use windows_sys::Win32::System::Memory::GlobalLock;
    GlobalLock(h as _)
}

#[cfg(windows)]
unsafe fn global_unlock(h: *mut std::ffi::c_void) {
    use windows_sys::Win32::System::Memory::GlobalUnlock;
    let _ = GlobalUnlock(h as _);
}

#[cfg(windows)]
unsafe fn set_clipboard_data(h: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    use windows_sys::Win32::System::DataExchange::SetClipboardData;
    SetClipboardData(CF_UNICODETEXT, h as _)
}

// 非 Windows 平台（编译兜底；TPI 面向 Windows，此处仅保证可编译）。
#[cfg(not(windows))]
unsafe fn open_clipboard() -> bool {
    false
}
#[cfg(not(windows))]
unsafe fn empty_clipboard() -> bool {
    false
}
#[cfg(not(windows))]
unsafe fn close_clipboard() -> bool {
    false
}
#[cfg(not(windows))]
unsafe fn global_alloc(_bytes: usize) -> *mut std::ffi::c_void {
    ptr::null_mut()
}
#[cfg(not(windows))]
unsafe fn global_lock(_h: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    ptr::null_mut()
}
#[cfg(not(windows))]
unsafe fn global_unlock(_h: *mut std::ffi::c_void) {}
#[cfg(not(windows))]
unsafe fn set_clipboard_data(_h: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    ptr::null_mut()
}
