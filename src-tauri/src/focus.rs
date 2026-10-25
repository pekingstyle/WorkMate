// WorkMate · 专注模式与打断趋势统计
//
// 设计原则（用户定义的极简实用方案）：
//   1. 右键点击 Mate 可手动开启一轮专注（默认 50 分钟），不改变宠物形象；
//   2. 专注期间若切到即时通讯（IM：微信/企微/飞书/钉钉/Teams聊天等）且有键鼠活动，
//      立即冒泡提示「专注被打断了一次」，并计入当日打断次数（同一次进入 IM 只计一次，切回工作后重置）；
//   3. 专注满 50 分钟（默认 3000 秒）后，提示用户「起来走走，喝喝水」，自动结束本轮专注，
//      由用户休息后手动选择进入下一次专注；
//   4. 定期统计专注被打断的次数及近期趋势（上升 / 下降 / 持平），在气泡、今日概览与习惯页展示。

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 默认单轮专注目标时长：50 分钟（3000 秒）
pub const DEFAULT_TARGET_SECS: i64 = 50 * 60;

/// 同一次 IM 打断的最小去抖间隔（秒）：防止 15 秒内频繁 Alt+Tab 重复计数
const IM_DEBOUNCE_SECS: i64 = 15;

#[derive(Debug, Clone)]
struct SessionState {
    active: bool,
    started_at: i64,
    target_secs: i64,
    /// 本轮累计活跃专注秒数（非空闲、非 IM）
    active_secs: i64,
    /// 距离上次被 IM 打断以来的连续专注秒数
    uninterrupted_secs: i64,
    /// 本轮被 IM 打断的次数
    im_interrupts: i64,
    /// 当前是否正处于 IM 窗口内（同一次切进 IM 回复消息只算 1 次打断，切回非 IM 后才复位）
    in_im_episode: bool,
    /// 上次触发 IM 打断提示的时间戳
    last_im_ts: i64,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            active: false,
            started_at: 0,
            target_secs: DEFAULT_TARGET_SECS,
            active_secs: 0,
            uninterrupted_secs: 0,
            im_interrupts: 0,
            in_im_episode: false,
            last_im_ts: 0,
        }
    }
}

fn state_cell() -> &'static Mutex<SessionState> {
    static CELL: OnceLock<Mutex<SessionState>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(SessionState::default()))
}

fn now_sec() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 打断趋势统计结果
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct InterruptTrend {
    /// 今日打断次数
    pub today_count: i64,
    /// 昨日打断次数
    pub yesterday_count: i64,
    /// 近期窗口（最近最多 7 个活跃日）日均打断次数
    pub recent_avg: f64,
    /// 对照窗口（前一个周期活跃日）日均打断次数
    pub prev_avg: f64,
    /// 趋势方向："up"（上升）| "down"（下降）| "flat"（持平）| "new"（积累中）
    pub direction: String,
    /// 变化百分比（如 +35 表示上升 35%，-28 表示下降 28%）
    pub delta_pct: i64,
    /// 最近 14 天按日明细 `[(day, count)]`（按日期正序）
    pub daily: Vec<(String, i64)>,
    /// 一句话总结（供面板、气泡直接展示）
    pub summary: String,
}

/// 当前专注状态快照（供前端右键菜单、面板查询）
#[derive(Debug, Clone, serde::Serialize)]
pub struct FocusStatus {
    pub active: bool,
    pub started_at: i64,
    pub target_secs: i64,
    pub active_secs: i64,
    pub elapsed_secs: i64,
    pub remaining_secs: i64,
    pub uninterrupted_secs: i64,
    pub im_interrupts: i64,
    pub trend: InterruptTrend,
}

/// 感知线程每拍触发的专注事件
#[derive(Debug, Clone, serde::Serialize)]
pub struct FocusEvent {
    /// `"im_interrupted"` | `"completed"` | `"started"` | `"stopped"`
    pub kind: String,
    pub message: String,
    pub status: FocusStatus,
}

/// 开启新一轮手动专注（默认 50 分钟）
pub fn start(target_minutes: Option<i64>) -> FocusEvent {
    let now = now_sec();
    let target_secs = target_minutes
        .map(|m| (m.clamp(1, 240)) * 60)
        .unwrap_or(DEFAULT_TARGET_SECS);
    if let Ok(mut g) = state_cell().lock() {
        *g = SessionState {
            active: true,
            started_at: now,
            target_secs,
            active_secs: 0,
            uninterrupted_secs: 0,
            im_interrupts: 0,
            in_im_episode: false,
            last_im_ts: 0,
        };
    }
    let st = status();
    let mins = target_secs / 60;
    let msg = format!(
        "🎯 已开启 {mins} 分钟专注模式\n期间若被即时通讯打断会提醒你；满 {mins} 分钟后提示你起来走走、喝口水。\n📊 {}",
        st.trend.summary
    );
    FocusEvent {
        kind: "started".into(),
        message: msg,
        status: st,
    }
}

