// WorkMate · Tauri 命令层
// 编制：WorkMate Contributors
//
// 为什么命令必须放在独立子模块（而不是 main.rs）：
// tauri-macros 2.7 的 #[tauri::command] 会生成两个隐藏宏（__cmd__xxx / __tauri_command_name_xxx）。
// 当命令函数带 pub / pub(crate) 可见性时，宏会被打上 #[macro_export]，
// 同时 wrapper 末尾还会生成一条 `use {__cmd__xxx, ...}` 重新导入。
// 在 crate 根模块（main.rs）里，「宏导出目标」与「use 导入目标」是同一个作用域，
// 触发 E0255 "the name __cmd__xxx is defined multiple times"（tauri 已知架构限制，
// 见 tauri-apps/tauri#9362）。放进子模块后两者错开，冲突消失。
//
// 这里所有命令统一用 pub(crate)：main.rs 的托盘/快捷键要直接调用，
// control.rs 的 HTTP 控制面也要复用同一份逻辑（crate::commands::xxx）。
use crate::{db, llm, now_sec, perception, startup_log};
use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

#[derive(serde::Serialize)]
pub(crate) struct CurrentContext {
    ts: i64,
    idle_s: i64,
    app_cat: i64,
    app_label: String,
    focus_min: i64,
}

#[tauri::command]
pub(crate) fn current_context(db: tauri::State<db::Db>) -> Result<Option<CurrentContext>, String> {
    let row = db.with(db::repo::latest_event)?;
    Ok(row.map(|(ts, idle_s, app_cat, focus_min)| {
        let cat = match app_cat {
            0 => perception::AppCategory::Ide,
            1 => perception::AppCategory::Office,
            2 => perception::AppCategory::Browser,
            3 => perception::AppCategory::Meeting,
            4 => perception::AppCategory::Im,
            5 => perception::AppCategory::Media,
            7 => perception::AppCategory::AiAgent,
            _ => perception::AppCategory::Other,
        };
        CurrentContext {
            ts,
            idle_s,
            app_cat,
            app_label: cat.label().to_string(),
            focus_min,
        }
    }))
}

/* ------------------------------ 模型服务配置 ------------------------------ */

#[tauri::command]
pub(crate) fn get_llm_config() -> llm::LlmView {
    llm::to_view(&llm::load_file())
}

/// 保存配置。api_key 为空表示「沿用已存的 key」（避免前端每次都要重填）。
/// 传 "__CLEAR__" 表示清空 key。
#[tauri::command]
pub(crate) fn save_llm_config(
    base_url: String,
    model: String,
    api_key: String,
    stream: Option<bool>,
) -> Result<llm::LlmView, String> {
    let mut f = llm::load_file();
    f.base_url = base_url.trim().trim_end_matches('/').to_string();
    f.model = model.trim().to_string();
    if let Some(st) = stream {
        f.stream = st;
    }

    match api_key.trim() {
        "" => { /* 保持不变 */ }
        "__CLEAR__" => f.key_cipher.clear(),
        k => {
            f.key_cipher = llm::protect_str(k).ok_or("API Key 加密失败")?;
        }
    }

    llm::save_file(&f)?;
    Ok(llm::to_view(&f))
}

pub(crate) fn test_llm_config_sync() -> Result<String, String> {
    llm::test_connection(&llm::load_file())
}

#[tauri::command]
pub(crate) async fn test_llm_config() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(test_llm_config_sync)
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/* ------------------------------ 选中即问（A1） ------------------------------ */

/// 送入大模型的单次最大字符数上限：防止复制大体积日志/源码把 Token 与延迟撑爆。
const MAX_LLM_INPUT_CHARS: usize = 4000;

/// 评估用户当前的工作负载与日程冲突情况（供剪贴板交互、待办入库反馈与负载盘点共用）。
///
/// 统计口径：
///   - `active_count`：所有未完成的事项数（`status == 0` 待触发 + `status == 1` 已提醒未结）
///   - `overdue_count`：已到点但尚未完成的事项数
///   - `est_minutes_total`：当前未完成事项的预估总工时（分钟）
///   - `focus_min_today`：今日已累计专注时长（分钟）
///   - `conflicts`：与新加入事项 `new_ids` 时间间隔在 ±30 分钟内的既有待办标题
///   - `trip_cities`：未完成事项中涉及的出差/异地城市
///   - `summary_line`：直接展示在宠物气泡里的单行负载衔接提示
pub(crate) fn assess_workload(new_ids: &[i64]) -> serde_json::Value {
    let now = now_sec();
    let day_start = local_day_start(now);
    let home = crate::location::home();
    let cur_loc = crate::location::current_label();

    let reminders = crate::db::with_conn(db::repo::list_reminders).unwrap_or_default();
    let focus_sec_today: i64 = crate::db::with_conn(|c| db::repo::day_summary(c, day_start, day_start + 86_400))
        .unwrap_or_default()
        .iter()
        .map(|r| r.focus_sec)
        .sum();
    let focus_min_today = (focus_sec_today as f64 / 60.0).round() as i64;

    let active: Vec<&db::repo::Reminder> = reminders
        .iter()
        .filter(|r| r.status != 2 && r.kind == 0)
        .collect();
    let active_count = active.len();
    let pending_count = active.iter().filter(|r| r.status == 0).count();
    let overdue_count = active
        .iter()
        .filter(|r| r.status == 1 || (r.status == 0 && r.fire_at < now))
        .count();
    let high_pri_count = active.iter().filter(|r| r.priority == 1).count();
    let est_minutes_total: i64 = active.iter().map(|r| r.duration_min.max(0)).sum();

    // 检查新入库事项与既有未完成事项是否存在 ±30 分钟内的日程碰撞
    let new_items: Vec<&&db::repo::Reminder> = active.iter().filter(|r| new_ids.contains(&r.id)).collect();
    let mut conflicts: Vec<String> = Vec::new();
    let mut new_trip_city = String::new();
    for nr in &new_items {
        if !nr.location.is_empty()
            && (crate::location::is_trip(&nr.location, home.as_deref()) || nr.title.contains("出差"))
            && new_trip_city.is_empty()
        {
            new_trip_city = nr.location.clone();
        }
        for other in &active {
            if new_ids.contains(&other.id) || other.status != 0 {
                continue;
            }
            if (other.fire_at - nr.fire_at).abs() <= 1800 {
                if !conflicts.contains(&other.title) {
                    conflicts.push(other.title.clone());
                }
            }
        }
    }

    // 收集所有未完成事项中的异地/出差行程
    let mut trip_cities: Vec<String> = Vec::new();
    for r in &active {
        let loc = r.location.trim();
        if !loc.is_empty() && (crate::location::is_trip(loc, home.as_deref()) || r.title.contains("出差")) {
            if !trip_cities.contains(&loc.to_string()) {
                trip_cities.push(loc.to_string());
            }
        }
    }

    // 挑出当前最紧迫的一件事（供负载盘点推荐）
    let top_item = active
        .iter()
        .min_by_key(|r| (r.priority, r.fire_at))
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "title": r.title,
                "fire_at": r.fire_at,
                "priority": r.priority,
                "duration_min": r.duration_min,
            })
        });

    let dur_tag = if est_minutes_total > 0 {
        format!("，预估共 {est_minutes_total} 分钟")
    } else {
        String::new()
    };

    let summary_line = if !conflicts.is_empty() {
        format!(
            "⚠️ 日程紧邻：与「{}」间隔不足 30 分钟；手头共 {active_count} 项待办{dur_tag}",
            crate::clipboard::truncate_chars(&conflicts[0], 14)
        )
    } else if !new_trip_city.is_empty() {
        format!(
            "🧳 已衔接{}差旅行程；当前手头共 {active_count} 项待办{dur_tag}",
            new_trip_city
        )
    } else if overdue_count >= 2 || active_count >= 5 || est_minutes_total >= 150 {
        format!(
            "📊 负载偏高：手头 {active_count} 项待办（含 {overdue_count} 项已到点/逾期{dur_tag}），建议先处理高优项"
        )
    } else {
        format!(
            "📊 当前手头 {active_count} 项待办{dur_tag} · 今日已专注 {focus_min_today} 分钟"
        )
    };

    let prompt_ctx = format!(
        "当前所在地：{}（常驻地：{}）；今日已专注 {} 分钟；手头未完成待办 {} 项（高优 {} 项，已到点/逾期 {} 项{}）{}",
        if cur_loc.is_empty() { "未知" } else { &cur_loc },
        home.as_deref().unwrap_or("未设置"),
        focus_min_today,
        active_count,
        high_pri_count,
        overdue_count,
        dur_tag,
        if trip_cities.is_empty() {
            String::new()
        } else {
            format!("；近期差旅目的地：{}", trip_cities.join("、"))
        }
    );

    serde_json::json!({
        "active_count": active_count,
        "pending_count": pending_count,
        "overdue_count": overdue_count,
        "high_pri_count": high_pri_count,
        "est_minutes_total": est_minutes_total,
        "focus_min_today": focus_min_today,
        "conflicts": conflicts,
        "trip_cities": trip_cities,
        "top_item": top_item,
        "summary_line": summary_line,
        "prompt_ctx": prompt_ctx,
    })
}

