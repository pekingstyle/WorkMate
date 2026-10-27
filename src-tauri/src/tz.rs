// WorkMate · 系统底层时区引擎（City-First Timezone Engine）
// 编制：WorkMate Contributors
//
// 核心设计目标：
//   1. 解决「本机操作系统时区（如美区 UTC-7）与用户真实生活/办公时区（如中国 UTC+8）不一致」
//      导致待办解析、日程簿记、提醒提示与跨天统计错乱的问题。
//   2. 以「用户所在城市」（手填城市 / 自动定位城市 / 常驻地城市，默认中国标准时间 UTC+8）
//      作为系统的默认基准时区，彻底脱离对操作系统本地时区的盲目依赖。
//   3. 簿记与跨时区漫游：
//      · 记录待办/日程时，按当前城市时区（或目的地城市时区）识别自然语言时间（如「明天早晨」「明天中午」），
//        同时在数据库记录该事项创建时的时区偏移 `tz_offset_min`。
//      · 提醒与展示时，自动转换到用户「当前所在城市」的当地时间；若用户出国换了城市/时区，
//        自动按新时区换算并提示原记录时区；若本机系统时区与城市时区不一致，同时提供本机时间对照。

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 默认业务时区：中国标准时间 UTC+8（480 分钟 / 28800 秒）
pub const CN_OFFSET_MIN: i64 = 480;
pub const CN_OFFSET_SECS: i64 = 8 * 3600;

fn now_sec() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 缓存的当前生效时区状态（避免高频调用时反复查库或引发 DB 互斥锁重入）
#[derive(Debug, Clone)]
struct CachedTz {
    initialized: bool,
    offset_min: i64,
    city_label: String,
    source: String,
}

impl Default for CachedTz {
    fn default() -> Self {
        Self {
            initialized: false,
            offset_min: CN_OFFSET_MIN,
            city_label: "中国标准时间".to_string(),
            source: "default_cn".to_string(),
        }
    }
}

fn tz_cache() -> &'static Mutex<CachedTz> {
    static CACHE: OnceLock<Mutex<CachedTz>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(CachedTz::default()))
}

/// 完整时区快照（供前端面板、宠物气泡、提示词与控制面统一消费）
#[derive(Debug, Clone, serde::Serialize)]
pub struct TzInfo {
    /// 当前生效的业务时区偏移（分钟，如 UTC+8 = 480，美西夏令时 UTC-7 = -420）
    pub offset_min: i64,
    /// 当前生效的业务时区偏移（秒）
    pub offset_secs: i64,
    /// 标准 UTC 偏移标签，如 "UTC+8"、"UTC-7"、"UTC+05:30"
    pub utc_label: String,
    /// 当前时区对应的城市或区域名称，如 "上海"、"北京"、"东京"、"中国时间"
    pub city_label: String,
    /// 时区来源："manual_tz"（手动指定时区）| "city"（当前城市）| "home_city"（常驻城市）| "default_cn"（默认中国时区）
    pub source: String,
    /// 本机操作系统时区偏移（分钟）
    pub machine_offset_min: i64,
    /// 本机操作系统时区标签，如 "UTC-7"
    pub machine_utc_label: String,
    /// 本机系统时区是否与当前生效的城市时区不一致（例如本机是美区 UTC-7，而用户城市是中国 UTC+8）
    pub machine_mismatch: bool,
    /// 当前城市时区下的完整时间："YYYY-MM-DD HH:MM"
    pub now_text: String,
    /// 当前城市时区下的时分："HH:MM"
    pub now_hm: String,
    /// 本机系统时区下的完整时间："YYYY-MM-DD HH:MM"
    pub machine_now_text: String,
    /// 本机系统时区下的时分："HH:MM"
    pub machine_now_hm: String,
}

/// 将分钟偏移格式化为标准 UTC 标签，如 480 -> "UTC+8"，-420 -> "UTC-7"，330 -> "UTC+05:30"
pub fn format_utc_label(offset_min: i64) -> String {
    let sign = if offset_min >= 0 { '+' } else { '-' };
    let abs = offset_min.abs();
    let h = abs / 60;
    let m = abs % 60;
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h:02}:{m:02}")
    }
}

/// 生成供 SQLite `date(ts, 'unixepoch', ...)` 使用的时区修饰符，如 `"+480 minutes"` 或 `"-420 minutes"`
pub fn sqlite_modifier() -> String {
    let min = offset_min();
    if min >= 0 {
        format!("+{min} minutes")
    } else {
        format!("{min} minutes")
    }
}

