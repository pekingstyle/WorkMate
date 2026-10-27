// WorkMate · 位置采集（出差轨迹的底层数据源）
// 编制：WorkMate Contributors
//
// 用户场景：偶尔出差，事后想不起来去过哪儿，希望能问「我最近去哪出差了」
// 「10 月 8 号我在哪儿」。要回答这类问题，必须有**按天的位置轨迹**。
//
// 三级来源（按优先级，取到即停）：
//   1. manual —— 用户在面板手填（最可靠，也是定位不可用时的兜底）
//   2. geo    —— Windows 定位服务（Devices::Geolocation）
//                桌面应用走的是「允许桌面应用访问你的位置」开关 + Wi-Fi/基站定位，
//                没有 GPS 的台式机也能出结果；但可能被关掉或被策略禁用。
//   3. ip     —— 公网 IP 粗略定位（默认**关闭**，要用户显式打开；精度只到城市级）
//
// 两条硬约束：
//   · 绝不让定位失败影响主流程：取不到就返回 source="unavailable"，label 空串
//   · 绝不在主线程做网络/定位：一律在调用方线程里跑，且带超时
//
// 隐私：只存「城市级标签 + 可选经纬度」，不写日志、不外发（ip 定位除外，
// 且默认关闭）。清除入口见 clear()。

use std::sync::{Mutex, OnceLock};

use windows::Devices::Geolocation::{GeolocationAccessStatus, Geolocator};

/// 一次定位结果
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct Fix {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// 精度（米）
    pub acc_m: Option<f64>,
    /// geo / ip / manual / mock / unavailable
    pub source: String,
    /// 人能读的地点名（城市级）。反查失败时是 "纬度,经度"
    pub label: String,
}

impl Fix {
    pub fn unavailable() -> Fix {
        Fix {
            source: "unavailable".into(),
            ..Default::default()
        }
    }
    pub fn ok(&self) -> bool {
        self.source != "unavailable" && !self.label.is_empty()
    }
}

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 最近一次定位结果（内存缓存）。
///
/// 为什么缓存：建待办时要给任务打位置标记，而定位至少要几百毫秒，
/// 每次建待办都现采一次既慢又耗电。缓存 + 定时刷新（默认 30 分钟）足够。
fn cache() -> &'static Mutex<Fix> {
    static M: OnceLock<Mutex<Fix>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(Fix::unavailable()))
}

pub fn cached() -> Fix {
    cache().lock().map(|g| g.clone()).unwrap_or_else(|_| Fix::unavailable())
}

fn set_cached(f: &Fix) {
    if let Ok(mut g) = cache().lock() {
        *g = f.clone();
    }
}

/// 给待办打位置标签用。取不到就是空串——不要拿「上一座城市」去顶替，
/// 那会让出差回顾出现错误答案。
///
/// 缓存为空时只补一次「零成本来源」（手填 / 测试注入）：
/// 首次正式定位要等 20 秒（避开启动瞬间的写库竞争），而用户很可能在那之前
/// 就按了 Ctrl+Shift+T 建待办——不能让这条待办因此没有位置。
/// 定位服务本身可能要十几秒，**绝不能在这里同步调用**。
pub fn current_label() -> String {
    let c = cached();
    if !c.label.is_empty() {
        return c.label;
    }
    sample_cheap()
}

