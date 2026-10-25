// WorkMate · 打扰预算与静默时段
// 编制：WorkMate Contributors
//
// 需求第 4 点「贴心」的落地点：喝水、站起、到点任务提醒如果各喊各的，
// 用户听到的只是一串噪音。所有主动打扰必须共享同一套规则：
//
//   ① 静默时段：21:30–08:30（睡觉）与 12:00–13:30（午休）只放行硬提醒（level=1）；
//      软提醒（喝水/站起/普通任务）顺延到窗口结束后，而不是丢掉。
//   ② 会议中：前台是会议类应用时软提醒顺延 10 分钟——开会时弹「喝口水」很失礼。
//   ③ 预算：30 分钟内软提醒最多 2 次，两次间隔至少 6 分钟；超预算顺延。
//      （规划 §5.7 的「4 小时挂机主动气泡 ≤ 32 次」靠这条兜住）
//
// 顺延 = fire_at 往后推 + status 复位为 0，下一轮轮询会重新取，
// 因此任何一次打扰都不会真正丢失，只是晚一点。
//
// 状态存在进程内（atomics + Mutex），不落库：预算本来就是「最近半小时的体感」，
// 重启后清零反而更合理（用户刚打开电脑时不该被上一次的额度限制）。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 顺延步长（秒）：软提醒被压制时统一推后 10 分钟
pub const DEFER_SECS: i64 = 600;

/// 默认中国标准时区偏移（秒）：UTC+8 = 28800。
/// 底层实际生效时区由 `crate::tz` 按用户当前城市动态解析（未设置城市或国内城市默认 UTC+8）。
#[cfg(test)]
pub const BJ_OFFSET_SECS: i64 = 8 * 3600;

fn now_sec() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 当前生效城市时区的（时、分）
fn bj_hm() -> (u32, u32) {
    crate::tz::now_hm()
}

/// 当前生效城市时区 "YYYY-MM-DD HH:MM"（给 LLM 当上下文，让它能换算「明天下午三点」）
pub fn bj_now_text() -> String {
    crate::tz::now_text()
}

/// 任意时间戳 → 当前生效城市时区 "YYYY-MM-DD HH:MM"。
/// 把「相对分钟」转成绝对时间是解析层的事，别再让调用方各写一份。
pub fn bj_text(ts: i64) -> String {
    crate::tz::fmt_datetime(ts)
}

/// 当前生效城市时区「今天零点」的时间戳
pub fn bj_day_start() -> i64 {
    crate::tz::day_start()
}

/// 解析 "YYYY-MM-DD HH:MM"（当前生效城市时区）→ 时间戳。
///
/// 供大模型返回的绝对时间使用。为什么要绝对时间：
/// 「在出差之前订票」是**跨条目的相对关系**，单一的「从现在起多少分钟」
/// 表达不了——实测模型会直接回 -1，再被兜底成「30 分钟后」，完全答非所问。
#[cfg(test)]
pub fn parse_bj_datetime(s: &str) -> Option<i64> {
    crate::tz::parse_datetime_in_tz(s, crate::tz::offset_min())
}

fn minutes(h: u32, m: u32) -> u32 {
    h * 60 + m
}