/// 获取本机操作系统真实的本地时区偏移（分钟）。
/// 通过对比 Win32 `GetSystemTime`（UTC）与 `GetLocalTime`（OS 本地墙钟），
/// 按 15 分钟粒度取整，天然兼容 Windows 夏令时且零外部依赖。
pub fn machine_offset_min() -> i64 {
    if let Ok(v) = std::env::var("WORKMATE_MACHINE_TZ_MIN") {
        if let Ok(n) = v.trim().parse::<i64>() {
            return n.clamp(-720, 840);
        }
    }
    unsafe {
        use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
        let sys = GetSystemTime();
        let loc = GetLocalTime();
        let sys_s = civil_to_epoch_secs(
            sys.wYear as i64,
            sys.wMonth as i64,
            sys.wDay as i64,
            sys.wHour as i64,
            sys.wMinute as i64,
            sys.wSecond as i64,
        );
        let loc_s = civil_to_epoch_secs(
            loc.wYear as i64,
            loc.wMonth as i64,
            loc.wDay as i64,
            loc.wHour as i64,
            loc.wMinute as i64,
            loc.wSecond as i64,
        );
        if let (Some(u), Some(l)) = (sys_s, loc_s) {
            let diff_sec = l - u;
            // 全球所有法定标准时区均为 15 分钟（900 秒）的整数倍
            let quarters = ((diff_sec as f64) / 900.0).round() as i64;
            return (quarters * 15).clamp(-720, 840);
        }
    }
    CN_OFFSET_MIN
}

/// 判断给定公历日期是否处于北美夏令时（3 月第二个周日 02:00 至 11 月第一个周日 02:00）
fn is_us_dst(year: i64, month: i64, day: i64) -> bool {
    if month < 3 || month > 11 {
        return false;
    }
    if month > 3 && month < 11 {
        return true;
    }
    // 计算某年某月 1 号是周几（0=周日 .. 6=周六）
    let first_dow = |y: i64, m: i64| -> i64 {
        let ts = civil_to_epoch_secs(y, m, 1, 0, 0, 0).unwrap_or(0);
        (ts.div_euclid(86_400) + 4).rem_euclid(7)
    };
    if month == 3 {
        let dow1 = first_dow(year, 3);
        let first_sun = if dow1 == 0 { 1 } else { 8 - dow1 };
        let second_sun = first_sun + 7;
        day >= second_sun
    } else {
        let dow1 = first_dow(year, 11);
        let first_sun = if dow1 == 0 { 1 } else { 8 - dow1 };
        day < first_sun
    }
}

/// 判断给定公历日期是否处于欧洲夏令时（3 月最后一个周日至 10 月最后一个周日）
fn is_eu_dst(year: i64, month: i64, day: i64) -> bool {
    if month < 3 || month > 10 {
        return false;
    }
    if month > 3 && month < 10 {
        return true;
    }
    let last_dow = |y: i64, m: i64| -> i64 {
        let ts = civil_to_epoch_secs(y, m, 31, 0, 0, 0).unwrap_or(0);
        (ts.div_euclid(86_400) + 4).rem_euclid(7)
    };
    if month == 3 {
        let last_sun = 31 - last_dow(year, 3);
        day >= last_sun
    } else {
        let last_sun = 31 - last_dow(year, 10);
        day < last_sun
    }
}

/// 解析字符串中显式书写的 `UTC+8` / `UTC-7` / `GMT+9` / `UTC+05:30`
pub fn parse_explicit_utc(s: &str) -> Option<i64> {
    let upper = s.to_ascii_uppercase();
    for prefix in ["UTC", "GMT"] {
        if let Some(idx) = upper.find(prefix) {
            let rest = upper[idx + prefix.len()..].trim_start();
            let mut chars = rest.chars();
            let sign_ch = chars.next()?;
            let sign = match sign_ch {
                '+' => 1i64,
                '-' | '−' => -1i64,
                _ => continue,
            };
            let after_sign: String = chars
                .take_while(|c| c.is_ascii_digit() || *c == ':')
                .collect();
            if after_sign.is_empty() {
                continue;
            }
            if let Some((h_str, m_str)) = after_sign.split_once(':') {
                let h: i64 = h_str.parse().ok()?;
                let m: i64 = m_str.parse().ok()?;
                if h <= 14 && m < 60 {
                    return Some(sign * (h * 60 + m));
                }
            } else if let Ok(h) = after_sign.parse::<i64>() {
                if h <= 14 {
                    return Some(sign * h * 60);
                }
            }
        }
    }
    None
}