/// 手动结束当前专注
pub fn stop() -> FocusEvent {
    let mut prev = SessionState::default();
    if let Ok(mut g) = state_cell().lock() {
        prev = g.clone();
        g.active = false;
        g.in_im_episode = false;
    }
    let st = status();
    let focused_min = (prev.active_secs / 60).max(if prev.started_at > 0 {
        ((now_sec() - prev.started_at) / 60).max(0)
    } else {
        0
    });
    let intr_txt = if prev.im_interrupts == 0 {
        "全程零打断".to_string()
    } else {
        format!("期间被 IM 打断 {} 次", prev.im_interrupts)
    };
    let msg = format!(
        "⏹ 已结束本轮专注（专注约 {focused_min} 分钟，{intr_txt}）。\n📊 {}\n准备好后可随时右键点击「开始专注」进入下一轮。",
        st.trend.summary
    );
    FocusEvent {
        kind: "stopped".into(),
        message: msg,
        status: st,
    }
}

/// 读取当前专注状态与打断趋势
pub fn status() -> FocusStatus {
    let now = now_sec();
    let s = state_cell()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let elapsed = if s.active && s.started_at > 0 {
        (now - s.started_at).max(s.active_secs)
    } else {
        s.active_secs
    };
    let remaining = if s.active {
        (s.target_secs - s.active_secs).max(0)
    } else {
        0
    };
    FocusStatus {
        active: s.active,
        started_at: s.started_at,
        target_secs: s.target_secs,
        active_secs: s.active_secs,
        elapsed_secs: elapsed,
        remaining_secs: remaining,
        uninterrupted_secs: s.uninterrupted_secs,
        im_interrupts: s.im_interrupts,
        trend: compute_interrupt_trend(),
    }
}

/// 供感知线程（每 5 秒一拍）调用：
/// - `is_im`: 当前前台是否为即时通讯（`AppCategory::Im`）
/// - `idle_s`: 键鼠空闲秒数
/// - `delta_secs`: 距上一拍的秒数（1..=30）
/// - `self_window`: 前台是否为 WorkMate 自身窗口
pub fn on_perception_tick(
    ts: i64,
    is_im: bool,
    idle_s: u32,
    delta_secs: i64,
    self_window: bool,
) -> Option<FocusEvent> {
    if self_window {
        return None;
    }

    let mut trigger_im: Option<(i64, i64)> = None; // (session_im_cnt, active_min)
    let mut trigger_done: Option<(i64, i64)> = None; // (target_min, session_im_cnt)

    {
        let Ok(mut g) = state_cell().lock() else {
            return None;
        };
        if !g.active {
            return None;
        }

        // 键鼠空闲 < 60s 视为用户正在操作电脑
        if idle_s < 60 {
            if is_im {
                // 刚切入 IM 窗口（且距上次打断提示 ≥ 15s）→ 记一次 IM 打断
                if !g.in_im_episode && ts - g.last_im_ts >= IM_DEBOUNCE_SECS {
                    g.in_im_episode = true;
                    g.last_im_ts = ts;
                    g.im_interrupts += 1;
                    g.uninterrupted_secs = 0;
                    trigger_im = Some((g.im_interrupts, g.active_secs / 60));
                }
            } else {
                // 在非 IM 窗口正常工作：解除 IM episode 标记并累加专注时长
                g.in_im_episode = false;
                let d = delta_secs.clamp(1, 30);
                g.active_secs += d;
                g.uninterrupted_secs += d;

                if g.active_secs >= g.target_secs {
                    let target_min = (g.target_secs / 60).max(1);
                    let im_cnt = g.im_interrupts;
                    g.active = false;
                    g.in_im_episode = false;
                    trigger_done = Some((target_min, im_cnt));
                }
            }
        }
    }

    if let Some((session_im_cnt, focused_min)) = trigger_im {
        let day = crate::logs::bj_date(ts);
        let today_total = crate::db::with_conn(|c| crate::db::repo::bump_interrupt(c, &day))
            .unwrap_or(session_im_cnt);
        let st = status();
        let msg = format!(
            "⚠️ 专注被打断了一次（切到了即时通讯）\n本轮已专注 {focused_min} 分钟 · 本轮第 {session_im_cnt} 次打断（今日累计 {today_total} 次）。\n📊 {}",
            st.trend.summary
        );
        return Some(FocusEvent {
            kind: "im_interrupted".into(),
            message: msg,
            status: st,
        });
    }

    if let Some((target_min, im_cnt)) = trigger_done {
        let st = status();
        let msg = if im_cnt == 0 {
            format!(
                "🎉 已连续专注满 {target_min} 分钟且没有被打断！\n起来走走、喝口水放松一下吧。\n休息好后可右键点击 Mate 选择「开始专注」进入下一轮。"
            )
        } else {
            format!(
                "⏰ 本轮 {target_min} 分钟专注已完成（期间被 IM 打断 {im_cnt} 次）。\n起来走走、喝口水吧！休息好后可右键点击「开始专注」进入下一轮。"
            )
        };
        return Some(FocusEvent {
            kind: "completed".into(),
            message: msg,
            status: st,
        });
    }

    None
}

