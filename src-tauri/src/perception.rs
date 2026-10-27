// WorkMate · 状态感知（M5）
// 编制：WorkMate Contributors
//
// 原则（规划 §7.2）：**只感知"节奏"，不感知"内容"**。
//   - 键鼠：只读"最后的输入时间戳"，不记录按了什么键
//   - 前台：只取进程名并映射到 7 个类别，**不落窗口标题**
// 这样上层永远拿不到你在写什么文档，但知道你在编码还是在开会。
//
// 输出统一为 ContextSnapshot，直接对应 events 表的数值列（不存 JSON）。

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{CloseHandle, MAX_PATH, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, IsZoomed,
};

/// 应用类别。数值与 events.app_cat 一一对应。
/// 新增类别只能追加编号，不能改已有数值——历史 rollup_5m/day_profile
/// 里存的就是这些整数，改号会把旧统计整体映射错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppCategory {
    Ide = 0,
    Office = 1,
    Browser = 2,
    Meeting = 3,
    Im = 4,
    Media = 5,
    Other = 6,
    AiAgent = 7,
}

impl AppCategory {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn label(self) -> &'static str {
        match self {
            AppCategory::Ide => "IDE / 编码",
            AppCategory::Office => "Paper Work",
            AppCategory::Browser => "浏览器",
            AppCategory::Meeting => "会议",
            AppCategory::Im => "即时通讯",
            AppCategory::Media => "媒体",
            AppCategory::Other => "其他",
            AppCategory::AiAgent => "AI Agent",
        }
    }
}

/// 一次感知快照。字段与 events 表对应，零序列化开销。
#[derive(Debug, Clone, Copy)]
pub struct ContextSnapshot {
    pub ts: i64,
    pub idle_s: u32,
    pub app_cat: AppCategory,
    pub focus_min: u32,
    pub flags: u8,
    /// 前台是不是 WorkMate 自己。
    ///
    /// 用户打开面板看统计时，前台就是本程序——如果照常记账，
    /// 会凭空多出一段「其他」，把当天最重要的类别挤下去（实测「其他」曾排第一）。
    /// 这类采样不写库，也不参与专注/打断判定。
    pub is_self: bool,
}

pub const FLAG_FULLSCREEN: u8 = 1 << 0;
pub const FLAG_MEETING: u8 = 1 << 3;

pub fn now_sec() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 计算两个 32 位毫秒 Tick 之间的空闲秒数。
/// 必须用 `wrapping_sub` 而非 `saturating_sub`：`GetTickCount` 与 `LASTINPUTINFO.dwTime`
/// 均为 32 位无符号毫秒计数，开机满 49.7 天后会从 `u32::MAX` 回绕到 `0`，
/// `wrapping_sub` 在模 $2^{32}$ 算术下跨回绕点依然精确成立。
#[inline]
pub fn tick_diff_seconds(now_tick: u32, last_input_tick: u32) -> u32 {
    now_tick.wrapping_sub(last_input_tick) / 1000
}

/// 键鼠空闲秒数。只读时间戳，不碰输入内容，也不会触发杀软（不需要全局钩子）。
pub fn idle_seconds() -> u32 {
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut info).as_bool() {
            let now = windows::Win32::System::SystemInformation::GetTickCount();
            tick_diff_seconds(now, info.dwTime)
        } else {
            0
        }
    }
}

/// 前台窗口所属进程的可执行文件名（小写，如 "code.exe"）。
/// 拿不到就返回 None —— 上层按 Other 处理。
pub fn foreground_exe() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; MAX_PATH as usize];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);

        if !ok || len == 0 {
            return None;
        }

        let path = OsString::from_wide(&buf[..len as usize]);
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())?;
        Some(name)
    }
}