/// 提取精简城市名（如 `"上海·上海市"` -> `"上海"`）
fn short_city_name(label: &str) -> String {
    let head = label.split(['·', ',', '，', ' ']).next().unwrap_or(label).trim();
    if head.chars().count() > 2 && head.ends_with('市') {
        head.strip_suffix('市').unwrap_or(head).to_string()
    } else {
        head.to_string()
    }
}

/// 根据城市名称（及可选经度）推断该城市的本地时区偏移（分钟）与显示名。
///
/// 覆盖：
///   · 中国全部省市及港澳台 → UTC+8 (480)
///   · 亚太（东京/首尔 UTC+9、新加坡/吉隆坡/马尼拉/珀斯 UTC+8、曼谷/河内/雅加达 UTC+7、
///     孟买/新德里 UTC+5:30、迪拜 UTC+4、悉尼/墨尔本 UTC+10、奥克兰 UTC+12）
///   · 欧洲（伦敦 UTC+0/+1、巴黎/法兰克福/柏林/苏黎世/阿姆斯特丹/罗马/马德里 UTC+1/+2、莫斯科 UTC+3）
///   · 北美（纽约/波士顿/华盛顿/多伦多 美东 UTC-5/-4、芝加哥/休斯顿/达拉斯 美中 UTC-6/-5、
///     丹佛 UTC-7/-6、洛杉矶/旧金山/西雅图/硅谷/温哥华 美西 UTC-8/-7）
pub fn city_to_tz(label: &str, lon: Option<f64>) -> Option<(i64, String)> {
    let raw = label.trim();
    if raw.is_empty() {
        return None;
    }
    let short = short_city_name(raw);

    // 1) 若标签里直接写了 UTC+X / GMT-X，优先按显式偏移解析
    if let Some(off) = parse_explicit_utc(raw) {
        return Some((off, short));
    }

    let lower = raw.to_ascii_lowercase();
    let (y, mo, d, _, _, _) = epoch_to_civil(now_sec() + CN_OFFSET_SECS);
    let us_dst = is_us_dst(y, mo, d);
    let eu_dst = is_eu_dst(y, mo, d);

    // 2) 中国大陆及港澳台、新加坡、马来西亚、菲律宾、西澳（全部 UTC+8）
    const UTC_PLUS_8_KEYWORDS: &[&str] = &[
        "北京", "上海", "深圳", "广州", "杭州", "南京", "苏州", "成都", "重庆", "武汉",
        "西安", "天津", "长沙", "郑州", "青岛", "宁波", "厦门", "合肥", "福州", "济南",
        "大连", "沈阳", "昆明", "南昌", "南宁", "贵阳", "太原", "石家庄", "哈尔滨", "长春",
        "兰州", "乌鲁木齐", "海口", "三亚", "珠海", "佛山", "东莞", "无锡", "常州", "温州",
        "嘉兴", "绍兴", "湖州", "金华", "台州", "徐州", "南通", "扬州", "盐城", "泰州",
        "镇江", "烟台", "潍坊", "威海", "临沂", "洛阳", "襄阳", "宜昌", "岳阳", "株洲",
        "惠州", "中山", "江门", "湛江", "汕头", "桂林", "柳州", "遵义", "绵阳", "乐山",
        "保定", "唐山", "廊坊", "包头", "呼和浩特", "银川", "西宁", "拉萨", "香港", "澳门",
        "台北", "高雄", "新竹", "台中", "台南", "台湾", "中国", "广东", "浙江", "江苏",
        "四川", "湖北", "陕西", "湖南", "河南", "山东", "福建", "安徽", "辽宁", "云南",
        "江西", "广西", "贵州", "山西", "河北", "黑龙江", "吉林", "甘肃", "新疆", "海南",
        "内蒙古", "西藏", "宁夏", "青海", "新加坡", "吉隆坡", "马来西亚", "马尼拉", "菲律宾",
        "珀斯", "文莱", "beijing", "shanghai", "shenzhen", "guangzhou", "hangzhou",
        "nanjing", "suzhou", "chengdu", "chongqing", "wuhan", "xi'an", "xian", "tianjin",
        "hong kong", "hongkong", "macau", "taipei", "taiwan", "china", "singapore",
        "kuala lumpur", "manila", "perth",
    ];
    if UTC_PLUS_8_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((480, short));
    }

    // 3) 日本 / 韩国（UTC+9）
    const UTC_PLUS_9_KEYWORDS: &[&str] = &[
        "东京", "大阪", "京都", "横滨", "名古屋", "神户", "福冈", "札幌", "冲绳", "日本",
        "首尔", "釜山", "仁川", "济州", "韩国", "tokyo", "osaka", "kyoto", "yokohama",
        "nagoya", "fukuoka", "sapporo", "japan", "seoul", "busan", "incheon", "korea",
    ];
    if UTC_PLUS_9_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((540, short));
    }

    // 4) 东南亚 UTC+7（泰国 / 越南 / 印尼西部）
    const UTC_PLUS_7_KEYWORDS: &[&str] = &[
        "曼谷", "普吉", "清迈", "泰国", "胡志明", "河内", "岘港", "越南", "雅加达",
        "巴厘岛", "印尼", "印度尼西亚", "金边", "柬埔寨", "万象", "bangkok", "phuket",
        "chiang mai", "thailand", "ho chi minh", "hanoi", "vietnam", "jakarta", "bali",
        "indonesia", "phnom penh",
    ];
    if UTC_PLUS_7_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((420, short));
    }

    // 5) 印度（UTC+5:30 = 330 分钟）
    const INDIA_KEYWORDS: &[&str] = &[
        "孟买", "新德里", "德里", "班加罗尔", "海得拉巴", "金奈", "加尔各答", "印度",
        "mumbai", "new delhi", "delhi", "bangalore", "bengaluru", "india",
    ];
    if INDIA_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((330, short));
    }

    // 6) 中东（迪拜/阿布扎比 UTC+4，利雅得/多哈/莫斯科/伊斯坦布尔 UTC+3）
    const UTC_PLUS_4_KEYWORDS: &[&str] = &[
        "迪拜", "阿布扎比", "阿联酋", "dubai", "abu dhabi", "uae",
    ];
    if UTC_PLUS_4_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((240, short));
    }
    const UTC_PLUS_3_KEYWORDS: &[&str] = &[
        "莫斯科", "圣彼得堡", "俄罗斯", "利雅得", "沙特", "多哈", "卡塔尔", "伊斯坦布尔",
        "土耳其", "moscow", "russia", "riyadh", "doha", "istanbul", "turkey",
    ];
    if UTC_PLUS_3_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((180, short));
    }

    // 7) 澳新（悉尼/墨尔本/布里斯班 UTC+10，奥克兰/惠灵顿 UTC+12）
    const AU_EAST_KEYWORDS: &[&str] = &[
        "悉尼", "墨尔本", "布里斯班", "堪培拉", "黄金海岸", "澳大利亚", "澳洲",
        "sydney", "melbourne", "brisbane", "canberra", "australia",
    ];
    if AU_EAST_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((600, short));
    }
    const NZ_KEYWORDS: &[&str] = &[
        "奥克兰", "惠灵顿", "基督城", "皇后镇", "新西兰", "auckland", "wellington", "new zealand",
    ];
    if NZ_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((720, short));
    }

    // 8) 英国 / 爱尔兰 / 葡萄牙（冬令时 UTC+0，夏令时 UTC+1）
    const UK_KEYWORDS: &[&str] = &[
        "伦敦", "爱丁堡", "曼彻斯特", "伯明翰", "剑桥", "牛津", "英国", "都柏林",
        "爱尔兰", "里斯本", "葡萄牙", "london", "edinburgh", "manchester", "uk",
        "united kingdom", "dublin", "ireland", "lisbon",
    ];
    if UK_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((if eu_dst { 60 } else { 0 }, short));
    }

    // 9) 欧洲中部（冬令时 UTC+1，夏令时 UTC+2）
    const CET_KEYWORDS: &[&str] = &[
        "巴黎", "法国", "法兰克福", "柏林", "慕尼黑", "汉堡", "德国", "苏黎世",
        "日内瓦", "瑞士", "阿姆斯特丹", "荷兰", "布鲁塞尔", "比利时", "卢森堡",
        "米兰", "罗马", "意大利", "马德里", "巴塞罗那", "西班牙", "维也纳", "奥地利",
        "布拉格", "华沙", "斯德哥尔摩", "瑞典", "哥本哈根", "丹麦", "奥斯陆", "挪威",
        "paris", "france", "frankfurt", "berlin", "munich", "germany", "zurich",
        "geneva", "switzerland", "amsterdam", "netherlands", "brussels", "milan",
        "rome", "italy", "madrid", "barcelona", "spain", "vienna", "stockholm",
    ];
    if CET_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((if eu_dst { 120 } else { 60 }, short));
    }

    // 10) 北美美东（冬令时 UTC-5 = -300，夏令时 UTC-4 = -240）
    const US_EAST_KEYWORDS: &[&str] = &[
        "纽约", "波士顿", "华盛顿", "费城", "亚特兰大", "迈阿密", "奥兰多", "底特律",
        "多伦多", "蒙特利尔", "渥太华", "美东", "new york", "nyc", "boston",
        "washington", "philadelphia", "atlanta", "miami", "toronto", "montreal",
    ];
    if US_EAST_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((if us_dst { -240 } else { -300 }, short));
    }

    // 11) 北美美中（冬令时 UTC-6 = -360，夏令时 UTC-5 = -300）
    const US_CENTRAL_KEYWORDS: &[&str] = &[
        "芝加哥", "休斯顿", "达拉斯", "奥斯汀", "圣安东尼奥", "新奥尔良", "明尼阿波利斯",
        "美中", "chicago", "houston", "dallas", "austin",
    ];
    if US_CENTRAL_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((if us_dst { -300 } else { -360 }, short));
    }

    // 12) 北美山地（冬令时 UTC-7 = -420，夏令时 UTC-6 = -360；亚利桑那凤凰城常年 -420）
    if raw.contains("凤凰城") || lower.contains("phoenix") {
        return Some((-420, short));
    }
    const US_MOUNTAIN_KEYWORDS: &[&str] = &[
        "丹佛", "盐湖城", "卡尔加里", "denver", "salt lake city", "calgary",
    ];
    if US_MOUNTAIN_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((if us_dst { -360 } else { -420 }, short));
    }

    // 13) 北美美西（冬令时 UTC-8 = -480，夏令时 UTC-7 = -420）
    const US_WEST_KEYWORDS: &[&str] = &[
        "洛杉矶", "旧金山", "三藩", "西雅图", "硅谷", "圣何塞", "圣地亚哥", "拉斯维加斯",
        "波特兰", "温哥华", "美西", "加州", "los angeles", "san francisco", "seattle",
        "silicon valley", "san jose", "san diego", "las vegas", "portland", "vancouver",
        "california",
    ];
    if US_WEST_KEYWORDS.iter().any(|k| raw.contains(k) || lower.contains(k)) {
        return Some((if us_dst { -420 } else { -480 }, short));
    }

    // 14) 夏威夷（常年 UTC-10 = -600）
    if raw.contains("夏威夷") || raw.contains("檀香山") || lower.contains("hawaii") || lower.contains("honolulu") {
        return Some((-600, short));
    }

    // 15) 若带有经度坐标（例如通过 GPS/IP 定位到了未在表内的海外城市），按经度推算时区
    if let Some(longitude) = lon {
        // 中国版图常见经度范围且含中文时优先按 UTC+8
        if (73.5..=135.0).contains(&longitude) && raw.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
            return Some((480, short));
        }
        let est_hours = (longitude / 15.0).round() as i64;
        return Some(((est_hours * 60).clamp(-720, 840), short));
    }

    None
}