/// 纯函数：根据最近 14 天按时间正序排列的 `(day, active_sec, interrupt_cnt)` 计算打断趋势。
/// 提取出来便于写确定性单元测试。
pub fn analyze_trend_series(series: &[(String, i64, i64)]) -> InterruptTrend {
    if series.is_empty() {
        return InterruptTrend {
            direction: "new".into(),
            summary: "暂无打断统计，开启专注后将自动统计近期上升或下降趋势".into(),
            ..Default::default()
        };
    }

    let today_count = series.last().map(|x| x.2).unwrap_or(0);
    let yesterday_count = if series.len() >= 2 {
        series[series.len() - 2].2
    } else {
        0
    };
    let daily: Vec<(String, i64)> = series.iter().map(|(d, _, c)| (d.clone(), *c)).collect();

    // 筛选有实际活动或有打断记录的有效工作日（避免周末没开机全是 0 把均值拉低）
    let active_days: Vec<(String, i64)> = series
        .iter()
        .enumerate()
        .filter(|(idx, (_, act_sec, intr))| {
            // 今天如果已有打断或活跃 ≥ 10 分钟则纳入；历史日只要活跃 ≥ 10 分钟或有打断即纳入
            *intr > 0 || *act_sec >= 600 || *idx + 1 == series.len()
        })
        .map(|(_, (d, _, intr))| (d.clone(), *intr))
        .collect();

    if active_days.len() < 2 {
        return InterruptTrend {
            today_count,
            yesterday_count,
            recent_avg: today_count as f64,
            prev_avg: 0.0,
            direction: "new".into(),
            delta_pct: 0,
            daily,
            summary: format!("今日专注被打断 {today_count} 次（样本积累满 2 天后自动对比升降趋势）"),
        };
    }

    // 将有效工作日分成「前半段（对照期）」与「后半段（近期，最多取最近 5~7 天）」
    let n = active_days.len();
    let split = if n >= 6 { n - (n / 2).min(7) } else { n / 2 };
    let prev_slice = &active_days[..split];
    let recent_slice = &active_days[split..];

    let prev_avg = prev_slice.iter().map(|x| x.1 as f64).sum::<f64>() / prev_slice.len().max(1) as f64;
    let recent_avg =
        recent_slice.iter().map(|x| x.1 as f64).sum::<f64>() / recent_slice.len().max(1) as f64;

    let diff = recent_avg - prev_avg;
    let delta_pct = if prev_avg > 0.05 {
        ((diff / prev_avg) * 100.0).round() as i64
    } else if recent_avg > 0.2 {
        100
    } else {
        0
    };

    // 判定阈值：均差 ≥ 0.4 次/天 且变化幅度 ≥ 15% 判为明显上升/下降
    let (direction, trend_phrase) = if diff >= 0.4 && delta_pct >= 15 {
        (
            "up",
            format!(
                "近期打断呈上升趋势 ↑（近 {} 日日均 {:.1} 次 vs 前期 {:.1} 次，+{}%）",
                recent_slice.len(),
                recent_avg,
                prev_avg,
                delta_pct
            ),
        )
    } else if diff <= -0.4 && delta_pct <= -15 {
        (
            "down",
            format!(
                "近期打断呈下降趋势 ↓（近 {} 日日均 {:.1} 次 vs 前期 {:.1} 次，{}%，专注度提升）",
                recent_slice.len(),
                recent_avg,
                prev_avg,
                delta_pct
            ),
        )
    } else {
        (
            "flat",
            format!(
                "近期打断次数基本持平 →（近 {} 日日均 {:.1} 次 vs 前期 {:.1} 次）",
                recent_slice.len(),
                recent_avg,
                prev_avg
            ),
        )
    };

    let summary = format!("今日被打断 {today_count} 次 · {trend_phrase}");

    InterruptTrend {
        today_count,
        yesterday_count,
        recent_avg: (recent_avg * 10.0).round() / 10.0,
        prev_avg: (prev_avg * 10.0).round() / 10.0,
        direction: direction.to_string(),
        delta_pct,
        daily,
        summary,
    }
}

