// WorkMate · Tauri 入口
// 编制：WorkMate Contributors
//
// 发布版设为 windows 子系统，避免双击弹出黑终端（调试版仍保留控制台便于看日志）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//
// 窗口：pet 由 tauri.conf.json 声明（透明/置顶/无边框/跳任务栏），panel 按需创建。
//
// 退出路径必须有多条：桌面助手 skipTaskbar，一旦托盘图标被 Windows 折叠进
// 「隐藏的图标」，用户就找不到入口。因此提供：
//   ① 右键菜单「退出」② 托盘菜单 ③ 全局快捷键 Ctrl+Shift+Q。
//
// 关于鼠标穿透：曾实现「Rust 40ms 轮询 + alpha 掩码查表」（见 bench/gen_hitmask*.py），
// 实测判定失效、且掩码与窗口尺寸/DPI 强耦合，已废弃。当前窗口整体可交互。
mod commands;
mod clipboard;
mod control;
mod db;
mod display;
mod focus;
mod llm;
mod location;
mod logs;
mod memory;
mod notify;
mod perception;
mod todo;
mod tz;
mod watchdog;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

static LAST_SAVE_MS: AtomicI64 = AtomicI64::new(0);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn now_sec() -> i64 {
    now_ms() / 1000
}

/// 数据目录（数据库 + 日志）。
///
/// `WORKMATE_DATA_DIR` 可整体覆盖：**自动化测试必须用它指向一次性沙箱目录**。
/// 不隔离的后果是真实发生过的——测试建的「发周报/应付审计」待办到点弹到
/// 用户脸上、模拟数据污染今日概览、/db/reset 抹掉真实统计。
/// 与 `WORKMATE_LLM_PATH`（只管模型配置）分工：那个要读用户真实 Key 做真连测试，
/// 这个必须默认隔离。
pub(crate) fn data_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("WORKMATE_DATA_DIR") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    std::env::var("APPDATA")
        .map(|a| std::path::PathBuf::from(a).join("org.atomgit.workmate"))
        .unwrap_or_else(|_| ".".into())
}

/// 启动日志：写到数据目录下的 startup.log。
/// 宠物窗是 transparent + skipTaskbar，一旦起在屏幕外或崩溃，用户看不到任何东西，
/// 这条日志是唯一的诊断线索。
pub(crate) fn startup_log(msg: &str) {
    use std::path::PathBuf;
    let p: PathBuf = data_dir().join("startup.log");
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "[{}] {}", now_sec(), msg);
    }
}

fn save_state_throttled(app: &tauri::AppHandle) {
    let now = now_ms();
    let last = LAST_SAVE_MS.load(Ordering::Relaxed);
    if now - last < 1200 {
        return;
    }
    LAST_SAVE_MS.store(now, Ordering::Relaxed);
    // 位置存进 settings（而不是 window-state 插件）：恢复前要做「是否还在屏内」校验，
    // 插件那套无脑还原正是「猫跑到不存在的显示器上」的成因。
    display::save_position(app);
}

/* ------------------------------ 演示 / 投屏让位 ------------------------------ */

/// 是否是我们主动藏起来的（避免与用户手动隐藏打架）
static PET_HIDDEN_BY_PRESENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn clear_present_hidden() {
    PET_HIDDEN_BY_PRESENT.store(false, Ordering::Relaxed);
}

/// 最后一次到点投递的提醒（`/debug/last-reminder` 可读）。
/// 存在的理由：气泡会淡出、被点击覆盖，自动化断言依赖它必然抖动；
/// 直接看投递结果才是确定的。
pub(crate) static LAST_REMINDER: std::sync::Mutex<serde_json::Value> =
    std::sync::Mutex::new(serde_json::Value::Null);

/// 按当前演示状态收/放宠物窗。只在状态变化时调用（内部幂等）。
pub(crate) fn apply_presenting(app: &tauri::AppHandle) {
    let on = perception::presenting();
    let Some(w) = app.get_webview_window("pet") else {
        return;
    };
    if on {
        if let Some(a) = app.get_webview_window("alert") {
            let _ = a.hide();
        }
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
            PET_HIDDEN_BY_PRESENT.store(true, Ordering::Relaxed);
            let (_, auto, manual_left) = perception::presenting_detail();
            startup_log(&format!(
                "presentation: pet hidden (auto={auto} manual_left={manual_left}s)"
            ));
            let _ = app.emit("wm:present", true);
        }
    } else if PET_HIDDEN_BY_PRESENT.swap(false, Ordering::Relaxed) {
        let _ = w.show();
        startup_log("presentation: pet shown again");
        let _ = app.emit("wm:present", false);
    }
}

/* ------------------------------ 感知（M5） ------------------------------ */