/// 从数据库设置与当前定位刷新系统生效时区缓存。
///
/// 优先级：
///   1. 测试环境变量 `WORKMATE_TZ_OFFSET_MIN`（仅供跨时区自动化单测注入）
///   2. 用户显式指定的时区设置 `tz.manual_offset_min`（非空且非 `"auto"`）
///   3. 当前所在城市（手填 `loc.manual_label` / 测试 `WORKMATE_LOC_MOCK` / 定位缓存 `location::cached()`）
///   4. 常驻城市（`loc.home` 或最近 90 天常驻地）
///   5. 默认兜底：中国标准时间 `UTC+8`（480 分钟）
pub fn refresh() -> TzInfo {
    let resolved = resolve_from_sources();
    if let Ok(mut g) = tz_cache().lock() {
        *g = resolved.clone();
    }
    build_info(&resolved)
}

fn resolve_from_sources() -> CachedTz {
    // 1) 测试环境变量注入
    if let Ok(v) = std::env::var("WORKMATE_TZ_OFFSET_MIN") {
        if let Ok(min) = v.trim().parse::<i64>() {
            let clamped = min.clamp(-720, 840);
            return CachedTz {
                initialized: true,
                offset_min: clamped,
                city_label: format_utc_label(clamped),
                source: "env".to_string(),
            };
        }
    }

    // 读取 DB 配置（若当前线程已在 db.with 锁内，with_conn 会立即返回 Err，安全降级不死锁）
    let manual_tz = crate::db::with_conn(|c| crate::db::repo::get_setting(c, "tz.manual_offset_min"))
        .ok()
        .flatten();
    if let Some(ref s) = manual_tz {
        let t = s.trim();
        if !t.is_empty() && !t.eq_ignore_ascii_case("auto") {
            if let Ok(min) = t.parse::<i64>() {
                let clamped = min.clamp(-720, 840);
                return CachedTz {
                    initialized: true,
                    offset_min: clamped,
                    city_label: format!("手动指定 ({})", format_utc_label(clamped)),
                    source: "manual_tz".to_string(),
                };
            }
        }
    }

    // 2) 当前城市（手填位置 / 定位缓存）
    let cur_fix = crate::location::cached();
    let cur_label = if !cur_fix.label.is_empty() {
        cur_fix.label.clone()
    } else {
        crate::location::current_label()
    };
    if !cur_label.is_empty() {
        if let Some((min, city)) = city_to_tz(&cur_label, cur_fix.lon) {
            return CachedTz {
                initialized: true,
                offset_min: min,
                city_label: city,
                source: "city".to_string(),
            };
        }
        // 即使城市名未命中海外表，默认也是中国城市（UTC+8），保留城市名显示
        return CachedTz {
            initialized: true,
            offset_min: CN_OFFSET_MIN,
            city_label: short_city_name(&cur_label),
            source: "city".to_string(),
        };
    }

    // 3) 常驻城市（loc.home）
    if let Some(home) = crate::location::home() {
        if !home.is_empty() {
            if let Some((min, city)) = city_to_tz(&home, None) {
                return CachedTz {
                    initialized: true,
                    offset_min: min,
                    city_label: city,
                    source: "home_city".to_string(),
                };
            }
            return CachedTz {
                initialized: true,
                offset_min: CN_OFFSET_MIN,
                city_label: short_city_name(&home),
                source: "home_city".to_string(),
            };
        }
    }

    // 4) 默认中国标准时间（UTC+8）
    CachedTz {
        initialized: true,
        offset_min: CN_OFFSET_MIN,
        city_label: "中国时间".to_string(),
        source: "default_cn".to_string(),
    }
}