/// 盘点当前工作负载并推给宠物气泡展示（右键菜单「盘点当前工作负载」入口）。
pub(crate) fn workload_brief_sync(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let w = assess_workload(&[]);
    let active = w["active_count"].as_u64().unwrap_or(0);
    let overdue = w["overdue_count"].as_u64().unwrap_or(0);
    let hi = w["high_pri_count"].as_u64().unwrap_or(0);
    let est = w["est_minutes_total"].as_i64().unwrap_or(0);
    let focus = w["focus_min_today"].as_i64().unwrap_or(0);
    let trips: Vec<String> = w["trip_cities"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut lines: Vec<String> = Vec::new();
    if active == 0 {
        lines.push(format!(
            "📊 今日已专注 {focus} 分钟，当前手头没有未完成待办。"
        ));
        lines.push("可在任意聊天/邮件中选中文字按 Ctrl+C，再按 Ctrl+Shift+T 提取待办或出差行程。".into());
    } else {
        let est_str = if est > 0 { format!(" · 预估 {est} 分钟") } else { String::new() };
        lines.push(format!(
            "📊 手头共 {active} 项待办（高优 {hi} · 逾期/已提醒 {overdue}{est_str}），今日已专注 {focus} 分钟"
        ));
        if !trips.is_empty() {
            lines.push(format!("🧳 关联差旅行程：{}", trips.join("、")));
        }
        if let Some(top) = w.get("top_item").and_then(|v| v.as_object()) {
            let title = top.get("title").and_then(|x| x.as_str()).unwrap_or("");
            let pri = top.get("priority").and_then(|x| x.as_i64()).unwrap_or(2);
            let dur = top.get("duration_min").and_then(|x| x.as_i64()).unwrap_or(0);
            if !title.is_empty() {
                let dur_part = if dur > 0 { format!("（约 {dur} 分钟）") } else { String::new() };
                lines.push(format!(
                    "👉 建议优先处理：{}「{title}」{dur_part}",
                    if pri == 1 { "[高优] " } else { "" }
                ));
            }
        }
    }

    let fst = crate::focus::status();
    if fst.active {
        lines.push(format!(
            "🎯 当前正在专注：已专注 {} / {} 分钟（本轮 IM 打断 {} 次）",
            fst.active_secs / 60,
            fst.target_secs / 60,
            fst.im_interrupts
        ));
    }
    if !fst.trend.summary.is_empty() {
        lines.push(format!("📈 专注打断统计：{}", fst.trend.summary));
    }

    let text = lines.join("\n");
    let _ = app.emit("wm:answer", &text);
    Ok(serde_json::json!({
        "text": text,
        "workload": w,
    }))
}

#[tauri::command]
pub(crate) async fn workload_brief(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || workload_brief_sync(app))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/// 读剪贴板 → 理解用户当前面临的场景（问题解答 / 待办交办 / 出差行程） → 结合工作负载问 LLM → 推给宠物气泡。
/// 返回值供 HTTP 控制面断言用（前端忽略返回值，等事件驱动展示）。
pub(crate) fn ask_clipboard_sync(app: tauri::AppHandle, mode: Option<String>) -> Result<String, String> {
    let text = crate::clipboard::read_text()?;
    if text.trim().is_empty() {
        return Err("剪贴板是空的：先选中文字按 Ctrl+C 复制，再按 Ctrl+Shift+Space".into());
    }
    let bounded = crate::clipboard::truncate_chars(text.trim(), MAX_LLM_INPUT_CHARS);
    let (scene, _, _) = llm::detect_scene(&bounded);
    let raw_mode = mode.as_deref().unwrap_or("explain");
    let m = llm::Mode::parse(raw_mode);
    startup_log(&format!(
        "ask_clipboard: {} chars (sent {}) scene={} kind={} mode={:?}",
        text.chars().count(),
        bounded.chars().count(),
        scene,
        llm::detect_kind(&text),
        m
    ));
    static STREAM_REQ_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let req_id = STREAM_REQ_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let f = llm::load_file();
    let app_stream = app.clone();
    let answer = if m == llm::Mode::Explain {
        let wl = assess_workload(&[]);
        let ctx = wl["prompt_ctx"].as_str().unwrap_or("");
        llm::chat_clipboard_scene_with_stream(&f, &bounded, scene, ctx, |phase, chunk_text| {
            let _ = app_stream.emit(
                "wm:answer_stream",
                serde_json::json!({
                    "req_id": req_id,
                    "phase": phase,
                    "text": chunk_text,
                }),
            );
        })?
    } else {
        llm::chat_mode_with_stream(&f, &bounded, m, |phase, chunk_text| {
            let _ = app_stream.emit(
                "wm:answer_stream",
                serde_json::json!({
                    "req_id": req_id,
                    "phase": phase,
                    "text": chunk_text,
                }),
            );
        })?
    };
    startup_log(&format!("ask_clipboard: answer {} chars (stream={})", answer.chars().count(), f.stream));
    let _ = app.emit("wm:answer", &answer);
    Ok(answer)
}

#[tauri::command]
pub(crate) async fn ask_clipboard(app: tauri::AppHandle, mode: Option<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || ask_clipboard_sync(app, mode))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/// 剪贴板场景与内容分析（A2：识别用户面临的场景 → 衔接待办/行程/问答与当前工作负载）。只读，不落库、不上传。
#[tauri::command]
pub(crate) fn clipboard_info() -> Result<serde_json::Value, String> {
    let text = crate::clipboard::read_text()?;
    let kind = llm::detect_kind(&text);
    let (scene, scene_label, scene_hint) = llm::detect_scene(&text);
    let recommended_act = match scene {
        "trip" | "task" => "task",
        "question" => "ask",
        _ => "",
    };
    Ok(serde_json::json!({
        "kind": kind,
        "scene": scene,
        "scene_label": scene_label,
        "scene_hint": scene_hint,
        "recommended_act": recommended_act,
        "chars": text.chars().count(),
        "actions": llm::suggest_actions(kind),
        "preview": crate::clipboard::truncate_chars(text.trim(), 120),
    }))
}

/// 用给定文本直接问 LLM，不碰剪贴板。
/// HTTP 控制面 /llm/chat 用它验证 LLM 链路（免去往剪贴板灌数据）；
/// 面板后续的「问答测试框」也可复用。
pub(crate) fn chat_text_sync(text: String, mode: Option<String>) -> Result<String, String> {
    let bounded = crate::clipboard::truncate_chars(text.trim(), MAX_LLM_INPUT_CHARS);
    llm::chat_mode(
        &llm::load_file(),
        &bounded,
        llm::Mode::parse(mode.as_deref().unwrap_or("explain")),
    )
}

#[tauri::command]
pub(crate) async fn chat_text(text: String, mode: Option<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || chat_text_sync(text, mode))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/* --------------------------- AI 解析选中内容 → 待办 --------------------------- */

/// 让 AI 把一段话抽成待办并落库，同时返回与当前工作负载的衔接评估。
///
/// 与 A3 的规则解析（`todo.rs`）分工：
///   · 规则解析：用户手打的一句话，确定性、零延迟、离线可用
///   · AI 解析：从聊天记录/邮件里「找」出这件事，并给出优先级与预估时长
/// 两者互补：AI 说没提时间时，仍回落到规则解析，再不行按 30 分钟。
pub(crate) fn ai_task_sync(
    app: tauri::AppHandle,
    db: &db::Db,
    text: String,
) -> Result<serde_json::Value, String> {
    let raw = text.trim();
    if raw.is_empty() {
        return Err("没有可解析的内容".into());
    }
    let bounded = crate::clipboard::truncate_chars(raw, MAX_LLM_INPUT_CHARS);
    let draft = llm::extract_task(&llm::load_file(), &bounded)?;

    // 位置标记：取不到就是空串，绝不让定位失败把建待办搞黄
    let loc = crate::location::current_label();
    let tz_offset_min = crate::tz::resolve_booking_offset_min(&loc);

    let now = now_sec();
    let (fire_at, has_time) = if draft.in_minutes >= 0 {
        (now + draft.in_minutes * 60, true)
    } else if let Some(p) = crate::todo::parse_in_tz(&draft.title, tz_offset_min)
        .or_else(|| crate::todo::parse_in_tz(raw, tz_offset_min))
    {
        (p.fire_at, !p.matched.is_empty())
    } else {
        (now + 30 * 60, false)
    };

    // P1 高优任务设为硬提醒（level=1），中低优设为软提醒（level=0，受静默时段与打扰预算保护）
    let level = if draft.priority <= 1 { 1 } else { 0 };
    let note_combined = combine_note(&draft.who, &draft.note);
    let id = db.with(|c| {
        db::repo::add_reminder_full(
            c,
            &db::repo::NewReminder {
                title: draft.title.clone(),
                fire_at,
                level,
                kind: 0,
                priority: draft.priority,
                duration_min: draft.duration_min,
                note: note_combined.clone(),
                location: loc,
                tz_offset_min,
            },
        )
    })?;
    // 记一笔生命周期：created 是后面算「响应时长」的分母
    let _ = db.with(|c| db::repo::log_task_event(c, Some(id), "created", None, &draft.title));

    let wl = assess_workload(&[id]);
    let wl_line = wl["summary_line"].as_str().unwrap_or("").to_string();
    let active_tz = crate::tz::active();

    let payload = serde_json::json!({
        "id": id,
        "title": draft.title,
        "fire_at": fire_at,
        "duration_min": draft.duration_min,
        "priority": draft.priority,
        "note": draft.note,
        "has_time": has_time,
        "tz_offset_min": tz_offset_min,
        "city_mdhm": crate::tz::fmt_mdhm_with_offset(fire_at, active_tz.offset_min),
        "cross_hint": crate::tz::cross_tz_hint(fire_at, tz_offset_min),
        "tz": active_tz,
        "workload_line": wl_line,
        "workload": wl,
    });
    startup_log(&format!(
        "ai_task: #{id} '{}' in={}min dur={}min pri={}",
        draft.title, draft.in_minutes, draft.duration_min, draft.priority
    ));
    let _ = app.emit("wm:task", &payload);
    let _ = app.emit("wm:reminders-changed", ());
    Ok(payload)
}

#[tauri::command]
pub(crate) async fn ai_task(
    app: tauri::AppHandle,
    db: tauri::State<'_, db::Db>,
    text: String,
) -> Result<serde_json::Value, String> {
    let db_cloned = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || ai_task_sync(app, &db_cloned, text))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/// 从剪贴板取文字（用户先 Ctrl+C 复制选中内容），理解场景并提取待办 / 出差行程，同时衔接到当前工作负载。
pub(crate) fn ai_task_from_clipboard_sync(
    app: tauri::AppHandle,
    db: &db::Db,
) -> Result<serde_json::Value, String> {
    let text = crate::clipboard::read_text()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("剪贴板是空的：先选中文字按 Ctrl+C，再按 Ctrl+Shift+T".into());
    }
    let bounded = crate::clipboard::truncate_chars(trimmed, MAX_LLM_INPUT_CHARS);
    let (scene, _, _) = llm::detect_scene(&bounded);
    // 出差行程或含多步骤前置条件（「在此之前」「提前」「还要」）时，走多条目拆解管线（event + task），
    // 并把拆解结果与工作负载评估一并推给宠物气泡。
    let multi_cues = ["在此之前", "提前", "还要", "另外", "同时", "并且"];
    if scene == "trip" || multi_cues.iter().any(|k| bounded.contains(k)) {
        let res = add_todo_sync(db, bounded, None)?;
        let ids: Vec<i64> = res["items"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x["id"].as_i64()).collect())
            .unwrap_or_default();
        let wl = assess_workload(&ids);
        let wl_line = wl["summary_line"].as_str().unwrap_or("").to_string();
        let payload = serde_json::json!({
            "id": res["id"],
            "title": res["title"],
            "fire_at": res["fire_at"],
            "duration_min": 0,
            "priority": 1,
            "note": "",
            "has_time": res["time_recognized"],
            "count": res["count"],
            "items": res["items"],
            "scene": scene,
            "workload_line": wl_line,
            "workload": wl,
        });
        let _ = app.emit("wm:task", &payload);
        let _ = app.emit("wm:reminders-changed", ());
        return Ok(payload);
    }
    ai_task_sync(app, db, bounded)
}