/// 测试用强制静默开关（HTTP /notify?force_quiet=1），
/// 让「夜间/午休顺延」这条分支可以在白天被验证，且不改系统时钟。
static FORCE_QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 测试用「强制清醒」开关：忽略静默时段。
/// 没有它，用例在 21:30–08:30 之间跑会全部被顺延——不是功能坏了，是时间不对，
/// 这种"看钟才过"的测试比没有测试更糟（已经误报过一次）。
static FORCE_AWAKE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_force_awake(on: bool) {
    FORCE_AWAKE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn set_force_quiet(on: bool) {
    FORCE_QUIET.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// 是否处于「强制清醒」测试态。调度层用它跳过「人不在就延后」的判断——
/// 否则锁屏跑回归时 idle 很大，所有提醒都会被一直延后，测试表现为「没提醒」。
pub fn forced_awake() -> bool {
    FORCE_AWAKE.load(std::sync::atomic::Ordering::Relaxed)
}

/// 自适应静默是否开启（默认开，显式设 "0" 才关）
fn adaptive_quiet_enabled() -> bool {
    match crate::db::with_conn(|c| crate::db::repo::get_setting(c, "remind.adaptive_quiet")) {
        Ok(Some(v)) => v.trim() != "0",
        _ => true,
    }
}

/// 是否处于静默时段
pub fn quiet_hours() -> bool {
    if FORCE_QUIET.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    // 测试钩子优先级高于真实时钟：否则 21:30–08:30 跑用例会被静默时段整体吞掉，
    // 表现成「提醒功能坏了」，实际只是时间不对（踩过一次，排查半天）。
    if FORCE_AWAKE.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let (h, m) = bj_hm();
    let t = minutes(h, m);
    // 21:30 – 次日 08:30
    let night = t >= minutes(21, 30) || t < minutes(8, 30);
    // 12:00 – 13:30
    let lunch = t >= minutes(12, 0) && t < minutes(13, 30);
    if !(night || lunch) {
        return false;
    }

    // 自适应：窗口内如果用户**确实在用电脑**，就不算静默。
    // 用户经常工作到凌晨（实测 01:00–03:00 有真实活跃采样），
    // 一刀切 21:30 就静默会把整晚的提醒全推到第二天早上——
    // 这正是「设了提醒却没收到」的常见来源。
    if !adaptive_quiet_enabled() {
        return true;
    }
    let now = now_sec();
    let active = crate::db::with_conn(|c| crate::db::repo::recently_active(c, now, 600))
        .unwrap_or(false);
    !active
}

/// 距离静默窗口结束还有多少秒（用于把软提醒推到窗口外）
pub fn quiet_remaining() -> i64 {
    // 测试用强制静默没有真实窗口，给一个 1 小时的合成窗口，
    // 这样「顺延到窗口之外」这条逻辑也能在白天被验证。
    if FORCE_QUIET.load(std::sync::atomic::Ordering::Relaxed) {
        return 3600;
    }
    let (h, m) = bj_hm();
    let t = minutes(h, m) as i64;
    let sec = now_sec() % 60;
    let to = |h2: u32, m2: u32| -> i64 {
        let mut d = (minutes(h2, m2) as i64 - t) * 60 - sec;
        if d <= 0 {
            d += 86_400;
        }
        d
    };

    if t >= minutes(21, 30) as i64 || t < minutes(8, 30) as i64 {
        return to(8, 30);
    }
    if t >= minutes(12, 0) as i64 && t < minutes(13, 30) as i64 {
        return to(13, 30);
    }
    0
}

/* ------------------------------ 打扰预算 ------------------------------ */

/// 最近 30 分钟内的软打扰时间戳
fn history() -> &'static Mutex<VecDeque<i64>> {
    static H: OnceLock<Mutex<VecDeque<i64>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(VecDeque::new()))
}

const WINDOW_SECS: i64 = 1800;
const MAX_IN_WINDOW: usize = 2;
const MIN_GAP_SECS: i64 = 360;

/// 软提醒是否还有额度
pub fn budget_ok() -> bool {
    let now = now_sec();
    let mut h = match history().lock() {
        Ok(g) => g,
        Err(_) => return true,
    };
    while matches!(h.front(), Some(&t) if now - t > WINDOW_SECS) {
        h.pop_front();
    }
    if h.len() >= MAX_IN_WINDOW {
        return false;
    }
    if let Some(&last) = h.back() {
        if now - last < MIN_GAP_SECS {
            return false;
        }
    }
    true
}

/// 记一次打扰（只有真正弹出去的才计入）
pub fn note_interrupt() {
    let now = now_sec();
    if let Ok(mut h) = history().lock() {
        h.push_back(now);
        while h.len() > 8 {
            h.pop_front();
        }
    }
}

/// 软提醒当前是否可以放行（静默 / 预算）
pub fn soft_allowed() -> bool {
    !quiet_hours() && budget_ok()
}

/// 给面板/日志看的调度状态
pub fn status() -> serde_json::Value {
    let (h, m) = bj_hm();
    let local_sec = now_sec() + crate::tz::offset_secs();
    let tz = crate::tz::active();
    serde_json::json!({
        "beijing_hm": format!("{:02}:{:02}", h, m),
        "beijing_sec_of_day": local_sec.rem_euclid(86_400),
        "day_start": bj_day_start(),
        "quiet": quiet_hours(),
        "quiet_remaining_s": quiet_remaining(),
        "budget_ok": budget_ok(),
        "defer_secs": DEFER_SECS,
        "force_quiet": FORCE_QUIET.load(std::sync::atomic::Ordering::Relaxed),
        "force_awake": FORCE_AWAKE.load(std::sync::atomic::Ordering::Relaxed),
        "tz": tz,
    })
}

/* ------------------------------ 提醒文案 ------------------------------ */

/// 把秒数说成人话：2 天 / 3 小时 20 分 / 15 分钟
pub fn human_span(secs: i64) -> String {
    let s = secs.max(0);
    let d = s / 86_400;
    let h = (s % 86_400) / 3_600;
    let m = (s % 3_600) / 60;
    if d > 0 {
        if h > 0 {
            format!("{d}天{h}小时")
        } else {
            format!("{d}天")
        }
    } else if h > 0 {
        if m > 0 {
            format!("{h}小时{m}分")
        } else {
            format!("{h}小时")
        }
    } else {
        format!("{}分钟", m.max(1))
    }
}