fn current_cached() -> CachedTz {
    // 若设置了测试环境变量（单测场景），实时响应环境变量
    if std::env::var("WORKMATE_TZ_OFFSET_MIN").is_ok() || std::env::var("WORKMATE_LOC_MOCK").is_ok() {
        return resolve_from_sources();
    }
    if let Ok(g) = tz_cache().lock() {
        if g.initialized {
            return g.clone();
        }
    }
    let resolved = resolve_from_sources();
    if let Ok(mut g) = tz_cache().lock() {
        *g = resolved.clone();
    }
    resolved
}

fn build_info(c: &CachedTz) -> TzInfo {
    let now = now_sec();
    let offset_min = c.offset_min;
    let offset_secs = offset_min * 60;
    let machine_min = machine_offset_min();
    TzInfo {
        offset_min,
        offset_secs,
        utc_label: format_utc_label(offset_min),
        city_label: c.city_label.clone(),
        source: c.source.clone(),
        machine_offset_min: machine_min,
        machine_utc_label: format_utc_label(machine_min),
        machine_mismatch: machine_min != offset_min,
        now_text: fmt_datetime_with_offset(now, offset_min),
        now_hm: fmt_hm_with_offset(now, offset_min),
        machine_now_text: fmt_datetime_with_offset(now, machine_min),
        machine_now_hm: fmt_hm_with_offset(now, machine_min),
    }
}