#[tauri::command]
pub(crate) async fn ai_task_from_clipboard(
    app: tauri::AppHandle,
    db: tauri::State<'_, db::Db>,
) -> Result<serde_json::Value, String> {
    let db_cloned = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || ai_task_from_clipboard_sync(app, &db_cloned))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/* ------------------------------ 自然语言待办（A3） ------------------------------ */

/// 只解析不落库：面板输入时实时预览「将会定在什么时候」（含当前城市时区与本机时区对照）
#[tauri::command]
pub(crate) fn parse_todo(text: String) -> serde_json::Value {
    let active_tz = crate::tz::active();
    match crate::todo::parse(&text) {
        Some(p) => serde_json::json!({
            "ok": true,
            "fire_at": p.fire_at,
            "title": p.title,
            "matched": p.matched,
            "tz_offset_min": p.tz_offset_min,
            "city_mdhm": crate::tz::fmt_mdhm_with_offset(p.fire_at, active_tz.offset_min),
            "machine_mdhm": crate::tz::fmt_mdhm_with_offset(p.fire_at, active_tz.machine_offset_min),
            "cross_hint": crate::tz::cross_tz_hint(p.fire_at, p.tz_offset_min),
            "tz": active_tz,
        }),
        None => serde_json::json!({
            "ok": false,
            "tz": active_tz,
        }),
    }
}

/// 规则式解析的结果（时间与优先级给默认值，与模型解析对齐字段）。
/// 规则解析器只管时间，优先级/时长/对象它判断不了，一律按默认。
fn rule_parse(raw: &str, fallback_minutes: Option<i64>) -> TodoFields {
    // 优先级必须显式给 2（中）：0 在入库时会被 clamp 成 1，
    // 那样每条规则解析的待办都变成「高优先级」。
    match crate::todo::parse(raw) {
        Some(p) => TodoFields {
            fire_at: p.fire_at,
            title: p.title,
            matched: p.matched,
            priority: 2,
            tz_offset_min: p.tz_offset_min,
            ..Default::default()
        },
        None => TodoFields {
            fire_at: now_sec() + fallback_minutes.unwrap_or(30) * 60,
            title: raw.to_string(),
            priority: 2,
            tz_offset_min: crate::tz::offset_min(),
            ..Default::default()
        },
    }
}

/// 对象写进备注，这样待办行里能直接看到「对象：张总」。
/// 单独抽出来的原因：规则解析器最容易把人名当成动作的一部分切走。
fn combine_note(who: &str, note: &str) -> String {
    if who.is_empty() {
        return note.trim().to_string();
    }
    if note.is_empty() {
        format!("对象：{who}")
    } else {
        format!("对象：{who}；{note}")
    }
}

/// 一次解析的结果。字段对齐，方便「模型解析 / 规则兜底」两条路互换。
#[derive(Default)]
struct TodoFields {
    fire_at: i64,
    title: String,
    matched: String,
    priority: i64,
    duration_min: i64,
    note: String,
    /// event = 有明确发生时刻的日程（出差/开会/拜访）；task = 要动手做的事
    kind: String,
    /// 日程所在地（出差城市）。待办留空，落库时用当前城市兜底。
    place: String,
    /// 簿记时的时区偏移（分钟，相对 UTC）
    tz_offset_min: i64,
}

