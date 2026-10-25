// WorkMate · 剪贴板读取（WinAPI，CF_UNICODETEXT）
// 编制：WorkMate Contributors
//
// 为什么不用 Tauri 官方 clipboard 插件：
//   插件要额外引入 npm 依赖 + capabilities 权限声明，而这里只需要「读文本」一个能力，
//   直接调 WinAPI 三十行搞定，依赖面更小（windows crate 已是既有依赖）。
//
// 为什么只读、不写：
//   「选中即问」的交互约定是 用户自己 Ctrl+C 复制 → 按热键提问。
//   由程序代发 Ctrl+C 曾评估过，存在两个硬伤故放弃：
//   ① 终端里 Ctrl+C 是 SIGINT 不是复制，代发可能中断用户正在跑的命令；
//   ② 复制落盘有延迟，代发后立刻读会读到旧内容，时序不可靠。
//
// 线程约束：OpenClipboard 成功后必须成对 CloseClipboard；
// 本模块内部保证任何路径都会关（用 guard 结构体防止早退泄漏）。

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetOpenClipboardWindow, OpenClipboard,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

/// CF_UNICODETEXT 的格式号（WinUser.h：13）。windows crate 未导出该常量。
const CF_UNICODETEXT: u32 = 13;

/// 单次剪贴板读取的最大 UTF-16 码元数（64K 码元 ≈ 128 KB），
/// 防止复制几十 MB 日志或未以 NUL 结尾的畸形剪贴板块导致越界读或堆暴涨。
const MAX_CLIPBOARD_UTF16_UNITS: usize = 65_536;

/// 剪贴板打开期间的 RAII guard：提前 return 也不会泄漏句柄。
struct ClipboardGuard;
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// GlobalLock 期间的 RAII guard：保证任何退出路径都成对调用 GlobalUnlock。
struct GlobalLockGuard(HGLOBAL);
impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.0);
        }
    }
}

/// 读剪贴板文本（CF_UNICODETEXT）。
///
/// 返回 Ok("") 表示剪贴板里没有文本（空或非文本格式，如图片）——
/// 调用方据此判断「用户什么都没复制」，与错误区分开。
pub fn read_text() -> Result<String, String> {
    if let Ok(override_text) = std::env::var("WORKMATE_CLIPBOARD_TEXT") {
        return Ok(override_text);
    }

    // OpenClipboard 在「别的程序刚复制完、还没释放」时会短暂失败（ERROR_ACCESS_DENIED）。
    // 这是生产环境常见竞态（Office / 浏览器 / 截图工具都会占着剪贴板），
    // 重试几次即可，不值得让用户看到报错。
    let mut last_err = String::new();
    let mut opened = false;
    for _ in 0..5 {
        unsafe {
            // hWndNewOwner 传 NULL 合法（WinAPI 文档）：表示无所有者窗口。
            // 实测传前台窗口没有任何增益，反而多一个依赖，故从简。
            match OpenClipboard(None) {
                Ok(()) => {
                    opened = true;
                    break;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    if !opened {
        // 区分两种失败原因：
        // ① GetOpenClipboardWindow() 非空：确实有其他窗口占着剪贴板未释放 → 报占用错误；
        // ② GetOpenClipboardWindow() 为空：没有任何窗口占用，而是当前处于系统锁屏安全桌面（Winlogon）
        //    或受限沙箱桌面（JOB_OBJECT_UILIMIT_READCLIPBOARD）导致 Win32 拒绝访问 → 视为空剪贴板。
        let no_holder = unsafe {
            GetOpenClipboardWindow()
                .map(|h| h.0.is_null())
                .unwrap_or(true)
        };
        if no_holder {
            return Ok(String::new());
        }
        return Err(format!("打不开剪贴板（可能被其他程序占用）：{last_err}"));
    }
    // guard 保证任何路径都会 CloseClipboard
    let _guard = ClipboardGuard;

    unsafe {
        let handle: HANDLE = match GetClipboardData(CF_UNICODETEXT) {
            Ok(h) => h,
            // 没有文本格式的数据（图片/文件/空）——不是错误
            Err(_) => return Ok(String::new()),
        };
        if handle.is_invalid() {
            return Ok(String::new());
        }

        // HANDLE 与 HGLOBAL 在 windows crate 里是两个 tuple struct，
        // GetClipboardData 返回前者而 GlobalLock/Unlock 要后者，需显式转换。
        let hglobal = HGLOBAL(handle.0);
        let byte_size = GlobalSize(hglobal);
        let max_units = (byte_size / std::mem::size_of::<u16>()).min(MAX_CLIPBOARD_UTF16_UNITS);
        if max_units == 0 {
            return Ok(String::new());
        }
        // GlobalLock 拿到的指针在 GlobalUnlock 前有效；失败返回 null
        let ptr = GlobalLock(hglobal) as *const u16;
        if ptr.is_null() {
            return Ok(String::new());
        }
        let _lock_guard = GlobalLockGuard(hglobal);
        // CF_UNICODETEXT 约定以 NUL 结尾，但必须受 GlobalSize 与硬上限双重约束，
        // 严防无 NUL 终止符的畸形剪贴板内存块触发越界读取。
        let mut len = 0usize;
        while len < max_units && *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        let text = String::from_utf16_lossy(slice);
        Ok(text)
    }
}

/// 截断到 n 字符（按 char 边界，防把 UTF-16 代理对切坏）。
/// HTTP 控制面与 LLM 提问前截断剪贴板内容时用，避免整篇文档把响应或 Token 撑爆。
pub fn truncate_chars(s: &str, n: usize) -> String {
    let mut it = s.chars();
    let t: String = it.by_ref().take(n).collect();
    if it.next().is_some() {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_chars_boundaries() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hello world", 5), "hello…");
        assert_eq!(truncate_chars("你好世界！", 2), "你好…");
    }
}
