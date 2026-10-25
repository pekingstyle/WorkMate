// WorkMate · 资源看护（CPU / 内存 / 句柄熔断）
// 编制：WorkMate Contributors
//
// 为什么需要它：
//   桌面助手是常驻进程。一旦某个循环泄漏内存或空转烧 CPU，用户只会觉得
//   「风扇狂转 / 电脑变卡」，根本不会想到是它。这里做三件事：
//     1) 每 20s 采样自身资源，并**把 WebView2 子进程树一起算进去**
//        （Chromium 渲染进程才是内存大头，只看主进程会漏掉真正的泄漏）
//     2) 超硬阈值连续两次 → 先写诊断快照（这是事后 Debug 的唯一线索）再优雅退出
//     3) 指标暴露给 HTTP 控制面，测试侧可断言；阈值可运行时调低以验证熔断路径
//
// 默认阈值（验收指标：私有内存 < 200MB、idle CPU < 1%，阈值留了 3~4 倍余量）：
//   · 私有内存 软 500MB / 硬 900MB（含子进程树）
//   · CPU       软 25%   / 硬 60%（相对单核满载）
//   · 句柄      软 4000  /（只告警，句柄泄漏不致命且退出会丢用户现场）
//
// 采样成本：Toolhelp 快照 + GetProcessMemoryInfo，每 20s 一次 < 2ms，可忽略。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use tauri::{Emitter, Manager};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount, GetProcessTimes};

/// 采样结果。字段名直接对应 /resource 的 JSON。
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct Metrics {
    pub ts: i64,
    /// 自身工作集（MB）
    pub rss_mb: f64,
    /// 自身私有提交（MB）——泄漏看这个
    pub priv_mb: f64,
    /// 含全部后代进程（WebView2 浏览器 + 渲染器）的工作集合计（MB）
    /// 注意：工作集含各进程重复计入的共享 DLL 页，指标偏高，仅作参考
    pub tree_rss_mb: f64,
    /// 含全部后代进程的**私有提交**合计（MB）——这才是真实内存成本与泄漏指标
    pub tree_priv_mb: f64,
    /// 后代进程数
    pub procs: usize,
    /// 句柄数
    pub handles: u32,
    /// 距上次采样的全进程树 CPU 占用（%，相对单核满载，含 WebView2 子进程树）
    pub cpu_pct: f64,
}

/* ------------------------------ 阈值与状态 ------------------------------ */

static MEM_SOFT_MB: AtomicU64 = AtomicU64::new(500);
static MEM_HARD_MB: AtomicU64 = AtomicU64::new(900);
static CPU_SOFT_X100: AtomicU64 = AtomicU64::new(2500); // 25.00%
static CPU_HARD_X100: AtomicU64 = AtomicU64::new(6000); // 60.00%
static INTERVAL_S: AtomicU64 = AtomicU64::new(20);
static HARD_BREACH: AtomicU32 = AtomicU32::new(0);
static TRIPPED: AtomicU32 = AtomicU32::new(0);

fn last_metrics() -> &'static Mutex<Metrics> {
    static M: OnceLock<Mutex<Metrics>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(Metrics::default()))
}

/// CPU 差分用的上一拍（各 PID 的累计 CPU 时间 100ns + 墙钟）。
/// 按 PID 独立记账可同时覆盖主进程与 WebView2 子进程树，且子进程退出时不会造成差分下溢。
fn last_cpu() -> &'static Mutex<Option<(HashMap<u32, u64>, Instant)>> {
    static M: OnceLock<Mutex<Option<(HashMap<u32, u64>, Instant)>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

pub fn configure(mem_soft: Option<u64>, mem_hard: Option<u64>, cpu_soft: Option<f64>, cpu_hard: Option<f64>) {
    if let Some(v) = mem_soft {
        MEM_SOFT_MB.store(v, Ordering::Relaxed);
    }
    if let Some(v) = mem_hard {
        MEM_HARD_MB.store(v, Ordering::Relaxed);
    }
    if let Some(v) = cpu_soft {
        CPU_SOFT_X100.store((v * 100.0) as u64, Ordering::Relaxed);
    }
    if let Some(v) = cpu_hard {
        CPU_HARD_X100.store((v * 100.0) as u64, Ordering::Relaxed);
    }
}

pub fn thresholds() -> serde_json::Value {
    serde_json::json!({
        "mem_soft_mb": MEM_SOFT_MB.load(Ordering::Relaxed),
        "mem_hard_mb": MEM_HARD_MB.load(Ordering::Relaxed),
        "cpu_soft_pct": CPU_SOFT_X100.load(Ordering::Relaxed) as f64 / 100.0,
        "cpu_hard_pct": CPU_HARD_X100.load(Ordering::Relaxed) as f64 / 100.0,
        "interval_s": INTERVAL_S.load(Ordering::Relaxed),
        "tripped": TRIPPED.load(Ordering::Relaxed) != 0,
    })
}