/// 感知采样线程：每 5 秒一次，写明细 + 推给前端。
///
/// 只采两件事：键鼠空闲秒数、前台进程类别。
/// **不取窗口标题、不记录输入内容** —— 这是架构级隐私约束，不是承诺。
fn spawn_perception(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut last_ts = perception::now_sec();
        let mut streak_secs: u32 = 0;
        // 专注判定三件套：
        //   streak_cat  —— 当前这段连续专注是哪个类别
        //   away_since  —— 类别切走了多久（宽限期内切回来不算断）
        let mut streak_cat: Option<perception::AppCategory> = None;
        let mut away_since: Option<i64> = None;
        let mut passive_im_episode = false;

        loop {
            std::thread::sleep(Duration::from_secs(5));

            let mut snap = perception::sample(0);
            // 单拍最多计 30 秒：机器休眠/进程挂起后，两拍之间的墙钟间隔可能是几小时，
            // 直接累加会让「连续专注」凭空跳几十分钟（实测踩过）。
            let delta = (snap.ts - last_ts).clamp(0, 30) as u32;
            // 打断判定：只有「已经专注满 5 分钟（非 IM）」的一段工作被即时通讯（IM）切入打断，
            // 才算一次被动专注打断；单纯停手阅读 60 秒或 IDE↔浏览器查资料不算打断。
            let streak_before = streak_secs;
            let cat_before = streak_cat;
            // 前台是 WorkMate 自己（用户正在看面板）：这一拍不记账、不累加、不判打断。
            // 否则「看统计」的几分钟会凭空变成「其他」，把真实类别挤下去；
            // 深度专注中打开面板还会被误判成被打断。
            let self_window = snap.is_self;

            // 连续专注的判定规则（对用户可见，见面板提示）：
            //   · 键鼠空闲 ≥ 60s → 认为离开，立即归零
            //   · 前台仍是同一应用类别 → 继续累加
            //   · 类别切换 → 给 120s 宽限：短时切出去查个文档再切回来不算断，
            //     但离开的那段时间不计入（否则「专注」会虚高）
            if !self_window {
                if snap.idle_s >= 60 {
                    streak_secs = 0;
                    streak_cat = None;
                    away_since = None;
                    passive_im_episode = false;
                } else {
                    if snap.app_cat != perception::AppCategory::Im {
                        passive_im_episode = false;
                    }
                    match streak_cat {
                        None => {
                            streak_cat = Some(snap.app_cat);
                            streak_secs = 0;
                            away_since = None;
                        }
                        Some(c) if c == snap.app_cat => {
                            streak_secs = streak_secs.saturating_add(delta);
                            away_since = None;
                        }
                        Some(_) => match away_since {
                            None => away_since = Some(snap.ts),
                            Some(t) if snap.ts - t > 120 => {
                                // 离开超过宽限期：以新类别重新起算
                                streak_cat = Some(snap.app_cat);
                                streak_secs = 0;
                                away_since = None;
                            }
                            _ => {}
                        },
                    }
                }
            }
            let interrupted_by_im = !self_window
                && snap.idle_s < 60
                && snap.app_cat == perception::AppCategory::Im
                && cat_before != Some(perception::AppCategory::Im)
                && streak_before >= 300
                && !passive_im_episode;
            if interrupted_by_im {
                passive_im_episode = true;
            }
            snap.focus_min = streak_secs / 60;
            last_ts = snap.ts;

            let focus_ev = focus::on_perception_tick(
                snap.ts,
                snap.app_cat == perception::AppCategory::Im,
                snap.idle_s,
                delta as i64,
                self_window,
            );
            let manual_im_counted = focus_ev
                .as_ref()
                .map(|e| e.kind == "im_interrupted")
                .unwrap_or(false);

            if interrupted_by_im && !manual_im_counted {
                let day = logs::bj_date(snap.ts);
                let db = app.state::<db::Db>();
                if let Ok(n) = db.with(|c| db::repo::bump_interrupt(c, &day)) {
                    startup_log(&format!("focus interrupted by IM #{n} on {day}"));
                }
            }
            if let Some(ref ev) = focus_ev {
                startup_log(&format!("focus session event: {} ({})", ev.kind, ev.message));
                let _ = app.emit("wm:focus", ev);
                if !ev.deferred_ids.is_empty() {
                    commands::flush_focus_deferred_alert(&app, &ev.deferred_ids);
                }
            }

            let db = app.state::<db::Db>();
            if !self_window {
                let row = db::repo::Snapshot {
                    ts: snap.ts,
                    kind: 0,
                    idle_s: snap.idle_s as i64,
                    app_cat: snap.app_cat.as_i64(),
                    focus_min: snap.focus_min as i64,
                    flags: snap.flags as i64,
                };
                if let Err(e) = db.with(|c| db::repo::insert_event(c, &row)) {
                    eprintln!("[workmate] perception write failed: {e}");
                }
            }

            // 演示/投屏检测：全屏放映一律让位；手动演示模式到期后自动复位。
            // apply_presenting 内部幂等，只在「需要隐藏但还可见 / 已隐藏且该显示」时才动窗口。
            perception::update_auto_present();
            apply_presenting(&app);

            // 喝水 / 站起：把「连续专注」转成一条普通提醒，走同一条提醒管线。
            // 当用户正处于手动开启的 50 分钟专注模式时不重复调度（50 分钟完成自带休息喝水提示）。
            if !focus::is_active() {
                maybe_schedule_body_reminders(&app, snap.focus_min as i64);
            }

            // 推给前端（面板实时刷新 / 后续情绪引擎订阅）
            let payload = serde_json::json!({
                "ts": snap.ts,
                "idleS": snap.idle_s,
                "appCat": snap.app_cat.as_i64(),
                "appLabel": snap.app_cat.label(),
                "focusMin": snap.focus_min,
                "flags": snap.flags,
                "presenting": perception::presenting(),
            });
            let _ = app.emit("wm:tick", payload);
        }
    });
}

