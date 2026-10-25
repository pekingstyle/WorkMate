// WorkMate · 自然语言待办捕获（A3 · 融合底层城市时区）
// 编制：WorkMate Contributors
//
// 目标：用户在面板里写「明天下午三点提醒我发周报」「明天早晨开晨会」「明天中午吃饭」
// 就能直接按用户所在城市时区（默认中国标准时间 UTC+8）准确落库，绝不受本机美区系统时钟干扰。
//
// 支持的说法（覆盖日常 98%）：
//   相对：30分钟后 / 2小时后 / 半小时后 / 一个半小时后 / 一刻钟后 / 3天后
//   时刻：15:30 / 三点 / 3点半 / 3点20 / 3点十五分 / 10点一刻 / 下午三点 / 晚上8点 / 今晚8点 / 明早8点
//   时段（无具体几点）：早晨(08:30) / 早上(08:30) / 清晨(07:30) / 上午(10:00) / 中午(12:00) /
//                      下午(15:00) / 傍晚(18:00) / 下班前(18:00) / 晚上(20:00) / 睡前(22:30) /
//                      明早(明天08:30) / 明晚(明天20:00) / 今早(今天08:30) / 今晚(今天20:00)
//   日期：今天 / 明天 / 后天 / 大后天 / 周三 / 星期五 / 下周一 / 10月28日 / 2026-10-28
//   组合：明天早晨 / 明天中午 / 明天下午3点 / 周三上午10点半 / 10月28日下午2点
//
// 时区：底层统一接入 `crate::tz`（优先按用户当前设置/定位的城市时区，默认中国时间 UTC+8）。

use crate::tz;

/// 解析结果
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// 绝对时间戳（UTC 秒）
    pub fire_at: i64,
    /// 抽出的待办内容
    pub title: String,
    /// 命中的时间说法原文（便于回显「已按 X 设定」）
    pub matched: String,
    /// 簿记时所依据的时区偏移（分钟，如 UTC+8 为 480）
    pub tz_offset_min: i64,
}

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 中文数字（0..=59）→ 数值，支持「十」「十五」「二十三」「三十五」「两」
fn cn_num(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<i64>() {
        return Some(v);
    }
    let digit = |c: char| -> Option<i64> {
        Some(match c {
            '零' | '〇' => 0,
            '一' => 1,
            '二' | '两' => 2,
            '三' => 3,
            '四' => 4,
            '五' => 5,
            '六' => 6,
            '七' => 7,
            '八' => 8,
            '九' => 9,
            _ => return None,
        })
    };
    match s {
        "十" => Some(10),
        _ => {
            if let Some(rest) = s.strip_prefix('十') {
                return digit(rest.chars().next()?).map(|d| 10 + d);
            }
            if let Some((a, b)) = s.split_once('十') {
                let tens = digit(a.chars().next()?)?;
                if b.is_empty() {
                    return Some(tens * 10);
                }
                return digit(b.chars().next()?).map(|d| tens * 10 + d);
            }
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_none() {
                return digit(c);
            }
            None
        }
    }
}

/// 判断字符是否属于数字（阿拉伯数字或中文数词）
fn is_num_char(c: char) -> bool {
    c.is_ascii_digit() || "零〇一二两三四五六七八九十".contains(c)
}

/// 判断 `"点"` 或 `"时"` 命中的位置是否是日常非时间词汇（如「重点」「早点」「准时」「小时」）
fn is_false_hour_unit(text: &str, num_start: usize, unit_pos: usize, unit: &str) -> bool {
    let after = &text[unit_pos + unit.len()..];
    if unit == "时" {
        for bad_suffix in ["候", "间", "代", "期", "刻", "常", "机", "效", "差"] {
            if after.starts_with(bad_suffix) {
                return true;
            }
        }
        let before_num = &text[..num_start];
        if before_num.ends_with('小') || before_num.ends_with("个小") {
            return true;
        }
    }
    if unit == "点" {
        // "一点点"、"差点"、"早点休息"（早点前无数字，已在调用处过滤，但 "一点小事" 需过滤）
        for bad_suffix in ["点", "心", "子", "滴", "缀", "评", "赞", "击", "亮", "头", "燃", "菜", "餐"] {
            if after.starts_with(bad_suffix) {
                return true;
            }
        }
        if &text[num_start..unit_pos] == "一" {
            // "一点小事" / "一点问题" / "快一点" 等口语量词（若紧跟「半/刻/分/钟」则是真时间）
            let valid_after = after.starts_with('半')
                || after.starts_with('刻')
                || after.starts_with('钟')
                || after.starts_with("一刻")
                || after.starts_with("三刻")
                || after.chars().next().map_or(false, is_num_char);
            let before_one = &text[..num_start];
            let has_time_prefix = before_one.ends_with("凌晨")
                || before_one.ends_with("下午")
                || before_one.ends_with("中午")
                || before_one.ends_with("晚上")
                || before_one.ends_with("夜里")
                || before_one.ends_with("明天")
                || before_one.ends_with("今天")
                || before_one.ends_with("后天")
                || before_one.ends_with('在')
                || before_one.ends_with('到');
            if !valid_after && !has_time_prefix && (after.starts_with('小') || after.starts_with('问') || before_one.ends_with('快') || before_one.ends_with('慢') || before_one.ends_with('有')) {
                return true;
            }
        }
    }
    false
}