/// 进程名 → 类别。只做映射，不保存原始进程名。
pub fn classify(exe: &str) -> AppCategory {
    let e = exe.trim().to_ascii_lowercase();
    match e.as_str() {
        // 编码与终端 + Notepad 类文本编辑器 + 云桌面（VDI/RDP/Citrix/Horizon） + 虚拟机与远程开发终端
        "code.exe" | "code-insiders.exe" | "cursor.exe" | "idea64.exe" | "devenv.exe"
        | "pycharm64.exe" | "webstorm64.exe" | "goland64.exe" | "clion64.exe"
        | "rustrover64.exe" | "rider64.exe" | "datagrip64.exe" | "androidstudio64.exe"
        | "sublime_text.exe" | "notepad.exe" | "notepad++.exe" | "notepad2.exe"
        | "notepad3.exe" | "notepads.exe" | "editplus.exe" | "ultraedit.exe"
        | "uedit64.exe" | "emeditor.exe" | "winmergeu.exe" | "bcompare.exe"
        | "zed.exe" | "nvim.exe" | "vim.exe" | "gvim.exe"
        | "windowsterminal.exe" | "wt.exe" | "powershell.exe" | "pwsh.exe" | "cmd.exe"
        | "bash.exe" | "git-bash.exe" | "mintty.exe" | "wezterm-gui.exe" | "alacritty.exe"
        // Windows 远程桌面 / Azure Virtual Desktop / Hyper-V / WSL
        | "mstsc.exe" | "msrdc.exe" | "msrdcw.exe" | "vmconnect.exe" | "wsl.exe" | "vcxsrv.exe" | "x410.exe"
        // Citrix 云桌面（Workspace / Receiver）
        | "wfica32.exe" | "wfcrun32.exe" | "cdviewer.exe" | "selfservice.exe" | "citrixworkspace.exe" | "concentr.exe"
        // VMware Horizon 云桌面 & VMware Workstation / Player 虚拟机
        | "vmware-view.exe" | "vmware.exe" | "vmplayer.exe" | "vmware-remotemks.exe" | "vmware-kvm.exe"
        // VirtualBox / QEMU / Spice 虚拟机
        | "virtualbox.exe" | "virtualboxvm.exe" | "qemu-system-x86_64.exe" | "qemu-system-aarch64.exe" | "remote-viewer.exe"
        // 深信服 aDesk / 华为云桌面 FusionAccess & Workspace / 阿里云无影 / 中兴 / 天翼云桌面
        | "adesk.exe" | "sangforvdi.exe" | "vdi_client.exe" | "vdiclient.exe" | "hnaclient.exe"
        | "hwcloud.exe" | "fusionaccess.exe" | "workspaceclient.exe" | "hdpclient.exe" | "accessclient.exe"
        | "wuying.exe" | "edsclient.exe" | "zcloud.exe" | "ctyunclouddesktop.exe" | "clouddesktop.exe"
        // 远程开发机连接与 SSH 终端
        | "xshell.exe" | "xftp.exe" | "mobaxterm.exe" | "windterm.exe" | "securecrt.exe"
        | "putty.exe" | "kitty.exe" | "tabby.exe" | "finalshell.exe" | "termius.exe" | "electerm.exe"
        | "parsec.exe" | "rustdesk.exe" | "todesk.exe" | "sunloginclient.exe" | "anydesk.exe"
        | "teamviewer.exe" | "nomachine.exe" | "nxplayer.bin" | "nxclient.exe" => {
            AppCategory::Ide
        }
        // Paper Work：文档 / 表格 / 演示 / 邮件 / 笔记 / 图表。
        // 这一类是「产出型」工作，和 IDE 的「构建型」分开看才有意义。
        "winword.exe" | "excel.exe" | "powerpnt.exe" | "outlook.exe" | "onenote.exe"
        | "wps.exe" | "et.exe" | "wpp.exe" | "wpspdf.exe" | "wpsoffice.exe"
        | "acrobat.exe" | "acrord32.exe" | "foxitpdfreader.exe" | "sumatrapdf.exe"
        | "typora.exe" | "obsidian.exe" | "notion.exe" | "joplin.exe" | "youdaonote.exe"
        | "evernote.exe" | "visio.exe" | "drawio.exe" | "xmind.exe"
        | "wemail.exe" /* 企业微信邮箱客户端 */ | "thunderbird.exe" | "foxmail.exe" => {
            AppCategory::Office
        }
        // 浏览器
        "chrome.exe" | "msedge.exe" | "firefox.exe" | "brave.exe" | "opera.exe" | "vivaldi.exe" => {
            AppCategory::Browser
        }
        // 会议**专用**客户端。飞书/钉钉/Teams 也带会议，但它们的主场景是聊天，
        // 归到会议会让「会议时长」被日常沟通撑爆（实测：一整天开着飞书 =
        // 整天都在"开会"）。所以只把专门的会议端放这里。
        "wemeetapp.exe" | "tencentmeeting.exe" | "zoom.exe" | "webexmta.exe" | "webex.exe"
        | "gotomeeting.exe" | "bluejeans.exe" => AppCategory::Meeting,
        // 即时通讯（含企业 IM；企业微信的进程名是 WXWork.exe）
        "wxwork.exe" | "wxworkweb.exe" | "wechat.exe" | "weixin.exe" | "qq.exe" | "tim.exe"
        | "telegram.exe" | "slack.exe" | "discord.exe" | "whatsapp.exe" | "feishu.exe"
        | "lark.exe" | "dingtalk.exe" | "ms-teams.exe" | "teams.exe" => AppCategory::Im,
        // 媒体
        "potplayermini64.exe" | "potplayer.exe" | "vlc.exe" | "mpc-hc64.exe" | "mpc-be64.exe"
        | "musicbee.exe" | "cloudmusic.exe" | "qqmusic.exe" => AppCategory::Media,
        // AI Agent：独立成类，不并进 IDE。
        // 这些工具的核心动作是「对话/委派/生成」，不是连续写代码；
        // 混进 IDE 会让「编码时长」虚高，也看不到每天真正用了多久 Agent。
        "workbuddy.exe" | "zcode.exe" | "chatgpt.exe" | "antigravity.exe" | "qoder.exe"
        | "trae.exe" | "windsurf.exe" | "claude.exe" | "cherry-studio.exe"
        | "chatbox.exe" | "doubao.exe" | "kimi.exe" => {
            AppCategory::AiAgent
        }
        _ => {
            // 兼容带版本号或企业定制后缀的 Notepad / 终端 / 云桌面 / 虚拟机可执行文件
            // （如 MobaXterm_Personal_24.1.exe、Xshell7.exe、VMware-view-client.exe 等）
            if e.starts_with("notepad")
                || e.starts_with("mobaxterm")
                || e.starts_with("xshell")
                || e.starts_with("putty")
                || e.starts_with("securecrt")
                || e.starts_with("windterm")
                || e.starts_with("vmware")
                || e.starts_with("virtualbox")
                || e.starts_with("vbox")
                || e.starts_with("qemu")
                || e.starts_with("wfica")
                || e.starts_with("citrix")
                || e.starts_with("msrdc")
                || e.starts_with("mstsc")
                || e.contains("vdi")
                || e.contains("clouddesktop")
                || e.contains("cloud_desktop")
                || e.contains("fusionaccess")
                || e.contains("wuying")
            {
                AppCategory::Ide
            } else {
                AppCategory::Other
            }
        }
    }
}