/// 到点检查：每 5 秒扫一次。
///
/// 为什么 5 秒而不是精确调度：桌面应用常被休眠打断，精确 timer 会漂移；
/// 轮询 + 绝对时间戳天然容忍休眠（醒来后补触发）。代价是提醒最多晚 5 秒。
/// 到点检查：每 5 秒扫一次。
///
/// 为什么 5 秒而不是精确调度：桌面应用常被休眠打断，精确 timer 会漂移；
/// 轮询 + 绝对时间戳天然容忍休眠（醒来后补触发）。代价是提醒最多晚 5 秒。
///
/// 这一版加了「贴心」三件事（需求第 4 点）：
///   · 静默时段（夜间/午休）与会议中：软提醒顺延，不打扰
///   · 打扰预算：30 分钟内软提醒最多 2 次
///   · 同时到点的多条提醒合并成一个气泡，而不是连珠炮
/// 每天最多一次的「逾期摘要」。
///
/// 为什么需要：逾期的事如果不主动提，会安静地堆起来——
/// 用户曾反馈「逾期堆到九件才自己发现」。
///
/// 触发条件（全部满足，缺一不可）：
///   ① 不在演示/投屏（调用方已保证） ② 用户此刻真的在用电脑
///   ③ 不在静默时段 ④ 北京时间 09:00–21:00 ⑤ 逾期 ≥2 件 ⑥ 今天还没发过
///
/// 用户在用电脑这个条件很重要：如果他不在，气泡几秒就淡出，
/// 却已经把「今天发过」的标记写下了，等于白丢一次。
fn maybe_overdue_digest(app: &tauri::AppHandle, db: &db::Db, now: i64) {
    // 50 分钟专注期间自动屏蔽逾期摘要，顺延到专注结束后再提醒
    if focus::is_active() {
        return;
    }
    let today = logs::bj_date(now);

    // 今天发过就跳过。这一条查询挡掉绝大多数调用（每 5 秒一次）。
    if let Ok(Some(v)) = db.with(|c| db::repo::get_setting(c, "remind.digest_day")) {
        if v == today {
            return;
        }
    }
    // 用户显式关掉
    if let Ok(Some(v)) = db.with(|c| db::repo::get_setting(c, "remind.digest")) {
        if v.trim() == "0" {
            return;
        }
    }

    let hour = (now + tz::offset_secs()).rem_euclid(86_400) / 3600;
    if !(9..21).contains(&hour) {
        return;
    }
    if notify::quiet_hours() {
        return;
    }
    // 用户不在电脑前就先不发——发了也没人看见，还会白占掉今天的额度。
    // 强制清醒（自动化钩子）时跳过，否则锁屏环境永远验证不到这条路径。
    if !notify::forced_awake()
        && !db
            .with(|c| db::repo::recently_active(c, now, 600))
            .unwrap_or(true)
    {
        return;
    }

    let overdue = match db.with(|c| db::repo::overdue_count(c, now)) {
        Ok(n) => n,
        Err(_) => return,
    };
    if overdue < 2 {
        return;
    }

    // 最久的那件：按 fired_at 升序取第一条（仅真实待办 kind = 0）
    let oldest = db
        .with(|c| {
            c.query_row(
                "SELECT title, fired_at FROM reminders
                 WHERE kind = 0 AND status = 1 AND fired_at IS NOT NULL
                 ORDER BY fired_at ASC LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
        })
        .ok();
    let meta = match oldest {
        Some((t, at)) => format!(
            "最久的一件拖了 {}：{}",
            notify::human_span(now - at),
            crate::clipboard::truncate_chars(&t, 24)
        ),
        None => String::new(),
    };

    let all_reminders = db.with(db::repo::list_reminders).unwrap_or_default();
    let mut overdue_ids = Vec::new();
    let mut overdue_items = Vec::new();
    for r in all_reminders {
        if r.status == 1 && r.kind == 0 {
            overdue_ids.push(r.id);
            let item_meta = notify::brief_with_tz(
                now,
                r.fire_at,
                r.duration_min,
                r.priority,
                &r.note,
                r.tz_offset_min,
            );
            overdue_items.push(serde_json::json!({
                "id": r.id,
                "title": r.title,
                "meta": item_meta,
                "priority": r.priority,
                "kind": r.kind,
            }));
        }
    }

    let payload = serde_json::json!({
        "ids": overdue_ids,
        "title": format!("有 {overdue} 件事逾期了"),
        "count": overdue,
        "level": 0,
        "kinds": [],
        "badge": format!("⏰ 逾期事项摘要 ({overdue})"),
        "meta": meta,
        "warm": "挑最小的一件先做掉，比全做完更现实。",
        "items": overdue_items,
        "digest": true,
        "tz": tz::active(),
        "ts": now,
    });
    let _ = app.emit("wm:reminder", &payload);
    commands::push_alert_window(app, payload);
    let _ = db.with(|c| db::repo::set_setting(c, "remind.digest_day", &today));
    startup_log(&format!("reminder: overdue digest sent ({overdue} items)"));
}

fn spawn_reminder_watcher(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));

        // 演示/投屏中不打扰：**不取出**到点提醒（保持 status=0），让它在演示结束后补触发。
        // 语义上「晚几分钟提醒」远好于「在客户面前弹一个气泡」。
        if perception::presenting() {
            continue;
        }

        let db = app.state::<db::Db>();
        let now = now_sec();

        // 人不在电脑前时，本轮不消耗任何提醒（保持 status=0）。
        // 气泡 9 秒就淡出，用户回来什么都看不到，待办却已变成「已提醒」——
        // 这是逾期悄悄堆起来的主要原因。等他一碰键鼠，下一轮立刻补发。
        // 强制清醒（自动化钩子）时跳过这个判断，否则锁屏跑回归会全部延后。
        if !notify::forced_awake()
            && db
                .with(|c| db::repo::user_away(c, now))
                .unwrap_or(false)
        {
            continue;
        }

        // 逾期摘要：与到点提醒互不影响，放在取 due 之前，
        // 这样即使今天没有到点提醒也能发出去。
        maybe_overdue_digest(&app, db.inner(), now);

        let due = match db.with_mut(|c| db::repo::take_due_reminders(c, now)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[workmate] reminder scan failed: {e}");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }

        // 开会时不弹软提醒（硬提醒是用户明确要求的，照弹）
        let meeting = perception::current_category() == perception::AppCategory::Meeting;
        let in_focus = focus::is_active();
        let soft_ok = notify::soft_allowed() && !meeting && !in_focus;
        let defer_to = if in_focus {
            focus::estimated_end_ts(now)
        } else if notify::quiet_hours() {
            now + notify::quiet_remaining().max(60)
        } else {
            now + notify::DEFER_SECS
        };

        let mut deliver = Vec::new();
        for r in due {
            // 50 分钟专注期间：自动拦截所有 level == 0 的非紧急提醒，顺延到本轮专注结束后合并播报；
            // 仅 level == 1 的高优硬提醒/明确日程允许穿透。
            if in_focus && r.level == 0 {
                if r.kind == 1 || r.kind == 2 {
                    // 自动喝水/站起提醒在手动 50 分钟专注期间直接静默消化（专注结束自带休息喝水提示）
                    let _ = db.with(|c| db::repo::set_reminder_status(c, r.id, 2));
                } else {
                    focus::note_deferred_reminder(r.id, &r.title);
                    if let Err(e) = db.with(|c| db::repo::rearm_reminder(c, r.id, defer_to)) {
                        eprintln!("[workmate] rearm in focus failed: {e}");
                    }
                    let _ = db.with(|c| {
                        db::repo::log_task_event(
                            c,
                            Some(r.id),
                            "deferred",
                            Some((defer_to - now).max(60)),
                            "50分钟专注期自动拦截顺延",
                        )
                    });
                }
                continue;
            }

            if r.level == 0 && !soft_ok {
                if let Err(e) = db.with(|c| db::repo::rearm_reminder(c, r.id, defer_to)) {
                    eprintln!("[workmate] rearm failed: {e}");
                }
                // 顺延也要记一笔：连续顺延是「时间点定错了」的强信号
                let _ = db.with(|c| {
                    db::repo::log_task_event(
                        c,
                        Some(r.id),
                        "deferred",
                        Some(notify::DEFER_SECS),
                        "静默/预算/会议避让",
                    )
                });
                continue;
            }

            // 硬提醒（用户自己定了时间）：会议中当场弹气泡很失礼，但也不能一直压。
            // 折中——最多顺延 2 次（每次 5 分钟），之后照弹。
            // 否则「会一直开着」会把提醒彻底吞掉，用户以为没提醒。
            if r.level == 1 && meeting {
                let n = db.with(|c| db::repo::defer_count(c, r.id)).unwrap_or(0);
                if n < 2 {
                    let to = now + 300;
                    if let Err(e) = db.with(|c| db::repo::rearm_reminder(c, r.id, to)) {
                        eprintln!("[workmate] rearm failed: {e}");
                    }
                    let _ = db.with(|c| {
                        db::repo::log_task_event(
                            c,
                            Some(r.id),
                            "deferred",
                            Some(300),
                            "会议中短暂顺延",
                        )
                    });
                    continue;
                }
            }

            deliver.push(r.clone());
            // 自动喝水/站起轻提醒（kind != 0）一经投递即自动标为完成（保留 fired_at 供当日喝水/站起次数统计），
            // 避免留在待办列表里或 30 分钟后被误算进「逾期待办」。
            if r.kind != 0 {
                let _ = db.with(|c| db::repo::set_reminder_status(c, r.id, 2));
            } else {
                let _ = db.with(|c| db::repo::log_task_event(c, Some(r.id), "reminded", None, ""));
            }
        }
        if deliver.is_empty() {
            continue;
        }

        // 合并：同一轮到的提醒放一个气泡里，避免连续弹三次
        let titles: Vec<String> = deliver.iter().map(|r| r.title.clone()).collect();
        let joined = titles.join(" · ");
        // 上下文只取「最早那条」——它最逾期，也是用户最该先处理的。
        // 多件时在前面加个件数，让人知道气泡里塞了好几条。
        let head = &deliver[0];
        let mut meta = notify::brief_with_tz(
            now,
            head.fire_at,
            head.duration_min,
            head.priority,
            &head.note,
            head.tz_offset_min,
        );
        if deliver.len() > 1 {
            meta = if meta.is_empty() {
                format!("共 {} 件", deliver.len())
            } else {
                format!("共 {} 件 · {meta}", deliver.len())
            };
        }
        let ids: Vec<i64> = deliver.iter().map(|r| r.id).collect();
        let level = deliver.iter().map(|r| r.level).max().unwrap_or(1);
        let kinds: Vec<i64> = deliver.iter().map(|r| r.kind).collect();
        let items: Vec<serde_json::Value> = deliver
            .iter()
            .map(|r| {
                let item_meta = notify::brief_with_tz(
                    now,
                    r.fire_at,
                    r.duration_min,
                    r.priority,
                    &r.note,
                    r.tz_offset_min,
                );
                serde_json::json!({
                    "id": r.id,
                    "title": r.title,
                    "meta": item_meta,
                    "priority": r.priority,
                    "kind": r.kind,
                })
            })
            .collect();

        println!("[workmate] reminder due: {:?} {}", ids, joined);
        // 留一份"最后一次投递"的原始结果：自动化断言不该依赖气泡何时淡出。
        LAST_REMINDER.lock().map(|mut g| {
            *g = serde_json::json!({
                "ids": ids,
                "title": joined,
                "count": deliver.len(),
                "ts": now_sec(),
                "meta": meta,
            });
        }).ok();
        // 温度提示：读最近的记忆（今天完成了几件、专注多久、是否在出差、
        // 有没有熬夜）挑一句话。规则生成，不额外调模型——提醒是实时路径，
        // 多等一秒体感就很差；模型的产出是每天日记里留下的 care。
        let warm = memory::warm_hint();
        let badge = if in_focus && level == 1 {
            "⚡ 专注期高优穿透提醒".to_string()
        } else if level == 1 {
            "🔔 重要待办 / 日程到点".to_string()
        } else {
            "🔔 待办提醒".to_string()
        };
        let payload = serde_json::json!({
            "ids": ids,
            "title": joined,
            "count": titles.len(),
            "level": level,
            "kinds": kinds,
            "badge": badge,
            "meta": meta,
            "warm": warm,
            "items": items,
            "tz": tz::active(),
            "ts": now,
        });
        let _ = app.emit("wm:reminder", &payload);
        // 仅真实待办事项（含至少一条 kind == 0）弹出独立任务提示窗；纯喝水/站起轻提醒只走宠物气泡
        if kinds.iter().any(|k| *k == 0) {
            commands::push_alert_window(&app, payload);
        }
        // 只有软提醒计入预算（硬提醒是用户自己的约定，不该被额度限制）
        if level == 0 {
            notify::note_interrupt();
        }
    });
}