/* ------------------------------ 采样 ------------------------------ */

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mem_of(process: windows::Win32::Foundation::HANDLE) -> Option<(f64, f64)> {
    unsafe {
        let mut ex = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // 函数签名要的是 PROCESS_MEMORY_COUNTERS*，按 WinAPI 约定传 EX 指针 + EX 大小即可
        GetProcessMemoryInfo(
            process,
            &mut ex as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
        .ok()?;
        Some((
            ex.WorkingSetSize as f64 / 1_048_576.0,
            ex.PrivateUsage as f64 / 1_048_576.0,
        ))
    }
}

fn cpu_time_of(process: windows::Win32::Foundation::HANDLE) -> Option<u64> {
    unsafe {
        let mut c = windows::Win32::Foundation::FILETIME::default();
        let mut e = windows::Win32::Foundation::FILETIME::default();
        let mut k = windows::Win32::Foundation::FILETIME::default();
        let mut u = windows::Win32::Foundation::FILETIME::default();
        GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u).ok()?;
        let user = (u.dwHighDateTime as u64) << 32 | u.dwLowDateTime as u64;
        let kern = (k.dwHighDateTime as u64) << 32 | k.dwLowDateTime as u64;
        Some(user.saturating_add(kern))
    }
}

/// 枚举后代进程（BFS，任意深度）并累加工作集、私有提交与各 PID 的累计 CPU 时间（100ns）。
/// WebView2 的浏览器进程是直接子进程，渲染器/GPU 是孙进程，必须递归。
fn tree_usage(cpu_times: &mut HashMap<u32, u64>) -> (f64, f64, usize) {
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return (0.0, 0.0, 0),
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        // pid -> parent_pid
        let mut parent: Vec<(u32, u32)> = Vec::with_capacity(512);
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                parent.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);

        let me = std::process::id();
        // BFS 收集后代
        let mut queue = vec![me];
        let mut descendants: Vec<u32> = Vec::new();
        while let Some(p) = queue.pop() {
            for &(pid, ppid) in &parent {
                if ppid == p && pid != me && !descendants.contains(&pid) {
                    descendants.push(pid);
                    queue.push(pid);
                }
            }
        }

        let mut ws_total = 0.0;
        let mut priv_total = 0.0;
        let mut count = 0;
        for pid in &descendants {
            // PROCESS_QUERY_LIMITED_INFORMATION(0x1000)：同用户进程即可查询，无需提权
            let h = windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                windows::Win32::Foundation::BOOL(0),
                *pid,
            );
            if let Ok(h) = h {
                if let Some((ws, priv_mb)) = mem_of(h) {
                    ws_total += ws;
                    priv_total += priv_mb;
                    count += 1;
                }
                if let Some(t) = cpu_time_of(h) {
                    cpu_times.insert(*pid, t);
                }
                let _ = CloseHandle(h);
            }
        }
        (ws_total, priv_total, count)
    }
}

/// 采一拍。cpu_pct 是「距上次采样」的全进程树差分（主进程 + WebView2 子进程树），首次调用返回 0。
pub fn sample() -> Metrics {
    let mut m = Metrics {
        ts: now_sec(),
        ..Default::default()
    };

    let mut cur_cpu: HashMap<u32, u64> = HashMap::with_capacity(16);
    unsafe {
        let me = GetCurrentProcess();
        if let Some((ws, priv_mb)) = mem_of(me) {
            m.rss_mb = (ws * 10.0).round() / 10.0;
            m.priv_mb = (priv_mb * 10.0).round() / 10.0;
        }
        let mut hc = 0u32;
        if GetProcessHandleCount(me, &mut hc).is_ok() {
            m.handles = hc;
        }
        if let Some(t) = cpu_time_of(me) {
            cur_cpu.insert(std::process::id(), t);
        }
    }

    let (tree_ws, tree_priv, procs) = tree_usage(&mut cur_cpu);
    m.tree_rss_mb = ((m.rss_mb + tree_ws) * 10.0).round() / 10.0;
    m.tree_priv_mb = ((m.priv_mb + tree_priv) * 10.0).round() / 10.0;
    m.procs = procs;

    let now = Instant::now();
    if let Ok(mut g) = last_cpu().lock() {
        if let Some((ref prev_map, prev_at)) = *g {
            let wall = now.duration_since(prev_at).as_secs_f64();
            if wall > 0.05 {
                let mut delta_100ns: u64 = 0;
                for (&pid, &cur_t) in &cur_cpu {
                    if let Some(&prev_t) = prev_map.get(&pid) {
                        delta_100ns = delta_100ns.saturating_add(cur_t.saturating_sub(prev_t));
                    }
                }
                let used = delta_100ns as f64 / 1e7;
                m.cpu_pct = ((used / wall) * 1000.0).round() / 10.0;
            }
        }
        *g = Some((cur_cpu, now));
    }

    if let Ok(mut g) = last_metrics().lock() {
        *g = m.clone();
    }
    m
}