/// 解析并落库。识别不出时间时用 fallback_minutes（默认 30 分钟）兜底，
/// 保证「随手记一句话」不会因为格式不标准而失败。
/// 时间与对象优先交给大模型，模型不可用才回落规则解析。
pub(crate) fn add_todo_sync(
    db: &db::Db,
    text: String,
    fallback_minutes: Option<i64>,
) -> Result<serde_json::Value, String> {
    let raw = text.trim();
    if raw.is_empty() {
        return Err("内容是空的".into());
    }

    // 位置必须**在 db.with 之外**取：current_label() 内部会走 with_conn 拿同一把
    // DB 互斥锁，而 std::sync::Mutex 不可重入 —— 写在闭包里会直接死锁，
    // 表现为 /todo/add 一直挂到 HTTP 超时（实测踩到）。
    let loc = crate::location::current_label();
    let default_tz_min = crate::tz::resolve_booking_offset_min(&loc);

    // 大模型优先，而且**允许一句话拆成多条**。
    // 「后天去深圳出差，在此之前还要订机票和酒店」是两件事：
    // 一个日程 + 一个前置待办。旧的单条抽取只会留下"订机票"，
    // 时间还因为表达不了"在此之前"而退化成 30 分钟后（实测踩到）。
    let f = llm::load_file();
    let mut source = "rule";
    // 时间是否真的从话里认出来了（而不是按兜底算的）
    let mut time_ok = false;
    let mut fields: Vec<TodoFields> = Vec::new();

    if crate::llm::api_key(&f).is_some() && !f.model.trim().is_empty() {
        match llm::extract_tasks(&f, raw) {
            Ok(list) => {
                source = "ai";
                for it in list {
                    let item_place = if it.location.trim().is_empty() {
                        loc.clone()
                    } else {
                        it.location.trim().to_string()
                    };
                    let item_tz = crate::tz::resolve_booking_offset_min(&item_place);
                    // 单条时间解析：先尝试按该条目所在城市时区解析 YYYY-MM-DD HH:MM，
                    // 若模型未给或给了相对词，再回落到本地规则解析器（支持「明天早晨」「明天中午」等）
                    let mut matched = String::new();
                    let parsed = crate::tz::parse_datetime_in_tz(&it.when, item_tz).or_else(|| {
                        crate::todo::parse_in_tz(&it.when, item_tz)
                            .or_else(|| crate::todo::parse_in_tz(&it.title, item_tz))
                            .or_else(|| crate::todo::parse_in_tz(raw, item_tz))
                            .map(|p| {
                                matched = p.matched;
                                p.fire_at
                            })
                    });
                    if parsed.is_some() {
                        time_ok = true;
                    }
                    let fire =
                        parsed.unwrap_or_else(|| now_sec() + fallback_minutes.unwrap_or(30) * 60);
                    fields.push(TodoFields {
                        fire_at: fire,
                        title: it.title,
                        matched,
                        priority: it.priority,
                        duration_min: it.duration_min,
                        note: combine_note(&it.who, &it.note),
                        kind: it.kind,
                        place: it.location,
                        tz_offset_min: item_tz,
                    });
                }
            }
            Err(e) => {
                startup_log(&format!("todo: ai parse failed ({e}), fallback to rule"));
                fields.push(rule_parse(raw, fallback_minutes));
            }
        }
    } else {
        fields.push(rule_parse(raw, fallback_minutes));
    }

    // 规则路径没有绝对时间，靠「有没有匹配到时间片段」判断；
    // AI 路径在分支里已经设过 time_ok，不要覆盖。
    if source == "rule" {
        time_ok = !fields[0].matched.is_empty();
    }

    let active_tz = crate::tz::active();
    let mut created: Vec<serde_json::Value> = Vec::new();
    for fld in &fields {
        // 日程用目的地城市（出差记录才有意义），普通待办用当前所在城市
        let place = if fld.place.is_empty() {
            loc.clone()
        } else {
            fld.place.clone()
        };
        let tz_min = if fld.tz_offset_min != 0 || place.is_empty() {
            fld.tz_offset_min
        } else {
            default_tz_min
        };
        // 日程（event）或高优任务（priority=1）按硬提醒（level=1）投递；
        // 普通中低优待办按软提醒（level=0）投递，自动继承静默时段与打扰预算保护。
        let level = if fld.priority <= 1 || fld.kind == "event" {
            1
        } else {
            0
        };
        let id = db.with(|c| {
            db::repo::add_reminder_full(
                c,
                &db::repo::NewReminder {
                    title: fld.title.clone(),
                    fire_at: fld.fire_at,
                    level,
                    kind: 0,
                    priority: fld.priority,
                    duration_min: fld.duration_min,
                    note: fld.note.clone(),
                    location: place,
                    tz_offset_min: tz_min,
                },
            )
        })?;
        let _ = db.with(|c| db::repo::log_task_event(c, Some(id), "created", None, &fld.title));
        startup_log(&format!(
            "todo added: #{id} '{}' at {} tz={} (source={source}, kind={})",
            fld.title,
            fld.fire_at,
            crate::tz::format_utc_label(tz_min),
            if fld.kind.is_empty() { "task" } else { &fld.kind }
        ));
        created.push(serde_json::json!({
            "id": id,
            "title": fld.title,
            "fire_at": fld.fire_at,
            "kind": if fld.kind.is_empty() { "task" } else { fld.kind.as_str() },
            "location": fld.place,
            "duration_min": fld.duration_min,
            "priority": fld.priority,
            "who_in_note": fld.note.starts_with("对象："),
            "tz_offset_min": tz_min,
            "city_mdhm": crate::tz::fmt_mdhm_with_offset(fld.fire_at, active_tz.offset_min),
            "cross_hint": crate::tz::cross_tz_hint(fld.fire_at, tz_min),
        }));
    }

    let created_ids: Vec<i64> = created.iter().filter_map(|x| x["id"].as_i64()).collect();
    let wl = assess_workload(&created_ids);
    let wl_line = wl["summary_line"].as_str().unwrap_or("").to_string();

    // 兼容字段取第一条：旧调用方（与既有测试）只认单条
    let first = &fields[0];
    Ok(serde_json::json!({
        "id": created[0]["id"],
        "fire_at": first.fire_at,
        "title": first.title,
        "matched": first.matched,
        // parsed 的真实含义是「时间认出来了没有」，两条路径口径统一
        "parsed": time_ok,
        "time_recognized": time_ok,
        "source": source,
        "who_in_note": first.note.starts_with("对象："),
        "tz_offset_min": first.tz_offset_min,
        "city_mdhm": crate::tz::fmt_mdhm_with_offset(first.fire_at, active_tz.offset_min),
        "cross_hint": crate::tz::cross_tz_hint(first.fire_at, first.tz_offset_min),
        "tz": active_tz,
        "count": created.len(),
        "items": created,
        "workload_line": wl_line,
        "workload": wl,
    }))
}

#[tauri::command]
pub(crate) async fn add_todo(
    db: tauri::State<'_, db::Db>,
    text: String,
    fallback_minutes: Option<i64>,
) -> Result<serde_json::Value, String> {
    let db_cloned = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || add_todo_sync(&db_cloned, text, fallback_minutes))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/// 大模型用量：今天 + 最近 N 天 + 本次启动以来。
///
/// 「本次启动以来」是内存态，重启清零；按天的是库里的，重启不丢。
/// 两个都给是因为排查口径不同：看今天烧了多少用按天，看刚才那次操作花多少用启动以来。
#[tauri::command]
pub(crate) fn llm_usage(days: Option<i64>) -> serde_json::Value {
    let n = days.unwrap_or(14).clamp(1, 90);
    let today = crate::logs::bj_date(now_sec());
    let by_day = crate::db::with_conn(|c| crate::db::repo::usage_days(c, n)).unwrap_or_default();
    let today_row = crate::db::with_conn(|c| crate::db::repo::usage_of_day(c, &today))
        .unwrap_or(crate::db::repo::UsageDay {
            day: today.clone(),
            ..Default::default()
        });
    let since = crate::llm::usage_since_start();
    let total_30: crate::db::repo::UsageDay =
        crate::db::with_conn(|c| crate::db::repo::usage_days(c, 30))
            .unwrap_or_default()
            .iter()
            .fold(
                crate::db::repo::UsageDay {
                    day: "30d".to_string(),
                    ..Default::default()
                },
                |mut a, d| {
                    a.requests += d.requests;
                    a.prompt_tokens += d.prompt_tokens;
                    a.completion_tokens += d.completion_tokens;
                    a.total_tokens += d.total_tokens;
                    a.failed += d.failed;
                    a
                },
            );
    serde_json::json!({
        "today": today_row,
        "total30": total_30,
        "since_start": since,
        "days": by_day,
    })
}

/// 模型接入状态（面板「LLM API」页只显示状态，参数折叠在高级设置里）
#[tauri::command]
pub(crate) fn llm_status() -> serde_json::Value {
    let f = llm::load_file();
    let key = crate::llm::api_key(&f);
    let base_host = f
        .base_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    serde_json::json!({
        "configured": key.is_some() && !f.model.trim().is_empty() && !f.base_url.trim().is_empty(),
        "model": f.model.clone(),
        "base_host": base_host,
        "has_key": key.is_some(),
        "last": llm::last_call(),
    })
}

/* ------------------------------ 演示 / 投屏让位 ------------------------------ */

/// 当前演示状态明细（自动检测 + 手动剩余秒数）
#[tauri::command]
pub(crate) fn presentation_state() -> serde_json::Value {
    let (on, auto, manual_left) = perception::presenting_detail();
    serde_json::json!({
        "presenting": on,
        "auto_fullscreen": auto,
        "manual_left_s": manual_left,
        "fullscreen_now": perception::fullscreen_foreground(),
    })
}

/// 开启/解除手动演示模式。
///
/// 窗口化共享屏幕（Teams/Zoom 只共享一个窗口）无法被全屏检测识别，
/// 这是用户主动声明"别出来"的唯一入口。seconds <= 0 表示立即解除。
#[tauri::command]
pub(crate) fn set_presentation(app: tauri::AppHandle, seconds: Option<i64>) -> serde_json::Value {
    let secs = seconds.unwrap_or(3600);
    perception::set_manual_present(secs.max(0));
    crate::apply_presenting(&app);
    startup_log(&format!("presentation: manual set to {secs}s"));
    presentation_state()
}

/* ------------------------------ 提醒 / 待办（M6） ------------------------------ */

#[tauri::command]
pub(crate) fn add_reminder(
    db: tauri::State<db::Db>,
    title: String,
    fire_at: i64,
    level: Option<i64>,
    kind: Option<i64>,
) -> Result<i64, String> {
    let t = title.trim();
    if t.is_empty() {
        return Err("标题不能为空".into());
    }
    db.with(|c| db::repo::add_reminder(c, t, fire_at, level.unwrap_or(1), kind.unwrap_or(0)))
}

#[tauri::command]
pub(crate) fn list_reminders(db: tauri::State<db::Db>) -> Result<Vec<db::repo::Reminder>, String> {
    db.with(db::repo::list_reminders)
}

/// 标完成。顺带记一笔生命周期——「提醒后多久才处理」就是从这里来的，
/// 不记的话「处理时机」这个维度根本无从量化。
#[tauri::command]
pub(crate) fn complete_reminder(
    app: tauri::AppHandle,
    db: tauri::State<db::Db>,
    id: i64,
) -> Result<bool, String> {
    let ok = db.with(|c| db::repo::set_reminder_status(c, id, 2))?;
    if ok {
        let _ = db.with(|c| db::repo::log_task_event(c, Some(id), "done", None, ""));
        let _ = app.emit("wm:reminders-changed", ());
    }
    Ok(ok)
}