/// 把连续专注转成「喝水 / 站起」提醒。
///
/// 冷却策略（比原先前端那套更保守，避免和任务提醒撞车）：
///   · 喝水：连续专注 ≥ 45 分钟，且最近 2 小时没提醒过
///   · 站起：连续专注 ≥ 90 分钟，且最近 1 小时没提醒过
/// 已有同类型待触发提醒时不再插入（演示/静默中被顺延时，否则每 5 秒插一条）。
fn maybe_schedule_body_reminders(app: &tauri::AppHandle, focus_min: i64) {
    let db = app.state::<db::Db>();
    let now = now_sec();

    let mut plan: Vec<(i64, &str, i64)> = Vec::new();
    if focus_min >= 90 {
        plan.push((2, "起来走两步", 3600));
    }
    if focus_min >= 45 {
        plan.push((1, "喝口水吧", 7200));
    }

    for (kind, title, cooldown) in plan {
        let state = db.with(|c| {
            Ok((
                db::repo::last_auto_kind_ts(c, kind)?,
                db::repo::has_pending_kind(c, kind)?,
            ))
        });
        let Ok((last, pending)) = state else { continue };
        if pending {
            continue;
        }
        if let Some(t) = last {
            if now - t < cooldown {
                continue;
            }
        }
        if db.with(|c| db::repo::add_reminder(c, title, now, 0, kind)).is_ok() {
            startup_log(&format!("auto reminder scheduled: kind={kind} focus_min={focus_min}"));
        }
    }
}

