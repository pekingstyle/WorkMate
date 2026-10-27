// WorkMate · 数据访问层
// 编制：WorkMate Contributors
//
// 三条硬约束（来自 §3 实测）：
//   1. 明细表只做"按时间取最近 N 条"，统计一律走 rollup_5m
//   2. events 不存 JSON，字段拆成数值列
//   3. 清理用分段表 DROP 思路；P0 数据量小，先用 DELETE + 备注

use rusqlite::{params, Connection};

/// 一次感知快照。字段与 events 表一一对应，零序列化开销。
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub ts: i64,
    pub kind: i64,
    pub idle_s: i64,
    pub app_cat: i64,
    pub focus_min: i64,
    pub flags: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct DbStats {
    pub schema_version: i64,
    pub events: i64,
    pub rollup_5m: i64,
    pub facts: i64,
    pub reminders: i64,
    pub size_bytes: u64,
    pub path: String,
}

#[derive(Debug, serde::Serialize)]
pub struct CategoryRow {
    pub app_cat: i64,
    /// 采样点数（每个 5s 一拍）。**只作诊断用，不要当用户在意的指标展示**
    pub cnt: i64,
    /// 活跃秒数（idle < 60s 的拍数 × 5s）。minute 由上层换算，避免再犯单位错误
    pub focus_sec: i64,
}

/* ------------------------------ 写入 ------------------------------ */

pub fn insert_event(conn: &Connection, s: &Snapshot) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO events(ts, kind, idle_s, app_cat, focus_min, flags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![s.ts, s.kind, s.idle_s, s.app_cat, s.focus_min, s.flags],
    )?;
    Ok(())
}

/// 批量写入（回填/测试用）。一个事务内完成，实测 13 万条/秒。
pub fn insert_events(conn: &mut Connection, rows: &[Snapshot]) -> rusqlite::Result<usize> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO events(ts, kind, idle_s, app_cat, focus_min, flags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for s in rows {
            stmt.execute(params![s.ts, s.kind, s.idle_s, s.app_cat, s.focus_min, s.flags])?;
        }
    }
    tx.commit()?;
    Ok(rows.len())
}

/* ------------------------------ 聚合 ------------------------------ */

/// 把 [from, to) 的明细聚合进 rollup_5m。
///
/// 用 ON CONFLICT DO UPDATE 覆盖而非累加：本函数是从明细重新计算的幂等操作，
/// 重复跑不应该把计数翻倍（这点和"增量累加"的直觉相反，踩过）。
pub fn rollup_range(conn: &Connection, from_ts: i64, to_ts: i64) -> rusqlite::Result<usize> {
    // focus_sec：活跃拍数 × 5s。绝不能用 sum(focus_min)——那是瞬时值（gauge），
    // 一加就翻几十倍（v4 修的就是这个 bug）。
    let n = conn.execute(
        "INSERT INTO rollup_5m(bucket, app_cat, kind, cnt, focus_sec)
         SELECT (ts / 300) * 300 AS b, app_cat, kind, count(*),
                sum(CASE WHEN idle_s < 60 THEN 5 ELSE 0 END)
         FROM events
         WHERE ts >= ?1 AND ts < ?2
         GROUP BY b, app_cat, kind
         ON CONFLICT(bucket, app_cat, kind) DO UPDATE SET
           cnt = excluded.cnt,
           focus_sec = excluded.focus_sec",
        params![from_ts, to_ts],
    )?;
    Ok(n)
}

/// 预聚合表里最新的桶时间（没有则 None）。
/// 用来判断「图表为空」到底是没数据，还是聚合还没跑。
pub fn latest_rollup_bucket(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT MAX(bucket) FROM rollup_5m", [], |r| {
        r.get::<_, Option<i64>>(0)
    })
}