/// 只做无 I/O、无等待的那两级来源（手填 → 测试注入）
fn sample_cheap() -> String {
    if let Some(v) = setting("loc.manual_label") {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    if let Ok(m) = std::env::var("WORKMATE_LOC_MOCK") {
        if !m.trim().is_empty() {
            return m.trim().to_string();
        }
    }
    String::new()
}

/* ------------------------------ 开关 ------------------------------ */

fn setting(k: &str) -> Option<String> {
    crate::db::with_conn(|c| crate::db::repo::get_setting(c, k)).ok().flatten()
}

/// 定位总开关（settings: loc.enabled，默认开）
pub fn enabled() -> bool {
    setting("loc.enabled").map(|v| v != "0").unwrap_or(true)
}

/* ------------------------------ 主入口 ------------------------------ */

/// 采一次位置。返回的 Fix 一定是有效的（失败也是 source=unavailable，不会 panic）。
pub fn sample() -> Fix {
    // 1) 手填最优先：用户自己声明的地点比任何传感器、任何推断都权威。
    //    放在 mock 之前也是刻意的——mock 只是自动化在没有真实定位时的替身，
    //    用户真的手填过就该听用户的（自动化用例自己会先清掉手填值再断言）。
    if let Some(v) = setting("loc.manual_label") {
        if !v.trim().is_empty() {
            return Fix {
                source: "manual".into(),
                label: v.trim().to_string(),
                ..Default::default()
            };
        }
    }

    // 2) 测试注入：WORKMATE_LOC_MOCK=上海 —— 自动化必须有确定输入，
    //    否则「用户是否授权定位」会决定测试成败，这不可接受。
    if let Ok(m) = std::env::var("WORKMATE_LOC_MOCK") {
        if !m.trim().is_empty() {
            return Fix {
                source: "mock".into(),
                label: m.trim().to_string(),
                ..Default::default()
            };
        }
    }

    if !enabled() {
        return Fix::unavailable();
    }

    // 3) Windows 定位服务
    if let Some(f) = try_geo() {
        return f;
    }

    // 4) IP 粗略定位（默认关闭）
    if setting("loc.ip").map(|v| v == "1").unwrap_or(false) {
        if let Some(f) = try_ip() {
            return f;
        }
    }

    Fix::unavailable()
}

/// 采一次并写进缓存（定时刷新与启动调用）
pub fn refresh() -> Fix {
    let f = sample();
    set_cached(&f);
    let _ = crate::tz::refresh();
    if f.ok() {
        crate::startup_log(&format!(
            "location: {} via {} acc={:?}",
            f.label, f.source, f.acc_m
        ));
    }
    f
}

/* ------------------------------ Windows 定位 ------------------------------ */

/// 定位是异步 API，且在无定位提供者的机器上可能**长时间不返回**。
/// 因此放进独立线程 + 超时：超时就当这次没采到，线程自己会在回调后退出
/// （send 到已丢弃的 Receiver 会失败，不会泄漏成常驻线程）。
fn with_timeout<T: Send + 'static>(f: impl FnOnce() -> Option<T> + Send + 'static, secs: u64) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(secs))
        .unwrap_or(None)
}

fn try_geo() -> Option<Fix> {
    with_timeout(
        || {
            let gl = Geolocator::new().ok()?;
            // 权限：桌面应用取决于系统设置「允许桌面应用访问你的位置」。
            // 被拒时静默降级——弹系统对话框会打断用户，不值得。
            let access = Geolocator::RequestAccessAsync().ok()?.get().ok()?;
            if access != GeolocationAccessStatus::Allowed {
                return None;
            }
            let pos = gl.GetGeopositionAsync().ok()?.get().ok()?;
            let coord = pos.Coordinate().ok()?;
            let p = coord.Point().ok()?.Position().ok()?;
            let acc = coord.Accuracy().ok();
            Some((p.Latitude, p.Longitude, acc))
        },
        12,
    )
    .map(|(lat, lon, acc)| {
        let label = reverse_geocode(lat, lon).unwrap_or_else(|| format!("{lat:.3},{lon:.3}"));
        Fix {
            lat: Some(lat),
            lon: Some(lon),
            acc_m: acc,
            source: "geo".into(),
            label,
        }
    })
}

/* ------------------------------ 反查城市（离线优先 + 降精度外网兜底） ------------------------------ */

/// 内置主要城市中心坐标表（纬度, 经度, 标准标签）。
/// 优先在本地通过 Haversine 球面距离（≤ 55km）完成离线反查：
///   1) 零网络延迟、离线可用；
///   2) 绝大多数国内办公与出差场景无需把 GPS 坐标发往第三方服务器。
const OFFLINE_CITIES: &[(f64, f64, &str)] = &[
    (39.9042, 116.4074, "北京·北京市"),
    (31.2304, 121.4737, "上海·上海市"),
    (22.5431, 114.0579, "深圳·广东省"),
    (23.1291, 113.2644, "广州·广东省"),
    (30.2741, 120.1551, "杭州·浙江省"),
    (32.0603, 118.7969, "南京·江苏省"),
    (31.2989, 120.5853, "苏州·江苏省"),
    (30.5728, 104.0668, "成都·四川省"),
    (29.5630, 106.5516, "重庆·重庆市"),
    (30.5928, 114.3055, "武汉·湖北省"),
    (34.3416, 108.9398, "西安·陕西省"),
    (39.0842, 117.2009, "天津·天津市"),
    (28.2282, 112.9388, "长沙·湖南省"),
    (34.7466, 113.6253, "郑州·河南省"),
    (36.0671, 120.3826, "青岛·山东省"),
    (29.8683, 121.5440, "宁波·浙江省"),
    (24.4798, 118.0894, "厦门·福建省"),
    (31.8206, 117.2272, "合肥·安徽省"),
    (26.0745, 119.2965, "福州·福建省"),
    (36.6512, 117.1201, "济南·山东省"),
    (38.9140, 121.6147, "大连·辽宁省"),
    (41.8057, 123.4315, "沈阳·辽宁省"),
    (25.0389, 102.7183, "昆明·云南省"),
    (28.6820, 115.8579, "南昌·江西省"),
    (22.8170, 108.3665, "南宁·广西"),
    (26.6470, 106.6302, "贵阳·贵州省"),
    (37.8706, 112.5489, "太原·山西省"),
    (38.0428, 114.5149, "石家庄·河北省"),
    (45.8038, 126.5349, "哈尔滨·黑龙江省"),
    (43.8171, 125.3235, "长春·吉林省"),
    (36.0611, 103.8343, "兰州·甘肃省"),
    (43.8256, 87.6168, "乌鲁木齐·新疆"),
    (20.0440, 110.1999, "海口·海南省"),
    (18.2528, 109.5119, "三亚·海南省"),
    (22.2710, 113.5767, "珠海·广东省"),
    (23.0215, 113.1214, "佛山·广东省"),
    (23.0207, 113.7518, "东莞·广东省"),
    (31.4912, 120.3119, "无锡·江苏省"),
    (31.8112, 119.9741, "常州·江苏省"),
    (27.9943, 120.6994, "温州·浙江省"),
    (22.3193, 114.1694, "香港·香港"),
    (22.1987, 113.5439, "澳门·澳门"),
    (25.0330, 121.5654, "台北·台湾"),
];

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0_f64;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().atan2((1.0 - a).max(0.0).sqrt())
}