/// 获取当前生效的时区详情快照
pub fn active() -> TzInfo {
    build_info(&current_cached())
}

/// 当前生效时区的分钟偏移（如 UTC+8 返回 480）
pub fn offset_min() -> i64 {
    current_cached().offset_min
}

/// 当前生效时区的秒数偏移（如 UTC+8 返回 28800）
pub fn offset_secs() -> i64 {
    current_cached().offset_min * 60
}

/// 根据待办事项的目标地点或当前所在城市，决定簿记该事项时应记录的时区偏移（分钟）
pub fn resolve_booking_offset_min(place: &str) -> i64 {
    if !place.trim().is_empty() {
        if let Some((min, _)) = city_to_tz(place, None) {
            return min;
        }
    }
    offset_min()
}

/* ============================ 日历与时间换算底层函数 ============================ */

/// 将偏移后的秒数拆解为公历 `(年, 月, 日, 时, 分, 秒)`
pub fn epoch_to_civil(shifted_secs: i64) -> (i64, i64, i64, u32, u32, u32) {
    let sod = shifted_secs.rem_euclid(86_400);
    let h = (sod / 3_600) as u32;
    let m = ((sod % 3_600) / 60) as u32;
    let s = (sod % 60) as u32;
    let days = shifted_secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, m, s)
}