#[tauri::command]
pub(crate) fn delete_reminder(
    app: tauri::AppHandle,
    db: tauri::State<db::Db>,
    id: i64,
) -> Result<bool, String> {
    let ok = db.with(|c| {
        c.execute("DELETE FROM reminders WHERE id = ?1", rusqlite::params![id])
            .map(|n| n > 0)
    })?;
    if ok {
        let _ = app.emit("wm:reminders-changed", ());
    }
    Ok(ok)
}

/* ------------------------------ 数据层命令 ------------------------------ */

#[tauri::command]
pub(crate) fn db_stats(db: tauri::State<db::Db>) -> Result<db::repo::DbStats, String> {
    let mut st = db.with(db::repo::stats)?;
    st.size_bytes = db.size_bytes();
    st.path = db.path().display().to_string();
    Ok(st)
}

/// 按需刷新统计：聚合落后就补一次。
///
/// 以前只在后台每 5 分钟聚合一次，用户切到「今日」页经常看到空图，
/// 只能去技术页点「立即聚合」。现在改成**由动作触发**：
/// 打开面板、切回今日页、点刷新，都会先补齐聚合再渲染。
pub(crate) fn ensure_stats_fresh_sync(db: &db::Db) -> Result<serde_json::Value, String> {
    let now = now_sec();
    let last = db.with(db::repo::latest_rollup_bucket)?;
    let cur_bucket = (now / 300) * 300;
    let stale = match last {
        None => true,
        Some(b) => b < cur_bucket - 300,
    };
    let mut rolled = 0usize;
    if stale {
        // 回补 26 小时：覆盖「今日概览」与「近 24 小时」两个视图
        rolled = db.with(|c| db::repo::rollup_range(c, now - 26 * 3600, now))?;
    }
    let recent_from = cur_bucket - 600;
    rolled += db.with(|c| db::repo::rollup_range(c, recent_from, now))?;
    Ok(serde_json::json!({ "stale": stale, "rolled": rolled }))
}

#[tauri::command]
pub(crate) async fn ensure_stats_fresh(db: tauri::State<'_, db::Db>) -> Result<serde_json::Value, String> {
    let db_cloned = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || ensure_stats_fresh_sync(&db_cloned))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/// 按标题前缀清理提醒（自动化用例收尾 / 手动清理测试数据）
#[tauri::command]
pub(crate) fn gc_reminders(db: tauri::State<db::Db>, prefix: String) -> Result<usize, String> {
    if prefix.trim().is_empty() {
        return Err("prefix 不能为空".into());
    }
    let n = db.with(|c| db::repo::delete_reminders_by_prefix(c, prefix.trim()))?;
    startup_log(&format!("gc_reminders: prefix={prefix} deleted={n}"));
    Ok(n)
}

/// 清空统计（明细 + 聚合），不清提醒与设置
#[tauri::command]
pub(crate) fn reset_stats(db: tauri::State<db::Db>) -> Result<serde_json::Value, String> {
    let (ev, ru) = db.with(db::repo::reset_stats)?;
    startup_log(&format!("stats reset: events={ev} rollup={ru}"));
    Ok(serde_json::json!({ "events": ev, "rollup": ru }))
}

/* ------------------------------ 记忆层（v6） ------------------------------ */

/// 给 AI 的记忆摘要（结构化，便于面板展示与测试断言）
pub(crate) fn memory_brief_sync(days: Option<i64>, trip_days: Option<i64>) -> serde_json::Value {
    crate::memory::brief(days.unwrap_or(7).clamp(1, 60), trip_days.unwrap_or(120).clamp(1, 365))
}

#[tauri::command]
pub(crate) async fn memory_brief(days: Option<i64>, trip_days: Option<i64>) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || memory_brief_sync(days, trip_days))
        .await
        .unwrap_or_default()
}

/// 用自然语言问记忆。内部：意图识别 → SQL 取事实 → 模型把事实说成人话。
///
/// 返回值同时带 `facts`（可靠）与 `result.answer`（模型措辞），
/// 模型不可用时 facts 照常返回——功能降级而不是整体失效。
pub(crate) fn memory_ask_sync(question: String) -> Result<serde_json::Value, String> {
    let q = question.trim();
    if q.is_empty() {
        return Err("问题是空的".into());
    }
    let r = crate::memory::answer(q);
    startup_log(&format!(
        "memory_ask: intent={} ok={}",
        r["intent"].as_str().unwrap_or("?"),
        r["result"]["ok"].as_bool().unwrap_or(false)
    ));
    Ok(r)
}

#[tauri::command]
pub(crate) async fn memory_ask(question: String) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || memory_ask_sync(question))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/// 立即重算风险（面板「刷新」与每日维护共用）
pub(crate) fn memory_scan_sync() -> serde_json::Value {
    let rs = crate::memory::scan_risks();
    serde_json::json!({ "risks": rs, "count": rs.len() })
}

#[tauri::command]
pub(crate) async fn memory_scan() -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(memory_scan_sync)
        .await
        .unwrap_or_default()
}

/// 重算最近 N 天的每日画像
pub(crate) fn memory_rebuild_sync(days: Option<i64>) -> serde_json::Value {
    let n = crate::memory::rebuild_recent(days.unwrap_or(7).clamp(1, 60));
    serde_json::json!({ "days": n })
}

#[tauri::command]
pub(crate) async fn memory_rebuild(days: Option<i64>) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || memory_rebuild_sync(days))
        .await
        .unwrap_or_default()
}

/// 生成某一天的短期记忆（后端每天调一次模型写日记）。
/// day 留空 = 昨天；force=1 时覆盖已有记录（重跑、演示用）。
pub(crate) fn memory_daily_sync(day: Option<String>, force: Option<bool>) -> serde_json::Value {
    let d = day
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::logs::bj_date(crate::now_sec() - 86_400));
    let r = crate::memory::write_daily(&d, force.unwrap_or(false));
    startup_log(&format!(
        "memory_daily: {} via {}",
        d,
        r["source"].as_str().unwrap_or("skipped")
    ));
    r
}

#[tauri::command]
pub(crate) async fn memory_daily(day: Option<String>, force: Option<bool>) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || memory_daily_sync(day, force))
        .await
        .unwrap_or_default()
}

/// 手动触发分层抽象（short → mid → long）。正常由每日巡检自动跑。
pub(crate) fn memory_rollup_sync() -> serde_json::Value {
    crate::memory::rollup_notes()
}

#[tauri::command]
pub(crate) async fn memory_rollup() -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(memory_rollup_sync)
        .await
        .unwrap_or_default()
}

/// 读分层记忆：scope = short（1 周内，按天详细）/ mid（1 月内，按月抽象）/ long（3 月以上）
#[tauri::command]
pub(crate) fn memory_notes(scope: String, limit: Option<i64>) -> serde_json::Value {
    let sc = match scope.as_str() {
        "mid" => "mid",
        "long" => "long",
        _ => "short",
    };
    let rows = crate::db::with_conn(|c| crate::db::repo::list_notes(c, sc, limit.unwrap_or(30).clamp(1, 200)))
        .unwrap_or_default();
    serde_json::json!({
        "scope": sc,
        "notes": rows.iter().map(|n| serde_json::json!({
            "period": n.period,
            "ts": n.ts,
            "source": n.source,
            "summary": n.summary,
            "weight": n.weight,
        })).collect::<Vec<_>>(),
    })
}

/// 当前这句「有温度」的提示是什么（面板预览 / 自动化断言用）
#[tauri::command]
pub(crate) fn memory_warm() -> String {
    crate::memory::warm_hint()
}

/* ------------------------------ 位置 ------------------------------ */

/// 读当前缓存的定位结果（不触发采集）
#[tauri::command]
pub(crate) fn location_now() -> serde_json::Value {
    let f = crate::location::cached();
    let home = crate::location::home();
    serde_json::json!({
        "label": f.label,
        "source": f.source,
        "lat": f.lat,
        "lon": f.lon,
        "acc_m": f.acc_m,
        "home": home.clone().unwrap_or_default(),
        "trip": crate::location::is_trip(&f.label, home.as_deref()),
    })
}

/// 立即采一次位置并落库
pub(crate) fn location_refresh_sync() -> serde_json::Value {
    crate::location::record()
}

#[tauri::command]
pub(crate) async fn location_refresh() -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(location_refresh_sync)
        .await
        .unwrap_or_default()
}

/// 手填位置（定位不可用时的兜底，也是最可靠的一种来源）
pub(crate) fn location_set_sync(
    app: tauri::AppHandle,
    db: &db::Db,
    label: String,
    as_home: Option<bool>,
) -> Result<serde_json::Value, String> {
    let l = label.trim().to_string();
    if l.is_empty() {
        // 传空串 = 清除手填，回到自动定位，并立即同步时区
        db.with(|c| crate::db::repo::set_setting(c, "loc.manual_label", ""))?;
        let _ = crate::location::refresh();
        let tz = crate::tz::refresh();
        let _ = app.emit("wm:tz", &tz);
        return Ok(serde_json::json!({ "manual": false, "tz": tz }));
    }
    db.with(|c| crate::db::repo::set_setting(c, "loc.manual_label", &l))?;
    if as_home.unwrap_or(false) {
        db.with(|c| crate::db::repo::set_setting(c, "loc.home", &l))?;
    }
    let mut r = crate::location::record();
    let tz = crate::tz::refresh();
    if let Some(obj) = r.as_object_mut() {
        obj.insert("tz".into(), serde_json::to_value(&tz).unwrap_or_default());
    }
    let _ = app.emit("wm:tz", &tz);
    startup_log(&format!("location set manually: {l} -> tz={}", tz.utc_label));
    Ok(r)
}

