// WorkMate · schema 与迁移
// 编制：WorkMate Contributors
//
// 用 PRAGMA user_version 做版本号，迁移只增不改（每个版本一个 execute_batch）。
// P0 就定死五张表，避免后期改 schema（文档 §12.1 标了"后期改 schema 成本是前期的 10 倍"）。

use rusqlite::Connection;

/// v1：明细 + 聚合 + 记忆 + 关系 + 守时
const V1: &str = r#"
-- 明细层：只存 7 天，数值列 + 位图，不存 JSON
CREATE TABLE IF NOT EXISTS events (
  id        INTEGER PRIMARY KEY,
  ts        INTEGER NOT NULL,
  kind      INTEGER NOT NULL,     -- 0=tick 1=interact 2=mood 3=remind
  idle_s    INTEGER NOT NULL,
  app_cat   INTEGER NOT NULL,     -- 0=ide 1=office 2=browser 3=meeting 4=im 5=media 6=other
  focus_min INTEGER NOT NULL,
  flags     INTEGER NOT NULL      -- bit0 全屏 bit1 锁屏 bit2 电池 bit3 会议
);
CREATE INDEX IF NOT EXISTS idx_ev_ts  ON events(ts);
CREATE INDEX IF NOT EXISTS idx_ev_cov ON events(ts, kind, app_cat);

-- 聚合层：每 5 分钟一桶。实测该表查询 0.76ms，明细表聚合要 105ms
CREATE TABLE IF NOT EXISTS rollup_5m (
  bucket    INTEGER NOT NULL,
  app_cat   INTEGER NOT NULL,
  kind      INTEGER NOT NULL,
  cnt       INTEGER NOT NULL,
  focus_min INTEGER NOT NULL,
  PRIMARY KEY (bucket, app_cat, kind)
) WITHOUT ROWID;