/// 从备注里取「对象：张总；其余…」中的对象名
pub fn extract_who(note: &str) -> Option<&str> {
    let rest = note.strip_prefix("对象：")?;
    let end = rest.find(['；', ';']).unwrap_or(rest.len());
    let who = rest[..end].trim();
    if who.is_empty() {
        None
    } else {
        Some(who)
    }
}

/// 提醒的「一句话上下文」——用户第一眼要知道这事有多急、要花多久、找谁。
///
/// 以前气泡只显示标题，库里存的 priority / duration_min / note 全被浪费了。
/// 逾期放在最前面：拖了很久的事，用户最需要先知道这一点。
pub fn brief(now: i64, fire_at: i64, duration_min: i64, priority: i64, note: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    let late = now - fire_at;
    // 2 分钟内的偏差是轮询 + 投递的正常延迟，别报成「已逾期 1 分钟」
    if late >= 120 {
        parts.push(format!("已逾期 {}", human_span(late)));
    }
    if duration_min > 0 {
        parts.push(format!("约 {}", human_span(duration_min * 60)));
    }
    if let Some(who) = extract_who(note) {
        parts.push(format!("对象 {who}"));
    }
    if priority == 1 {
        parts.push("重要".to_string());
    }
    parts.join(" · ")
}

/// 带时区换算的提醒摘要：当用户出国切换时区，或本机 OS 时区与城市时区不一致时，
/// 自动附上换算后的当地/本机时刻提示。
pub fn brief_with_tz(
    now: i64,
    fire_at: i64,
    duration_min: i64,
    priority: i64,
    note: &str,
    tz_offset_min: i64,
) -> String {
    let base = brief(now, fire_at, duration_min, priority, note);
    let tz_hint = crate::tz::cross_tz_hint(fire_at, tz_offset_min);
    match (base.is_empty(), tz_hint.is_empty()) {
        (true, true) => String::new(),
        (false, true) => base,
        (true, false) => tz_hint,
        (false, false) => format!("{base} · {tz_hint}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_formats() {
        assert_eq!(human_span(0), "1分钟");
        assert_eq!(human_span(30), "1分钟");
        assert_eq!(human_span(15 * 60), "15分钟");
        assert_eq!(human_span(3600), "1小时");
        assert_eq!(human_span(3600 + 20 * 60), "1小时20分");
        assert_eq!(human_span(2 * 86_400), "2天");
        assert_eq!(human_span(2 * 86_400 + 3 * 3600), "2天3小时");
    }

    #[test]
    fn who_extraction() {
        assert_eq!(extract_who("对象：产品组；带上中债登估值"), Some("产品组"));
        assert_eq!(extract_who("对象：张总"), Some("张总"));
        assert_eq!(extract_who("对象："), None);
        assert_eq!(extract_who("没有对象"), None);
        assert_eq!(extract_who(""), None);
    }

    #[test]
    fn brief_composition() {
        let now = 1_000_000;
        // 逾期 2 天 + 预估 30 分钟 + 对象 + 重要，四段都要出现
        let s = brief(now, now - 2 * 86_400, 30, 1, "对象：产品组；带估值");
        assert!(s.contains("已逾期 2天"), "{s}");
        assert!(s.contains("约 30分钟"), "{s}");
        assert!(s.contains("对象 产品组"), "{s}");
        assert!(s.contains("重要"), "{s}");

        // 刚好到点：不报逾期，其余字段为空 → 整体为空
        assert!(brief(now, now, 0, 2, "").is_empty());

        // 1 分钟偏差属投递延迟，不能报「已逾期 1 分钟」
        assert!(brief(now, now - 60, 0, 2, "").is_empty());
    }

    #[test]
    fn datetime_parsing() {
        let t = parse_bj_datetime("2026-10-27 09:00").expect("应能解析");
        assert_eq!((t + BJ_OFFSET_SECS).rem_euclid(86_400), 9 * 3600);
        // 只有日期 → 09:00
        let t2 = parse_bj_datetime("2026-10-27").expect("应能解析");
        assert_eq!((t2 + BJ_OFFSET_SECS).rem_euclid(86_400), 9 * 3600);
        // 非法输入一律 None，让调用方走兜底而不是算出离谱时间
        assert!(parse_bj_datetime("2026-13-01 09:00").is_none());
        assert!(parse_bj_datetime("后天").is_none());
        assert!(parse_bj_datetime("").is_none());
    }
}