#[tauri::command]
pub(crate) async fn location_set(
    app: tauri::AppHandle,
    db: tauri::State<'_, db::Db>,
    label: String,
    as_home: Option<bool>,
) -> Result<serde_json::Value, String> {
    let db_cloned = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || location_set_sync(app, &db_cloned, label, as_home))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/* ------------------------------ 时区（底层城市驱动） ------------------------------ */

/// 读取当前系统生效的时区详情（所在城市、UTC 偏移、与本机 OS 时区对照）。
#[tauri::command]
pub(crate) fn tz_status() -> serde_json::Value {
    let info = crate::tz::refresh();
    serde_json::to_value(info).unwrap_or_default()
}

/// 手动覆盖或恢复自动城市时区（`offset_min = None` 表示恢复跟随城市/默认中国 UTC+8）。
#[tauri::command]
pub(crate) fn tz_set(
    app: tauri::AppHandle,
    db: tauri::State<db::Db>,
    offset_min: Option<i64>,
) -> Result<serde_json::Value, String> {
    match offset_min {
        Some(m) => {
            let clamped = m.clamp(-12 * 60, 14 * 60);
            db.with(|c| crate::db::repo::set_setting(c, "tz.manual_offset_min", &clamped.to_string()))?;
        }
        None => {
            db.with(|c| crate::db::repo::set_setting(c, "tz.manual_offset_min", ""))?;
        }
    }
    let info = crate::tz::refresh();
    let _ = app.emit("wm:tz", &info);
    startup_log(&format!(
        "tz set: {} ({}) source={}",
        info.city_label, info.utc_label, info.source
    ));
    Ok(serde_json::to_value(info).unwrap_or_default())
}

/// 清除全部位置轨迹（隐私开关）
#[tauri::command]
pub(crate) fn location_clear() -> Result<usize, String> {
    let n = crate::location::clear()?;
    startup_log(&format!("location cleared: {n} rows"));
    Ok(n)
}

/// 位置轨迹（按天聚合后返回，面板「行程」用）
#[tauri::command]
pub(crate) fn location_history(days: Option<i64>) -> serde_json::Value {
    let d = days.unwrap_or(120).clamp(1, 365);
    let since = crate::now_sec() - d * 86_400;
    let rows = crate::db::with_conn(|c| crate::db::repo::location_days(c, since, 200))
        .unwrap_or_default();
    serde_json::json!({
        "days": rows.iter().map(|(day, place, trip)| serde_json::json!({
            "day": day, "place": place, "trip": *trip == 1
        })).collect::<Vec<_>>()
    })
}

/// 原始位置打点（诊断用：看精度、来源、经纬度）
#[tauri::command]
pub(crate) fn location_raw(days: Option<i64>) -> serde_json::Value {
    let d = days.unwrap_or(30).clamp(1, 365);
    let since = crate::now_sec() - d * 86_400;
    let rows = crate::db::with_conn(|c| crate::db::repo::recent_locations(c, since, 200))
        .unwrap_or_default();
    serde_json::json!({ "rows": rows })
}

/* ------------------------------ 设置（v3） ------------------------------ */

#[tauri::command]
pub(crate) fn get_setting(db: tauri::State<db::Db>, key: String) -> Result<Option<String>, String> {
    db.with(|c| db::repo::get_setting(c, &key))
}

#[tauri::command]
pub(crate) fn set_setting(
    app: tauri::AppHandle,
    db: tauri::State<db::Db>,
    key: String,
    value: String,
) -> Result<(), String> {
    db.with(|c| db::repo::set_setting(c, &key, &value))?;
    // 特定设置要立刻生效：低功耗模式切换形象，不用重启也不用前端自己发事件
    // （前端发事件需要额外权限，而这里是同一个命令的自然延伸）
    if key == "pet.low_power" {
        let _ = app.emit("wm:lowpower", value);
    }
    Ok(())
}

/* ------------------------------ 今日概览（A4） ------------------------------ */

#[derive(serde::Serialize)]
pub(crate) struct TodaySummary {
    /// 今日零点（本地时间）与当前时刻
    day_start: i64,
    now: i64,
    /// 今日累计专注分钟（各桶 focus_min 之和）
    focus_min: i64,
    /// 各分类分布（按专注时长倒序）
    categories: Vec<db::repo::CategoryRow>,
    /// 今日提醒：待办 / 已完成 / 喝水 / 站起
    rm_pending: i64,
    rm_done: i64,
    water_count: i64,
    standup_count: i64,
    /// 当前这次连续专注已经多久（分钟）
    sit_streak_min: i64,
    /// 今日已写入的明细条数（数据层自检用）
    events: i64,
}

/// 本地时间「今天零点」的时间戳。
///
/// 用北京时间（notify::bj_day_start）：本机系统时区被故意设成美国时区，
/// 按机器本地时间算「今天」会让统计口径与用户认知差 15 小时。
fn local_day_start(_now: i64) -> i64 {
    crate::notify::bj_day_start()
}

/// 当天的工作块时间轴：回答「我今天到底在做什么」。
pub(crate) fn today_blocks_sync(min_secs: Option<i64>) -> serde_json::Value {
    let start = crate::notify::bj_day_start();
    let now = now_sec();
    let all = crate::db::with_conn(|c| db::repo::work_blocks(c, start, now)).unwrap_or_default();
    // 少于 1 分钟的碎片不进时间轴：它们大多是"点开看一眼"，画出来全是锯齿。
    let min = min_secs.unwrap_or(60).clamp(0, 3600);

    let total: i64 = all.iter().map(|b| b.secs).sum();

    // 过滤掉 < min 的极短切屏碎片后，若相邻两块属于同一类别且间隔 ≤ 5 分钟（300s），
    // 则自动合并为一段连续工作块，避免「中间点了一下桌面 5 秒」把一段 20 分钟的工作拆成多行。
    let mut merged: Vec<db::repo::WorkBlock> = Vec::new();
    for b in all.iter().filter(|b| b.secs >= min) {
        match merged.last_mut() {
            Some(prev) if prev.app_cat == b.app_cat && b.start - prev.end <= 300 => {
                prev.end = b.end;
                prev.secs += b.secs;
            }
            _ => merged.push(b.clone()),
        }
    }

    // 深度块：连续 ≥25 分钟（一个番茄钟的下限）。
    let deep: Vec<&db::repo::WorkBlock> = merged.iter().filter(|b| b.secs >= 25 * 60).collect();
    let deep_sec: i64 = deep.iter().map(|b| b.secs).sum();
    // 切换次数 = 过滤碎片后相邻工作块换了类别的次数（避免 5s 误触把切换刷到上百次）。
    let switches = merged
        .windows(2)
        .filter(|w| w[0].app_cat != w[1].app_cat)
        .count();

    let blocks: Vec<serde_json::Value> = merged
        .iter()
        .map(|b| {
            serde_json::json!({
                "start": b.start,
                "end": b.end,
                "secs": b.secs,
                "app_cat": b.app_cat,
                "label": crate::memory::cat_label(b.app_cat),
            })
        })
        .collect();

    serde_json::json!({
        "day_start": start,
        "now": now,
        "total_sec": total,
        "deep_sec": deep_sec,
        "deep_count": deep.len(),
        "switches": switches,
        "block_count": all.len(),
        "blocks": blocks,
    })
}

#[tauri::command]
pub(crate) async fn today_blocks(min_secs: Option<i64>) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || today_blocks_sync(min_secs))
        .await
        .unwrap_or_default()
}