/* ---------------------------- 后台维护任务 ---------------------------- */

/// 每 5 分钟把明细聚合进 rollup_5m；每天清一次超过 7 天的明细。
fn spawn_maintenance(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut last_purge = now_sec();
        loop {
            std::thread::sleep(Duration::from_secs(300));
            let db = app.state::<db::Db>();
            let now = now_sec();

            // 窗口取最近 1 小时，覆盖可能的调度延迟（幂等，重复跑不会翻倍）
            match db.with(|c| db::repo::rollup_range(c, now - 3600, now)) {
                Ok(n) if n > 0 => println!("[workmate] rollup {n} buckets"),
                Ok(_) => {}
                Err(e) => eprintln!("[workmate] rollup failed: {e}"),
            }

            if now - last_purge >= 86_400 {
                last_purge = now;
                // 长跑跨天时也要归档（启动时那次只管开机场景）
                logs::rotate_if_needed();
                // 明细只留 2 天（统计已由 rollup 承担，明细仅用于回溯与复算）
                if let Err(e) = db.with(|c| db::repo::purge_events(c, 2, now)) {
                    eprintln!("[workmate] purge events failed: {e}");
                }
                // 聚合桶留一年
                if let Err(e) = db.with(|c| db::repo::purge_rollup(c, 365, now)) {
                    eprintln!("[workmate] purge rollup failed: {e}");
                }
            }
        }
    });
}