/// 判断窗口标题是否表明用户正在通过 Web 浏览器或企业自研壳访问「云桌面 / 虚拟机 / 远程开发机」。
/// 仅在内存中做瞬时关键词匹配，绝不记录或落库窗口标题。
pub fn is_cloud_or_vm_title(title: &str) -> bool {
    let t = title.trim();
    if t.is_empty() {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    const CN_KEYWORDS: &[&str] = &[
        "云桌面",
        "虚拟桌面",
        "远程桌面",
        "桌面云",
        "虚拟机",
        "无影云",
        "深信服",
    ];
    if CN_KEYWORDS.iter().any(|k| t.contains(k)) {
        return true;
    }
    const EN_KEYWORDS: &[&str] = &[
        "cloud desktop",
        "virtual desktop",
        "remote desktop",
        "citrix workspace",
        "citrix receiver",
        "vmware horizon",
        "vsphere client",
        "proxmox virtual environment",
        "novnc",
        "apache guacamole",
        "jupyterlab",
        "code-server",
    ];
    EN_KEYWORDS.iter().any(|k| lower.contains(k))
}

fn foreground_is_cloud_or_vm_by_title() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return false;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        is_cloud_or_vm_title(&title)
    }
}

/// 采集一次快照。
/// focus_min 由调用方累加（需要跨采样周期的连续判断），这里置 0。
pub fn sample(focus_min: u32) -> ContextSnapshot {
    let exe = foreground_exe();
    // 先判自己：WorkMate 的进程名固定，不必进 classify 的匹配表
    let is_self = exe.as_deref() == Some("workmate.exe");
    let mut app_cat = exe.as_deref().map(classify).unwrap_or(AppCategory::Other);
    if matches!(app_cat, AppCategory::Browser | AppCategory::Other) && foreground_is_cloud_or_vm_by_title() {
        app_cat = AppCategory::Ide;
    }
    LAST_CAT.store(app_cat.as_i64(), Ordering::Relaxed);
    LAST_FOCUS_MIN.store(focus_min as i64, Ordering::Relaxed);
    let mut flags = 0u8;
    if app_cat == AppCategory::Meeting {
        flags |= FLAG_MEETING;
    }
    if fullscreen_foreground() {
        flags |= FLAG_FULLSCREEN;
    }
    ContextSnapshot {
        ts: now_sec(),
        idle_s: idle_seconds(),
        app_cat,
        focus_min,
        flags,
        is_self,
    }
}