/// 最近一次循环采样的结果（无副作用，供 /health 用）
pub fn last() -> Metrics {
    last_metrics().lock().map(|g| g.clone()).unwrap_or_default()
}

/* ------------------------------ 看护循环 ------------------------------ */

/// 启动看护线程。
///
/// 熔断动作顺序（重要）：先落诊断快照 → 再推事件（让宠物说一句）→ 最后退出。
/// 顺序反了的话进程先死，日志和事件都留不下来。
pub fn start(app: tauri::AppHandle) {
    if let Ok(v) = std::env::var("WORKMATE_WD_INTERVAL_S") {
        if let Ok(n) = v.parse::<u64>() {
            if n >= 1 {
                INTERVAL_S.store(n, Ordering::Relaxed);
                crate::startup_log(&format!("watchdog interval = {n}s (env)"));
            }
        }
    }

    std::thread::spawn(move || {
        // 线程一启动先采一拍：/health 立刻有数据，不必等第一个 interval
        sample();
        // 再等 10s，避开冷启动的资源高峰（Chromium 初始化本来就吃内存）
        std::thread::sleep(std::time::Duration::from_secs(10));
        let mut warned_mem = false;
        let mut warned_cpu = false;

        loop {
            let interval = INTERVAL_S.load(Ordering::Relaxed).max(1);
            std::thread::sleep(std::time::Duration::from_secs(interval));

            let m = sample();
            let mem_soft = MEM_SOFT_MB.load(Ordering::Relaxed) as f64;
            let mem_hard = MEM_HARD_MB.load(Ordering::Relaxed) as f64;
            let cpu_soft = CPU_SOFT_X100.load(Ordering::Relaxed) as f64 / 100.0;
            let cpu_hard = CPU_HARD_X100.load(Ordering::Relaxed) as f64 / 100.0;

            // 阈值一律用「私有提交」判定：工作集含共享 DLL 页，波动大且偏高
            let mem_over_soft = m.tree_priv_mb >= mem_soft;
            let mem_over_hard = m.tree_priv_mb >= mem_hard;
            let cpu_over_soft = m.cpu_pct >= cpu_soft;
            let cpu_over_hard = m.cpu_pct >= cpu_hard;

            if (mem_over_soft || cpu_over_soft) && !(warned_mem && warned_cpu) {
                crate::startup_log(&format!(
                    "watchdog WARN | tree_priv={:.1}MB tree_rss={:.1}MB cpu={:.1}% procs={} handles={}",
                    m.tree_priv_mb, m.tree_rss_mb, m.cpu_pct, m.procs, m.handles
                ));
                warned_mem |= mem_over_soft;
                warned_cpu |= cpu_over_soft;
            }
            if !mem_over_soft && !cpu_over_soft {
                warned_mem = false;
                warned_cpu = false;
            }

            if mem_over_hard || cpu_over_hard {
                let n = HARD_BREACH.fetch_add(1, Ordering::Relaxed) + 1;
                crate::startup_log(&format!(
                    "watchdog HARD breach #{n} | tree_priv={:.1}MB tree_rss={:.1}MB cpu={:.1}% procs={} handles={}",
                    m.tree_priv_mb, m.tree_rss_mb, m.cpu_pct, m.procs, m.handles
                ));
                // 连续两次才熔断：单次尖峰（如刚弹出的面板首帧）不算故障
                if n >= 2 {
                    TRIPPED.store(1, Ordering::Relaxed);
                    let db = app.state::<crate::db::Db>();
                    let db_path = db.path().display().to_string();
                    let db_size = db.size_bytes();
                    crate::startup_log(&format!(
                        "watchdog TRIP | tree_priv={:.1}MB tree_rss={:.1}MB priv={:.1}MB cpu={:.1}% procs={} handles={} db={} ({:.1}MB) th={}",
                        m.tree_priv_mb, m.tree_rss_mb, m.priv_mb, m.cpu_pct, m.procs, m.handles,
                        db_path, db_size as f64 / 1_048_576.0,
                        thresholds()
                    ));
                    let _ = app.emit("wm:trip", &m);
                    // 留 1.2s 给前端把话说完、日志刷盘
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                    app.exit(7);
                    return;
                }
            } else {
                HARD_BREACH.store(0, Ordering::Relaxed);
            }
        }
    });
}