pub(crate) fn today_summary_sync(db: &db::Db) -> Result<TodaySummary, String> {
    let now = now_sec();
    let day_start = local_day_start(now);
    let categories = db.with(|c| db::repo::day_summary(c, day_start, day_start + 86_400))?;
    // 只在这里做一次秒→分钟换算（round），不让秒数散落到 UI
    let focus_sec: i64 = categories.iter().map(|r| r.focus_sec).sum();
    let focus_min: i64 = (focus_sec as f64 / 60.0).round() as i64;

    let counts = db.with(|c| -> rusqlite::Result<(i64, i64, i64, i64)> {
        let pending: i64 = c.query_row(
            "SELECT count(*) FROM reminders WHERE kind = 0 AND status = 0",
            [],
            |r| r.get(0),
        )?;
        let param_q = |sql: &str| -> rusqlite::Result<i64> {
            c.query_row(sql, rusqlite::params![day_start], |r| r.get(0))
        };
        Ok((
            pending,
            param_q("SELECT count(*) FROM reminders WHERE kind = 0 AND status = 2 AND done_at >= ?1")?,
            param_q("SELECT count(*) FROM reminders WHERE kind = 1 AND fired_at >= ?1")?,
            param_q("SELECT count(*) FROM reminders WHERE kind = 2 AND fired_at >= ?1")?,
        ))
    })?;

    let (events, sit_streak_min) = db.with(|c| -> rusqlite::Result<(i64, i64)> {
        let ev: i64 = c.query_row(
            "SELECT count(*) FROM events WHERE ts >= ?1",
            rusqlite::params![day_start],
            |r| r.get(0),
        )?;
        let fm: i64 = c.query_row(
            "SELECT COALESCE((SELECT focus_min FROM events ORDER BY ts DESC LIMIT 1), 0)",
            [],
            |r| r.get(0),
        )?;
        Ok((ev, fm))
    })?;

    Ok(TodaySummary {
        day_start,
        now,
        focus_min,
        categories,
        rm_pending: counts.0,
        rm_done: counts.1,
        water_count: counts.2,
        standup_count: counts.3,
        sit_streak_min,
        events,
    })
}

#[tauri::command]
pub(crate) async fn today_summary(db: tauri::State<'_, db::Db>) -> Result<TodaySummary, String> {
    let db_cloned = db.inner().clone();
    tauri::async_runtime::spawn_blocking(move || today_summary_sync(&db_cloned))
        .await
        .map_err(|e| format!("后台线程异常: {e}"))?
}

/// 生成模拟感知数据（每 5 分钟一条），用于验证 rollup 与查询链路
#[tauri::command]
pub(crate) fn simulate_ticks(db: tauri::State<db::Db>, count: Option<u32>) -> Result<usize, String> {
    let n = count.unwrap_or(24).min(5000) as i64;
    let now = now_sec();
    let mut rows = Vec::with_capacity(n as usize);
    // 类别按真实使用习惯加权（编码/浏览器/办公为主），
    // 不能再用 `i % 7` 平均分——那样七类时长完全相等，用户一眼就看出是假数据
    // （真机反馈：「每类统计时间完全相等，是不是按后台进程算的？」）。
    const WEIGHTS: [(i64, u32); 8] = [
        (0, 30), // IDE
        (1, 18), // 办公
        (2, 20), // 浏览器
        (3, 6),  // 会议
        (4, 9),  // IM
        (5, 5),  // 媒体
        (6, 4),  // 其他
        (7, 8),  // AI Agent
    ];
    let total: u32 = WEIGHTS.iter().map(|w| w.1).sum();
    for i in 0..n {
        // 用采样序号做确定性伪随机，保证每次生成结果一致（便于回归对比）
        let slot = ((i as u32).wrapping_mul(2654435761) >> 8) % total;
        let mut acc = 0u32;
        let mut cat = 6i64;
        for (c, w) in WEIGHTS {
            acc += w;
            if slot < acc {
                cat = c;
                break;
            }
        }
        rows.push(db::repo::Snapshot {
            // 间距 5 秒，与真实感知采样一致。
            // 原来是 300 秒——48 条就横跨 4 小时，跨过北京午夜后大半落到"昨天"，
            // 「今日概览」在凌晨跑测试时会显示 0（真踩过：00:20 跑回归，focus_min=0）。
            ts: now - (n - i) * 5,
            kind: 0,
            idle_s: 8 + (i % 20),
            app_cat: cat,
            // 刻意写成递增的「连续专注时长」：这是 gauge（瞬时值）不是增量。
            // 聚合层若误用 sum(focus_min)，总数会被放大几十倍——这条正好防回归。
            focus_min: i % 40,
            flags: if i % 5 == 0 { 1 } else { 0 },
        });
    }
    db.with_mut(|c| db::repo::insert_events(c, &rows))
}

#[tauri::command]
pub(crate) fn rollup_now(db: tauri::State<db::Db>) -> Result<usize, String> {
    let now = now_sec();
    db.with(|c| db::repo::rollup_range(c, now - 86_400, now))
}

#[tauri::command]
pub(crate) fn recent_summary(
    db: tauri::State<db::Db>,
    window_sec: Option<i64>,
) -> Result<Vec<db::repo::CategoryRow>, String> {
    let now = now_sec();
    db.with(|c| db::repo::recent_summary(c, window_sec.unwrap_or(86_400), now))
}

/* ------------------------------ 窗口控制命令 ------------------------------ */

#[tauri::command]
pub(crate) fn open_panel(app: tauri::AppHandle) -> Result<(), String> {
    // 面板窗口改为在 tauri.conf.json 里声明、随应用启动创建（默认 visible:false）。
    //
    // 原先是运行时用 WebViewWindowBuilder 现场创建第二个 WebView，实测不可靠：
    //   · setup 内同步创建 / 子线程创建 → HRESULT(0x8007139F)
    //   · 命令线程创建 → 窗口建出来但 WebView 是坏的（空白），且未注册成功
    //   · 托盘（主线程）创建 → 主线程挂死（标题栏「未响应」）、偶发崩溃
    // 与宠物窗走同一条已被验证可靠的创建路径（都由 Tauri 在启动期创建），
    // 这里只负责显示。
    match app.get_webview_window("panel") {
        Some(w) => {
            place_beside_pet(&app, &w);
            let _ = w.show();
            let _ = w.set_focus();
            let _ = app.emit("wm:panel-open", ());
            startup_log("panel shown");
            Ok(())
        }
        None => {
            startup_log("panel window missing");
            Err("panel window missing".into())
        }
    }
}

/// 把面板摆到宠物窗旁边（默认左侧，空间不够则右侧），底边对齐。
///
/// 为什么必须做：宠物窗是 alwaysOnTop，面板若按系统默认位置开，会被宠物压住
/// 左上角的内容——截图验证时确实挡住了「专注分钟」数字与分类 chip。
fn place_beside_pet(app: &tauri::AppHandle, panel: &tauri::WebviewWindow) {
    let Some(pet) = app.get_webview_window("pet") else {
        return;
    };
    let (Ok(pp), Ok(ps), Ok(pz)) = (pet.outer_position(), pet.outer_size(), panel.outer_size()) else {
        return;
    };

    // 用宠物所在显示器的工作区夹取，避免面板跑到屏外（多屏/云桌面同样适用）
    let monitor = pet.current_monitor().ok().flatten();
    let (mx, my, mw, mh) = match monitor {
        Some(m) => {
            let p = *m.position();
            let s = *m.size();
            (p.x, p.y, s.width as i32, s.height as i32)
        }
        None => (0, 0, 1920, 1080),
    };

    let gap = 12;
    let mut x = pp.x - pz.width as i32 - gap; // 优先左侧
    if x < mx {
        x = pp.x + ps.width as i32 + gap; // 左侧放不下 → 右侧
    }
    // 底边对齐，再夹到工作区内
    let mut y = pp.y + ps.height as i32 - pz.height as i32;
    if y < my {
        y = my;
    }
    if y + pz.height as i32 > my + mh {
        y = my + mh - pz.height as i32;
    }
    if x + pz.width as i32 > mx + mw {
        x = mx + mw - pz.width as i32;
    }

    let _ = panel.set_position(tauri::PhysicalPosition::new(x, y));
}

/// 测试模式开关（WORKMATE_TEST_PANEL=1），仅供自动化验证面板渲染用。
#[tauri::command]
pub(crate) fn test_mode() -> bool {
    std::env::var("WORKMATE_TEST_PANEL").is_ok()
}

/// 前端诊断通道：把 WebView 里的真实 DOM 状态写进 startup.log。
/// 桌面助手是 transparent + skipTaskbar，自动化侧截不到画面（ImageGrab/PrintWindow 都是黑），
/// 这条命令是唯一能确认「页面到底加载/渲染了没有」的手段。
#[tauri::command]
pub(crate) fn log_frontend(msg: String) {
    startup_log(&format!("FE | {msg}"));
}

#[tauri::command]
pub(crate) fn toggle_pet(app: tauri::AppHandle) -> Result<bool, String> {
    let Some(w) = app.get_webview_window("pet") else {
        return Err("pet window missing".into());
    };
    let visible = w.is_visible().unwrap_or(true);
    if visible {
        let _ = w.hide();
    } else {
        // 用户手动唤出宠物时，解除任何正在倒计时的 30 分钟隐藏/手动演示状态
        perception::set_manual_present(0);
        crate::clear_present_hidden();
        let _ = w.show();
    }
    Ok(!visible)
}

/// 隐藏宠物 30 分钟（1800 秒），到期由感知线程自动恢复显示。
#[tauri::command]
pub(crate) fn hide_pet(app: tauri::AppHandle) -> Result<(), String> {
    perception::set_manual_present(1800);
    crate::apply_presenting(&app);
    if let Some(w) = app.get_webview_window("pet") {
        let _ = w.hide();
    }
    startup_log("pet hidden for 30m (1800s)");
    Ok(())
}

#[tauri::command]
pub(crate) fn quit_app(app: tauri::AppHandle) {
    // 退出前先落盘位置，避免下次启动跑回默认点
    let _ = app.save_window_state(StateFlags::POSITION);
    app.exit(0);
}