/// 从数据库读取最近 14 天的打断统计并计算升降趋势
pub fn compute_interrupt_trend() -> InterruptTrend {
    let today_start = crate::tz::day_start();
    let series: Vec<(String, i64, i64)> = crate::db::with_conn(|c| {
        let mut out = Vec::with_capacity(14);
        // 从 13 天前到今天（正序）
        for i in (0..14).rev() {
            let day_ts = today_start - i * 86_400;
            let day_str = crate::tz::date_str(day_ts);
            let (act_sec, prof_intr): (i64, i64) = c
                .query_row(
                    "SELECT COALESCE(active_sec, 0), COALESCE(interrupt_cnt, 0)
                     FROM day_profile WHERE day = ?1",
                    [&day_str],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap_or((0, 0));
            // settings 里的实时打断计数可能比尚未重算的 day_profile 更新，取两者较大值
            let live_intr = crate::db::repo::read_interrupt(c, &day_str).unwrap_or(0);
            let intr = prof_intr.max(live_intr);
            out.push((day_str, act_sec, intr));
        }
        Ok(out)
    })
    .unwrap_or_default();

    analyze_trend_series(&series)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trend_detects_rising_and_falling() {
        // 1. 近期上升：前 3 天日均 1 次，后 3 天日均 4 次
        let rising = vec![
            ("2026-10-20".into(), 3600, 1),
            ("2026-10-21".into(), 3600, 1),
            ("2026-10-22".into(), 3600, 1),
            ("2026-10-23".into(), 3600, 3),
            ("2026-10-24".into(), 3600, 4),
            ("2026-10-25".into(), 3600, 5),
        ];
        let t_up = analyze_trend_series(&rising);
        assert_eq!(t_up.direction, "up", "should detect rising trend: {:?}", t_up);
        assert!(t_up.summary.contains("上升趋势"), "{}", t_up.summary);

        // 2. 近期下降：前 3 天日均 5 次，后 3 天日均 1 次
        let falling = vec![
            ("2026-10-20".into(), 3600, 5),
            ("2026-10-21".into(), 3600, 6),
            ("2026-10-22".into(), 3600, 4),
            ("2026-10-23".into(), 3600, 2),
            ("2026-10-24".into(), 3600, 1),
            ("2026-10-25".into(), 3600, 0),
        ];
        let t_down = analyze_trend_series(&falling);
        assert_eq!(t_down.direction, "down", "should detect falling trend: {:?}", t_down);
        assert!(t_down.summary.contains("下降趋势"), "{}", t_down.summary);
    }

    #[test]
    fn focus_session_im_interruption_and_50min_completion() {
        let ev = start(Some(50));
        assert_eq!(ev.kind, "started");
        assert!(status().active);

        // 在 IDE 专注 10 分钟（200 拍 × 3s）
        let mut base_ts = 1_792_800_000i64;
        for _ in 0..200 {
            base_ts += 5;
            let res = on_perception_tick(base_ts, false, 0, 5, false);
            assert!(res.is_none());
        }

        // 切换到 IM 窗口：第一拍应触发打断提醒
        base_ts += 20;
        let im1 = on_perception_tick(base_ts, true, 0, 5, false);
        assert!(im1.is_some());
        assert_eq!(im1.unwrap().kind, "im_interrupted");

        // 仍在 IM 窗口内继续打字回复：不应重复轰炸提示
        base_ts += 20;
        let im2 = on_perception_tick(base_ts, true, 0, 5, false);
        assert!(im2.is_none());

        // 切回工作窗口，继续专注直到累计满 50 分钟（3000s）
        let mut completed = None;
        for _ in 0..500 {
            base_ts += 5;
            if let Some(done) = on_perception_tick(base_ts, false, 0, 5, false) {
                completed = Some(done);
                break;
            }
        }
        let done_ev = completed.expect("should complete 50-minute focus session");
        assert_eq!(done_ev.kind, "completed");
        assert!(done_ev.message.contains("起来走走"));
        assert!(!status().active, "session should end automatically so user can manually start next");
    }
}