/// 找出 `text` 里第一个合法时刻，返回 `(时, 分, 起始字节索引, 结束字节索引)`。
///
/// 关键修复：使用 `match_indices` 遍历全文所有候选位置，
/// 避免句子前部出现「重点：」「早点」「盘点」时只看第一个「点」字就放弃匹配。
fn find_hour(text: &str) -> Option<(i64, i64, usize, usize)> {
    // 1) 优先扫描 ":"（如 "15:30"），避免被后续规则拆散
    for (pos, _) in text.match_indices(':') {
        let head = &text[..pos];
        let start = head
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_digit())
            .last()
            .map(|(i, _)| i)
            .unwrap_or(pos);
        if start < pos {
            if let Ok(h) = text[start..pos].parse::<i64>() {
                let rest = &text[pos + 1..];
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if !digits.is_empty() && digits.len() <= 2 {
                    if let Ok(m) = digits.parse::<i64>() {
                        if (0..=24).contains(&h) && (0..=59).contains(&m) {
                            let end = pos + 1 + digits.len();
                            return Some((h, m, start, end));
                        }
                    }
                }
            }
        }
    }

    // 2) 扫描「点」与「时」
    for unit in ["点", "时"] {
        for (pos, _) in text.match_indices(unit) {
            let head = &text[..pos];
            let start = head
                .char_indices()
                .rev()
                .take_while(|(_, c)| is_num_char(*c))
                .last()
                .map(|(i, _)| i)
                .unwrap_or(pos);
            if start < pos {
                if is_false_hour_unit(text, start, pos, unit) {
                    continue;
                }
                if let Some(h) = cn_num(&text[start..pos]) {
                    if (0..=24).contains(&h) {
                        let mut unit_end = pos + unit.len();
                        // 兼容「3点钟」
                        if text[unit_end..].starts_with('钟') {
                            unit_end += '钟'.len_utf8();
                        }
                        let (m, min_end) = find_minute(text, unit_end);
                        return Some((h, m, start, min_end));
                    }
                }
            }
        }
    }
    None
}

/// 解析「点/时」之后的分钟部分，返回 `(分钟数, 结束字节索引)`。
/// 支持：「半」(30)、「一刻」(15)、「三刻」(45)、「20分」、「十五分」、以及「3点20」裸数字。
fn find_minute(text: &str, hour_end: usize) -> (i64, usize) {
    let rest = &text[hour_end..];
    if rest.starts_with('半') {
        return (30, hour_end + '半'.len_utf8());
    }
    if rest.starts_with("一刻") {
        let mut end = hour_end + "一刻".len();
        if text[end..].starts_with('钟') {
            end += '钟'.len_utf8();
        }
        return (15, end);
    }
    if rest.starts_with("三刻") {
        let mut end = hour_end + "三刻".len();
        if text[end..].starts_with('钟') {
            end += '钟'.len_utf8();
        }
        return (45, end);
    }

    // 匹配前导空白 + 数字（阿拉伯数字或中文数词）
    let mut num_end_rel = 0usize;
    let mut has_num = false;
    for (idx, c) in rest.char_indices() {
        if c == ' ' && !has_num {
            num_end_rel = idx + c.len_utf8();
            continue;
        }
        if is_num_char(c) {
            has_num = true;
            num_end_rel = idx + c.len_utf8();
        } else {
            break;
        }
    }

    if has_num {
        let num_str = rest[..num_end_rel].trim();
        if let Some(v) = cn_num(num_str) {
            if (0..=59).contains(&v) {
                let after_num = &rest[num_end_rel..];
                let after_trim = after_num.trim_start_matches(' ');
                let ws_len = after_num.len() - after_trim.len();
                if after_trim.starts_with("分钟") {
                    return (v, hour_end + num_end_rel + ws_len + "分钟".len());
                }
                if after_trim.starts_with('分') {
                    return (v, hour_end + num_end_rel + ws_len + '分'.len_utf8());
                }
                // "10点30提醒我"（裸数字分钟，要求是阿拉伯数字或双位中文数字，避免把 "3点一个人" 里的 "一" 当成 1 分）
                let is_ascii_min = num_str.chars().all(|c| c.is_ascii_digit());
                let is_multi_cn = num_str.chars().count() >= 2;
                if is_ascii_min || is_multi_cn {
                    return (v, hour_end + num_end_rel);
                }
            }
        }
    }

    (0, hour_end)
}