/// 将公历 `(年, 月, 日, 时, 分, 秒)` 转为未减时区偏移的纪元秒数
pub fn civil_to_epoch_secs(y: i64, mo: i64, day: i64, h: i64, mi: i64, sec: i64) -> Option<i64> {
    if !(1970..=2200).contains(&y)
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&day)
        || !(0..=24).contains(&h)
        || !(0..=59).contains(&mi)
        || !(0..=59).contains(&sec)
    {
        return None;
    }
    let my = if mo <= 2 { y - 1 } else { y };
    let mp = if mo <= 2 { mo + 9 } else { mo - 3 };
    let era = my.div_euclid(400);
    let yoe = my.rem_euclid(400);
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// 当前生效时区下的「今天零点」UTC 时间戳（秒）
pub fn day_start() -> i64 {
    let now = now_sec();
    let off = offset_secs();
    now - (now + off).rem_euclid(86_400)
}

/// 当前生效时区下的 `(时, 分)`
pub fn now_hm() -> (u32, u32) {
    let sod = (now_sec() + offset_secs()).rem_euclid(86_400);
    ((sod / 3600) as u32, ((sod % 3600) / 60) as u32)
}

/// 时间戳 → 当前生效时区下的日期 `"YYYY-MM-DD"`
pub fn date_str(ts: i64) -> String {
    let (y, mo, d, _, _, _) = epoch_to_civil(ts + offset_secs());
    format!("{y:04}-{mo:02}-{d:02}")
}

/// 时间戳 → 指定分钟偏移时区下的 `"YYYY-MM-DD HH:MM"`
pub fn fmt_datetime_with_offset(ts: i64, tz_min: i64) -> String {
    let (y, mo, d, h, m, _) = epoch_to_civil(ts + tz_min * 60);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

/// 时间戳 → 指定分钟偏移时区下的 `"HH:MM"`
pub fn fmt_hm_with_offset(ts: i64, tz_min: i64) -> String {
    let (_, _, _, h, m, _) = epoch_to_civil(ts + tz_min * 60);
    format!("{h:02}:{m:02}")
}

/// 时间戳 → 指定分钟偏移时区下的 `"M/D HH:MM"`
pub fn fmt_mdhm_with_offset(ts: i64, tz_min: i64) -> String {
    let (_, mo, d, h, m, _) = epoch_to_civil(ts + tz_min * 60);
    format!("{mo}/{d} {h:02}:{m:02}")
}

/// 时间戳 → 当前生效时区下的 `"YYYY-MM-DD HH:MM"`
pub fn fmt_datetime(ts: i64) -> String {
    fmt_datetime_with_offset(ts, offset_min())
}

/// 当前时刻在生效时区下的 `"YYYY-MM-DD HH:MM"`
pub fn now_text() -> String {
    fmt_datetime(now_sec())
}

/// 给大模型提示词使用的完整当前时间与时区上下文说明
pub fn prompt_time_context() -> String {
    let info = active();
    if info.machine_mismatch {
        format!(
            "{}（{} · {}；注：本机系统时区为 {} {}，但用户一切时间表述如「今天/明天/早晨/中午/下午」均以 {} {} 为准）",
            info.now_text,
            info.city_label,
            info.utc_label,
            info.machine_utc_label,
            info.machine_now_text,
            info.city_label,
            info.utc_label
        )
    } else {
        format!("{}（{} · {}）", info.now_text, info.city_label, info.utc_label)
    }
}

/// 解析当前生效时区（或指定时区）下的 `"YYYY-MM-DD HH:MM"` / `"YYYY-MM-DDTHH:MM"` / `"YYYY/MM/DD HH:MM"` / `"YYYY.MM.DD HH:MM"` → UTC 时间戳（秒）
pub fn parse_datetime_in_tz(s: &str, tz_min: i64) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let normalized = t.replace('/', "-");
    let (d_raw, hm) = if let Some((d, h)) = normalized.split_once(' ') {
        (d.trim(), h.trim())
    } else if let Some((d, h)) = normalized.split_once('T') {
        (d.trim(), h.trim())
    } else {
        (normalized.as_str(), "09:00")
    };
    let d = d_raw.replace('.', "-");
    let mut dp = d.split('-');
    let y: i64 = dp.next()?.trim().parse().ok()?;
    let mo: i64 = dp.next()?.trim().parse().ok()?;
    let day_part = dp.next()?.trim();
    let day: i64 = day_part.parse().ok()?;

    let mut hp = hm.split(':');
    let h: i64 = hp.next()?.trim().parse().ok()?;
    let mi_raw = hp.next().unwrap_or("0").trim();
    let mi_digits: String = mi_raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    let mi: i64 = if mi_digits.is_empty() { 0 } else { mi_digits.parse().ok()? };

    if h > 23 {
        return None;
    }
    let local_epoch = civil_to_epoch_secs(y, mo, day, h, mi, 0)?;
    Some(local_epoch - tz_min * 60)
}

