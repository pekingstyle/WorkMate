// WorkMate · AI 记忆层
// 编制：WorkMate Contributors
//
// 目的：让「问一句就能答」这件事有可靠的事实底座。
//
// 用户会问的四类问题，对应四张表（schema v6）：
//   · 我最近去哪出差了 / 某天我在哪儿   → locations + day_profile.location
//   · 我手上还有什么事 / 什么逾期了     → reminders + task_log
//   · 我一般几点开始干活 / 专注多久     → day_profile
//   · 现在有什么要注意的               → risks
//
// 关键设计：**AI 只负责把事实说成人话，不负责生成事实**。
// 事实全部由 SQL 算出来，拼成一段"事实清单"塞进提示词，并明确要求模型
// 「只能用清单里的信息回答，没提到就说不知道」。
// 这样做的原因很实在：模型编造一个出差城市，用户根本无从分辨；
// 而 SQL 算错了是可复现、可修的。
//
// 没有配模型时依然可用：直接返回结构化事实文本（fact-only 模式）。

use serde_json::json;

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn bj_date(ts: i64) -> String {
    crate::logs::bj_date(ts)
}

/// 北京时间的当前时刻文本（给提示词当"现在"）
fn bj_now() -> String {
    crate::notify::bj_now_text()
}

/* ============================ 每日画像 ============================ */