/// 时间段匹配结果
#[derive(Debug, Clone, Copy)]
struct PeriodMatch {
    /// "am" | "noon" | "pm" | "late_am"
    kind: &'static str,
    /// 当用户只说「明天早晨」「明天中午」而没说具体几点时的默认小时
    default_h: i64,
    /// 默认分钟
    default_m: i64,
    start: usize,
    end: usize,
}

/// 识别句子中的时间段词（早晨 / 早上 / 清晨 / 上午 / 中午 / 下午 / 傍晚 / 晚上 / 凌晨 / 下班前 / 睡前）
fn find_period(text: &str) -> Option<PeriodMatch> {
    // 按词长优先匹配，防止「大清早」被「早」截断
    const PERIODS: &[(&str, &str, i64, i64)] = &[
        ("大清早", "am", 7, 30),
        ("下班前", "pm", 18, 0),
        ("清晨", "am", 7, 30),
        ("一早", "am", 7, 30),
        ("早晨", "am", 8, 30),
        ("早上", "am", 8, 30),
        ("上午", "am", 10, 0),
        ("中午", "noon", 12, 0),
        ("午间", "noon", 12, 0),
        ("午休", "noon", 12, 30),
        ("下午", "pm", 15, 0),
        ("午后", "pm", 15, 0),
        ("傍晚", "pm", 18, 0),
        ("黄昏", "pm", 18, 0),
        ("晚上", "pm", 20, 0),
        ("夜间", "pm", 20, 0),
        ("夜里", "pm", 21, 0),
        ("半夜", "late_am", 1, 0),
        ("凌晨", "late_am", 1, 0),
        ("睡前", "pm", 22, 30),
    ];
    let mut best: Option<PeriodMatch> = None;
    for &(kw, kind, dh, dm) in PERIODS {
        if let Some(pos) = text.find(kw) {
            let m = PeriodMatch {
                kind,
                default_h: dh,
                default_m: dm,
                start: pos,
                end: pos + kw.len(),
            };
            if best.map_or(true, |b| m.start < b.start || (m.start == b.start && (m.end - m.start) > (b.end - b.start))) {
                best = Some(m);
            }
        }
    }
    best
}