/// 生成跨时区与本机时区对照提示（供提醒气泡与待办列表展示）
///
/// 场景 1：用户出国换了城市/时区（`recorded_tz_min != active.offset_min`）
///   → 返回 `"当地时间 10/26 09:30 (UTC+9 · 原 UTC+8 08:30)"`
/// 场景 2：本机系统时区与用户城市时区不同（如本机美区 UTC-7、城市是中国 UTC+8）
///   → 返回 `"UTC+8 08:30 (折合本机 UTC-7 17:30)"`
pub fn cross_tz_hint(fire_at: i64, recorded_tz_min: i64) -> String {
    let info = active();
    let cur_hm = fmt_hm_with_offset(fire_at, info.offset_min);
    if recorded_tz_min != info.offset_min {
        let orig_hm = fmt_hm_with_offset(fire_at, recorded_tz_min);
        let orig_label = format_utc_label(recorded_tz_min);
        return format!(
            "当地时间 {} ({} · 原 {} {})",
            cur_hm, info.utc_label, orig_label, orig_hm
        );
    }
    if info.machine_mismatch {
        let mach_mdhm = fmt_mdhm_with_offset(fire_at, info.machine_offset_min);
        return format!(
            "{} {}（本机 {} {}）",
            info.utc_label, cur_hm, info.machine_utc_label, mach_mdhm
        );
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_label_formatting() {
        assert_eq!(format_utc_label(480), "UTC+8");
        assert_eq!(format_utc_label(-420), "UTC-7");
        assert_eq!(format_utc_label(0), "UTC+0");
        assert_eq!(format_utc_label(330), "UTC+05:30");
    }

    #[test]
    fn city_timezone_resolution() {
        assert_eq!(city_to_tz("上海·上海市", None), Some((480, "上海".to_string())));
        assert_eq!(city_to_tz("深圳·广东省", None), Some((480, "深圳".to_string())));
        assert_eq!(city_to_tz("北京", None), Some((480, "北京".to_string())));
        assert_eq!(city_to_tz("东京", None), Some((540, "东京".to_string())));
        assert_eq!(city_to_tz("新加坡", None), Some((480, "新加坡".to_string())));
        assert_eq!(city_to_tz("曼谷", None), Some((420, "曼谷".to_string())));
        assert_eq!(city_to_tz("悉尼", None), Some((600, "悉尼".to_string())));
        assert_eq!(city_to_tz("迪拜", None), Some((240, "迪拜".to_string())));
        // 显式写 UTC 偏移
        assert_eq!(city_to_tz("某海外驻点 UTC-5", None), Some((-300, "某海外驻点".to_string())));
    }

    #[test]
    fn datetime_parsing_variants() {
        let t1 = parse_datetime_in_tz("2026-10-27 09:00", 480).unwrap();
        let t2 = parse_datetime_in_tz("2026-10-27T09:00:00", 480).unwrap();
        let t3 = parse_datetime_in_tz("2026/10/27 09:00", 480).unwrap();
        assert_eq!(t1, t2);
        assert_eq!(t1, t3);
        assert_eq!(fmt_datetime_with_offset(t1, 480), "2026-10-27 09:00");
        // 同一时刻在东京 (UTC+9) 应是 10:00
        assert_eq!(fmt_datetime_with_offset(t1, 540), "2026-10-27 10:00");
    }
}