/// 最近 window_sec 秒的分类统计（走预聚合表，实测 0.76ms）
pub fn recent_summary(conn: &Connection, window_sec: i64, now: i64) -> rusqlite::Result<Vec<CategoryRow>> {
    let from = now - window_sec;
    let mut stmt = conn.prepare(
        "SELECT app_cat, sum(cnt) AS c, sum(focus_sec) AS f
         FROM rollup_5m
         WHERE bucket >= ?1
         GROUP BY app_cat
         ORDER BY f DESC",
    )?;
    let rows = stmt
        .query_map(params![from], |r| {
            Ok(CategoryRow {
                app_cat: r.get(0)?,
                cnt: r.get(1)?,
                focus_sec: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/* ------------------------------ 设置（v3） ------------------------------ */

/// 读一个单值配置。不存在返回 None（调用方给默认值，避免到处硬编码兜底）。
pub fn get_setting(conn: &Connection, k: &str) -> rusqlite::Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT v FROM settings WHERE k = ?1")?;
    let mut rows = stmt.query(params![k])?;
    match rows.next()? {
        Some(r) => Ok(Some(r.get(0)?)),
        None => Ok(None),
    }
}

pub fn set_setting(conn: &Connection, k: &str, v: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings(k, v, updated_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v, updated_at = excluded.updated_at",
        params![k, v, now_sec()],
    )?;
    Ok(())
}

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/* ------------------------------ 维护 ------------------------------ */

/// 清理超过 keep_days 的明细。
/// 注意：DELETE 不归还磁盘空间，持续跑会让文件只增不减。
/// 数据量起来后应改为「按天建分段表 + DROP TABLE」（§3.6 实测：DROP 是 O(1)）。
pub fn purge_events(conn: &Connection, keep_days: i64, now: i64) -> rusqlite::Result<usize> {
    let cutoff = now - keep_days * 86_400;
    let n = conn.execute("DELETE FROM events WHERE ts < ?1", params![cutoff])?;
    Ok(n)
}

/// 清理过期聚合桶。rollup 是「长期记忆」，保留期远长于明细（默认一年），
/// 但也不能无限长——5 分钟一桶 × 7 类 ≈ 每天 2000 行，一年 70 万行，需要兜底。
pub fn purge_rollup(conn: &Connection, keep_days: i64, now: i64) -> rusqlite::Result<usize> {
    let cutoff = now - keep_days * 86_400;
    let n = conn.execute("DELETE FROM rollup_5m WHERE bucket < ?1", params![cutoff])?;
    Ok(n)
}

pub fn stats(conn: &Connection) -> rusqlite::Result<DbStats> {
    let count = |t: &str| -> rusqlite::Result<i64> {
        conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
    };
    Ok(DbStats {
        schema_version: super::schema::version(conn)?,
        events: count("events")?,
        rollup_5m: count("rollup_5m")?,
        facts: count("facts")?,
        reminders: count("reminders")?,
        size_bytes: 0, // 由 Db 层填充（需要文件元数据）
        path: String::new(),
    })
}

/// 最近一条明细，供面板显示"当前在干什么"
pub fn latest_event(conn: &Connection) -> rusqlite::Result<Option<(i64, i64, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT ts, idle_s, app_cat, focus_min FROM events ORDER BY ts DESC LIMIT 1",
    )?;
    let mut rows = stmt.query([])?;
    if let Some(r) = rows.next()? {
        Ok(Some((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
    } else {
        Ok(None)
    }
}

/* ------------------------------ 提醒 / 待办 ------------------------------ */

/// status: 0=待触发 1=已触发 2=已完成
/// kind:   0=任务 1=喝水 2=站起（有机整合的前提：同类合并、跨类避让）
#[derive(Debug, Clone, serde::Serialize)]
pub struct Reminder {
    pub id: i64,
    pub fire_at: i64,
    pub level: i64,
    pub title: String,
    pub status: i64,
    pub kind: i64,
    pub created_at: i64,
    /// 1=高 2=中 3=低
    pub priority: i64,
    /// 预估用时（分钟，0 表示未估）
    pub duration_min: i64,
    pub note: String,
    /// 创建（或完成）时的位置标签，出差回顾用
    pub location: String,
    /// 创建簿记时的时区偏移（分钟，相对 UTC，如 480 = UTC+8）
    pub tz_offset_min: i64,
    /// 周期循环规则：""=单次, "daily"=每天, "workdays"=每个工作日, "weekly"=每周, "monthly"=每月, "every:Nd"=每N天
    pub recur_rule: String,
}

pub fn add_reminder(
    conn: &Connection,
    title: &str,
    fire_at: i64,
    level: i64,
    kind: i64,
) -> rusqlite::Result<i64> {
    add_reminder_full(
        conn,
        &NewReminder {
            title: title.to_string(),
            fire_at,
            level,
            kind,
            tz_offset_min: crate::tz::offset_min(),
            ..Default::default()
        },
    )
}

/// 建待办的完整参数。
///
/// 收敛成结构体的原因：直接列参数已经涨到 9 个，调用处全靠位置对齐，
/// `priority` 和 `level` 这种同类型参数一旦传反，编译器查不出来、
/// 运行时也只表现为"优先级怪怪的"——属于最危险的那类静默错误。
#[derive(Debug, Clone)]
pub struct NewReminder {
    pub title: String,
    pub fire_at: i64,
    /// 0=软 1=硬
    pub level: i64,
    pub kind: i64,
    /// 1=高 2=中 3=低（默认中：手写待办不带优先级语义，不该都成"高"）
    pub priority: i64,
    /// 预估用时（分钟，0=未估）
    pub duration_min: i64,
    pub note: String,
    /// 创建时的位置标签（出差回顾用；取不到就空串，
    /// 绝不让「定位失败」把建待办这件事也搞失败）
    pub location: String,
    /// 创建簿记时的时区偏移（分钟，默认当前生效时区）
    pub tz_offset_min: i64,
    /// 周期循环规则（空串表示单次任务）
    pub recur_rule: String,
}

impl Default for NewReminder {
    fn default() -> Self {
        Self {
            title: String::new(),
            fire_at: 0,
            level: 1,
            kind: 0,
            priority: 2,
            duration_min: 0,
            note: String::new(),
            location: String::new(),
            tz_offset_min: crate::tz::CN_OFFSET_MIN,
            recur_rule: String::new(),
        }
    }
}

/// 带优先级/时长/备注/位置/时区/周期规则的插入（AI 解析待办用）
pub fn add_reminder_full(conn: &Connection, r: &NewReminder) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO reminders(fire_at, level, title, kind, created_at, priority, duration_min, note, location, tz_offset_min, recur_rule)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            r.fire_at,
            r.level,
            r.title,
            r.kind,
            now_sec(),
            r.priority.clamp(1, 3),
            r.duration_min.max(0),
            r.note,
            r.location,
            r.tz_offset_min,
            r.recur_rule.trim()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 未完成的待办提醒（仅真实任务 kind = 0），按时间正序。已触发的也列出来，方便回看。
pub fn list_reminders(conn: &Connection) -> rusqlite::Result<Vec<Reminder>> {
    let mut stmt = conn.prepare(
        "SELECT id, fire_at, level, title, status, kind, created_at, priority, duration_min,
                COALESCE(note, ''), COALESCE(location, ''), COALESCE(tz_offset_min, 480),
                COALESCE(recur_rule, '')
         FROM reminders WHERE status IN (0, 1) AND kind = 0 ORDER BY fire_at ASC LIMIT 200",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Reminder {
                id: r.get(0)?,
                fire_at: r.get(1)?,
                level: r.get(2)?,
                title: r.get(3)?,
                status: r.get(4)?,
                kind: r.get(5)?,
                created_at: r.get(6)?,
                priority: r.get(7)?,
                duration_min: r.get(8)?,
                note: r.get(9)?,
                location: r.get(10)?,
                tz_offset_min: r.get(11)?,
                recur_rule: r.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 周期规则的人类可读标签
pub fn recur_label(rule: &str) -> String {
    let r = rule.trim();
    match r {
        "" => String::new(),
        "daily" => "每天".to_string(),
        "workdays" => "每个工作日".to_string(),
        "weekly" => "每周".to_string(),
        "monthly" => "每月".to_string(),
        _ if r.starts_with("every:") && r.ends_with('d') => {
            let n = r
                .trim_start_matches("every:")
                .trim_end_matches('d')
                .parse::<i64>()
                .unwrap_or(1);
            format!("每{n}天")
        }
        _ => r.to_string(),
    }
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
            if leap { 29 } else { 28 }
        }
        _ => 30,
    }
}

/// 从 `fire_at` 往前推进一个周期
pub fn step_recurring_fire_at(fire_at: i64, rule: &str, tz_offset_min: i64) -> Option<i64> {
    let r = rule.trim();
    if r.is_empty() {
        return None;
    }
    let offset_sec = tz_offset_min * 60;
    let local = fire_at + offset_sec;
    let days = local.div_euclid(86_400);
    let tod = local.rem_euclid(86_400);

    match r {
        "daily" => Some(fire_at + 86_400),
        "workdays" => {
            let mut d = days + 1;
            loop {
                let wd = (d + 3).rem_euclid(7) + 1; // 1=Mon .. 7=Sun
                if wd <= 5 {
                    return Some(d * 86_400 + tod - offset_sec);
                }
                d += 1;
            }
        }
        "weekly" => Some(fire_at + 7 * 86_400),
        "monthly" => {
            let (y, m, d) = civil_from_days(days);
            let (ny, nm) = if m >= 12 { (y + 1, 1) } else { (y, m + 1) };
            let nd = d.min(days_in_month(ny, nm));
            let next_days = days_from_civil(ny, nm, nd);
            Some(next_days * 86_400 + tod - offset_sec)
        }
        _ if r.starts_with("every:") && r.ends_with('d') => {
            let n = r
                .trim_start_matches("every:")
                .trim_end_matches('d')
                .parse::<i64>()
                .ok()?
                .max(1);
            Some(fire_at + n * 86_400)
        }
        _ => None,
    }
}

/// 计算周期任务的下一次未来触发时间（确保 > now）
pub fn next_recurring_fire_at(
    fire_at: i64,
    now: i64,
    rule: &str,
    tz_offset_min: i64,
) -> Option<i64> {
    let mut cur = step_recurring_fire_at(fire_at, rule, tz_offset_min)?;
    let mut guard = 0;
    while cur <= now && guard < 1000 {
        cur = step_recurring_fire_at(cur, rule, tz_offset_min)?;
        guard += 1;
    }
    Some(cur)
}

/// 取出所有到点未触发的提醒，并原子地标记为已触发。
///
/// 必须在一个事务里做「查 + 改」，否则轮询线程与面板刷新并发时，
/// 同一条提醒可能被弹两次。
/// 对于周期任务（recur_rule <> ''）：如果上一轮 status=1 忘了点完成，而新周期已到点，
/// 自动推进到当前周期并重新触发提醒。
pub fn take_due_reminders(
    conn: &mut Connection,
    now: i64,
) -> rusqlite::Result<Vec<Reminder>> {
    let tx = conn.transaction()?;
    // 1) 检查已触发未完成（status = 1）的周期任务是否已进入下一周期
    let stale_recurring: Vec<(i64, i64, String, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT id, fire_at, COALESCE(recur_rule, ''), COALESCE(tz_offset_min, 480)
             FROM reminders WHERE status = 1 AND kind = 0 AND COALESCE(recur_rule, '') <> ''",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (id, fire_at, rule, tz_off) in stale_recurring {
        if let Some(next_step) = step_recurring_fire_at(fire_at, &rule, tz_off) {
            if next_step <= now {
                // 推进到 <= now 的最新一期，使其在本次轮询作为新周期提醒触发
                let mut latest_due = next_step;
                let mut guard = 0;
                while let Some(s) = step_recurring_fire_at(latest_due, &rule, tz_off) {
                    if s <= now && guard < 1000 {
                        latest_due = s;
                        guard += 1;
                    } else {
                        break;
                    }
                }
                tx.execute(
                    "UPDATE reminders SET status = 0, fired_at = NULL, fire_at = ?1 WHERE id = ?2",
                    params![latest_due, id],
                )?;
            }
        }
    }

    // 注意：必须把 rows 先收集到具名变量，否则 stmt 的临时值活得不够久（E0597）
    let due: Vec<Reminder> = {
        let mut stmt = tx.prepare(
            "SELECT id, fire_at, level, title, status, kind, created_at, priority, duration_min,
                    COALESCE(note, ''), COALESCE(location, ''), COALESCE(tz_offset_min, 480),
                    COALESCE(recur_rule, '')
             FROM reminders WHERE status = 0 AND fire_at <= ?1 ORDER BY fire_at ASC",
        )?;
        let rows = stmt
            .query_map(params![now], |r| {
                Ok(Reminder {
                    id: r.get(0)?,
                    fire_at: r.get(1)?,
                    level: r.get(2)?,
                    title: r.get(3)?,
                    status: r.get(4)?,
                    kind: r.get(5)?,
                    created_at: r.get(6)?,
                    priority: r.get(7)?,
                    duration_min: r.get(8)?,
                    note: r.get(9)?,
                    location: r.get(10)?,
                    tz_offset_min: r.get(11)?,
                    recur_rule: r.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for r in &due {
        tx.execute(
            "UPDATE reminders SET status = 1, fired_at = ?1 WHERE id = ?2",
            params![now, r.id],
        )?;
    }
    tx.commit()?;
    Ok(due)
}

pub fn set_reminder_status(conn: &Connection, id: i64, status: i64) -> rusqlite::Result<bool> {
    let now = now_sec();
    if status == 2 {
        let meta: Option<(i64, String, i64)> = conn
            .query_row(
                "SELECT fire_at, COALESCE(recur_rule, ''), COALESCE(tz_offset_min, 480)
                 FROM reminders WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        if let Some((fire_at, rule, tz_off)) = meta {
            if let Some(next_fire) = next_recurring_fire_at(fire_at, now, &rule, tz_off) {
                let n = conn.execute(
                    "UPDATE reminders
                     SET status = 0, fired_at = NULL, fire_at = ?1, done_at = ?2
                     WHERE id = ?3",
                    params![next_fire, now, id],
                )?;
                return Ok(n > 0);
            }
        }
    }
    let done_at = if status == 2 { Some(now) } else { None };
    let n = conn.execute(
        "UPDATE reminders SET status = ?1, done_at = COALESCE(?2, done_at) WHERE id = ?3",
        params![status, done_at, id],
    )?;
    Ok(n > 0)
}

/// 某一天（按本地时区自然日）的分类统计，供 A4 今日概览。
/// 数据来自 rollup，一天 288 个桶，实测 < 2ms。
pub fn day_summary(conn: &Connection, day_start: i64, day_end: i64) -> rusqlite::Result<Vec<CategoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT app_cat, sum(cnt) AS c, sum(focus_sec) AS f
         FROM rollup_5m
         WHERE bucket >= ?1 AND bucket < ?2
         GROUP BY app_cat
         ORDER BY f DESC",
    )?;
    let rows = stmt
        .query_map(params![day_start, day_end], |r| {
            Ok(CategoryRow {
                app_cat: r.get(0)?,
                cnt: r.get(1)?,
                focus_sec: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/* ------------------------ 自动提醒（喝水 / 站起）的状态 ------------------------ */

/// 最近一次某类型提醒的触发时间（kind: 1=喝水 2=站起）
pub fn last_auto_kind_ts(conn: &Connection, kind: i64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT max(fired_at) FROM reminders WHERE kind = ?1 AND fired_at IS NOT NULL",
        params![kind],
        |r| r.get::<_, Option<i64>>(0),
    )
}

/// 是否已有同类型的待触发提醒（防重复插入：演示中被顺延时尤其关键）
pub fn has_pending_kind(conn: &Connection, kind: i64) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM reminders WHERE kind = ?1 AND status = 0",
        params![kind],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 把已取出的提醒重新挂回待触发（软提醒被静默/预算/会议压制时用）
pub fn rearm_reminder(conn: &Connection, id: i64, new_fire_at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE reminders SET status = 0, fired_at = NULL, fire_at = ?1 WHERE id = ?2",
        params![new_fire_at, id],
    )?;
    Ok(())
}

/// 清空统计（明细 + 聚合），保留提醒与设置。
/// 用途：自检/压测写入的模拟数据会污染「今日概览」，需要一键归零。
pub fn reset_stats(conn: &Connection) -> rusqlite::Result<(usize, usize)> {
    let ev = conn.execute("DELETE FROM events", [])?;
    let ru = conn.execute("DELETE FROM rollup_5m", [])?;
    Ok((ev, ru))
}

/// 按标题前缀删除提醒（测试数据清理用）。
///
/// 为什么需要：自动化用例创建的提醒会留在真实库里，到点就弹到用户脸上——
/// 实拍截图里出现过「演示避让测试-1792833085」这种内容，属于测试污染生产数据。
pub fn delete_reminders_by_prefix(conn: &Connection, prefix: &str) -> rusqlite::Result<usize> {
    let n = conn.execute(
        "DELETE FROM reminders WHERE title LIKE ?1",
        params![format!("{prefix}%")],
    )?;
    Ok(n)
}

/* ============================ 记忆层（v6） ============================
 *
 * 设计原则（与 AI 接入直接相关）：
 *   · 写入端尽量"顺手"——感知/提醒管线上顺带记一笔，不要另起一套采集逻辑
 *   · 读出端必须是"结论"——模型拿到的是几点上班、拖了多久，不是 5 秒粒度采样点
 *   · 位置单独一张表，便于一键清除与开关控制
 */

/* ------------------------------ 任务生命周期 ------------------------------ */

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskLogRow {
    pub id: i64,
    pub reminder_id: Option<i64>,
    pub ts: i64,
    pub action: String,
    pub delta_sec: Option<i64>,
    pub note: String,
}

/// 记一条任务事件。delta_sec 传 None 时自动算「距该待办上一条事件多久」。
///
/// 这个自动补间隔很关键：响应时长（提醒后多久才处理）和顺延时长
/// 是「处理时机」的核心指标，散落在各处手算必然漏。
pub fn log_task_event(
    conn: &Connection,
    reminder_id: Option<i64>,
    action: &str,
    delta_sec: Option<i64>,
    note: &str,
) -> rusqlite::Result<i64> {
    let now = now_sec();
    // delta_sec 的语义是「距该待办上一次动作过了多久」，用来算响应/顺延时长。
    // 只有 done / deferred 这类**承接动作**才有意义；created 是起点，
    // 按「上一条记录」去算得到的是两条待办之间的间隔，
    // 会把平均响应时长污染成几小时（实测出现过 created 带 23487s）。
    let delta = match action {
        "created" => None,
        _ => match delta_sec {
            Some(d) => Some(d),
            None => match reminder_id {
                None => None,
                Some(rid) => conn
                    .query_row(
                        "SELECT max(ts) FROM task_log WHERE reminder_id = ?1",
                        params![rid],
                        |r| r.get::<_, Option<i64>>(0),
                    )?
                    .map(|t| now - t),
            },
        },
    };
    conn.execute(
        "INSERT INTO task_log(reminder_id, ts, action, delta_sec, note)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![reminder_id, now, action, delta, note],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 用户是否已经离开：连续 10 分钟没有键鼠输入。
///
/// 与 `recently_active` 方向相反，用途也不同：这里判「不在」，
/// 目的是**不要在人不在的时候消耗掉提醒**——
/// 气泡 9 秒就淡出，用户回来什么都没看到，待办却已经变成「已提醒」，
/// 逾期就是这样悄悄堆起来的。
///
/// 没有采样数据时返回 false（fail-open）：宁可照常提醒，也不能因为感知层异常而全线静音。
pub fn user_away(conn: &Connection, now: i64) -> rusqlite::Result<bool> {
    let last: Option<(i64, i64)> = conn
        .query_row(
            "SELECT ts, idle_s FROM events ORDER BY ts DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    match last {
        None => Ok(false),
        // 采样太旧说明感知停了，此时"离开"结论不可信，按没离开处理
        Some((ts, _)) if now - ts > 120 => Ok(false),
        Some((_, idle)) => Ok(idle >= 600),
    }
}

/// 一段连续同类工作的「工作块」。面板用它画当天时间轴——
/// 比「各类目占比」具体得多：占比只说做了多少，时间轴说**什么时候在做什么**。
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkBlock {
    pub start: i64,
    pub end: i64,
    /// 活跃秒数（拍数 × 5s）。不是 end-start：中间发愣/离开的时间不算。
    pub secs: i64,
    pub app_cat: i64,
}

/// 把一天的采样折成工作块。
///
/// 成块规则：
///   · 闲置拍（idle ≥ 60s）直接跳过，不参与成块——它既不算工作，也不该把块粘起来
///   · 类别相同且间隔 ≤120s → 并入当前块（120s 与专注判定的宽限期一致）
///   · 其余情况开新块
///
/// 刻意用「活跃拍数 × 5s」而不是 end-start：后者会把离开的十几分钟也算进去，
/// 那样「深度工作 2 小时」里可能有一半人在茶水间。
pub fn work_blocks(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<Vec<WorkBlock>> {
    let mut stmt = conn.prepare(
        "SELECT ts, app_cat, idle_s FROM events
         WHERE ts >= ?1 AND ts < ?2 ORDER BY ts ASC",
    )?;
    let rows = stmt
        .query_map(params![from, to], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out: Vec<WorkBlock> = Vec::new();
    for (ts, cat, idle) in rows {
        if idle >= 60 {
            continue;
        }
        match out.last_mut() {
            Some(b) if b.app_cat == cat && ts - b.end <= 120 => {
                b.end = ts;
                b.secs += 5;
            }
            _ => out.push(WorkBlock {
                start: ts,
                end: ts,
                secs: 5,
                app_cat: cat,
            }),
        }
    }
    Ok(out)
}

/// 这条待办被顺延过几次。
///
/// 用途：会议中的硬提醒最多顺延 2 次，之后必须放行——
/// 否则「会一直开着」会把提醒彻底吞掉，用户以为没提醒。
pub fn defer_count(conn: &Connection, reminder_id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM task_log WHERE reminder_id = ?1 AND action = 'deferred'",
        params![reminder_id],
        |r| r.get(0),
    )
}

pub fn task_log_since(
    conn: &Connection,
    since: i64,
    limit: i64,
) -> rusqlite::Result<Vec<TaskLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, reminder_id, ts, action, delta_sec, COALESCE(note, '')
         FROM task_log WHERE ts >= ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![since, limit], |r| {
            Ok(TaskLogRow {
                id: r.get(0)?,
                reminder_id: r.get(1)?,
                ts: r.get(2)?,
                action: r.get(3)?,
                delta_sec: r.get(4)?,
                note: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 「提醒 → 完成」的平均响应秒数。只统计有明确 delta 的 done 事件。
/// 这是「处理时机」最直白的量化：数字越大说明用户越倾向于拖。
///
/// 注意：SQLite 的 `avg(...)` 在有命中行时返回 `REAL` (`f64`)，无命中行时返回 `NULL`，
/// 必须按 `Option<f64>` 读取再四舍五入转为 `i64`，直接按 `Option<i64>` 读取会报 `InvalidColumnType`。
pub fn avg_response_sec(conn: &Connection, since: i64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT avg(delta_sec) FROM task_log
         WHERE action = 'done' AND delta_sec IS NOT NULL AND delta_sec >= 0 AND ts >= ?1",
        params![since],
        |r| r.get::<_, Option<f64>>(0),
    )
    .map(|opt| opt.map(|v| v.round() as i64))
}

/// 逾期未处理的待办数（到点已触发但还没完成，且已过 30 分钟；仅统计真实待办 kind = 0）
pub fn overdue_count(conn: &Connection, now: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM reminders
         WHERE kind = 0 AND status = 1 AND fired_at IS NOT NULL AND ?1 - fired_at > 1800",
        params![now],
        |r| r.get(0),
    )
}

/// 被顺延过的待办（status=0 但 fired_at 非空，或 task_log 中有顺延记录的未结待办）
pub fn deferred_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM reminders r
         WHERE r.kind = 0 AND r.status IN (0, 1)
           AND (r.fired_at IS NOT NULL AND r.status = 0
                OR EXISTS (SELECT 1 FROM task_log tl WHERE tl.reminder_id = r.id AND tl.action = 'deferred'))",
        [],
        |r| r.get(0),
    )
}

pub fn open_task_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM reminders WHERE kind = 0 AND status IN (0, 1)",
        [],
        |r| r.get(0),
    )
}

pub fn done_since(conn: &Connection, since: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM reminders WHERE kind = 0 AND done_at IS NOT NULL AND done_at >= ?1",
        params![since],
        |r| r.get(0),
    )
}

/// 用户此刻是否**真的在用电脑**——静默时段自适应用。
///
/// 判据是「最近 3 分钟内有活跃采样」+「窗口内至少 2 拍活跃」：
///   · 单看最后一拍不行：感知每 5 秒写一行，人离开后仍在写（idle_s 变大），
///     只要 ts 新就会被误判成在用电脑。
///   · 要求 ≥2 拍是为了过滤掉「路过碰一下鼠标」。
pub fn recently_active(conn: &Connection, now: i64, window_secs: i64) -> rusqlite::Result<bool> {
    let last: Option<(i64, i64)> = conn
        .query_row(
            "SELECT ts, idle_s FROM events ORDER BY ts DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let (ts, idle) = match last {
        Some(v) => v,
        None => return Ok(false),
    };
    if idle >= 60 || now - ts > 180 {
        return Ok(false);
    }
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM events WHERE ts >= ?1 AND idle_s < 60",
        params![now - window_secs],
        |r| r.get(0),
    )?;
    Ok(n >= 2)
}

/* ------------------------------ 每日画像 ------------------------------ */

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct DayProfile {
    pub day: String,
    pub first_active: Option<i64>,
    pub last_active: Option<i64>,
    pub active_sec: i64,
    pub focus_max_sec: i64,
    pub cat_json: String,
    pub remind_cnt: i64,
    pub done_cnt: i64,
    pub location: String,
    /// 当天「专注 ≥5 分钟后被打断」的次数（回答"被打断了多少次"）
    pub interrupt_cnt: i64,
}

pub fn upsert_day_profile(conn: &Connection, p: &DayProfile) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO day_profile(day, first_active, last_active, active_sec, focus_max_sec,
                                cat_json, remind_cnt, done_cnt, location, interrupt_cnt, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(day) DO UPDATE SET
           first_active  = excluded.first_active,
           last_active   = excluded.last_active,
           active_sec    = excluded.active_sec,
           focus_max_sec = excluded.focus_max_sec,
           cat_json      = excluded.cat_json,
           remind_cnt    = excluded.remind_cnt,
           done_cnt      = excluded.done_cnt,
           location      = CASE WHEN excluded.location <> '' THEN excluded.location
                                ELSE day_profile.location END,
           interrupt_cnt = CASE WHEN excluded.interrupt_cnt > 0 THEN excluded.interrupt_cnt
                                ELSE day_profile.interrupt_cnt END,
           updated_at    = excluded.updated_at",
        params![
            p.day,
            p.first_active,
            p.last_active,
            p.active_sec,
            p.focus_max_sec,
            p.cat_json,
            p.remind_cnt,
            p.done_cnt,
            p.location,
            p.interrupt_cnt,
            now_sec()
        ],
    )?;
    Ok(())
}

pub fn day_profiles(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<DayProfile>> {
    let mut stmt = conn.prepare(
        "SELECT day, first_active, last_active, active_sec, focus_max_sec,
                COALESCE(cat_json, ''), remind_cnt, done_cnt, COALESCE(location, ''),
                COALESCE(interrupt_cnt, 0)
         FROM day_profile ORDER BY day DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(DayProfile {
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
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/* ------------------------------ 位置轨迹 ------------------------------ */

#[derive(Debug, Clone, serde::Serialize)]
pub struct LocRow {
    pub id: i64,
    pub ts: i64,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub acc_m: Option<f64>,
    pub source: String,
    pub label: String,
    pub is_trip: i64,
}

/// 一次位置打点。收敛成结构体的原因同 NewReminder：
/// lat/lon/acc 三个 Option<f64> 靠位置区分，传反了编译器不报错。
pub struct NewLocation<'a> {
    pub ts: i64,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub acc_m: Option<f64>,
    /// geo / ip / manual / mock
    pub source: &'a str,
    pub label: &'a str,
    pub is_trip: bool,
}

/// 写一条位置记录。同一自然日内若最新一条已是同一标签则不重复新增，仅刷新 `is_trip` 与同日时间戳。
///
/// 修复跨日吞数据 bug：
/// 旧逻辑用 `ts > l.ts - 6 * 3600` 去重并执行 `UPDATE locations SET ts = ?2`，
/// 当电脑从白天一直开到次日凌晨（每 30 分钟采样一次，始终满足 <6h），
/// 会把昨天那唯一一条记录的 `ts` 一路往后推过午夜到次日，导致昨天的行程被彻底抹掉！
/// 现在严格限定在**当前生效时区的同一自然日 `[day_start, day_end)`** 内去重，绝不跨日覆盖。
pub fn insert_location(conn: &Connection, l: &NewLocation) -> rusqlite::Result<i64> {
    let is_trip = if l.is_trip { 1 } else { 0 };
    let offset = crate::tz::offset_secs();
    let local = l.ts + offset;
    let day_start = (local - local.rem_euclid(86_400)) - offset;
    let day_end = day_start + 86_400;

    let same_day_last: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, COALESCE(label, '') FROM locations
             WHERE ts >= ?1 AND ts < ?2
             ORDER BY ts DESC LIMIT 1",
            params![day_start, day_end],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    if let Some((last_id, last_label)) = same_day_last {
        if last_label == l.label {
            conn.execute(
                "UPDATE locations SET is_trip = ?1, ts = ?2 WHERE id = ?3",
                params![is_trip, l.ts, last_id],
            )?;
            return Ok(0);
        }
    }
    conn.execute(
        "INSERT INTO locations(ts, lat, lon, acc_m, source, label, is_trip)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![l.ts, l.lat, l.lon, l.acc_m, l.source, l.label, is_trip],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn recent_locations(conn: &Connection, since: i64, limit: i64) -> rusqlite::Result<Vec<LocRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, lat, lon, acc_m, source, COALESCE(label, ''), is_trip
         FROM locations WHERE ts >= ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![since, limit], |r| {
            Ok(LocRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                lat: r.get(2)?,
                lon: r.get(3)?,
                acc_m: r.get(4)?,
                source: r.get(5)?,
                label: r.get(6)?,
                is_trip: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 把一批位置记录聚成「按天 + 按地点」的行程：同一天同一地点只留一条。
/// 同时与 `day_profile` 的 `location` 做并集兜底，确保任何已记录在每日画像里的日期都不会缺失。
///
/// **不能只按天聚合**：一天里跑两个城市（早上在深圳、下午飞上海）是出差场景的常态，
/// 按天 GROUP 会随机丢掉一个，回答「那天我在哪」就错了一半。
pub fn location_days(
    conn: &Connection,
    since: i64,
    limit: i64,
) -> rusqlite::Result<Vec<(String, String, i64)>> {
    let tz_mod = crate::tz::sqlite_modifier();
    let since_day = crate::tz::date_str(since);
    let home = home_base(conn, since - 60 * 86_400)?.unwrap_or_default();
    let mut stmt = conn.prepare(
        "SELECT day, label, max(is_trip) AS trip
         FROM (
           SELECT substr(date(ts, 'unixepoch', ?3), 1, 10) AS day,
                  COALESCE(label, '') AS label,
                  is_trip,
                  ts AS sort_ts
           FROM locations
           WHERE ts >= ?1 AND COALESCE(label, '') <> ''
           UNION ALL
           SELECT day,
                  COALESCE(location, '') AS label,
                  CASE WHEN ?5 <> '' AND COALESCE(location, '') <> '' AND location <> ?5 THEN 1 ELSE 0 END AS is_trip,
                  updated_at AS sort_ts
           FROM day_profile
           WHERE day >= ?4 AND COALESCE(location, '') <> ''
         )
         GROUP BY day, label
         ORDER BY day DESC, min(sort_ts) DESC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![since, limit, tz_mod, since_day, home], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 常驻地：最近 window 天里出现天数最多的地点。
/// 没有位置数据时返回 None——绝不猜一个默认值（猜错会把"在家"判成"出差"）。
pub fn home_base(conn: &Connection, since: i64) -> rusqlite::Result<Option<String>> {
    let tz_mod = crate::tz::sqlite_modifier();
    let mut stmt = conn.prepare(
        "SELECT label, count(DISTINCT substr(date(ts,'unixepoch',?2),1,10)) AS d
         FROM locations WHERE ts >= ?1 AND COALESCE(label,'') <> ''
         GROUP BY label ORDER BY d DESC LIMIT 1",
    )?;
    let mut rows = stmt.query(params![since, tz_mod])?;
    match rows.next()? {
        Some(r) => Ok(Some(r.get(0)?)),
        None => Ok(None),
    }
}

pub fn clear_locations(conn: &Connection) -> rusqlite::Result<usize> {
    let n = conn.execute("DELETE FROM locations", [])?;
    conn.execute("UPDATE day_profile SET location = ''", [])?;
    Ok(n)
}

/* ------------------------------ 分层记忆（v7） ------------------------------ */

/// 一条记忆笔记。scope × period 唯一——同一天/同一月只留一条，重复生成是覆盖。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Note {
    pub id: i64,
    pub scope: String,
    pub period: String,
    pub ts: i64,
    pub source: String,
    pub summary: String,
    pub detail: String,
    pub weight: i64,
}

#[allow(clippy::too_many_arguments)]
pub fn upsert_note(
    conn: &Connection,
    scope: &str,
    period: &str,
    source: &str,
    summary: &str,
    detail: &str,
    weight: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO memory_notes(scope, period, ts, source, summary, detail, weight)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(scope, period) DO UPDATE SET
           ts = excluded.ts, source = excluded.source, summary = excluded.summary,
           detail = excluded.detail, weight = excluded.weight",
        params![scope, period, now_sec(), source, summary, detail, weight.clamp(1, 5)],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_notes(conn: &Connection, scope: &str, limit: i64) -> rusqlite::Result<Vec<Note>> {
    let mut stmt = conn.prepare(
        "SELECT id, scope, period, ts, source, summary, COALESCE(detail,''), weight
         FROM memory_notes WHERE scope = ?1 ORDER BY period DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![scope, limit], |r| {
            Ok(Note {
                id: r.get(0)?,
                scope: r.get(1)?,
                period: r.get(2)?,
                ts: r.get(3)?,
                source: r.get(4)?,
                summary: r.get(5)?,
                detail: r.get(6)?,
                weight: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_note(conn: &Connection, scope: &str, period: &str) -> rusqlite::Result<Option<Note>> {
    let mut stmt = conn.prepare(
        "SELECT id, scope, period, ts, source, summary, COALESCE(detail,''), weight
         FROM memory_notes WHERE scope = ?1 AND period = ?2",
    )?;
    let mut rows = stmt.query(params![scope, period])?;
    match rows.next()? {
        Some(r) => Ok(Some(Note {
            id: r.get(0)?,
            scope: r.get(1)?,
            period: r.get(2)?,
            ts: r.get(3)?,
            source: r.get(4)?,
            summary: r.get(5)?,
            detail: r.get(6)?,
            weight: r.get(7)?,
        })),
        None => Ok(None),
    }
}

/// 某个 period 区间内的笔记（把若干天的 short 汇总成 mid 时用）
pub fn notes_between(
    conn: &Connection,
    scope: &str,
    from: &str,
    to: &str,
) -> rusqlite::Result<Vec<Note>> {
    let mut stmt = conn.prepare(
        "SELECT id, scope, period, ts, source, summary, COALESCE(detail,''), weight
         FROM memory_notes WHERE scope = ?1 AND period >= ?2 AND period <= ?3
         ORDER BY period ASC",
    )?;
    let rows = stmt
        .query_map(params![scope, from, to], |r| {
            Ok(Note {
                id: r.get(0)?,
                scope: r.get(1)?,
                period: r.get(2)?,
                ts: r.get(3)?,
                source: r.get(4)?,
                summary: r.get(5)?,
                detail: r.get(6)?,
                weight: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 原件被子层吸收后即可删除——留着既不检索也不展示，纯占空间
pub fn purge_notes(conn: &Connection, scope: &str, before: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM memory_notes WHERE scope = ?1 AND period < ?2",
        params![scope, before],
    )
}

/// 打断计数：当天「连续专注 ≥5 分钟后中断」的次数。
///
/// 存在 settings 里而不是 day_profile：打断是**每 5 秒可能发生**的高频事件，
/// 每打断一次就 UPSERT 一次画像会把当天已算好的活跃时长覆盖回 0。
/// 由 build_day 在重算画像时读回来落表。
pub fn bump_interrupt(conn: &Connection, day: &str) -> rusqlite::Result<i64> {
    let k = format!("day.interrupt.{day}");
    let cur: i64 = get_setting(conn, &k)?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        + 1;
    set_setting(conn, &k, &cur.to_string())?;
    Ok(cur)
}

pub fn read_interrupt(conn: &Connection, day: &str) -> rusqlite::Result<i64> {
    let k = format!("day.interrupt.{day}");
    Ok(get_setting(conn, &k)?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0))
}

/// 清掉 N 天前的打断计数 key，避免 settings 表每天留一个垃圾键
pub fn purge_interrupt_keys(conn: &Connection, keep_days: i64) -> rusqlite::Result<usize> {
    let keep_from = crate::logs::bj_date(now_sec() - keep_days * 86_400);
    conn.execute(
        "DELETE FROM settings WHERE k LIKE 'day.interrupt.%' AND substr(k, 15) < ?1",
        params![keep_from],
    )
}

/* ------------------------------ 风险 ------------------------------ */

#[derive(Debug, Clone, serde::Serialize)]
pub struct RiskRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub level: i64,
    pub title: String,
    pub detail: String,
}

pub fn insert_risk(
    conn: &Connection,
    ts: i64,
    kind: &str,
    level: i64,
    title: &str,
    detail: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO risks(ts, kind, level, title, detail) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![ts, kind, level, title, detail],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn open_risks(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<RiskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, kind, level, title, COALESCE(detail, '')
         FROM risks WHERE resolved_at IS NULL ORDER BY level DESC, ts DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(RiskRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                level: r.get(3)?,
                title: r.get(4)?,
                detail: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 风险去重：同 kind 的未闭合风险只保留最新一条，避免每次扫描都堆一条。
pub fn replace_open_risk(
    conn: &Connection,
    ts: i64,
    kind: &str,
    level: i64,
    title: &str,
    detail: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "UPDATE risks SET resolved_at = ?1 WHERE kind = ?2 AND resolved_at IS NULL",
        params![ts, kind],
    )?;
    insert_risk(conn, ts, kind, level, title, detail)
}

pub fn clear_risk_kind(conn: &Connection, kind: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE risks SET resolved_at = ?1 WHERE kind = ?2 AND resolved_at IS NULL",
        params![now_sec(), kind],
    )
}

/* ============================ 大模型用量（v8） ============================ */

/// 一天的用量。tokens 全部按模型返回的 usage 累加；
/// 模型不返回 usage 时（少数网关），三个 token 字段保持 0，但 requests 仍会 +1。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UsageDay {
    pub day: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub failed: i64,
}

/// 累加一天的用量。传进来的都是**增量**，不是总量。
///
/// 写库只在这里做——调用方（llm.rs）拿不到 tauri::State，走的是 db::with_conn。
pub fn bump_usage(
    conn: &Connection,
    day: &str,
    prompt: i64,
    completion: i64,
    ok: bool,
) -> rusqlite::Result<()> {
    let total = prompt + completion;
    conn.execute(
        "INSERT INTO llm_usage(day, requests, prompt_tokens, completion_tok, total_tokens, failed)
         VALUES(?1, 1, ?2, ?3, ?4, ?5)
         ON CONFLICT(day) DO UPDATE SET
           requests       = requests + 1,
           prompt_tokens  = prompt_tokens + ?2,
           completion_tok = completion_tok + ?3,
           total_tokens   = total_tokens + ?4,
           failed         = failed + ?5",
        params![day, prompt, completion, total, if ok { 0 } else { 1 }],
    )?;
    Ok(())
}

/// 最近 N 天的用量（倒序）。无记录的日子不返回——前端自己补 0 画空档更真实。
pub fn usage_days(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<UsageDay>> {
    let mut stmt = conn.prepare(
        "SELECT day, requests, prompt_tokens, completion_tok, total_tokens, failed
         FROM llm_usage ORDER BY day DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(UsageDay {
                day: r.get(0)?,
                requests: r.get(1)?,
                prompt_tokens: r.get(2)?,
                completion_tokens: r.get(3)?,
                total_tokens: r.get(4)?,
                failed: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 某一天的用量（没有记录就返回全 0，而不是 None——前端不想处理空分支）
pub fn usage_of_day(conn: &Connection, day: &str) -> rusqlite::Result<UsageDay> {
    let mut d = UsageDay {
        day: day.to_string(),
        ..Default::default()
    };
    if let Ok((rq, p, c, t, f)) = conn.query_row(
        "SELECT requests, prompt_tokens, completion_tok, total_tokens, failed
         FROM llm_usage WHERE day = ?1",
        params![day],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        },
    ) {
        d.requests = rq;
        d.prompt_tokens = p;
        d.completion_tokens = c;
        d.total_tokens = t;
        d.failed = f;
    }
    Ok(d)
}