/// 最近一次采样的应用类别（供提醒调度判断「是否在开会」这类场景，避免重复取窗口）
static LAST_CAT: AtomicI64 = AtomicI64::new(6);
/// 最近一次采样的连续活跃/久坐分钟数
static LAST_FOCUS_MIN: AtomicI64 = AtomicI64::new(0);

pub fn current_sit_mins() -> i64 {
    LAST_FOCUS_MIN.load(Ordering::Relaxed).max(0)
}

pub fn current_category() -> AppCategory {
    match LAST_CAT.load(Ordering::Relaxed) {
        0 => AppCategory::Ide,
        1 => AppCategory::Office,
        2 => AppCategory::Browser,
        3 => AppCategory::Meeting,
        4 => AppCategory::Im,
        5 => AppCategory::Media,
        7 => AppCategory::AiAgent,
        _ => AppCategory::Other,
    }
}

/* ------------------------------ 演示 / 投屏检测 ------------------------------ */

/// 自动检测到的全屏前台（演示、全屏视频、投屏放映）
static AUTO_PRESENT: AtomicBool = AtomicBool::new(false);
/// 手动演示模式到期时间（0 = 未开启）。窗口化共享屏幕无法自动识别，靠它兜底。
static MANUAL_UNTIL: AtomicI64 = AtomicI64::new(0);

/// 当前是否处于「应当让位」的演示状态（自动 or 手动）
pub fn presenting() -> bool {
    AUTO_PRESENT.load(Ordering::Relaxed) || now_sec() < MANUAL_UNTIL.load(Ordering::Relaxed)
}

/// 给面板/日志用的明细
pub fn presenting_detail() -> (bool, bool, i64) {
    let manual_left = (MANUAL_UNTIL.load(Ordering::Relaxed) - now_sec()).max(0);
    (presenting(), AUTO_PRESENT.load(Ordering::Relaxed), manual_left)
}

/// 手动演示模式：secs 秒后自动解除；传 0 立即解除
pub fn set_manual_present(secs: i64) {
    MANUAL_UNTIL.store(if secs > 0 { now_sec() + secs } else { 0 }, Ordering::Relaxed);
}

/// 更新自动检测结果，返回「状态是否发生变化」
pub fn update_auto_present() -> bool {
    let now = fullscreen_foreground();
    AUTO_PRESENT.swap(now, Ordering::Relaxed) != now
}