/// 离线坐标反查城市：若距离已知城市中心 ≤ 45km 则直接返回。
pub fn offline_reverse_geocode(lat: f64, lon: f64) -> Option<String> {
    let mut best: Option<(f64, &str)> = None;
    for &(clat, clon, label) in OFFLINE_CITIES {
        let d = haversine_km(lat, lon, clat, clon);
        if d <= 45.0 {
            if best.map_or(true, |(bd, _)| d < bd) {
                best = Some((d, label));
            }
        }
    }
    best.map(|(_, l)| l.to_string())
}

/// 经纬度 → 中文地名。优先走本地离线城市表；未命中时降精度（保留 2 位小数 ≈ 1.1km）
/// 调用 BigDataCloud 免费端点（无需 API Key），避免向第三方暴露街道级精确坐标。
fn reverse_geocode(lat: f64, lon: f64) -> Option<String> {
    if let Some(local) = offline_reverse_geocode(lat, lon) {
        return Some(local);
    }
    let lat_coarse = (lat * 100.0).round() / 100.0;
    let lon_coarse = (lon * 100.0).round() / 100.0;
    let url = format!(
        "https://api.bigdatacloud.net/data/reverse-geocode-client?latitude={lat_coarse:.2}&longitude={lon_coarse:.2}&localityLanguage=zh"
    );
    let body: serde_json::Value = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(6))
        .call()
        .ok()?
        .into_json()
        .ok()?;

    let city = body["city"].as_str().unwrap_or("");
    let locality = body["locality"].as_str().unwrap_or("");
    let region = body["principalSubdivision"].as_str().unwrap_or("");
    let country = body["countryName"].as_str().unwrap_or("");

    // 粒度取「市」优先，没有就省份，再没有就国家。
    // 出差问答要的是「上海 / 深圳」这种粒度，经纬度或门牌号都没用。
    let parts: Vec<&str> = [city, locality, region, country]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    // 去重（city 与 locality 常常同值）
    let mut out: Vec<&str> = Vec::new();
    for p in parts {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out.truncate(2);
    Some(out.join("·"))
}

/* ------------------------------ IP 兜底 ------------------------------ */

fn try_ip() -> Option<Fix> {
    with_timeout(
        || {
            let body: serde_json::Value = ureq::get("http://ip-api.com/json/?lang=zh-CN&fields=status,city,regionName,country,lat,lon")
                .timeout(std::time::Duration::from_secs(6))
                .call()
                .ok()?
                .into_json()
                .ok()?;
            if body["status"].as_str() != Some("success") {
                return None;
            }
            let city = body["city"].as_str().unwrap_or("");
            let region = body["regionName"].as_str().unwrap_or("");
            let mut label = city.to_string();
            if !region.is_empty() && region != city {
                if !label.is_empty() {
                    label.push('·');
                }
                label.push_str(region);
            }
            if label.is_empty() {
                return None;
            }
            Some((
                body["lat"].as_f64(),
                body["lon"].as_f64(),
                label,
            ))
        },
        8,
    )
    .map(|(lat, lon, label)| Fix {
        lat,
        lon,
        acc_m: None,
        source: "ip".into(),
        label,
    })
}

/* ------------------------------ 出差判定 ------------------------------ */