/// 记忆层维护：位置采样 + 每日画像 + 风险 + **每天一次的 AI 日记与分层抽象**。
///
/// 巡检 10 分钟一次（不是 30 分钟）：跨天补写、23:50 写当天这两件事
/// 各有一个时间点，巡检太粗会错过。位置仍按 30 分钟节流——
/// 出差是按天计的，采太密既耗电又让 locations 表堆垃圾。
fn spawn_memory(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        // 先只刷缓存（不落库）：让「建待办打位置标记」这条路径尽早可用。
        // 正式落库仍在 20 秒后——启动瞬间 DB 锁竞争最凶，别去凑热闹。
        std::thread::sleep(Duration::from_secs(4));
        let f = location::refresh();
        if f.ok() {
            startup_log(&format!("location warm: {} via {}", f.label, f.source));
        }
        std::thread::sleep(Duration::from_secs(16));

        // 启动补写：昨天可能没写成（那天关机/没开 WorkMate）
        let yesterday = logs::bj_date(now_sec() - 86_400);
        let r = memory::write_daily(&yesterday, false);
        startup_log(&format!(
            "memory: backfill {} via {}",
            yesterday,
            r["source"].as_str().unwrap_or("skipped")
        ));

        let mut last_day = logs::bj_date(now_sec());
        let mut last_loc = 0i64;
        let mut last_daily_at = 0i64;

        loop {
            let now = now_sec();
            if now - last_loc >= 1800 {
                last_loc = now;
                let r = location::record();
                if r["ok"].as_bool().unwrap_or(false) {
                    startup_log(&format!(
                        "location: {} (trip={})",
                        r["label"].as_str().unwrap_or(""),
                        r["trip"].as_bool().unwrap_or(false)
                    ));
                }
            }

            let n = memory::rebuild_recent(2);
            let risks = memory::scan_risks();
            startup_log(&format!("memory: profiles={n} risks={}", risks.len()));

            let today = logs::bj_date(now);
            if today != last_day {
                // 跨天：把昨天落定成一条短期记忆，然后做一次分层抽象检查
                let y = logs::bj_date(now - 86_400);
                let d = memory::write_daily(&y, false);
                startup_log(&format!(
                    "memory: daily {} via {}",
                    y,
                    d["source"].as_str().unwrap_or("skipped")
                ));
                let _ = memory::rollup_notes();
                last_day = today.clone();
            }

            // 23:50（当前生效城市时区）主动写当天：不等跨天，因为用户可能合盖关机
            let mins_of_day = ((now + tz::offset_secs()).rem_euclid(86_400)) / 60;
            if mins_of_day >= 23 * 60 + 50 && now - last_daily_at > 3600 {
                last_daily_at = now;
                let d = memory::write_daily(&today, false);
                startup_log(&format!(
                    "memory: daily {} via {}",
                    today,
                    d["source"].as_str().unwrap_or("skipped")
                ));
            }

            std::thread::sleep(Duration::from_secs(600));
        }
    });
    let _ = app;
}