/// 前台窗口是否「真正全屏」。
///
/// 三个排除条件缺一不可（都是真机踩出来的）：
///   ① 自己的窗口：宠物窗置顶，否则会把自己判成全屏
///   ② 最大化（IsZoomed）：最大化的浏览器/IDE 也铺满屏幕，但任务栏还在，
///      用户仍在工作，此时藏猫属于误伤。真正的全屏（F11 / 幻灯片放映）不是 zoomed 状态。
///   ③ 桌面/任务栏窗口：Progman / WorkerW / Shell_TrayWnd
///
/// 剩余判定：窗口矩形覆盖所在显示器的 99%（4px 容差吸收边框与 DPI 取整误差）
pub fn fullscreen_foreground() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == std::process::id() {
            return false;
        }

        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool() {
            return false;
        }

        let mut cls = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut cls);
        let class = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Windows.UI.Core.CoreWindow") {
            return false;
        }

        // 云桌面（mstsc / Citrix / Horizon / aDesk 等）、虚拟机与 IDE 经常以无边框全屏运行。
        // 此时用户正在云桌面/虚拟机内编码工作，而非投屏放 PPT，绝不能误判成演示模式把桌宠藏起来（实测踩到）。
        if let Some(exe) = foreground_exe() {
            let cat = classify(&exe);
            if cat == AppCategory::Ide
                || (matches!(cat, AppCategory::Browser | AppCategory::Other)
                    && foreground_is_cloud_or_vm_by_title())
            {
                return false;
            }
        }

        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        let tol = 4;
        rect.left <= m.left + tol
            && rect.top <= m.top + tol
            && rect.right >= m.right - tol
            && rect.bottom >= m.bottom - tol
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_agent_apps_are_separate_category() {
        for exe in [
            "workbuddy.exe",
            "zcode.exe",
            "chatgpt.exe",
            "antigravity.exe",
            "qoder.exe",
        ] {
            assert_eq!(
                classify(exe),
                AppCategory::AiAgent,
                "{exe} 应该统计到 AI Agent，而不是其他"
            );
        }
    }

    #[test]
    fn ai_agent_category_id_is_stable() {
        // 类别编号一旦落库就不能改，历史统计靠它映射
        assert_eq!(AppCategory::AiAgent.as_i64(), 7);
        assert_eq!(AppCategory::AiAgent.label(), "AI Agent");
    }

    #[test]
    fn unrelated_apps_not_misclassified() {
        assert_eq!(classify("code.exe"), AppCategory::Ide);
        assert_eq!(classify("chrome.exe"), AppCategory::Browser);
        assert_eq!(classify("randomtool.exe"), AppCategory::Other);
    }

    #[test]
    fn cloud_desktop_vm_and_notepad_are_classified_as_ide() {
        for exe in [
            // Notepad 类
            "notepad.exe",
            "notepad++.exe",
            "notepad2.exe",
            "notepad3.exe",
            "sublime_text.exe",
            "ultraedit.exe",
            // 云桌面与远程连接
            "mstsc.exe",
            "msrdc.exe",
            "wfica32.exe",
            "cdviewer.exe",
            "vmware-view.exe",
            "adesk.exe",
            "sangforvdi.exe",
            "vdi_client.exe",
            "workspaceclient.exe",
            "wuying.exe",
            // 虚拟机与远程终端
            "vmware.exe",
            "vmplayer.exe",
            "virtualboxvm.exe",
            "vmconnect.exe",
            "xshell.exe",
            "mobaxterm_personal_24.1.exe",
            "windterm.exe",
            "putty.exe",
        ] {
            assert_eq!(
                classify(exe),
                AppCategory::Ide,
                "{exe} 应归类为 IDE / 编码"
            );
        }
        assert!(is_cloud_or_vm_title("研发云桌面 - 华为云"));
        assert!(is_cloud_or_vm_title("Citrix Workspace - Desktop"));
        assert!(!is_cloud_or_vm_title("百度一下，你就知道"));
    }

    #[test]
    fn paper_work_apps() {
        for exe in [
            "winword.exe",
            "excel.exe",
            "powerpnt.exe",
            "wps.exe",
            "et.exe",
            "wpp.exe",
            "typora.exe",
            "obsidian.exe",
            "sumatrapdf.exe",
            "wemail.exe",
        ] {
            assert_eq!(classify(exe), AppCategory::Office, "{exe} 应归 Paper Work");
        }
        assert_eq!(AppCategory::Office.label(), "Paper Work");
    }

    #[test]
    fn meeting_apps() {
        for exe in [
            "wemeetapp.exe",
            "tencentmeeting.exe",
            "zoom.exe",
            "webexmta.exe",
        ] {
            assert_eq!(classify(exe), AppCategory::Meeting, "{exe} 应归会议");
        }
    }

    #[test]
    fn enterprise_im_not_counted_as_meeting() {
        // 飞书/钉钉/Teams/企业微信的主场景是聊天。归到会议会让「会议时长」
        // 被日常沟通撑爆——一整天挂着飞书 = 整天都在"开会"。
        for exe in ["wxwork.exe", "feishu.exe", "lark.exe", "dingtalk.exe", "ms-teams.exe"] {
            assert_eq!(classify(exe), AppCategory::Im, "{exe} 应归即时通讯");
        }
    }

    #[test]
    fn tick_diff_handles_u32_wraparound() {
        assert_eq!(tick_diff_seconds(15_000, 5_000), 10);
        // 跨越 49.7 天 u32::MAX 回绕点：上次输入在 MAX-4999，当前在 5000，间隔正好 10000ms = 10s
        assert_eq!(tick_diff_seconds(5_000, u32::MAX - 4_999), 10);
    }
}