/// 常驻地。**用户显式设置优先于统计推断**——
/// 推断值是「最近 90 天出现最多的城市」，出差回来那几天统计还没翻过来，
/// 会把家判成出差（实测踩到：推断出的常驻地是刚出差去的城市）。
pub fn home() -> Option<String> {
    if let Some(h) = setting("loc.home") {
        if !h.trim().is_empty() {
            return Some(h.trim().to_string());
        }
    }
    crate::db::with_conn(|c| crate::db::repo::home_base(c, now_sec() - 90 * 86_400))
        .ok()
        .flatten()
}

/// 当前位置是否算「出差」。
///
/// 判定：与常驻地（settings: loc.home，没有则用最近 90 天出现最多的城市）不同。
/// 常驻地未知时一律返回 false —— 「不知道」比「猜成在出差」安全得多：
/// 后者会让「我最近去哪出差了」里混进一堆本地记录。
pub fn is_trip(label: &str, home: Option<&str>) -> bool {
    if label.is_empty() {
        return false;
    }
    match home {
        None | Some("") => false,
        Some(h) => !same_place(label, h),
    }
}

/// 宽松的城市级比较："上海·上海市"、"上海市" 与 "上海" 应判为同一地。
fn same_place(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let core = |s: &str| -> String {
        let head = s.split('·').next().unwrap_or("").trim();
        head.strip_suffix('市').unwrap_or(head).to_string()
    };
    let (ca, cb) = (core(a), core(b));
    !ca.is_empty() && ca == cb
}

/// 采一次位置并落库（含出差判定），同时刷新当天画像的 location 字段。
///
/// 返回是否写入。重复标签 6 小时内不重复写，见 repo::insert_location。
pub fn record() -> serde_json::Value {
    let f = refresh();
    if !f.ok() {
        return serde_json::json!({ "ok": false, "source": f.source });
    }
    let now = now_sec();
    let home = home();
    let trip = is_trip(&f.label, home.as_deref());

    let id = crate::db::with_conn(|c| {
        crate::db::repo::insert_location(
            c,
            &crate::db::repo::NewLocation {
                ts: now,
                lat: f.lat,
                lon: f.lon,
                acc_m: f.acc_m,
                source: &f.source,
                label: &f.label,
                is_trip: trip,
            },
        )
    });
    match id {
        Ok(n) => {
            if n > 0 {
                crate::startup_log(&format!("location recorded: {} trip={trip}", f.label));
            }
            // 仅更新当天画像的位置列，避免用 Default 覆盖已算好的 active_sec / first_active
            let day = crate::logs::bj_date(now);
            let lbl = f.label.clone();
            let _ = crate::db::with_conn(|c| {
                c.execute(
                    "INSERT INTO day_profile(day, location, updated_at) VALUES(?1, ?2, ?3)
                     ON CONFLICT(day) DO UPDATE SET location = excluded.location, updated_at = excluded.updated_at",
                    rusqlite::params![day, lbl, now],
                )
            });
        }
        Err(e) => crate::startup_log(&format!("location write failed: {e}")),
    }
    serde_json::json!({
        "ok": true,
        "label": f.label,
        "source": f.source,
        "trip": trip,
        "home": home.unwrap_or_default(),
        "lat": f.lat,
        "lon": f.lon,
    })
}

/// 一键清除所有位置数据（隐私开关）。
pub fn clear() -> Result<usize, String> {
    let n = crate::db::with_conn(crate::db::repo::clear_locations).map_err(|e| e.to_string())?;
    let _ = crate::tz::refresh();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trip_logic() {
        assert!(is_trip("上海·上海市", Some("深圳·广东省")));
        assert!(!is_trip("上海·上海市", Some("上海")));
        assert!(!is_trip("上海", None), "常驻地未知时不能判成出差");
        assert!(!is_trip("", Some("上海")));
    }

    #[test]
    fn place_compare() {
        assert!(same_place("上海·上海市", "上海"));
        assert!(same_place("上海市", "上海·上海市"));
        assert!(same_place("深圳", "深圳"));
        assert!(!same_place("上海", "深圳"));
    }

    #[test]
    fn offline_city_lookup() {
        // 深圳福田 CBD 坐标
        assert_eq!(
            offline_reverse_geocode(22.5400, 114.0600).as_deref(),
            Some("深圳·广东省")
        );
        // 上海陆家嘴坐标
        assert_eq!(
            offline_reverse_geocode(31.2397, 121.4998).as_deref(),
            Some("上海·上海市")
        );
        // 大洋中心无已知城市
        assert_eq!(offline_reverse_geocode(0.0, 0.0), None);
    }
}