/* ------------------------------ 调度与日志 IPC（解耦前端对 HTTP 控制面的依赖） ------------------------------ */

#[tauri::command]
pub(crate) fn notify_status() -> serde_json::Value {
    crate::notify::status()
}

#[tauri::command]
pub(crate) fn log_files() -> Vec<crate::logs::LogEntry> {
    crate::logs::list()
}

#[tauri::command]
pub(crate) fn log_open(name: Option<String>) -> Result<String, String> {
    let n = name.unwrap_or_default();
    if !crate::control::is_safe_log_filename(&n) {
        return Err("非法文件名".into());
    }
    let target = if n.is_empty() || n == "startup.log" {
        crate::logs::current_log()
    } else {
        crate::logs::log_dir().map(|d| d.join(&n))
    };
    let Some(p) = target else {
        return Err("找不到日志路径".into());
    };
    crate::logs::open_path(&p)?;
    Ok(p.display().to_string())
}

#[tauri::command]
pub(crate) fn log_folder() -> Result<String, String> {
    let Some(d) = crate::logs::log_dir() else {
        return Err("找不到日志目录".into());
    };
    crate::logs::open_path(&d)?;
    Ok(d.display().to_string())
}

#[tauri::command]
pub(crate) fn log_clear(days: Option<i64>) -> serde_json::Value {
    let (files, bytes) = crate::logs::clear(days.unwrap_or(0));
    serde_json::json!({ "files": files, "bytes": bytes })
}

#[tauri::command]
pub(crate) fn log_rotate() -> Option<String> {
    crate::logs::rotate_if_needed().map(|p| p.display().to_string())
}

/* ------------------------------ 专注模式与打断统计 IPC ------------------------------ */

#[tauri::command]
pub(crate) fn focus_status() -> serde_json::Value {
    serde_json::to_value(crate::focus::status()).unwrap_or_default()
}

#[tauri::command]
pub(crate) fn focus_start(app: tauri::AppHandle, minutes: Option<i64>) -> serde_json::Value {
    let ev = crate::focus::start(minutes);
    let _ = app.emit("wm:focus", &ev);
    serde_json::to_value(ev).unwrap_or_default()
}

#[tauri::command]
pub(crate) fn focus_stop(app: tauri::AppHandle) -> serde_json::Value {
    let ev = crate::focus::stop();
    let _ = app.emit("wm:focus", &ev);
    if !ev.deferred_ids.is_empty() {
        flush_focus_deferred_alert(&app, &ev.deferred_ids);
    }
    serde_json::to_value(ev).unwrap_or_default()
}

/* ------------------------------ 独立任务提示弹出窗面板（alert 窗口） ------------------------------ */

fn last_alert_cell() -> &'static std::sync::Mutex<serde_json::Value> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<serde_json::Value>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(serde_json::Value::Null))
}

/// 将独立任务提示弹出窗定位在桌宠窗口旁侧（优先左侧偏上，空间不足则右侧，并做屏幕边界夹取）
fn place_alert_beside_pet(app: &tauri::AppHandle, alert: &tauri::WebviewWindow) {
    let Some(pet) = app.get_webview_window("pet") else {
        return;
    };
    let (Ok(pp), Ok(ps), Ok(az)) = (pet.outer_position(), pet.outer_size(), alert.outer_size()) else {
        return;
    };

    let monitor = pet.current_monitor().ok().flatten();
    let (mx, my, mw, mh) = match monitor {
        Some(m) => {
            let p = *m.position();
            let s = *m.size();
            (p.x, p.y, s.width as i32, s.height as i32)
        }
        None => (0, 0, 1920, 1080),
    };

    let gap = 10;
    let mut x = pp.x - az.width as i32 - gap;
    if x < mx + 8 {
        x = pp.x + ps.width as i32 + gap;
    }
    // 与桌宠底部对齐并略微上提，使视觉重心与宠物平齐
    let mut y = pp.y + ps.height as i32 - az.height as i32 - 12;
    if y < my + 8 {
        y = my + 8;
    }
    if y + az.height as i32 > my + mh - 8 {
        y = my + mh - az.height as i32 - 8;
    }
    if x + az.width as i32 > mx + mw - 8 {
        x = mx + mw - az.width as i32 - 8;
    }
    let _ = alert.set_position(tauri::PhysicalPosition::new(x, y));
}

/// 唤出独立任务提示弹出窗并推送最新提醒数据（不抢占键盘焦点）
pub(crate) fn push_alert_window(app: &tauri::AppHandle, payload: serde_json::Value) {
    if let Ok(mut g) = last_alert_cell().lock() {
        *g = payload.clone();
    }
    if let Some(w) = app.get_webview_window("alert") {
        place_alert_beside_pet(app, &w);
        let _ = w.show();
    }
    let _ = app.emit("wm:alert", &payload);
}

/// 专注结束（满 50 分钟或手动停止）时，将期间拦截暂存的软提醒置为已提醒状态并推送至独立任务提示窗
pub(crate) fn flush_focus_deferred_alert(app: &tauri::AppHandle, deferred_ids: &[i64]) {
    if deferred_ids.is_empty() {
        return;
    }
    let now = now_sec();
    let all = crate::db::with_conn(db::repo::list_reminders).unwrap_or_default();
    let mut items = Vec::new();
    let mut valid_ids = Vec::new();
    for r in all {
        if deferred_ids.contains(&r.id) && r.status != 2 {
            let _ = crate::db::with_conn(|c| {
                c.execute(
                    "UPDATE reminders SET status = 1, fired_at = COALESCE(fired_at, ?2) WHERE id = ?1",
                    rusqlite::params![r.id, now],
                )
            });
            let _ = crate::db::with_conn(|c| {
                db::repo::log_task_event(c, Some(r.id), "reminded", None, "专注结束汇总播报")
            });
            let meta = crate::notify::brief_with_tz(
                now,
                r.fire_at,
                r.duration_min,
                r.priority,
                &r.note,
                r.tz_offset_min,
            );
            valid_ids.push(r.id);
            items.push(serde_json::json!({
                "id": r.id,
                "title": r.title,
                "meta": meta,
                "priority": r.priority,
                "kind": r.kind,
            }));
        }
    }
    if valid_ids.is_empty() {
        return;
    }
    let joined: Vec<String> = items
        .iter()
        .filter_map(|x| x["title"].as_str().map(String::from))
        .collect();
    let payload = serde_json::json!({
        "ids": valid_ids,
        "title": joined.join(" · "),
        "count": items.len(),
        "level": 0,
        "badge": format!("📬 专注期间暂存提醒 ({})", items.len()),
        "meta": "专注已完成，以下非紧急提醒在专注期为你静音暂存",
        "warm": "趁起来喝水走动的间隙，顺手勾掉已完成的事项吧～",
        "items": items,
        "focus_digest": true,
        "tz": crate::tz::active(),
        "ts": now,
    });
    push_alert_window(app, payload);
}

#[tauri::command]
pub(crate) fn alert_current() -> serde_json::Value {
    last_alert_cell()
        .lock()
        .map(|g| g.clone())
        .unwrap_or(serde_json::Value::Null)
}

#[tauri::command]
pub(crate) fn alert_close(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("alert") {
        let _ = w.hide();
    }
}

#[tauri::command]
pub(crate) fn alert_complete(
    app: tauri::AppHandle,
    db: tauri::State<db::Db>,
    ids: Vec<i64>,
) -> Result<usize, String> {
    let mut done_cnt = 0usize;
    for id in &ids {
        if db.with(|c| db::repo::set_reminder_status(c, *id, 2)).unwrap_or(false) {
            let _ = db.with(|c| db::repo::log_task_event(c, Some(*id), "done", None, "任务提示窗完成"));
            done_cnt += 1;
        }
    }
    let _ = app.emit("wm:reminders-changed", ());
    if done_cnt > 0 {
        let msg = if done_cnt == 1 {
            "✅ 已标为完成".to_string()
        } else {
            format!("✅ 已完成 {done_cnt} 项待办")
        };
        let _ = app.emit("wm:answer", &msg);
    }
    Ok(done_cnt)
}

#[tauri::command]
pub(crate) fn alert_snooze(
    app: tauri::AppHandle,
    db: tauri::State<db::Db>,
    ids: Vec<i64>,
    minutes: Option<i64>,
) -> Result<usize, String> {
    let mins = minutes.unwrap_or(15).clamp(1, 24 * 60);
    let defer_to = now_sec() + mins * 60;
    let mut cnt = 0usize;
    for id in &ids {
        if db.with(|c| db::repo::rearm_reminder(c, *id, defer_to)).is_ok() {
            let _ = db.with(|c| {
                db::repo::log_task_event(
                    c,
                    Some(*id),
                    "deferred",
                    Some(mins * 60),
                    "任务提示窗手动顺延",
                )
            });
            cnt += 1;
        }
    }
    let _ = app.emit("wm:reminders-changed", ());
    if cnt > 0 {
        let msg = format!("⏳ 已顺延 {mins} 分钟后再提醒你");
        let _ = app.emit("wm:answer", &msg);
    }
    Ok(cnt)
}