/// 重算某一天（北京时间）的画像并 UPSERT。
///
/// 数据源刻意只用 rollup_5m：明细只留 2 天，用它算昨天的画像会得到 0，
/// 而"昨天一片空白"比"没算过"更容易被误读成真的没干活。
pub fn build_day(day: &str) -> Result<crate::db::repo::DayProfile, String> {
    // 北京时间该日的 [起, 止)。日期非法直接报错，不猜"今天"
    let start = day_start_ts(day).ok_or_else(|| format!("日期格式非法：{day}"))?;
    let end = start + 86_400;

    let p = crate::db::with_conn(|c| {
        // 先把该日范围内尚未聚合的明细补进 rollup_5m，保证打开「记忆」页时实时可见最新活跃度
        let _ = crate::db::repo::rollup_range(c, start, end);

        let (active_sec, rollup_first, rollup_last) = c
            .query_row(
                "SELECT COALESCE(sum(focus_sec), 0), min(bucket), max(bucket)
                 FROM rollup_5m WHERE bucket >= ?1 AND bucket < ?2 AND focus_sec > 0",
                rusqlite::params![start, end],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?)),
            )
            .unwrap_or((0, None, None));

        // 若当天尚有未过期的 5s 明细（近 2 天），优先取精确到秒的首末活跃采样；
        // 更早的历史日则回落到 5 分钟聚合桶（首桶起点 → 末桶终点）。
        // 注意：前一天熬夜跨过 00:00（00:00 ~ 05:00）的残留采样不能算作当天早晨的「开始工作时间」，
        // 因此优先取当地凌晨 05:00（dawn = start + 5 * 3600）之后的首个活跃点。
        let dawn = start + 5 * 3600;
        let (ev_first, ev_last): (Option<i64>, Option<i64>) = c
            .query_row(
                "SELECT min(ts), max(ts) FROM events
                 WHERE ts >= ?1 AND ts < ?2 AND idle_s < 60",
                rusqlite::params![start, end],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((None, None));
        let ev_first_daytime: Option<i64> = c
            .query_row(
                "SELECT min(ts) FROM events
                 WHERE ts >= ?1 AND ts < ?2 AND idle_s < 60",
                rusqlite::params![dawn, end],
                |r| r.get(0),
            )
            .unwrap_or(None);
        let rollup_first_daytime: Option<i64> = c
            .query_row(
                "SELECT min(bucket) FROM rollup_5m
                 WHERE bucket >= ?1 AND bucket < ?2 AND focus_sec > 0",
                rusqlite::params![dawn, end],
                |r| r.get(0),
            )
            .unwrap_or(None);
        let first_active = ev_first_daytime
            .or(rollup_first_daytime)
            .or(ev_first)
            .or(rollup_first);
        let last_active = ev_last.or_else(|| rollup_last.map(|b| b + 300));

        // 分类分布：{"0":秒,"1":秒,...}
        let mut cats: Vec<(i64, i64)> = {
            let mut st = c.prepare(
                "SELECT app_cat, sum(focus_sec) FROM rollup_5m
                 WHERE bucket >= ?1 AND bucket < ?2 AND focus_sec > 0
                 GROUP BY app_cat ORDER BY 2 DESC",
            )?;
            // 必须先把 query_map 的结果绑到具名变量：直接把
            // `st.query_map(..)?.collect()?` 当块尾表达式，MappedRows 这个临时值
            // 会活到块结束、晚于 st 被 drop（E0597，rustc 会明确提示）
            let rows = st.query_map(rusqlite::params![start, end], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        cats.truncate(8);
        let cat_json = serde_json::to_string(
            &cats.iter().map(|(k, v)| (k.to_string(), *v)).collect::<std::collections::BTreeMap<_, _>>(),
        )
        .unwrap_or_else(|_| "{}".into());

        // 最长连续专注：events.focus_min 是瞬时值，取 max 才有意义（不能 sum）。
        // 与已存的画像取 max 而不是直接覆盖：build_day 是幂等重算，
        // 若直接覆盖，重跑一次会把历史高水位冲掉（明细已过期、查不到旧值）。
        let ev_max_min: i64 = c
            .query_row(
                "SELECT COALESCE(max(focus_min), 0) FROM events WHERE ts >= ?1 AND ts < ?2",
                rusqlite::params![start, end],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let hist_max_sec: i64 = c
            .query_row(
                "SELECT COALESCE(focus_max_sec, 0) FROM day_profile WHERE day = ?1",
                [day],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let focus_max_sec = (ev_max_min * 60).max(hist_max_sec);

        let remind_cnt: i64 = c
            .query_row(
                "SELECT count(*) FROM reminders WHERE kind = 0 AND fired_at >= ?1 AND fired_at < ?2",
                rusqlite::params![start, end],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let done_cnt: i64 = c
            .query_row(
                "SELECT count(*) FROM reminders WHERE kind = 0 AND done_at >= ?1 AND done_at < ?2",
                rusqlite::params![start, end],
                |r| r.get(0),
            )
            .unwrap_or(0);

        // 打断次数来自 settings（每打断一次 +1），这里读回来落进画像
        let interrupt_cnt = crate::db::repo::read_interrupt(c, day).unwrap_or(0);

        let p = crate::db::repo::DayProfile {
            day: day.to_string(),
            first_active,
            last_active,
            active_sec,
            focus_max_sec,
            cat_json,
            remind_cnt,
            done_cnt,
            location: String::new(),
            interrupt_cnt,
        };
        crate::db::repo::upsert_day_profile(c, &p)?;
        // 顺手清掉过期计数键，settings 不该每天留一个垃圾 key
        let _ = crate::db::repo::purge_interrupt_keys(c, 40);
        Ok(p)
    })
    .map_err(|e| e.to_string())?;
    Ok(p)
}

/// "YYYY-MM-DD" → 该日当前生效城市时区零点的时间戳。
/// 格式非法返回 None——**绝不能静默回落到"今天"**：
/// 用户问「10月8号我在哪」，解析失败却按今天查，会把答案整个答错还毫无异常迹象。
fn day_start_ts(day: &str) -> Option<i64> {
    let parts: Vec<i64> = day
        .split('-')
        .filter_map(|s| s.parse::<i64>().ok())
        .collect();
    if parts.len() != 3 {
        return None;
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // 民用历 → 1970 起天数（与 logs.rs 同一套算法，不引 chrono）
    let my = if m <= 2 { y - 1 } else { y };
    let mp = if m <= 2 { m + 9 } else { m - 3 };
    let era = my.div_euclid(400);
    let yoe = my.rem_euclid(400);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 - crate::tz::offset_secs())
}

/// 补算最近 N 天（含今天）。启动时与每日维护各跑一次。
pub fn rebuild_recent(days: i64) -> usize {
    let mut n = 0;
    // 从今天往回数。bj_day_start() 本身就是北京时间零点，
    // 直接换算成日期串即可——早先的 `bj_date(ts + 60)` 是想防"零点前 1 秒"，
    // 但 ts 来自 bj_day_start() 必然落在当天，+60 纯属多余。
    let today_start = crate::notify::bj_day_start();
    for i in 0..days {
        let day = bj_date(today_start - i * 86_400);
        if build_day(&day).is_ok() {
            n += 1;
        }
    }
    n
}

/* ============================ 每日日记（后端每天调一次模型） ============================
 *
 * 用户要的不是流水账，是「今天做了多少事、被打断几次、去过哪、大概干了什么」。
 * 因此：先由 SQL 把当天压成一个**事实口袋**，再让模型写成一段自然语言。
 * 模型只做措辞，不做统计——统计错了没法查，措辞错了无所谓。
 *
 * 模型不可用时用规则兜底：宁可写得干巴，也不能当天没记录（断档后无法补写感受）。
 */

/// 一天的原始素材
struct DayFacts {
    day: String,
    /// 当天的感知采样条数。**这是判断「有没有数据」的唯一依据**：
    /// active_min=0 既可能是"人不在"，也可能是"程序没运行"，
    /// 不区分就会让模型写出「你今天几乎没坐在屏幕前」这种假结论。
    samples: i64,
    active_min: i64,
    focus_max_min: i64,
    interrupt_cnt: i64,
    created: i64,
    done: i64,
    overdue: i64,
    places: Vec<String>,
    cats: Vec<(String, i64)>,
    done_titles: Vec<String>,
    open_titles: Vec<String>,
}

fn collect_day(day: &str) -> DayFacts {
    // 调用链（write_daily ← 命令/巡检）只传内部生成的合法日期；
    // 万一非法，按"全天无数据"处理而不是 panic 或猜今天。
    let Some(start) = day_start_ts(day) else {
        return default_facts(day);
    };
    let end = start + 86_400;
    let now = now_sec();

    // 先重算画像，保证读到的是最新口径
    let _ = build_day(day);

    crate::db::with_conn(|c| {
        let p: crate::db::repo::DayProfile = c
            .query_row(
                "SELECT day, first_active, last_active, active_sec, focus_max_sec,
                        COALESCE(cat_json,''), remind_cnt, done_cnt, COALESCE(location,''),
                        COALESCE(interrupt_cnt,0)
                 FROM day_profile WHERE day = ?1",
                [day],
                |r| {
                    Ok(crate::db::repo::DayProfile {
                        day: r.get(0)?,
                        first_active: r.get(1)?,
                        last_active: r.get(2)?,
                        active_sec: r.get(3)?,
                        focus_max_sec: r.get(4)?,
                        cat_json: r.get(5)?,
                        remind_cnt: r.get(6)?,
                        done_cnt: r.get(7)?,
                        location: r.get(8)?,
                        interrupt_cnt: r.get(9)?,
                    })
                },
            )
            .unwrap_or(crate::db::repo::DayProfile {
                day: day.to_string(),
                ..Default::default()
            });

        let mut cats: Vec<(String, i64)> = serde_json::from_str::<std::collections::BTreeMap<String, i64>>(&p.cat_json)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (cat_label(k.parse::<i64>().unwrap_or(6)).to_string(), v))
            .collect();
        cats.sort_by_key(|c| std::cmp::Reverse(c.1));

        let places: Vec<String> = {
            let tz_mod = crate::tz::sqlite_modifier();
            let mut st = c.prepare(
                "SELECT DISTINCT COALESCE(label,'') FROM locations
                 WHERE substr(date(ts,'unixepoch',?2),1,10) = ?1
                   AND COALESCE(label,'') <> ''",
            )?;
            let rows = st.query_map(rusqlite::params![day, tz_mod], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let done_titles: Vec<String> = {
            let mut st = c.prepare(
                "SELECT title FROM reminders WHERE kind = 0 AND done_at >= ?1 AND done_at < ?2 ORDER BY done_at",
            )?;
            let rows = st.query_map(rusqlite::params![start, end], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let created: i64 = c.query_row(
            "SELECT count(*) FROM reminders WHERE kind = 0 AND created_at >= ?1 AND created_at < ?2",
            rusqlite::params![start, end],
            |r| r.get(0),
        ).unwrap_or(0);
        let open_titles: Vec<String> = {
            let mut st = c.prepare("SELECT title FROM reminders WHERE kind = 0 AND status IN (0,1) LIMIT 8")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        Ok(DayFacts {
            day: day.to_string(),
            samples: c
                .query_row(
                    "SELECT count(*) FROM events WHERE ts >= ?1 AND ts < ?2",
                    rusqlite::params![start, end],
                    |r| r.get(0),
                )
                .unwrap_or(0),
            active_min: (p.active_sec as f64 / 60.0).round() as i64,
            focus_max_min: (p.focus_max_sec as f64 / 60.0).round() as i64,
            interrupt_cnt: p.interrupt_cnt,
            created,
            done: p.done_cnt,
            overdue: crate::db::repo::overdue_count(c, now).unwrap_or(0),
            places,
            cats,
            done_titles,
            open_titles,
        })
    })
    .unwrap_or_else(|_| default_facts(day))
}

fn default_facts(day: &str) -> DayFacts {
    DayFacts {
        day: day.to_string(),
        samples: 0,
        active_min: 0,
        focus_max_min: 0,
        interrupt_cnt: 0,
        created: 0,
        done: 0,
        overdue: 0,
        places: vec![],
        cats: vec![],
        done_titles: vec![],
        open_titles: vec![],
    }
}

/// 把一天压成给模型的输入（几十个字，不是流水账）
fn day_facts_text(f: &DayFacts) -> String {
    let cats = f
        .cats
        .iter()
        .take(3)
        .map(|(k, _)| k.clone())
        .collect::<Vec<_>>()
        .join("、");
    // 没有采样时**绝不能**输出「活跃 0 分钟」——模型会把它读成「你今天没干活」。
    // 真实原因通常只是程序没运行。缺失必须说成缺失。
    let activity = if f.samples == 0 {
        "活动数据：当天没有采集到任何采样（程序未运行或未启用）——\
         禁止据此评价忙闲、专注、作息，只写任务相关的事实"
            .to_string()
    } else if f.samples < 36 {
        format!(
            "活动数据：只采集到 {n} 条采样（不足 3 分钟）——不要据此判断当天忙闲",
            n = f.samples
        )
    } else {
        format!(
            "活跃：{active} 分钟（最长连续专注 {focus} 分钟，被打断 {intr} 次；共 {n} 条采样）",
            active = f.active_min,
            focus = f.focus_max_min,
            intr = f.interrupt_cnt,
            n = f.samples
        )
    };
    format!(
        "日期：{day}\n{activity}\n\
         任务：新建 {created} 件、完成 {done} 件；当前逾期 {overdue} 件\n\
         地点：{places}\n主要活动类别：{cats}\n完成的事：{done_t}\n还挂着：{open_t}",
        day = f.day,
        activity = activity,
        created = f.created,
        done = f.done,
        overdue = f.overdue,
        places = if f.places.is_empty() { "无记录".into() } else { f.places.join("、") },
        cats = if cats.is_empty() { "无记录".into() } else { cats },
        done_t = if f.done_titles.is_empty() { "无".into() } else { f.done_titles.join("、") },
        open_t = if f.open_titles.is_empty() { "无".into() } else { f.open_titles.join("、") },
    )
}

/// 规则兜底的日记（模型不可用 / 没配模型时）
fn rule_summary(f: &DayFacts) -> String {
    // 用采样条数判断「有没有数据」，不是用活跃分钟：
    // active=0 既可能是人不在，也可能是程序没运行，两者不能混为一谈。
    if f.samples == 0 {
        let mut s = format!("{}：没有采集到活动记录（程序未运行或未启用）", f.day);
        if f.done > 0 {
            s.push_str(&format!("；完成 {} 件", f.done));
            if !f.done_titles.is_empty() {
                s.push_str(&format!("：{}", f.done_titles.join("、")));
            }
        }
        s.push('。');
        return s;
    }
    let place = if f.places.is_empty() {
        String::new()
    } else {
        format!("主要在{}，", f.places.join("、"))
    };
    let focus = if f.focus_max_min >= 45 {
        format!("有一段 {} 分钟的深度专注，", f.focus_max_min)
    } else {
        String::new()
    };
    let intr = if f.interrupt_cnt >= 5 {
        format!("但被打断了 {} 次，节奏比较碎；", f.interrupt_cnt)
    } else if f.interrupt_cnt > 0 {
        format!("中途被打断 {} 次；", f.interrupt_cnt)
    } else {
        String::new()
    };
    let task = if f.done > 0 {
        format!("完成了 {} 件事", f.done)
    } else {
        "没有标记完成的事项".to_string()
    };
    format!(
        "{day}：{place}活跃约 {active} 分钟。{focus}{intr}{task}，新建 {created} 条待办，尚有 {overdue} 件逾期。",
        day = f.day,
        place = place,
        active = f.active_min,
        focus = focus,
        intr = intr,
        task = task,
        created = f.created,
        overdue = f.overdue,
    )
}

/// 生成某一天的短期记忆。**每天只调一次模型**（调用方控制节奏）。
///
/// 返回写入的笔记。force=false 时若当天已有 short note 就跳过，
/// 避免「每 30 分钟巡检一次」变成一天调 48 次模型。
pub fn write_daily(day: &str, force: bool) -> serde_json::Value {
    if !force {
        let exists = crate::db::with_conn(|c| crate::db::repo::get_note(c, "short", day))
            .ok()
            .flatten();
        if exists.is_some() {
            return serde_json::json!({ "skipped": true, "day": day });
        }
    }

    let f = collect_day(day);
    let facts = day_facts_text(&f);

    let (summary, source, care) = {
        let cfg = crate::llm::load_file();
        if crate::llm::api_key(&cfg).is_some() && !cfg.model.trim().is_empty() {
            match crate::llm::daily_note(&cfg, &facts) {
                Ok((s, c)) if !s.trim().is_empty() => (s, "ai", c),
                Ok(_) => (rule_summary(&f), "rule", String::new()),
                Err(e) => {
                    crate::startup_log(&format!("daily note llm failed: {e}"));
                    (rule_summary(&f), "rule", String::new())
                }
            }
        } else {
            (rule_summary(&f), "rule", String::new())
        }
    };

    let detail = serde_json::json!({
        "active_min": f.active_min,
        "focus_max_min": f.focus_max_min,
        "interrupt_cnt": f.interrupt_cnt,
        "created": f.created,
        "done": f.done,
        "overdue": f.overdue,
        "places": f.places,
        "care": care,
    });
    let weight = if f.done > 3 || f.focus_max_min >= 90 || f.overdue >= 2 {
        4
    } else if f.active_min < 30 {
        2
    } else {
        3
    };

    let id = crate::db::with_conn(|c| {
        crate::db::repo::upsert_note(c, "short", day, source, &summary, &detail.to_string(), weight)
    });
    crate::startup_log(&format!("memory: short note {} via {} ({})", day, source, id.is_ok()));

    serde_json::json!({
        "day": day,
        "source": source,
        "summary": summary,
        "care": care,
        "weight": weight,
        "facts": facts,
    })
}

/* ============================ 分层抽象（short → mid → long） ============================ */

const MID_SYSTEM_HINT: &str = "下面是用户最近若干天的每日记录。请压成**月度小结**：\
丢掉具体日期与时刻，只保留——本月重点在做什么、精力是否集中、身体状态如何、\
作息是否规律、值得记住的事。用简体中文，不超过 150 字，不要分条列点，写成连贯的一段。\
用语必须是平实规范的书面中文：不要生造词、不要网络口语或俚语（如「很松」「很顶」「拿捏」）。";

const LONG_SYSTEM_HINT: &str = "下面是用户若干个月的小结。请再压缩成**不超过两句话**的长期印象：\
只留这个阶段最稳定的特征（长期在做的事、一贯的作息与精力水平、长期风险）。\
用简体中文，不要出现具体日期，也不要再分点。\
用语必须是平实规范的书面中文：不要生造词、不要网络口语或俚语。";

/// 把某个自然月的 short 压成 mid。period 形如 "2026-10"。
///
/// 抽象掉时间戳这件事是这个阶段的**全部意义**：一个月后还留着
/// "10月8日 14:32 完成 X" 没有任何价值，只会稀释检索与上下文。
pub fn rollup_mid(month: &str) -> serde_json::Value {
    let from = format!("{month}-01");
    let to = format!("{month}-31");
    let notes = crate::db::with_conn(|c| crate::db::repo::notes_between(c, "short", &from, &to))
        .unwrap_or_default();
    if notes.is_empty() {
        return serde_json::json!({ "skipped": true, "reason": "该月没有日记", "month": month });
    }

    // mid 的输入是 short 的**要点**而不是全文：全文堆叠会让模型照抄流水
    let input: String = notes
        .iter()
        .map(|n| {
            let d: serde_json::Value =
                serde_json::from_str(&n.detail).unwrap_or(serde_json::json!({}));
            format!(
                "- 活跃{}分钟/最长专注{}分钟/打断{}次/完成{}件，地点：{}",
                d["active_min"].as_i64().unwrap_or(0),
                d["focus_max_min"].as_i64().unwrap_or(0),
                d["interrupt_cnt"].as_i64().unwrap_or(0),
                d["done"].as_i64().unwrap_or(0),
                d["places"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("、"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let (summary, source) = {
        let cfg = crate::llm::load_file();
        if crate::llm::api_key(&cfg).is_some() && !cfg.model.trim().is_empty() {
            match crate::llm::abstract_chat(&cfg, MID_SYSTEM_HINT, &input) {
                Ok(s) if !s.trim().is_empty() => (s, "ai"),
                _ => (rule_mid(&notes), "rule"),
            }
        } else {
            (rule_mid(&notes), "rule")
        }
    };

    let days = notes.len() as i64;
    let id = crate::db::with_conn(|c| {
        crate::db::repo::upsert_note(
            c,
            "mid",
            month,
            source,
            &summary,
            &serde_json::json!({ "days": days }).to_string(),
            4,
        )
    });
    // 原件已被吸收：短期窗口只留 7 天，更早的删掉（用户定的口径）
    let _ = crate::db::with_conn(|c| crate::db::repo::purge_notes(c, "short", &from));
    crate::startup_log(&format!("memory: mid {} from {} days via {}", month, days, source));
    serde_json::json!({ "month": month, "days": days, "source": source, "summary": summary, "id": id.is_ok() })
}

fn rule_mid(notes: &[crate::db::repo::Note]) -> String {
    let mut active = 0i64;
    let mut done = 0i64;
    let mut intr = 0i64;
    let mut focus_max = 0i64;
    let mut places: Vec<String> = Vec::new();
    for n in notes {
        let d: serde_json::Value = serde_json::from_str(&n.detail).unwrap_or(serde_json::json!({}));
        active += d["active_min"].as_i64().unwrap_or(0);
        done += d["done"].as_i64().unwrap_or(0);
        intr += d["interrupt_cnt"].as_i64().unwrap_or(0);
        focus_max = focus_max.max(d["focus_max_min"].as_i64().unwrap_or(0));
        if let Some(a) = d["places"].as_array() {
            for p in a.iter().filter_map(|x| x.as_str()) {
                if !places.contains(&p.to_string()) {
                    places.push(p.to_string());
                }
            }
        }
    }
    let n = notes.len().max(1) as i64;
    format!(
        "这个月有记录 {} 天：日均活跃约 {} 分钟，共完成 {} 件事，累计被打断 {} 次，最长一段专注 {} 分钟。去过：{}。",
        notes.len(),
        (active as f64 / n as f64).round() as i64,
        done,
        intr,
        focus_max,
        if places.is_empty() { "无记录".into() } else { places.join("、") }
    )
}

/// 把一个季度的 mid 压成长期印象（一两句话）。period 形如 "2026-Q3"。
pub fn rollup_long(quarter: &str, months: &[String]) -> serde_json::Value {
    let mut parts: Vec<String> = Vec::new();
    for m in months {
        if let Ok(Some(n)) = crate::db::with_conn(|c| crate::db::repo::get_note(c, "mid", m)) {
            parts.push(n.summary);
        }
    }
    if parts.is_empty() {
        return serde_json::json!({ "skipped": true, "reason": "该季度没有月度小结" });
    }
    let input = parts.join("\n");
    let (summary, source) = {
        let cfg = crate::llm::load_file();
        if crate::llm::api_key(&cfg).is_some() && !cfg.model.trim().is_empty() {
            match crate::llm::abstract_chat(&cfg, LONG_SYSTEM_HINT, &input) {
                Ok(s) if !s.trim().is_empty() => (s, "ai"),
                _ => (rule_long(&input), "rule"),
            }
        } else {
            (rule_long(&input), "rule")
        }
    };
    let id = crate::db::with_conn(|c| {
        crate::db::repo::upsert_note(
            c,
            "long",
            quarter,
            source,
            &summary,
            &serde_json::json!({ "months": months }).to_string(),
            5,
        )
    });
    // mid 已被长期印象吸收且超出 3 个月窗口 → 清掉原件
    if let Some(first) = months.first() {
        let _ = crate::db::with_conn(|c| crate::db::repo::purge_notes(c, "mid", first));
    }
    crate::startup_log(&format!("memory: long {} from {} months via {}", quarter, months.len(), source));
    serde_json::json!({ "quarter": quarter, "months": months.len(), "source": source, "summary": summary, "id": id.is_ok() })
}

fn rule_long(input: &str) -> String {
    let first = input.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        "这一阶段没有足够的记录可以总结。".to_string()
    } else {
        format!("长期印象：{}（更细的月度记录已被压缩）。", first.chars().take(60).collect::<String>())
    }
}

/// 定期抽象的总入口：**每天跑一次**，内部自己判断该压哪一层。
///
/// 判定规则（按北京时间自然月/季度）：
///   · 每月 1–3 号：压上个月 → mid
///   · 每月 1–3 号且当前月是季度首月：压上一个季度 → long
/// 幂等：同一 period 重复生成只是覆盖，不会堆多条。
pub fn rollup_notes() -> serde_json::Value {
    let today = crate::logs::bj_date(now_sec());
    let (y, m): (i64, i64) = {
        let p: Vec<i64> = today.split('-').filter_map(|s| s.parse().ok()).collect();
        if p.len() < 2 {
            return serde_json::json!({ "skipped": true });
        }
        (p[0], p[1])
    };
    // 只在月初做抽象：月中反复压既浪费模型调用，也会让 mid 一直被半成品覆盖
    let day_of_month: i64 = today
        .split('-')
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if day_of_month > 3 {
        return serde_json::json!({ "skipped": true, "reason": "非月初，不做抽象" });
    }

    let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    let last_month = format!("{py}-{pm:02}");
    let mid = rollup_mid(&last_month);

    let long = if m == 1 || m == 4 || m == 7 || m == 10 {
        let (qy, q) = if m == 1 { (y - 1, 4) } else { (y, (m - 1) / 3) };
        let months: Vec<String> = (0..3)
            .map(|i| {
                let mm = (q - 1) * 3 + 1 + i;
                format!("{qy}-{mm:02}")
            })
            .collect();
        rollup_long(&format!("{qy}-Q{q}"), &months)
    } else {
        serde_json::json!({ "skipped": true })
    };

    serde_json::json!({ "mid": mid, "long": long })
}

/* ============================ 温度提示 ============================ */

/// 给提醒配一句「有温度」的话。
///
/// 规则生成而不是每次调模型：提醒是实时路径，多等 1–2 秒的体感很差，
/// 而且一天弹十几次就要调十几次模型。模型的角色是**每天写日记时产出 care**
/// （对明天的关照），这里只做「选哪句 + 结合当前事实」。
pub fn warm_hint() -> String {
    let now = now_sec();
    let today = crate::logs::bj_date(now);

    // 1) 先看今天的实时危险信号——优先级最高
    let focus_min = crate::db::with_conn(|c| {
        c.query_row(
            "SELECT COALESCE(max(focus_min),0) FROM events WHERE ts >= ?1",
            [crate::notify::bj_day_start()],
            |r| r.get::<_, i64>(0),
        )
    })
    .unwrap_or(0);
    let (done_today, overdue, open_cnt) = crate::db::with_conn(|c| {
        Ok((
            crate::db::repo::done_since(c, crate::notify::bj_day_start())?,
            crate::db::repo::overdue_count(c, now)?,
            crate::db::repo::open_task_count(c)?,
        ))
    })
    .unwrap_or((0, 0, 0));

    let hour = {
        let s = (now + crate::tz::offset_secs()).rem_euclid(86_400);
        s / 3600
    };

    if !(6..22).contains(&hour) {
        return "已经很晚了，这件事明天做也不迟。".to_string();
    }
    if focus_min >= 90 {
        return format!("你已经连着专注 {} 分钟了，起来走两步再继续。", focus_min);
    }
    if overdue >= 3 {
        return format!("有 {} 件事拖着了，先挑最小的一件做掉。", overdue);
    }
    if focus_min >= 45 {
        return "刚结束一段深度专注，正好顺手把它处理掉。".to_string();
    }

    // 2) 实时信号都不触发时，用最近日记里模型留下的 care
    if let Ok(Some(n)) = crate::db::with_conn(|c| crate::db::repo::get_note(c, "short", &today)) {
        let d: serde_json::Value = serde_json::from_str(&n.detail).unwrap_or(serde_json::json!({}));
        if let Some(care) = d["care"].as_str() {
            if !care.trim().is_empty() {
                return care.trim().to_string();
            }
        }
    }
    if let Ok(notes) = crate::db::with_conn(|c| crate::db::repo::list_notes(c, "short", 3)) {
        for n in notes {
            let d: serde_json::Value =
                serde_json::from_str(&n.detail).unwrap_or(serde_json::json!({}));
            if let Some(care) = d["care"].as_str() {
                if !care.trim().is_empty() {
                    return care.trim().to_string();
                }
            }
        }
    }

    // 3) 兜底：按当天进展给一句中性的
    if done_today >= 3 {
        format!("今天已经完成 {} 件了，收个尾吧。", done_today)
    } else if open_cnt == 0 {
        "暂时没有别的事，安心做这件就好。".to_string()
    } else {
        "别忘了照顾一下自己。".to_string()
    }
}

/* ============================ 风险扫描 ============================ */

/// 仅扫描高优先级实时风险（L3：高优/批量待办逾期、当前连续专注/久坐 ≥ 120 分钟）。
/// 用于 30 分钟级巡检与定期主动提醒。
pub fn scan_high_risks() -> Vec<crate::db::repo::RiskRow> {
    let now = now_sec();

    // 1) 逾期：到点已弹、半小时还没标完成
    let (overdue, high_overdue) = crate::db::with_conn(|c| {
        let total = crate::db::repo::overdue_count(c, now).unwrap_or(0);
        let hi: i64 = c
            .query_row(
                "SELECT count(*) FROM reminders
                 WHERE kind = 0 AND status = 1 AND priority = 1
                   AND fired_at IS NOT NULL AND ?1 - fired_at > 1800",
                rusqlite::params![now],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok((total, hi))
    })
    .unwrap_or((0, 0));

    if overdue > 0 {
        let lvl = if overdue >= 3 || high_overdue >= 1 { 3 } else { 2 };
        let title = if high_overdue >= 1 {
            format!("{overdue} 件待办逾期未处理（含 {high_overdue} 件高优）")
        } else {
            format!("{overdue} 件待办逾期未处理")
        };
        let _ = crate::db::with_conn(|c| {
            crate::db::repo::replace_open_risk(
                c,
                now,
                "overdue",
                lvl,
                &title,
                "已提醒但超过 30 分钟仍未标记完成",
            )
        });
    } else {
        let _ = crate::db::with_conn(|c| crate::db::repo::clear_risk_kind(c, "overdue"));
    }

    // 2) 超长专注 / 连续久坐：当前仍在进行且 ≥120 分钟升为 L3 高优警告；今日历史峰值 ≥90 分钟为 L1 轻度提示
    let cur_sit = crate::perception::current_sit_mins();
    let focus_max = crate::db::with_conn(|c| {
        c.query_row(
            "SELECT COALESCE(max(focus_min), 0) FROM events WHERE ts >= ?1",
            [crate::notify::bj_day_start()],
            |r| r.get::<_, i64>(0),
        )
    })
    .unwrap_or(0)
    .max(cur_sit);

    if cur_sit >= 120 {
        let _ = crate::db::with_conn(|c| {
            crate::db::repo::replace_open_risk(
                c,
                now,
                "long_focus",
                3,
                &format!("当前已连续工作 {cur_sit} 分钟未休息"),
                "连续久坐超过 2 小时，建议立即起身活动 3–5 分钟",
            )
        });
    } else if focus_max >= 90 {
        let _ = crate::db::with_conn(|c| {
            crate::db::repo::replace_open_risk(
                c,
                now,
                "long_focus",
                1,
                &format!("今天最长连续专注 {focus_max} 分钟"),
                "超过 90 分钟未起身，注意劳逸结合",
            )
        });
    } else {
        let _ = crate::db::with_conn(|c| crate::db::repo::clear_risk_kind(c, "long_focus"));
    }

    crate::db::with_conn(|c| crate::db::repo::open_risks(c, 20)).unwrap_or_default()
}

/// 风险不是"记下来"的，是"算出来"的。每次调用重算一遍全部五类。
///
/// 分级策略：
///   - L3（高优警告）：高优任务逾期 / ≥3 件待办逾期 / 当前连续久坐 ≥120 分钟（支持定期主动提醒，避让演示模式）
///   - L2（中优注意）：1–2 件普通待办逾期 / ≥3 件待办被反复顺延 / 专注打断明显上升
///   - L1（轻度提示）：今日曾有 ≥90 分钟长专注 / 近 7 天 ≥3 天熬夜（低频巡检，仅面板展示，不主动弹窗打扰）
pub fn scan_risks() -> Vec<crate::db::repo::RiskRow> {
    let now = now_sec();
    let _ = scan_high_risks();

    // 3) 连续顺延（L2）：待办被反复挂回去，说明时间点定得不对（或一直在躲）
    let deferred = crate::db::with_conn(crate::db::repo::deferred_count).unwrap_or(0);
    if deferred >= 3 {
        let _ = crate::db::with_conn(|c| {
            crate::db::repo::replace_open_risk(
                c,
                now,
                "defer_chain",
                2,
                &format!("{deferred} 件待办被反复顺延"),
                "提醒时间可能定得不合适，建议改到真正会处理它的时段",
            )
        });
    } else {
        let _ = crate::db::with_conn(|c| crate::db::repo::clear_risk_kind(c, "defer_chain"));
    }

    // 4) 熬夜（L1 轻度习惯提示）：近 7 天里有 3 天以上在 23 点后还在活跃
    let late_days = crate::db::with_conn(late_night_days).unwrap_or(0);
    if late_days >= 3 {
        let _ = crate::db::with_conn(|c| {
            crate::db::repo::replace_open_risk(
                c,
                now,
                "late_night",
                1,
                &format!("近 7 天有 {late_days} 天在 23 点后仍在工作"),
                "长期熬夜会显著拉低次日专注时长",
            )
        });
    } else {
        let _ = crate::db::with_conn(|c| crate::db::repo::clear_risk_kind(c, "late_night"));
    }

    // 5) 专注打断趋势（L2）：近期被打断次数明显上升时给出提醒
    let trend = crate::focus::compute_interrupt_trend();
    if trend.direction == "up" && (trend.recent_avg >= 3.0 || trend.today_count >= 5) {
        let _ = crate::db::with_conn(|c| {
            crate::db::repo::replace_open_risk(
                c,
                now,
                "interrupt_up",
                2,
                &format!("近期专注打断次数呈上升趋势（+{}%）", trend.delta_pct),
                &trend.summary,
            )
        });
    } else {
        let _ = crate::db::with_conn(|c| crate::db::repo::clear_risk_kind(c, "interrupt_up"));
    }

    crate::db::with_conn(|c| crate::db::repo::open_risks(c, 20)).unwrap_or_default()
}

/// 近 7 天里「23 点后仍有活跃」的天数（用聚合桶判断，明细只留 2 天不够）
fn late_night_days(c: &rusqlite::Connection) -> rusqlite::Result<i64> {
    let mut n = 0i64;
    for i in 0..7 {
        let start = crate::notify::bj_day_start() - i * 86_400;
        // 该日 23:00（北京时间）对应的桶
        let late = start + 23 * 3600;
        let hit: i64 = c.query_row(
            "SELECT COALESCE(sum(focus_sec), 0) FROM rollup_5m
             WHERE bucket >= ?1 AND bucket < ?2",
            rusqlite::params![late, start + 86_400],
            |r| r.get(0),
        )?;
        if hit >= 600 {
            n += 1;
        }
    }
    Ok(n)
}

/* ============================ 摘要（给 AI 的上下文） ============================ */

/// 把记忆压缩成一段适合塞进提示词的结构化摘要。
///
/// 尺寸控制很重要：上下文越长越贵、越慢，也越容易被模型忽略。
/// 这里只给「结论级」字段，近期画像默认 7 天，行程默认 30 天。
pub fn brief(days: i64, trip_days: i64) -> serde_json::Value {
    let now = now_sec();
    let since_profile = now - days * 86_400;
    let since_loc = now - trip_days * 86_400;

    // 刷新今日画像：确保用户打开「记忆」页时看到的是实时最新统计（无需手动点重算）
    let _ = build_day(&bj_date(now));

    let profiles: Vec<crate::db::repo::DayProfile> = crate::db::with_conn(|c| {
        let all = crate::db::repo::day_profiles(c, days.max(1))?;
        Ok(all)
    })
    .unwrap_or_default();

    let habits = summarize_habits(&profiles);

    let tasks = crate::db::with_conn(|c| {
        let open = crate::db::repo::open_task_count(c).unwrap_or(0);
        let overdue = crate::db::repo::overdue_count(c, now).unwrap_or(0);
        let deferred = crate::db::repo::deferred_count(c).unwrap_or(0);
        let done_7d = crate::db::repo::done_since(c, since_profile).unwrap_or(0);
        let avg_response_min = crate::db::repo::avg_response_sec(c, since_profile)
            .ok()
            .flatten()
            .map(|s| (s as f64 / 60.0).round() as i64);
        Ok(json!({
            "open": open,
            "overdue": overdue,
            "deferred": deferred,
            "done_7d": done_7d,
            "avg_response_min": avg_response_min,
        }))
    })
    .unwrap_or_else(|_| {
        json!({
            "open": 0,
            "overdue": 0,
            "deferred": 0,
            "done_7d": 0,
            "avg_response_min": null,
        })
    });

    let risks: Vec<crate::db::repo::RiskRow> =
        crate::db::with_conn(|c| crate::db::repo::open_risks(c, 10)).unwrap_or_default();

    let home = crate::db::with_conn(|c| crate::db::repo::home_base(c, now - 90 * 86_400))
        .ok()
        .flatten();
    let trips: Vec<(String, String, i64)> =
        crate::db::with_conn(|c| crate::db::repo::location_days(c, since_loc, 60))
            .unwrap_or_default();

    // 任务生命周期流水：让 AI 能说清「这件事什么时候建的、提醒过没、多久才做」
    let events: Vec<crate::db::repo::TaskLogRow> =
        crate::db::with_conn(|c| crate::db::repo::task_log_since(c, now - 7 * 86_400, 40))
            .unwrap_or_default();

    let open_titles: Vec<String> = crate::db::with_conn(|c| {
        let mut st = c.prepare(
            "SELECT title FROM reminders WHERE kind = 0 AND status IN (0,1) ORDER BY fire_at ASC LIMIT 12",
        )?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .unwrap_or_default();

    json!({
        "now_bj": bj_now(),
        "home_base": home.clone().unwrap_or_default(),
        "habits": habits,
        "days": profiles.iter().map(profile_line).collect::<Vec<_>>(),
        "tasks": tasks,
        "open_titles": open_titles,
        "task_events": events.iter().map(|e| json!({
            "ts": e.ts,
            "action": e.action,
            "delta_min": e.delta_sec.map(|s| (s as f64 / 60.0).round() as i64),
            "title": e.note,
        })).collect::<Vec<_>>(),
        "risks": risks.iter().map(|r| json!({
            "level": r.level, "title": r.title, "kind": r.kind, "detail": r.detail
        })).collect::<Vec<_>>(),
        "trips": trips.iter().map(|(d, l, t)| json!({
            "day": d, "place": l, "trip": *t == 1
        })).collect::<Vec<_>>(),
    })
}

fn profile_line(p: &crate::db::repo::DayProfile) -> serde_json::Value {
    let mut categories: Vec<(i64, i64)> =
        serde_json::from_str::<std::collections::BTreeMap<String, i64>>(&p.cat_json)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(cat, secs)| cat.parse::<i64>().ok().map(|id| (id, secs)))
            .collect();
    categories.sort_by_key(|(_, secs)| std::cmp::Reverse(*secs));
    json!({
        "day": p.day,
        "start": p.first_active.map(bj_hm_of).unwrap_or_default(),
        "end": p.last_active.map(bj_hm_of).unwrap_or_default(),
        "active_min": (p.active_sec as f64 / 60.0).round() as i64,
        "focus_max_min": (p.focus_max_sec as f64 / 60.0).round() as i64,
        "interrupt_cnt": p.interrupt_cnt,
        "reminded": p.remind_cnt,
        "done": p.done_cnt,
        "place": p.location,
        "categories": categories.into_iter().take(8).map(|(cat, secs)| json!({
            "cat_id": cat,
            "cat": cat_label(cat),
            "secs": secs,
        })).collect::<Vec<_>>(),
    })
}

/// 判断 "YYYY-MM-DD" 是否为工作日（周一至周五）。
fn is_workday_str(day: &str) -> bool {
    let parts: Vec<i64> = day
        .split('-')
        .filter_map(|s| s.parse::<i64>().ok())
        .collect();
    if parts.len() != 3 {
        return true;
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let my = if m <= 2 { y - 1 } else { y };
    let mp = if m <= 2 { m + 9 } else { m - 3 };
    let era = my.div_euclid(400);
    let yoe = my.rem_euclid(400);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let dow0 = (days + 4).rem_euclid(7); // 0=周日, 1..5=周一..周五, 6=周六
    (1..=5).contains(&dow0)
}

/// 从若干天的画像里提炼「工作习惯」（仅统计工作日周一至周五，排除周末晚起对均值/中位数的污染）
fn summarize_habits(ps: &[crate::db::repo::DayProfile]) -> serde_json::Value {
    summarize_habits_with_offset(ps, crate::tz::offset_secs())
}

fn summarize_habits_with_offset(ps: &[crate::db::repo::DayProfile], tz_off: i64) -> serde_json::Value {
    let trend = crate::focus::compute_interrupt_trend();
    let all_live: Vec<&crate::db::repo::DayProfile> =
        ps.iter().filter(|p| p.active_sec > 0).collect();
    if all_live.is_empty() {
        return json!({
            "samples": 0,
            "workday_only": true,
            "interrupt_trend": trend,
        });
    }
    // 只用工作日（周一至周五）计算工作习惯；仅当完全没有任何工作日样本时才回退全部样本
    let workdays: Vec<&crate::db::repo::DayProfile> = all_live
        .iter()
        .copied()
        .filter(|p| is_workday_str(&p.day))
        .collect();
    let live: &[&crate::db::repo::DayProfile] = if workdays.is_empty() {
        &all_live
    } else {
        &workdays
    };

    let n = live.len() as f64;
    let avg_active = live.iter().map(|p| p.active_sec).sum::<i64>() as f64 / n;
    // 中位数比算术平均更适合作息边界：出差、临时离开和跨午夜会制造离群点，
    // 直接平均会把「下午才开工」或「下午暂时关机」误当成典型收工时间。
    let median_wall = |values: Vec<f64>| -> f64 {
        if values.is_empty() { return 0.0; }
        let mut v = values;
        v.sort_by(|a, b| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = v.len() / 2;
        if v.len() % 2 == 0 { (v[mid - 1] + v[mid]) / 2.0 } else { v[mid] }
    };
    // 过滤掉 00:00~05:00 跨午夜熬夜残留点，只统计 05:00 之后的早晨/白天开工时间
    let start_samples: Vec<f64> = live
        .iter()
        .filter_map(|p| p.first_active)
        .map(|t| (t + tz_off).rem_euclid(86_400) as f64)
        .filter(|&sod| sod >= 5.0 * 3600.0)
        .collect();
    let avg_start = if !start_samples.is_empty() {
        median_wall(start_samples)
    } else {
        median_wall(
            live.iter()
                .filter_map(|p| p.first_active)
                .map(|t| (t + tz_off).rem_euclid(86_400) as f64)
                .collect(),
        )
    };
    let avg_end = median_wall(live
        .iter()
        .filter_map(|p| p.last_active)
        .map(|t| (t + tz_off).rem_euclid(86_400) as f64)
        .collect());

    // 分类占比：把 cat_json 累加后取 Top3
    let mut agg: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
    for p in live {
        if let Ok(m) = serde_json::from_str::<std::collections::BTreeMap<String, i64>>(&p.cat_json) {
            for (k, v) in m {
                if let Ok(cat) = k.parse::<i64>() {
                    *agg.entry(cat).or_insert(0) += v;
                }
            }
        }
    }
    let total: i64 = agg.values().sum();
    let mut top: Vec<(i64, i64)> = agg.into_iter().collect();
    top.sort_by_key(|c| std::cmp::Reverse(c.1));
    let top3: Vec<serde_json::Value> = top
        .into_iter()
        .take(3)
        .map(|(c, s)| {
            json!({
                "cat_id": c,
                "cat": cat_label(c),
                "share_pct": if total > 0 { (s as f64 * 100.0 / total as f64).round() as i64 } else { 0 }
            })
        })
        .collect();

    json!({
        "samples": live.len(),
        "total_days": all_live.len(),
        "workday_only": !workdays.is_empty(),
        "avg_start": fmt_hm(avg_start),
        "avg_end": fmt_hm(avg_end),
        "avg_active_min": (avg_active / 60.0).round() as i64,
        "top_categories": top3,
        "interrupt_trend": trend,
    })
}

pub(crate) fn cat_label(c: i64) -> &'static str {
    match c {
        0 => "IDE/开发",
        1 => "Paper Work",
        2 => "浏览器",
        3 => "会议",
        4 => "即时通讯",
        5 => "影音",
        7 => "AI Agent",
        _ => "其他",
    }
}

fn bj_hm_of(ts: i64) -> String {
    fmt_hm((ts + crate::tz::offset_secs()).rem_euclid(86_400) as f64)
}

fn fmt_hm(sec_of_day: f64) -> String {
    let s = sec_of_day.max(0.0) as i64;
    format!("{:02}:{:02}", s / 3600, (s % 3600) / 60)
}

/* ============================ 问答 ============================ */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Travel,
    Task,
    Habit,
    Risk,
    General,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Travel => "travel",
            Intent::Task => "task",
            Intent::Habit => "habit",
            Intent::Risk => "risk",
            Intent::General => "general",
        }
    }
}

/// 意图识别用关键词而不是让模型分类：
/// 分类本身也要消耗一次调用（几百毫秒 + 钱），而这类问题的词汇非常集中，
/// 规则命中率足够高，且**可测试**（模型分类没法写单测断言）。
pub fn classify(q: &str) -> Intent {
    let t = q.to_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| t.contains(w));

    if has(&["出差", "去哪", "去过", "在哪", "行程", "几号在", "旅行", "哪个城市", "哪座城"]) {
        return Intent::Travel;
    }
    if has(&["待办", "任务", "提醒", "没做", "逾期", "完成", "还有什么事", "要做"]) {
        return Intent::Task;
    }
    if has(&["习惯", "几点", "多久", "专注", "打断", "效率", "工作时长", "上班", "下班", "摸鱼"]) {
        return Intent::Habit;
    }
    if has(&["风险", "注意", "拖", "熬夜", "该休息", "问题在哪"]) {
        return Intent::Risk;
    }
    Intent::General
}

/// 从问句里抠出具体日期（"10月8号" / "昨天" / "2026-10-08" / "10-08"）
///
/// 只认这四种写法：更灵活的解析要上 chrono 或让模型干，
/// 但这两种都会引入不确定性；日期抠错会让"那天我在哪"整个答错。
pub fn parse_day(q: &str) -> Option<String> {
    let today = crate::logs::bj_date(now_sec());
    let year: i64 = today.get(0..4).and_then(|s| s.parse().ok()).unwrap_or(2026);

    if q.contains("前天") {
        return Some(bj_date(now_sec() - 2 * 86_400));
    }
    if q.contains("昨天") {
        return Some(bj_date(now_sec() - 86_400));
    }
    if q.contains("今天") || q.contains("今天") {
        return Some(today.clone());
    }

    // YYYY-MM-DD。必须按**字符**滑动，不能按字节切：
    // 中文是多字节字符，字节切会撞上 char boundary 直接 panic（真踩过）。
    let chars: Vec<char> = q.chars().collect();
    for w in chars.windows(10) {
        let s: String = w.iter().collect();
        let b = s.as_bytes();
        if b[4] == b'-'
            && b[7] == b'-'
            && s[0..4].parse::<i64>().is_ok()
            && s[5..7].parse::<i64>().is_ok()
            && s[8..10].parse::<i64>().is_ok()
        {
            return Some(s);
        }
    }

    // M月D号 / M月D日
    if let Some(p) = q.find('月') {
        let m: i64 = q[..p]
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()?;
        let d: i64 = q[p + '月'.len_utf8()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()?;
        if (1..=12).contains(&m) && (1..=31).contains(&d) {
            return Some(format!("{year}-{m:02}-{d:02}"));
        }
    }

    // MM-DD（不带年份，按今年算）
    for w in chars.windows(5) {
        let s: String = w.iter().collect();
        let b = s.as_bytes();
        if b[2] == b'-' && s[0..2].parse::<i64>().is_ok() && s[3..5].parse::<i64>().is_ok() {
            let m: i64 = s[0..2].parse().unwrap_or(0);
            let d: i64 = s[3..5].parse().unwrap_or(0);
            if (1..=12).contains(&m) && (1..=31).contains(&d) {
                return Some(format!("{year}-{m:02}-{d:02}"));
            }
        }
    }
    None
}

/// 问答主入口：先算事实，再让模型把事实说成人话。
///
/// 返回值里带 `facts`（结构化）与 `answer`（自然语言）。
/// 这样即使模型不可用，前端/测试也能拿到可靠的事实部分。
pub fn answer(question: &str) -> serde_json::Value {
    let q = question.trim();
    if q.is_empty() {
        return json!({ "ok": false, "error": "问题是空的" });
    }
    let intent = classify(q);
    let day = parse_day(q);
    let now = now_sec();

    let facts = match intent {
        Intent::Travel => travel_facts(day.as_deref()),
        Intent::Task => task_facts(now),
        Intent::Habit => habit_facts(),
        Intent::Risk => risk_facts(),
        Intent::General => {
            let mut all = travel_facts(day.as_deref());
            if let Some(o) = all.as_object_mut() {
                let t = task_facts(now);
                let h = habit_facts();
                let r = risk_facts();
                o.insert("tasks".into(), t["tasks"].clone());
                o.insert("habits".into(), h["habits"].clone());
                o.insert("risks".into(), r["risks"].clone());
            }
            all
        }
    };

    let facts_text = render_facts(&facts);
    let f = crate::llm::load_file();
    let grounded = if crate::llm::api_key(&f).is_some() && !f.model.trim().is_empty() {
        match crate::llm::grounded_chat(&f, q, &facts_text) {
            Ok(a) => json!({ "ok": true, "answer": a, "by": "llm" }),
            Err(e) => json!({ "ok": false, "error": e, "by": "fallback" }),
        }
    } else {
        json!({ "ok": false, "error": "未配置模型，只返回事实清单", "by": "fallback" })
    };

    json!({
        "intent": intent.as_str(),
        "day": day,
        "facts": facts,
        "facts_text": facts_text,
        "now_bj": bj_now(),
        "result": grounded,
    })
}

/* ------------------------------ 各类事实 ------------------------------ */

fn travel_facts(day: Option<&str>) -> serde_json::Value {
    let now = now_sec();
    let home = crate::db::with_conn(|c| crate::db::repo::home_base(c, now - 90 * 86_400))
        .ok()
        .flatten()
        .unwrap_or_default();

    match day {
        Some(d) => {
            let tz_mod = crate::tz::sqlite_modifier();
            let rows: Vec<crate::db::repo::LocRow> = crate::db::with_conn(|c| {
                let mut st = c.prepare(
                    "SELECT id, ts, lat, lon, acc_m, source, COALESCE(label,''), is_trip
                     FROM locations WHERE substr(date(ts,'unixepoch',?2),1,10) = ?1
                     ORDER BY ts ASC",
                )?;
                let rows = st.query_map(rusqlite::params![d, tz_mod], |r| {
                    Ok(crate::db::repo::LocRow {
                        id: r.get(0)?,
                        ts: r.get(1)?,
                        lat: r.get(2)?,
                        lon: r.get(3)?,
                        acc_m: r.get(4)?,
                        source: r.get(5)?,
                        label: r.get(6)?,
                        is_trip: r.get(7)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default();
            json!({
                "home_base": home,
                "day": d,
                "places": rows.iter().map(|r| json!({
                    "place": r.label, "source": r.source, "trip": r.is_trip == 1
                })).collect::<Vec<_>>(),
            })
        }
        None => {
            let since = now - 120 * 86_400;
            let days: Vec<(String, String, i64)> =
                crate::db::with_conn(|c| crate::db::repo::location_days(c, since, 90))
                    .unwrap_or_default();
            json!({
                "home_base": home,
                "days": days.iter().map(|(d, l, t)| json!({
                    "day": d, "place": l, "trip": *t == 1
                })).collect::<Vec<_>>(),
            })
        }
    }
}

fn task_facts(now: i64) -> serde_json::Value {
    let (open, overdue, deferred, done, avg_resp) = crate::db::with_conn(|c| {
        Ok((
            crate::db::repo::open_task_count(c).unwrap_or(0),
            crate::db::repo::overdue_count(c, now).unwrap_or(0),
            crate::db::repo::deferred_count(c).unwrap_or(0),
            crate::db::repo::done_since(c, now - 7 * 86_400).unwrap_or(0),
            crate::db::repo::avg_response_sec(c, now - 14 * 86_400)
                .ok()
                .flatten(),
        ))
    })
    .unwrap_or((0, 0, 0, 0, None));

    let titles: Vec<String> = crate::db::with_conn(|c| {
        let mut st = c.prepare(
            "SELECT title FROM reminders WHERE kind = 0 AND status IN (0,1) ORDER BY fire_at ASC LIMIT 15",
        )?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .unwrap_or_default();

    json!({
        "tasks": {
            "open": open,
            "overdue": overdue,
            "deferred": deferred,
            "done_7d": done,
            "avg_response_min": avg_resp.map(|s| (s as f64 / 60.0).round() as i64),
            "titles": titles,
        }
    })
}

fn habit_facts() -> serde_json::Value {
    let _ = build_day(&bj_date(now_sec()));
    let ps: Vec<crate::db::repo::DayProfile> =
        crate::db::with_conn(|c| crate::db::repo::day_profiles(c, 14)).unwrap_or_default();
    json!({ "habits": summarize_habits(&ps), "days": ps.iter().map(profile_line).collect::<Vec<_>>() })
}

fn risk_facts() -> serde_json::Value {
    let rs: Vec<crate::db::repo::RiskRow> =
        crate::db::with_conn(|c| crate::db::repo::open_risks(c, 10)).unwrap_or_default();
    json!({
        "risks": rs.iter().map(|r| json!({
            "level": r.level, "kind": r.kind, "title": r.title, "detail": r.detail
        })).collect::<Vec<_>>()
    })
}

/// 事实清单 → 紧凑文本。
///
/// 刻意用「标签: 值」的短行而不是 JSON：模型对紧凑自然语言的注意力
/// 明显好过对嵌套 JSON（同样信息量，token 也更少）。
fn render_facts(v: &serde_json::Value) -> String {
    let mut out: Vec<String> = Vec::new();
    out.push(format!("当前北京时间：{}", bj_now()));

    if let Some(home) = v["home_base"].as_str() {
        if !home.is_empty() {
            out.push(format!("常驻地：{home}"));
        }
    }
    if let Some(day) = v["day"].as_str() {
        let places: Vec<String> = v["places"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|p| {
                        format!(
                            "{}{}",
                            p["place"].as_str().unwrap_or(""),
                            if p["trip"].as_bool().unwrap_or(false) {
                                "（出差）"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(format!(
            "{day} 的位置记录：{}",
            if places.is_empty() {
                "没有记录".to_string()
            } else {
                places.join("、")
            }
        ));
    }
    if let Some(days) = v["days"].as_array() {
        if !days.is_empty() {
            out.push("按天的地点：".into());
            for d in days {
                let trip = if d["trip"].as_bool().unwrap_or(false) {
                    "（出差）"
                } else {
                    ""
                };
                out.push(format!(
                    "  {} {}{}",
                    d["day"].as_str().unwrap_or(""),
                    d["place"].as_str().unwrap_or(""),
                    trip
                ));
            }
        }
    }
    if let Some(h) = v.get("habits") {
        if h["samples"].as_i64().unwrap_or(0) > 0 {
            out.push(format!(
                "工作习惯（{} 个工作日样本，已排除周末）：工作日平均 {} 开始、{} 结束，日均活跃 {} 分钟",
                h["samples"].as_i64().unwrap_or(0),
                h["avg_start"].as_str().unwrap_or(""),
                h["avg_end"].as_str().unwrap_or(""),
                h["avg_active_min"].as_i64().unwrap_or(0)
            ));
            let cats: Vec<String> = h["top_categories"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|c| {
                            format!(
                                "{} {}%",
                                c["cat"].as_str().unwrap_or(""),
                                c["share_pct"].as_i64().unwrap_or(0)
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !cats.is_empty() {
                out.push(format!("  时间主要花在：{}", cats.join("、")));
            }
        }
        if let Some(tsum) = h["interrupt_trend"]["summary"].as_str() {
            if !tsum.is_empty() {
                out.push(format!("专注打断统计：{tsum}"));
            }
        }
    }
    if let Some(t) = v.get("tasks").and_then(|x| x.get("tasks")).or_else(|| v.get("tasks")) {
        let titles: Vec<String> = t["titles"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.push(format!(
            "待办：未完成 {} 件，逾期 {} 件，被顺延 {} 件，近 7 天完成 {} 件，平均响应 {} 分钟",
            t["open"].as_i64().unwrap_or(0),
            t["overdue"].as_i64().unwrap_or(0),
            t["deferred"].as_i64().unwrap_or(0),
            t["done_7d"].as_i64().unwrap_or(0),
            t["avg_response_min"].as_i64().map(|n| n.to_string()).unwrap_or_else(|| "未知".into())
        ));
        if !titles.is_empty() {
            out.push(format!("  待办清单：{}", titles.join("、")));
        }
    }
    if let Some(rs) = v["risks"].as_array() {
        if !rs.is_empty() {
            out.push("当前风险：".into());
            for r in rs {
                out.push(format!(
                    "  [{}级] {}（{}）",
                    r["level"].as_i64().unwrap_or(1),
                    r["title"].as_str().unwrap_or(""),
                    r["detail"].as_str().unwrap_or("")
                ));
            }
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_rules() {
        assert_eq!(classify("我最近都去哪里出差了"), Intent::Travel);
        assert_eq!(classify("10月8号我在哪里"), Intent::Travel);
        assert_eq!(classify("我还有什么待办没做"), Intent::Task);
        assert_eq!(classify("我一般几点开始工作"), Intent::Habit);
        assert_eq!(classify("现在有什么风险要注意"), Intent::Risk);
        assert_eq!(classify("今天天气怎么样"), Intent::General);
    }

    #[test]
    fn day_parsing() {
        let d = parse_day("10月8号我在哪里出差").unwrap();
        assert!(d.ends_with("-10-08"), "got {d}");
        assert_eq!(parse_day("2026-03-05 那天"), Some("2026-03-05".into()));
        assert_eq!(parse_day("没什么具体日期"), None);
    }

    #[test]
    fn hm_format() {
        assert_eq!(fmt_hm(9.0 * 3600.0 + 5.0 * 60.0), "09:05");
        assert_eq!(fmt_hm(0.0), "00:00");
    }

    #[test]
    fn summarize_habits_applies_timezone_offset() {
        // UTC 04:55 -> 北京时间 (UTC+8) 12:55；UTC 14:10 -> 北京时间 (UTC+8) 22:10
        let utc_04_55 = 4 * 3600 + 55 * 60;
        let utc_14_10 = 14 * 3600 + 10 * 60;
        let p = crate::db::repo::DayProfile {
            day: "2026-10-25".into(),
            first_active: Some(utc_04_55),
            last_active: Some(utc_14_10),
            active_sec: 123 * 60,
            focus_max_sec: 50 * 60,
            cat_json: r#"{"7":3500,"6":1600,"2":1400}"#.into(),
            remind_cnt: 2,
            done_cnt: 2,
            location: "上海".into(),
            interrupt_cnt: 1,
        };
        let h = summarize_habits_with_offset(&[p], 8 * 3600);
        assert_eq!(h["samples"].as_i64(), Some(1));
        assert_eq!(h["avg_start"].as_str(), Some("12:55"));
        assert_eq!(h["avg_end"].as_str(), Some("22:10"));
        assert_eq!(h["avg_active_min"].as_i64(), Some(123));
    }

    #[test]
    fn summarize_habits_excludes_weekends_and_midnight_spillover() {
        let sun = crate::db::repo::DayProfile {
            day: "2026-10-25".into(), // 周日 12:55 开工
            first_active: Some(4 * 3600 + 55 * 60),
            last_active: Some(14 * 3600),
            active_sec: 120 * 60,
            ..Default::default()
        };
        let mon = crate::db::repo::DayProfile {
            day: "2026-10-26".into(), // 周一 10:00 开工 (UTC 02:00)
            first_active: Some(2 * 3600),
            last_active: Some(12 * 3600),
            active_sec: 300 * 60,
            ..Default::default()
        };
        let tue = crate::db::repo::DayProfile {
            day: "2026-10-27".into(), // 周二 09:20 开工 (UTC 01:20)
            first_active: Some(1 * 3600 + 20 * 60),
            last_active: Some(11 * 3600),
            active_sec: 240 * 60,
            ..Default::default()
        };
        let h = summarize_habits_with_offset(&[sun, mon, tue], 8 * 3600);
        assert_eq!(h["samples"].as_i64(), Some(2), "应仅统计 2 个工作日");
        assert_eq!(h["workday_only"].as_bool(), Some(true));
        assert_eq!(h["avg_start"].as_str(), Some("09:40"), "周日 12:55 不应拉晚工作日均值");
        assert_eq!(h["avg_active_min"].as_i64(), Some(270));
    }

    #[test]
    fn avg_response_sec_and_deferred_count_with_sqlite_real() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();

        let id1 = crate::db::repo::add_reminder(&conn, "写周报", 1_792_000_000, 1, 0).unwrap();
        crate::db::repo::log_task_event(&conn, Some(id1), "created", None, "写周报").unwrap();
        crate::db::repo::log_task_event(&conn, Some(id1), "deferred", Some(900), "写周报").unwrap();
        assert_eq!(crate::db::repo::deferred_count(&conn).unwrap(), 1);

        crate::db::repo::set_reminder_status(&conn, id1, 2).unwrap();
        crate::db::repo::log_task_event(&conn, Some(id1), "done", Some(300), "写周报").unwrap();

        // 以前 SQLite avg(delta_sec) 返回 REAL 导致 Option<i64> 报 InvalidColumnType，现必须正常返回 Some(300)
        let avg = crate::db::repo::avg_response_sec(&conn, 0).unwrap();
        assert_eq!(avg, Some(300));
        assert_eq!(crate::db::repo::done_since(&conn, 0).unwrap(), 1);
    }
}

#[cfg(test)]
mod facts_tests {
    use super::*;

    fn facts(samples: i64, active_min: i64, done: i64) -> DayFacts {
        DayFacts {
            day: "2026-10-24".to_string(),
            samples,
            active_min,
            focus_max_min: 0,
            interrupt_cnt: 0,
            created: 0,
            done,
            overdue: 0,
            places: vec![],
            cats: vec![],
            done_titles: if done > 0 {
                vec!["跟产品组对口径".to_string()]
            } else {
                vec![]
            },
            open_titles: vec![],
        }
    }

    #[test]
    fn no_samples_must_not_be_reported_as_idle() {
        // 没有采样 ≠ 用户没活动。曾经输出「活跃：0 分钟」，
        // 模型于是写成「你今天几乎没坐在屏幕前」——把缺失当成了事实。
        let f = facts(0, 0, 1);
        let t = day_facts_text(&f);
        assert!(t.contains("没有采集到任何采样"), "{t}");
        assert!(!t.contains("活跃：0 分钟"), "不能把缺失说成 0 活跃：{t}");
        assert!(t.contains("禁止据此评价忙闲"), "必须明确禁止模型下结论：{t}");

        let s = rule_summary(&f);
        assert!(s.contains("没有采集到活动记录"), "{s}");
        assert!(s.contains("跟产品组对口径"), "完成的事仍要写出来：{s}");
    }

    #[test]
    fn few_samples_flagged_as_insufficient() {
        let t = day_facts_text(&facts(10, 0, 0));
        assert!(t.contains("只采集到 10 条采样"), "{t}");
        assert!(t.contains("不要据此判断"), "{t}");
    }

    #[test]
    fn enough_samples_show_real_numbers() {
        let t = day_facts_text(&facts(200, 75, 2));
        assert!(t.contains("活跃：75 分钟"), "{t}");
        assert!(t.contains("共 200 条采样"), "{t}");
        assert!(!t.contains("没有采集到"), "{t}");
    }
}