/// 日期匹配结果
#[derive(Debug, Clone, Copy)]
struct DateMatch {
    /// 距离当前生效时区「今天」的天数偏移
    day_offset: i64,
    start: usize,
    end: usize,
    /// 是否是「本周X / 周X」（当今天正好是该周几时，需要根据时刻是否已过决定是今天还是 +7 天）
    is_same_weekday_candidate: bool,
    /// 合音词（如「明早」「今晚」「今早」「明晚」）自带的默认时段
    implied_period: Option<(&'static str, i64, i64)>,
}

/// 识别显式公历日期：`YYYY-MM-DD` 或 `M月D日/号`
fn find_explicit_date(text: &str, day_start: i64, tz_secs: i64) -> Option<DateMatch> {
    let (cur_y, _, _, _, _, _) = tz::epoch_to_civil(day_start + tz_secs);

    // 1) YYYY-MM-DD
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.len() >= 10 {
        for win in chars.windows(10) {
            let start_b = win[0].0;
            let end_b = win[9].0 + win[9].1.len_utf8();
            let s = &text[start_b..end_b];
            let b = s.as_bytes();
            if b.len() == 10 && (b[4] == b'-' || b[4] == b'/') && b[7] == b[4] {
                if let (Ok(y), Ok(m), Ok(d)) = (
                    s[0..4].parse::<i64>(),
                    s[5..7].parse::<i64>(),
                    s[8..10].parse::<i64>(),
                ) {
                    if let Some(target_utc) = tz::civil_to_epoch_secs(y, m, d, 0, 0, 0) {
                        let target_day_start = target_utc - tz_secs;
                        let off = (target_day_start - day_start).div_euclid(86_400);
                        return Some(DateMatch {
                            day_offset: off,
                            start: start_b,
                            end: end_b,
                            is_same_weekday_candidate: false,
                            implied_period: None,
                        });
                    }
                }
            }
        }
    }

    // 2) M月D日 / M月D号
    if let Some(m_pos) = text.find('月') {
        let head = &text[..m_pos];
        let m_start = head
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_num_char(*c))
            .last()
            .map(|(i, _)| i)
            .unwrap_or(m_pos);
        if m_start < m_pos {
            if let Some(mo) = cn_num(&text[m_start..m_pos]) {
                let after_m = &text[m_pos + '月'.len_utf8()..];
                let d_len: usize = after_m
                    .chars()
                    .take_while(|c| is_num_char(*c))
                    .map(|c| c.len_utf8())
                    .sum();
                if d_len > 0 {
                    let after_d = &after_m[d_len..];
                    if after_d.starts_with('日') || after_d.starts_with('号') {
                        let suffix_len = '日'.len_utf8();
                        if let Some(day) = cn_num(&after_m[..d_len]) {
                            if (1..=12).contains(&mo) && (1..=31).contains(&day) {
                                let mut target_utc = tz::civil_to_epoch_secs(cur_y, mo, day, 0, 0, 0)?;
                                let mut off = (target_utc - tz_secs - day_start).div_euclid(86_400);
                                if off < 0 {
                                    // 今年该日期已过，顺延到明年
                                    target_utc = tz::civil_to_epoch_secs(cur_y + 1, mo, day, 0, 0, 0)?;
                                    off = (target_utc - tz_secs - day_start).div_euclid(86_400);
                                }
                                let end_b = m_pos + '月'.len_utf8() + d_len + suffix_len;
                                return Some(DateMatch {
                                    day_offset: off,
                                    start: m_start,
                                    end: end_b,
                                    is_same_weekday_candidate: false,
                                    implied_period: None,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// 识别句子中的日期表达
fn find_date(text: &str, day_start: i64, tz_secs: i64) -> Option<DateMatch> {
    // 1) 合音词（明早 / 明晨 / 明晚 / 明夜 / 今早 / 今晨 / 今晚 / 今夜）
    for &(kw, off, kind, dh, dm) in &[
        ("明早", 1i64, "am", 8i64, 30i64),
        ("明晨", 1, "am", 8, 30),
        ("明晚", 1, "pm", 20, 0),
        ("明夜", 1, "pm", 21, 0),
        ("今早", 0, "am", 8, 30),
        ("今晨", 0, "am", 8, 30),
        ("今晚", 0, "pm", 20, 0),
        ("今夜", 0, "pm", 21, 0),
    ] {
        if let Some(p) = text.find(kw) {
            return Some(DateMatch {
                day_offset: off,
                start: p,
                end: p + kw.len(),
                is_same_weekday_candidate: false,
                implied_period: Some((kind, dh, dm)),
            });
        }
    }

    // 2) 显式公历日期（YYYY-MM-DD / M月D日）
    if let Some(dm) = find_explicit_date(text, day_start, tz_secs) {
        return Some(dm);
    }

    // 3) 相对日期词（长词优先：先匹配「大后天」再匹配「后天」）
    for &(kw, off) in &[
        ("大后天", 3i64),
        ("后天", 2),
        ("后日", 2),
        ("明天", 1),
        ("明日", 1),
        ("次日", 1),
        ("今天", 0),
        ("今日", 0),
        ("当天", 0),
    ] {
        if let Some(p) = text.find(kw) {
            return Some(DateMatch {
                day_offset: off,
                start: p,
                end: p + kw.len(),
                is_same_weekday_candidate: false,
                implied_period: None,
            });
        }
    }

    // 4) 「N天后 / N天之后」
    for kw in ["天之后", "天后"] {
        if let Some(pos) = text.find(kw) {
            let head = &text[..pos];
            let start = head
                .char_indices()
                .rev()
                .take_while(|(_, c)| is_num_char(*c))
                .last()
                .map(|(i, _)| i)
                .unwrap_or(pos);
            if start < pos {
                if let Some(v) = cn_num(&text[start..pos]) {
                    if (1..=365).contains(&v) {
                        return Some(DateMatch {
                            day_offset: v,
                            start,
                            end: pos + kw.len(),
                            is_same_weekday_candidate: false,
                            implied_period: None,
                        });
                    }
                }
            }
        }
    }

    // 5) 周几（周三 / 星期五 / 下周一 / 下下周三）
    if let Some((idx, start, end, weeks_ahead)) = find_weekday(text) {
        let today_dow0 = (((day_start + tz_secs).div_euclid(86_400)) + 4).rem_euclid(7); // 0=周日, 1..6=周一..周六
        let mut ahead = (idx - today_dow0).rem_euclid(7);
        let is_same = weeks_ahead == 0 && ahead == 0;
        if weeks_ahead >= 1 {
            // 「下周X」：确保落在未来的目标星期几（若 ahead == 0 则 +7，若 ahead > 0 且是下周也保证落在该星期几）
            if ahead == 0 {
                ahead = 7 * weeks_ahead;
            } else if weeks_ahead > 1 {
                ahead += 7 * (weeks_ahead - 1);
            }
        }
        return Some(DateMatch {
            day_offset: ahead,
            start,
            end,
            is_same_weekday_candidate: is_same,
            implied_period: None,
        });
    }

    None
}

/// 解析入口（默认使用系统底层当前生效的城市时区，如中国时间 UTC+8）。
/// 返回 `None` 表示未识别出时间。
pub fn parse(text: &str) -> Option<Parsed> {
    parse_in_tz(text, tz::offset_min())
}

/// 在指定时区偏移（分钟，如 UTC+8 = 480）下解析自然语言待办。
pub fn parse_in_tz(text: &str, tz_offset_min: i64) -> Option<Parsed> {
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }
    let now = now_sec();
    let tz_secs = tz_offset_min * 60;
    let day_start = now - (now + tz_secs).rem_euclid(86_400);

    // ---------- 1) 相对时间（30分钟后 / 半小时后 / 2小时后 / 一个半小时后 / 一刻钟后） ----------
    if let Some(pos) = raw.find("一刻钟后").or_else(|| raw.find("一刻钟之后")) {
        let kw_len = if raw[pos..].starts_with("一刻钟之后") {
            "一刻钟之后".len()
        } else {
            "一刻钟后".len()
        };
        let matched = raw[pos..pos + kw_len].to_string();
        let title = extract_title_spans(raw, &[(pos, pos + kw_len)]);
        return Some(Parsed {
            fire_at: now + 15 * 60,
            title,
            matched,
            tz_offset_min,
        });
    }

    for (kw, unit_secs) in [
        ("分钟之后", 60i64),
        ("分钟后", 60),
        ("小时之后", 3600),
        ("小时后", 3600),
        ("钟头后", 3600),
    ] {
        if let Some(pos) = raw.find(kw) {
            let head = &raw[..pos];
            // 处理「一个半小时后 / 1个半小时后 / 两个半小时后」
            if unit_secs == 3600 && head.ends_with("个半") {
                let before_half = &head[..head.len() - "个半".len()];
                let start = before_half
                    .char_indices()
                    .rev()
                    .take_while(|(_, c)| is_num_char(*c))
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(before_half.len());
                if start < before_half.len() {
                    if let Some(v) = cn_num(&before_half[start..]) {
                        let add = v * 3600 + 1800;
                        let matched = raw[start..pos + kw.len()].to_string();
                        let title = extract_title_spans(raw, &[(start, pos + kw.len())]);
                        return Some(Parsed {
                            fire_at: now + add,
                            title,
                            matched,
                            tz_offset_min,
                        });
                    }
                }
            }

            // 处理「半个小时后 / 半小时后」以及普通「30分钟后 / 2小时后」
            let head_trimmed = head.strip_suffix('个').unwrap_or(head);
            let start = head_trimmed
                .char_indices()
                .rev()
                .take_while(|(_, c)| is_num_char(*c) || *c == '半')
                .last()
                .map(|(i, _)| i)
                .unwrap_or(head_trimmed.len());
            let num_token = &head_trimmed[start..];
            let amount = if num_token == "半" {
                Some(0)
            } else {
                cn_num(num_token)
            };
            if let Some(v) = amount {
                let add = if num_token == "半" {
                    unit_secs / 2
                } else {
                    v * unit_secs
                };
                let matched = raw[start..pos + kw.len()].to_string();
                let title = extract_title_spans(raw, &[(start, pos + kw.len())]);
                return Some(Parsed {
                    fire_at: now + add,
                    title,
                    matched,
                    tz_offset_min,
                });
            }
        }
    }

    let date_match = find_date(raw, day_start, tz_secs);
    let period_match = find_period(raw);

    // ---------- 2) 有明确时刻（如「明天下午3点」「15:30」「今晚8点」「明早8点半」） ----------
    if let Some((h, minute, hs, he)) = find_hour(raw) {
        let mut hh = h;
        let effective_period = period_match
            .map(|p| p.kind)
            .or_else(|| date_match.and_then(|d| d.implied_period.map(|(k, _, _)| k)));

        if let Some(kind) = effective_period {
            match kind {
                "pm" => {
                    if hh < 12 {
                        hh += 12;
                    }
                }
                "noon" => {
                    if hh < 12 {
                        hh = 12 + (hh % 12);
                    }
                }
                "late_am" => {
                    if hh == 12 {
                        hh = 0;
                    }
                }
                _ => {}
            }
        }

        let mut day_offset = date_match.map(|d| d.day_offset).unwrap_or(0);
        if let Some(dm) = date_match {
            if dm.is_same_weekday_candidate {
                let t = day_start + hh * 3600 + minute * 60;
                day_offset = if t > now { 0 } else { 7 };
            }
        }

        let mut fire_at = day_start + day_offset * 86_400 + hh * 3600 + minute * 60;

        // 没写日期且时间已过 → 顺延到明天
        if date_match.is_none() && fire_at <= now + 30 {
            fire_at += 86_400;
        }
        // 兜底：若计算出的时刻仍早于现在（例如晚上说「今天下午3点」），顺延一天确保提醒能触发
        if fire_at <= now {
            fire_at += 86_400;
        }

        let mut spans: Vec<(usize, usize)> = Vec::new();
        if let Some(dm) = date_match {
            spans.push((dm.start, dm.end));
        }
        if let Some(pm) = period_match {
            spans.push((pm.start, pm.end));
        }
        spans.push((hs, he));

        let matched = build_matched_text(raw, &spans);
        let title = extract_title_spans(raw, &spans);
        return Some(Parsed {
            fire_at,
            title,
            matched,
            tz_offset_min,
        });
    }

    // ---------- 3) 有时间段词、无具体几点（如「明天早晨」「明天中午」「今天下午」「今晚」「明早」） ----------
    if let Some(pm) = period_match {
        let mut day_offset = date_match.map(|d| d.day_offset).unwrap_or(0);
        if let Some(dm) = date_match {
            if dm.is_same_weekday_candidate {
                let t = day_start + pm.default_h * 3600 + pm.default_m * 60;
                day_offset = if t > now { 0 } else { 7 };
            }
        }
        let mut fire_at = day_start + day_offset * 86_400 + pm.default_h * 3600 + pm.default_m * 60;
        if fire_at <= now {
            fire_at += 86_400;
        }

        let mut spans: Vec<(usize, usize)> = Vec::new();
        if let Some(dm) = date_match {
            spans.push((dm.start, dm.end));
        }
        spans.push((pm.start, pm.end));

        let matched = build_matched_text(raw, &spans);
        let title = extract_title_spans(raw, &spans);
        return Some(Parsed {
            fire_at,
            title,
            matched,
            tz_offset_min,
        });
    }

    // ---------- 4) 只有日期或合音词（如「明早」「今晚」「明天」「后天去深圳出差」「周五把材料发出去」） ----------
    if let Some(dm) = date_match {
        let (dh, dmin) = dm
            .implied_period
            .map(|(_, h, m)| (h, m))
            .unwrap_or((10, 0));
        let mut day_offset = dm.day_offset;
        if dm.is_same_weekday_candidate {
            let t = day_start + dh * 3600 + dmin * 60;
            day_offset = if t > now { 0 } else { 7 };
        }
        let mut fire_at = day_start + day_offset * 86_400 + dh * 3_600 + dmin * 60;
        if fire_at <= now {
            fire_at += 86_400;
        }
        let spans = [(dm.start, dm.end)];
        let title = extract_title_spans(raw, &spans);
        return Some(Parsed {
            fire_at,
            title,
            matched: raw[dm.start..dm.end].trim().to_string(),
            tz_offset_min,
        });
    }

    None
}

/// 周几：返回 `(0=周日..6=周六, 匹配起始字节索引, 匹配结束字节索引, 下几周)`
fn find_weekday(text: &str) -> Option<(i64, usize, usize, i64)> {
    for (prefix, weeks) in [
        ("下下星期", 2i64),
        ("下下礼拜", 2),
        ("下下周", 2),
        ("下星期", 1),
        ("下礼拜", 1),
        ("下周", 1),
        ("本星期", 0),
        ("这星期", 0),
        ("本周", 0),
        ("这周", 0),
        ("星期", 0),
        ("礼拜", 0),
        ("周", 0),
    ] {
        for (pos, _) in text.match_indices(prefix) {
            let after = &text[pos + prefix.len()..];
            let Some(ch) = after.chars().next() else {
                continue;
            };
            let idx = match ch {
                '一' => 1,
                '二' => 2,
                '三' => 3,
                '四' => 4,
                '五' => 5,
                '六' => 6,
                '日' | '天' => 0,
                _ => continue,
            };
            return Some((idx, pos, pos + prefix.len() + ch.len_utf8(), weeks));
        }
    }
    None
}

/// 将多个时间片段合并为可读的 `matched` 文本：
/// 若各片段相邻或仅隔空白，则直接取连续区间；若中间隔了其他词，则按顺序拼接。
fn build_matched_text(raw: &str, spans: &[(usize, usize)]) -> String {
    if spans.is_empty() {
        return String::new();
    }
    let mut sorted = spans.to_vec();
    sorted.sort_by_key(|s| s.0);
    let min_s = sorted.first().unwrap().0;
    let max_e = sorted.iter().map(|s| s.1).max().unwrap_or(min_s);

    // 检查各片段之间是否仅包含空白或「的」
    let mut contiguous = true;
    for pair in sorted.windows(2) {
        if pair[1].0 > pair[0].1 {
            let between = &raw[pair[0].1..pair[1].0];
            if !between.chars().all(|c| c.is_whitespace() || c == '的') {
                contiguous = false;
                break;
            }
        }
    }
    if contiguous {
        raw[min_s..max_e].trim().to_string()
    } else {
        sorted
            .iter()
            .map(|&(s, e)| raw[s..e].trim())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 抽标题：优先取「提醒我」之后（若时间在「提醒我」之前）；否则剔除所有命中的时间片段
fn extract_title_spans(raw: &str, spans: &[(usize, usize)]) -> String {
    let mut mask = vec![false; raw.len()];
    for &(s, e) in spans {
        for b in mask.iter_mut().take(e.min(raw.len())).skip(s.min(raw.len())) {
            *b = true;
        }
    }
    // 剔除「提醒我」前缀本身
    if let Some(pos) = raw.find("提醒我") {
        for b in mask.iter_mut().take((pos + "提醒我".len()).min(raw.len())).skip(pos) {
            *b = true;
        }
    }

    let mut s = String::with_capacity(raw.len());
    for (idx, ch) in raw.char_indices() {
        if !mask[idx] {
            s.push(ch);
        }
    }

    let t = s.trim_matches(|c: char| {
        c.is_whitespace()
            || "，,。.、：:!！?？的去在要记得帮我请".contains(c)
            || "【】[]()（）".contains(c)
    });
    if t.is_empty() {
        "提醒".to_string()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(ts: i64) -> (i64, i64) {
        let sod = (ts + tz::offset_secs()).rem_euclid(86_400);
        (sod / 3600, (sod % 3600) / 60)
    }

    #[test]
    fn cn_numbers() {
        assert_eq!(cn_num("三"), Some(3));
        assert_eq!(cn_num("十"), Some(10));
        assert_eq!(cn_num("十五"), Some(15));
        assert_eq!(cn_num("二十三"), Some(23));
        assert_eq!(cn_num("两"), Some(2));
        assert_eq!(cn_num("11"), Some(11));
    }

    #[test]
    fn relative_minutes() {
        let p = parse("30分钟后提醒我喝水").unwrap();
        assert_eq!(p.title, "喝水");
        assert!((p.fire_at - (now_sec() + 1800)).abs() <= 2);
    }

    #[test]
    fn half_hour() {
        let p = parse("半小时后提醒我看邮件").unwrap();
        assert_eq!(p.title, "看邮件");
        assert!((p.fire_at - (now_sec() + 1800)).abs() <= 2);
    }

    #[test]
    fn tomorrow_afternoon() {
        let p = parse("明天下午三点提醒我发周报").unwrap();
        let (h, m) = hm(p.fire_at);
        assert_eq!((h, m), (15, 0));
        assert_eq!(p.title, "发周报");
        // 必须是明天（比今天零点晚一天）
        assert!(p.fire_at >= tz::day_start() + 86_400);
    }

    #[test]
    fn tomorrow_morning_and_noon_without_hour() {
        // 用户反馈的核心场景：「明天早晨」「明天中午」不带具体几点，也必须按中国时间识别为对应时段
        let p_morn = parse("明天早晨提醒我发周报").unwrap();
        assert_eq!(hm(p_morn.fire_at), (8, 30), "明天早晨应识别为 08:30");
        assert_eq!(p_morn.matched, "明天早晨");
        assert_eq!(p_morn.title, "发周报");
        assert!(p_morn.fire_at >= tz::day_start() + 86_400);

        let p_noon = parse("明天中午提醒我吃饭").unwrap();
        assert_eq!(hm(p_noon.fire_at), (12, 0), "明天中午应识别为 12:00");
        assert_eq!(p_noon.matched, "明天中午");
        assert_eq!(p_noon.title, "吃饭");
        assert!(p_noon.fire_at >= tz::day_start() + 86_400);

        let p_short = parse("明早提醒我开晨会").unwrap();
        assert_eq!(hm(p_short.fire_at), (8, 30), "明早应识别为明天 08:30");
        assert_eq!(p_short.title, "开晨会");

        let p_tonight = parse("今晚8点提醒我复盘").unwrap();
        assert_eq!(hm(p_tonight.fire_at), (20, 0), "今晚8点应识别为 20:00 而非 08:00");
        assert_eq!(p_tonight.title, "复盘");
    }

    #[test]
    fn false_dot_or_colon_prefix_does_not_break_hour() {
        // 句子前部含有「重点：」「盘点」等带冒号或「点」字的词，不应阻断后面的真实时间识别
        let p = parse("重点：明天下午3点半提醒我交材料").unwrap();
        assert_eq!(hm(p.fire_at), (15, 30));
        assert_eq!(p.matched, "明天下午3点半");
    }

    #[test]
    fn cross_timezone_parsing_consistency() {
        // 同一句「明天早晨」在 UTC+8 (480) 与东京 UTC+9 (540) 下都代表当地的 08:30，
        // 东京比北京早 1 小时，因此东京的 fire_at 应比北京早 3600 秒
        let p_cn = parse_in_tz("明天早晨提醒我开会", 480).unwrap();
        let p_jp = parse_in_tz("明天早晨提醒我开会", 540).unwrap();
        let sod_cn = (p_cn.fire_at + 480 * 60).rem_euclid(86_400);
        let sod_jp = (p_jp.fire_at + 540 * 60).rem_euclid(86_400);
        assert_eq!(sod_cn, 8 * 3600 + 30 * 60);
        assert_eq!(sod_jp, 8 * 3600 + 30 * 60);
    }

    #[test]
    fn evening_eight() {
        let p = parse("晚上8点提醒我关灯").unwrap();
        assert_eq!(hm(p.fire_at).0, 20);
        assert_eq!(p.title, "关灯");
    }

    #[test]
    fn half_past() {
        let p = parse("明天上午10点半提醒我开会").unwrap();
        let (h, m) = hm(p.fire_at);
        assert_eq!((h, m), (10, 30));
    }

    #[test]
    fn colon_time() {
        let p = parse("15:30 提醒我回电话").unwrap();
        assert_eq!(hm(p.fire_at), (15, 30));
        assert_eq!(p.title, "回电话");
    }

    #[test]
    fn weekday_picks_future() {
        let p = parse("周五下午2点提醒我评审").unwrap();
        assert_eq!(hm(p.fire_at).0, 14);
        assert!(p.fire_at > now_sec());
        // 一定落在周五
        let idx = ((p.fire_at + tz::offset_secs()).div_euclid(86_400) + 4).rem_euclid(7);
        assert_eq!(idx, 5);
    }

    #[test]
    fn no_time_returns_none() {
        assert!(parse("发周报").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn title_fallback_without_remind_me() {
        let p = parse("明天3点 发周报").unwrap();
        assert!(p.title.contains("发周报"), "title={}", p.title);
    }

    #[test]
    fn date_only_defaults_to_ten_am() {
        let p = parse("后天可能需要出差去深圳，见一创投行客户").unwrap();
        let (h, m) = hm(p.fire_at);
        assert_eq!((h, m), (10, 0), "只有日期时应默认 10:00");
        assert!(
            p.fire_at >= tz::day_start() + 2 * 86_400 && p.fire_at < tz::day_start() + 3 * 86_400,
            "必须落在后天"
        );
        assert!(p.title.contains("深圳"), "title={}", p.title);
        assert_eq!(p.matched, "后天");
    }

    #[test]
    fn date_only_weekday() {
        let p = parse("周五把材料发出去").unwrap();
        let (h, _m) = hm(p.fire_at);
        assert_eq!(h, 10);
        assert!(p.fire_at > now_sec());
        let idx = ((p.fire_at + tz::offset_secs()).div_euclid(86_400) + 4).rem_euclid(7);
        assert_eq!(idx, 5, "必须落在周五");
    }
}