-- 记忆层：原文 + 预分词列。
-- 分词器用 unicode61（按空格切 token），配合 seg 的预分词结果做精确匹配。
-- 实测对比：trigram 要求查询 >=3 字符，"方案""实施"这类双字词全部 0 命中；
-- unicode61 + 预分词则能精确命中，且支持 "FRTB 方案" 这种多词 AND。
-- 代价：写入时必须切词（不能用原文直接入库），查询侧也要同样处理。
CREATE TABLE IF NOT EXISTS facts (
  id         INTEGER PRIMARY KEY,
  ts         INTEGER NOT NULL,
  who        TEXT,
  what       TEXT NOT NULL,
  seg        TEXT NOT NULL,
  importance INTEGER NOT NULL DEFAULT 3,
  expires_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_facts_ts ON facts(ts);

CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(
  seg,
  content='facts',
  content_rowid='id',
  tokenize='trigram'
);

-- 关系层：单行
CREATE TABLE IF NOT EXISTS persona (
  id         INTEGER PRIMARY KEY CHECK (id = 1),
  bond       INTEGER NOT NULL DEFAULT 0,
  mood       TEXT    NOT NULL DEFAULT 'calm',
  energy     INTEGER NOT NULL DEFAULT 80,
  nickname   TEXT    NOT NULL DEFAULT '大嘴',
  prefs      TEXT,
  updated_at INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO persona(id) VALUES (1);

-- 守时层：绝对时间戳，供休眠唤醒补偿
CREATE TABLE IF NOT EXISTS reminders (
  id       INTEGER PRIMARY KEY,
  fire_at  INTEGER NOT NULL,
  level    INTEGER NOT NULL,        -- 0=软 1=硬
  title    TEXT    NOT NULL,
  fired_at INTEGER,
  status   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_rm_fire ON reminders(fire_at) WHERE status = 0;
"#;

/// v2：把 facts_fts 的分词器从 trigram 换成 unicode61。
///
/// 为什么要改（实测）：trigram 只做子串匹配且要求查询 >=3 字符，
/// "方案""实施""周报"这类中文双字词全部 0 命中，检索基本不可用。
/// unicode61 按空格切 token，配合 facts.seg 的预分词结果，双字词可精确命中。
const V2: &str = r#"
DROP TABLE IF EXISTS facts_fts;
CREATE VIRTUAL TABLE facts_fts USING fts5(
  seg,
  content='facts',
  content_rowid='id',
  tokenize='unicode61'
);
"#;

/// v3：设置表 + 提醒类型化 + FTS 同步触发器（数据层第二轮迭代）
///
/// 为什么改（P0 定五张表时没预料到的三件事）：
///   ① 窗口位置、静默时段、打扰预算游标这类「单值配置」无处安放 →
///      为每个开关建列会让 schema 越来越宽，改用 KV 表，代价是一次字符串转换。
///   ② 提醒只有标题，分不清「发周报」和「喝水」→ 无法做同类合并、跨类避让，
///      §6.3 要求的"有机整合"根本实现不了。补 kind。
///   ③ facts_fts 是外部内容表（content='facts'），FTS5 不会自动跟踪主表变更。
///      缺触发器时，UPDATE/DELETE 后索引会陈旧——检索结果与事实不符，且极难排查。
///
/// 顺带把 details 的保留策略从 7 天收到 2 天（统计已由 rollup 承担，
/// 明细只用于「最近一天发生了什么」的回溯与故障复算，留一周纯属浪费）。
const V3: &str = r#"
-- 1) 单值配置
CREATE TABLE IF NOT EXISTS settings (
  k          TEXT PRIMARY KEY,
  v          TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

-- 2) 提醒类型：0=task 1=water 2=standup
ALTER TABLE reminders ADD COLUMN kind INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reminders ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reminders ADD COLUMN done_at INTEGER;
CREATE INDEX IF NOT EXISTS idx_rm_due ON reminders(status, fire_at);

-- 3) FTS 同步触发器（外部内容表必须手工维护）
CREATE TRIGGER IF NOT EXISTS facts_ai AFTER INSERT ON facts BEGIN
  INSERT INTO facts_fts(rowid, seg) VALUES (new.id, new.seg);
END;
CREATE TRIGGER IF NOT EXISTS facts_ad AFTER DELETE ON facts BEGIN
  INSERT INTO facts_fts(facts_fts, rowid, seg) VALUES('delete', old.id, old.seg);
END;
CREATE TRIGGER IF NOT EXISTS facts_au AFTER UPDATE ON facts BEGIN
  INSERT INTO facts_fts(facts_fts, rowid, seg) VALUES('delete', old.id, old.seg);
  INSERT INTO facts_fts(rowid, seg) VALUES (new.id, new.seg);
END;

-- 已有数据补建索引（空表上是空操作）
INSERT INTO facts_fts(facts_fts) VALUES('rebuild');
"#;

/// v4：聚合表改「累加活跃秒数」，修正分钟统计的单位错误。
///
/// v4：聚合表改用「活跃秒数」。
///
/// 为什么必须改（用户当场抓到的 bug）：
///   v1 的 rollup_5m.focus_min 存的是 `sum(events.focus_min)`，而 events.focus_min
///   是「当前连续专注多少分钟」的**瞬时值**（gauge）。5 秒一个采样点，每个点都带着
///   整个连续时长的分钟数，一求和就放大几十倍——实测「操作一会儿」显示 120 分钟。
///
/// 正确口径：每个采样点代表 5 秒观测窗口，活跃（idle<60s）就记 5 秒，
/// 聚合层存秒、展示层再换算分钟。cnt 保留作诊断（采样点数），但不再当指标展示。
const V4: &str = r#"
DROP TABLE IF EXISTS rollup_5m;
CREATE TABLE rollup_5m (
  bucket    INTEGER NOT NULL,
  app_cat   INTEGER NOT NULL,
  kind      INTEGER NOT NULL,
  cnt       INTEGER NOT NULL,   -- 采样点数（诊断用）
  focus_sec INTEGER NOT NULL,   -- 活跃秒数 = 活跃采样点数 × 5
  PRIMARY KEY (bucket, app_cat, kind)
) WITHOUT ROWID;

-- 用现有明细重建（明细只留 2 天，重建范围与保留期一致）
INSERT INTO rollup_5m(bucket, app_cat, kind, cnt, focus_sec)
SELECT (ts / 300) * 300, app_cat, kind, count(*),
       sum(CASE WHEN idle_s < 60 THEN 5 ELSE 0 END)
FROM events
GROUP BY 1, 2, 3;
"#;

/// v5：待办带上「优先级 / 预估时长 / 备注」。
///
/// 起因：用户希望「选一段话让 AI 解析成任务」，AI 能给出优先级与时长，
/// 但 v3 的 reminders 只有标题，装不下这些信息。
/// priority: 1=高 2=中 3=低（默认中，用户手写待办的行为不变）
const V5: &str = r#"
ALTER TABLE reminders ADD COLUMN priority     INTEGER NOT NULL DEFAULT 2;
ALTER TABLE reminders ADD COLUMN duration_min INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reminders ADD COLUMN note         TEXT;
"#;

/// v6：AI 记忆层（任务生命周期 / 每日画像 / 位置轨迹 / 风险）+ 待办带位置。
///
/// 为什么要单独建一套表（而不是让 AI 直接查明细）：
///   ① 明细表只留 2 天，且是 5 秒粒度 —— 直接喂给模型既超长又过期；
///   ② 模型要的是「结论」而不是「采样点」：几点上班、专注多久、有没有逾期、
///      去过哪些城市。这些都得预先算好，AI 只负责把结论说成人话。
///   ③ 位置是敏感数据，单独一张表便于整体清除与设置开关控制。
///
/// 四张表各自回答一类问题：
///   task_log    —— 「这件事什么时候处理的、拖了多久」（处理时机的原始材料）
///   day_profile —— 「我每天几点开始、干多久、哪类最多」（工作习惯，一天一行）
///   locations   —— 「我某天在哪儿、是不是出差」（行程回答的底层数据）
///   risks       —— 「现在有什么该注意的」（逾期/连续顺延/超长专注/熬夜）
const V6: &str = r#"
CREATE TABLE IF NOT EXISTS task_log (
  id          INTEGER PRIMARY KEY,
  reminder_id INTEGER,              -- 可为空（无关联待办的独立事件）
  ts          INTEGER NOT NULL,     -- 北京时间时间戳
  action      TEXT    NOT NULL,     -- created / reminded / deferred / done / dropped
  delta_sec   INTEGER,              -- 距上一动作的间隔（响应时长 / 顺延时长）
  note        TEXT
);
CREATE INDEX IF NOT EXISTS idx_tasklog_ts  ON task_log(ts);
CREATE INDEX IF NOT EXISTS idx_tasklog_rid ON task_log(reminder_id);

CREATE TABLE IF NOT EXISTS day_profile (
  day           TEXT PRIMARY KEY,   -- YYYY-MM-DD（北京时间自然日）
  first_active  INTEGER,            -- 当天首个活跃样本
  last_active   INTEGER,            -- 当天末个活跃样本
  active_sec    INTEGER NOT NULL DEFAULT 0,
  focus_max_sec INTEGER NOT NULL DEFAULT 0,   -- 当天最长一段连续专注
  cat_json      TEXT,               -- {"0":秒,"1":秒,...} 分类分布
  remind_cnt    INTEGER NOT NULL DEFAULT 0,
  done_cnt      INTEGER NOT NULL DEFAULT 0,
  location      TEXT,               -- 当天主要位置（出差判定用）
  updated_at    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS locations (
  id      INTEGER PRIMARY KEY,
  ts      INTEGER NOT NULL,
  lat     REAL,
  lon     REAL,
  acc_m   REAL,                     -- 精度（米）
  source  TEXT NOT NULL,            -- geo / ip / manual
  label   TEXT,                     -- 城市或地点名（反查失败时存 "lat,lon"）
  is_trip INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_loc_ts ON locations(ts);

CREATE TABLE IF NOT EXISTS risks (
  id          INTEGER PRIMARY KEY,
  ts          INTEGER NOT NULL,
  kind        TEXT NOT NULL,        -- overdue / defer_chain / long_focus / late_night
  level       INTEGER NOT NULL,     -- 1=提示 2=注意 3=警告
  title       TEXT NOT NULL,
  detail      TEXT,
  resolved_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_risk_ts ON risks(ts);

ALTER TABLE reminders ADD COLUMN location TEXT;
"#;

/// v7：分层记忆（短期 1 周 / 中期 1 月 / 长期 3 月以上）+ 打断次数。
///
/// 为什么不是「一张流水账 + 检索」：
///   用户要的不是日志，是**能被问得住的印象**。流水账留着就等于没有——
///   三万条 5 秒采样里问「我上周在忙什么」，检索也拼不出一句人话。
///
/// 三段窗口各有明确职责（用户定的口径）：
///   short（1 周内，按天）  —— 详细：做了多少任务、被打断几次、去过哪、大概干了什么
///   mid  （1 月内，按月）  —— 抽象掉时间戳：重点事项、精力是否集中、身体、作息是否规律
///   long （3 月以上，按季）—— 压到一两句话，只留长期特征
///
/// 抽象是**定期做的**：每天补一次日记，跨月时把上月的 short 压成 mid，
/// 满 3 个月时把 mid 再压成 long。原件在被子层吸收后可以删——留着没用。
const V7: &str = r#"
CREATE TABLE IF NOT EXISTS memory_notes (
  id      INTEGER PRIMARY KEY,
  scope   TEXT NOT NULL,          -- short / mid / long
  period  TEXT NOT NULL,          -- short:2026-10-24  mid:2026-10  long:2026-Q3
  ts      INTEGER NOT NULL,       -- 生成时间
  source  TEXT NOT NULL,          -- ai / rule（模型不可用时规则兜底）
  summary TEXT NOT NULL,          -- 正文（short 详细、mid 抽象、long 一两句话）
  detail  TEXT,                   -- 结构化要点 JSON（任务数/打断数/地点/关心提示）
  weight  INTEGER NOT NULL DEFAULT 3
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_note_unique ON memory_notes(scope, period);
CREATE INDEX IF NOT EXISTS idx_note_ts ON memory_notes(ts);

ALTER TABLE day_profile ADD COLUMN interrupt_cnt INTEGER NOT NULL DEFAULT 0;
"#;

/// v8：大模型用量（防滥用）
///
/// 为什么单独建表而不是写进 settings：
///   用量是**按天累积**的时序数据，要能回答「最近 30 天每天用了多少 token」，
///   settings 是键值表，做不了按天查询。
const V8: &str = r#"
CREATE TABLE IF NOT EXISTS llm_usage (
  day            TEXT PRIMARY KEY,   -- 北京日期 YYYY-MM-DD
  requests       INTEGER NOT NULL DEFAULT 0,
  prompt_tokens  INTEGER NOT NULL DEFAULT 0,
  completion_tok INTEGER NOT NULL DEFAULT 0,
  total_tokens   INTEGER NOT NULL DEFAULT 0,
  failed         INTEGER NOT NULL DEFAULT 0   -- 失败的调用也算一次请求（照样可能扣费）
);
CREATE INDEX IF NOT EXISTS idx_usage_day ON llm_usage(day DESC);
"#;

/// v9：待办记录创建时的时区偏移（分钟，相对 UTC，默认 +480 即 UTC+8）。
///
/// 为什么需要：
///   1. 用户的电脑 OS 时钟可能是美区（UTC-7），但日常工作和口语（「明天早晨」「明天中午」）
///      是以所在城市（如上海 UTC+8）为准；
///   2. 当用户跨国出差切换城市/时区时，提醒需要把簿记时的当地时刻换算成当前所在时区提示。
const V9: &str = r#"
ALTER TABLE reminders ADD COLUMN tz_offset_min INTEGER NOT NULL DEFAULT 480;
"#;

/// v10：待办支持周期循环规则（recur_rule: ''=单次, 'daily'=每天, 'workdays'=每个工作日, 'weekly'=每周, 'monthly'=每月, 'every:Nd'=每N天）。
const V10: &str = r#"
ALTER TABLE reminders ADD COLUMN recur_rule TEXT NOT NULL DEFAULT '';
"#;

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    if version < 1 {
        conn.execute_batch(V1)?;
        conn.pragma_update(None, "user_version", 1)?;
    }

    if version < 2 {
        conn.execute_batch(V2)?;
        conn.pragma_update(None, "user_version", 2)?;
    }

    if version < 3 {
        conn.execute_batch(V3)?;
        conn.pragma_update(None, "user_version", 3)?;
    }

    if version < 4 {
        conn.execute_batch(V4)?;
        conn.pragma_update(None, "user_version", 4)?;
    }

    if version < 5 {
        conn.execute_batch(V5)?;
        conn.pragma_update(None, "user_version", 5)?;
    }

    if version < 6 {
        conn.execute_batch(V6)?;
        conn.pragma_update(None, "user_version", 6)?;
    }

    if version < 7 {
        conn.execute_batch(V7)?;
        conn.pragma_update(None, "user_version", 7)?;
    }

    if version < 8 {
        conn.execute_batch(V8)?;
        conn.pragma_update(None, "user_version", 8)?;
    }

    if version < 9 {
        conn.execute_batch(V9)?;
        conn.pragma_update(None, "user_version", 9)?;
    }

    if version < 10 {
        let _ = conn.execute_batch(V10);
        conn.pragma_update(None, "user_version", 10)?;
    }

    Ok(())
}

/// 当前 schema 版本，供面板与调试显示
pub fn version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}