/* ------------------------------ 托盘 ------------------------------ */

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "显示 / 隐藏", true, None::<&str>)?;
    let panel = MenuItem::with_id(app, "panel", "打开面板", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 WorkMate", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &panel, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("WorkMate")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle" => {
                let _ = commands::toggle_pet(app.clone());
            }
            "panel" => {
                // 不能吞掉错误：面板创建失败时窗口可能已注册但 WebView 是坏的，
                // 之后每次 open_panel 只 show() 这个坏窗口，表现就是「一直空白」。
                match commands::open_panel(app.clone()) {
                    Ok(_) => startup_log("panel opened"),
                    Err(e) => startup_log(&format!("panel open failed: {e}")),
                }
            }
            "quit" => {
                let _ = app.save_window_state(StateFlags::POSITION);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("pet") {
                    let _ = w.show();
                }
            }
        });

    // icon 缺失时不 panic：托盘没图标总好过整个应用起不来
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder.build(app)?;
    Ok(())
}

/* ---------------------------- 全局快捷键 ---------------------------- */

fn register_shortcuts(app: &tauri::App) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{
        Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
    };

    let quit = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyQ);
    let show = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space);
    let task = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyT);

    app.global_shortcut()
        .on_shortcut(quit, |app, _sc, event| {
            if event.state() == ShortcutState::Pressed {
                let _ = app.save_window_state(StateFlags::POSITION);
                app.exit(0);
            }
        })
        .map_err(|e| e.to_string())?;

    app.global_shortcut()
        .on_shortcut(show, |app, _sc, event| {
            if event.state() == ShortcutState::Pressed {
                if let Some(w) = app.get_webview_window("pet") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
                // 选中即问：读剪贴板 → LLM → 宠物气泡出答案。
                // ask_clipboard 内部是阻塞式 HTTP（最长 60s），而本回调跑在主线程 ——
                // 直接调会把整个 UI 冻住一分钟，必须起线程。
                // 失败时推 ai.error，由宠物窗把原因显示在气泡里（含「未配置模型」这类引导）。
                let app2 = app.clone();
                std::thread::spawn(move || {
                    if let Err(e) = commands::ask_clipboard_sync(app2.clone(), None) {
                        startup_log(&format!("ask failed: {e}"));
                        let _ = app2.emit("wm:error", e);
                    }
                });
            }
        })
        .map_err(|e| e.to_string())?;

    // Ctrl+Shift+T：把剪贴板里的内容交给 AI 抽成待办。
    // 与「选中即问」同样的理由：LLM 调用是阻塞的，必须离开主线程。
    app.global_shortcut()
        .on_shortcut(task, |app, _sc, event| {
            if event.state() == ShortcutState::Pressed {
                if let Some(w) = app.get_webview_window("pet") {
                    let _ = w.show();
                }
                let app2 = app.clone();
                std::thread::spawn(move || {
                    let db = app2.state::<db::Db>();
                    match commands::ai_task_from_clipboard_sync(app2.clone(), db.inner()) {
                        Ok(_) => {}
                        Err(e) => {
                            startup_log(&format!("ai_task failed: {e}"));
                            let _ = app2.emit("wm:error", e);
                        }
                    }
                });
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 进程级单实例文件锁：按 data_dir() 隔离（沙箱与生产互不干扰）。
/// 使用 Windows 独占 share_mode(0) 打开 workmate.lock 并持有至进程生命周期结束；
/// 进程无论是正常退出、panic 还是被强杀，内核都会立即释放句柄锁。
static INSTANCE_LOCK: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

fn acquire_single_instance_lock() -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let lock_path = dir.join("workmate.lock");
    // 给最多 1.5s 重试窗口（兼容自动化重启时旧实例正在收尾退出的瞬态）
    for _ in 0..10 {
        match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(&lock_path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                let _ = INSTANCE_LOCK.set(f);
                return true;
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }
    false
}

fn main() {
    // panic 也写进启动日志，避免崩溃只弹一次就消失、且无事件记录可查
    std::panic::set_hook(Box::new(|info| {
        startup_log(&format!("PANIC: {info}"));
    }));
    if !acquire_single_instance_lock() {
        startup_log("single-instance: another instance is already running on this data_dir, exiting");
        eprintln!("[workmate] another instance is already running, exiting.");
        return;
    }
    // 先归档昨天的日志，再开始写今天的（否则 process start 会写进旧文件）
    logs::rotate_if_needed();
    startup_log("process start");

    let app = tauri::Builder::default()
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::empty())
                .build(),
        )
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .on_window_event(|window, event| match window.label() {
            "pet" => {
                if let tauri::WindowEvent::Moved(_) = event {
                    save_state_throttled(window.app_handle());
                }
            }
            // 面板/任务提示弹出窗关闭 = 隐藏，窗口常驻。否则窗口被销毁后再打开就没有窗口可显示了。
            "panel" | "alert" => {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    let _ = window.hide();
                    api.prevent_close();
                    startup_log(&format!("{} hidden (prevent_close)", window.label()));
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            commands::open_panel,
            commands::test_mode,
            commands::log_frontend,
            commands::toggle_pet,
            commands::hide_pet,
            commands::quit_app,
            commands::db_stats,
            commands::simulate_ticks,
            commands::rollup_now,
            commands::recent_summary,
            commands::current_context,
            commands::add_reminder,
            commands::list_reminders,
            commands::complete_reminder,
            commands::delete_reminder,
            commands::get_llm_config,
            commands::save_llm_config,
            commands::test_llm_config,
            commands::ask_clipboard,
            commands::chat_text,
            commands::get_setting,
            commands::set_setting,
            commands::today_summary,
            commands::today_blocks,
            commands::presentation_state,
            commands::set_presentation,
            commands::clipboard_info,
            commands::parse_todo,
            commands::add_todo,
            commands::reset_stats,
            commands::llm_status,
            commands::llm_usage,
            commands::gc_reminders,
            commands::ai_task,
            commands::ai_task_from_clipboard,
            commands::ensure_stats_fresh,
            commands::memory_brief,
            commands::memory_ask,
            commands::memory_scan,
            commands::memory_rebuild,
            commands::memory_daily,
            commands::memory_rollup,
            commands::memory_notes,
            commands::memory_warm,
            commands::location_now,
            commands::location_refresh,
            commands::location_set,
            commands::location_clear,
            commands::location_history,
            commands::location_raw,
            commands::tz_status,
            commands::tz_set,
            commands::notify_status,
            commands::log_files,
            commands::log_open,
            commands::log_folder,
            commands::log_clear,
            commands::log_rotate,
            commands::workload_brief,
            commands::focus_status,
            commands::focus_start,
            commands::focus_stop,
            commands::alert_current,
            commands::alert_close,
            commands::alert_complete,
            commands::alert_snooze
        ])
        .setup(|app| {
            startup_log("setup begin");
            // 数据层：库放在数据目录（默认 app_data_dir，测试用 WORKMATE_DATA_DIR 隔离），
            // 随应用卸载可整体删除
            let data_dir = data_dir();
            match db::Db::open(&data_dir) {
                Ok(d) => {
                    println!("[workmate] db ready: {}", d.path().display());
                    // 后台线程（感知/定位/记忆）拿不到 tauri::State，走进程级句柄
                    db::set_global(d.clone());
                    app.manage(d);
                    let tz_info = tz::refresh();
                    startup_log(&format!(
                        "tz ready: {} ({}) source={} machine={}",
                        tz_info.city_label,
                        tz_info.utc_label,
                        tz_info.source,
                        tz_info.machine_utc_label
                    ));
                    spawn_maintenance(app.handle().clone());
                    spawn_perception(app.handle().clone());
                    spawn_reminder_watcher(app.handle().clone());
                    spawn_memory(app.handle().clone());
                    // 资源看护：CPU/内存/句柄超限 → 落诊断 + 优雅退出（exit 7）
                    watchdog::start(app.handle().clone());
                    startup_log("db ready");
                }
                Err(e) => eprintln!("[workmate] db init failed: {e}"),
            }

            if let Err(e) = build_tray(app) {
                eprintln!("[workmate] tray init failed: {e}");
            }
            startup_log("tray ready");
            if let Err(e) = register_shortcuts(app) {
                eprintln!("[workmate] shortcut init failed: {e}");
            }
            startup_log("shortcuts ready");

            // 启动定位：恢复上次位置（先在屏内校验），否则主屏右下角。
            // 关掉 window-state 的自动还原（Flags::empty）就是为了这里能做校验——
            // 拔掉外接屏/云桌面换分辨率后，无脑还原会把猫丢到不存在的坐标上。
            display::restore_position(app.handle());
            // 拓扑看护：拔插屏、RDP 重连、缩放变更 → 10s 内收拢回可见区
            display::start_watcher(app.handle().clone());

            // 测试开关：设 WORKMATE_TEST_PANEL=1 时启动即打开面板。
            // 面板没有全局快捷键、右键菜单又无法被自动化模拟（透明窗收不到合成输入），
            // 这是自动化侧唯一能验证面板渲染的入口，正常运行时不触发。
            // HTTP 控制面：把所有能力暴露成本机接口，供自动化自己驱动/断言
            control::serve(app.handle().clone());

            // 测试开关：设 WORKMATE_TEST_PANEL=1 时，由宠物窗前端在 2 秒后 invoke('open_panel')。
            // 注意：不能在 setup 里同步创建、也不能在 std::thread 里创建第二个 WebView ——
            // 前者是主循环未就绪，后者是非主线程，都会报 HRESULT(0x8007139F)
            // 「组或资源的状态不是执行请求操作的正确状态」。走前端 invoke 才是正常路径。
            if std::env::var("WORKMATE_TEST_PANEL").is_ok() {
                startup_log("TEST: test mode on, frontend will open panel");
            }

            println!(
                "[workmate] ready | Ctrl+Shift+Q 退出, Ctrl+Shift+Space 选中即问, Ctrl+Shift+T 选中内容建待办"
            );
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building WorkMate");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            let _ = app_handle.save_window_state(StateFlags::POSITION);
        }
    });
}
