// WorkMate · HTTP 控制面（自动化调试用）
// 编制：WorkMate Contributors
//
// 为什么要有这层：
// 桌面助手是 transparent + skipTaskbar —— 自动化侧既截不到画面（ImageGrab / PrintWindow 全是黑），
// 也模拟不了输入（透明窗收不到合成事件）。把所有能力暴露成本机 HTTP 接口后，
// 自动化就能自己驱动、自己断言，而不是把验证丢给用户人工点。
//
// 只监听 127.0.0.1:17820，不对外暴露。
//
// 常用：
//   GET /status                —— 窗口/数据库状态总览
//   GET /resource              —— 现采资源（内存/CPU/句柄/子进程树）
//   GET /watchdog?mem_hard=1   —— 调整看护阈值（测试熔断路径用）
//   GET /dom?window=panel      —— 读该窗口真实 DOM（前端上报的）
//   GET /dom?window=panel&refresh=1  —— 先触发前端重新上报，再返回
//   GET /panel/open | /panel/close | /pet/show | /pet/hide
//   GET /db/stats | /db/simulate?n=48 | /db/rollup
//   GET /llm/get | /llm/save?base=&model=&key= | /llm/test | /llm/chat?text= | /clipboard | /ask
//   GET /log?n=50              —— 读 startup.log 末尾
//   POST /report               —— 前端上报 DOM（自动化侧从 /dom 读回）

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{Emitter, Manager};

pub const PORT: u16 = 17820;

/// 并发连接上限：防止本机端口扫描或慢连接耗尽线程栈内存。
const MAX_ACTIVE_CONNS: usize = 32;
static ACTIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

struct ConnSlot;
impl ConnSlot {
    fn try_acquire() -> Option<Self> {
        let prev = ACTIVE_CONNS.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_ACTIVE_CONNS {
            ACTIVE_CONNS.fetch_sub(1, Ordering::Release);
            None
        } else {
            Some(Self)
        }
    }
}
impl Drop for ConnSlot {
    fn drop(&mut self) {
        ACTIVE_CONNS.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone, serde::Serialize)]
pub struct Report {
    pub window: String,
    pub msg: String,
    pub ts: i64,
}

fn reports() -> &'static Mutex<HashMap<String, Report>> {
    static M: OnceLock<Mutex<HashMap<String, Report>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 记录前端上报的 DOM 状态（限制窗口数与单条大小，防堆内存膨胀）
pub fn record(window: &str, msg: &str) {
    if window.is_empty() || window.len() > 32 {
        return;
    }
    if let Ok(mut m) = reports().lock() {
        if m.len() >= 8 && !m.contains_key(window) {
            return;
        }
        let capped: String = msg.chars().take(32_768).collect();
        m.insert(
            window.to_string(),
            Report {
                window: window.to_string(),
                msg: capped,
                ts: now_sec(),
            },
        );
    }
}

fn snap() -> HashMap<String, Report> {
    reports().lock().map(|m| m.clone()).unwrap_or_default()
}

pub fn serve(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let addr = format!("127.0.0.1:{PORT}");
        let listener = match bind_listener(&addr) {
            Ok(l) => l,
            Err(e) => {
                crate::startup_log(&format!("control bind failed: {e}"));
                return;
            }
        };
        crate::startup_log(&format!("control listening on http://{addr}"));
        // 每连接一个线程：/ask 这类端点最长要 60s（等 LLM），
        // 顺序处理会把整个调试面堵死（期间连 /status 都打不开）。
        // 并发上限 MAX_ACTIVE_CONNS + 读写超时，严防慢连接耗尽线程栈。
        for mut stream in listener.incoming().flatten() {
            let Some(slot) = ConnSlot::try_acquire() else {
                crate::startup_log("control rejected connection: max active connections reached");
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
            let app = app.clone();
            std::thread::spawn(move || {
                let _slot = slot;
                let _ = handle(&mut stream, &app);
            });
        }
    });
}

/// 绑定控制面端口。
///
/// Windows 上若开启 SO_REUSEADDR 会允许同机其他进程复绑同一端口劫持流量，
/// 因此不再启用 SO_REUSEADDR，改为依靠重试等待上一实例彻底释放套接字。
fn bind_listener(addr: &str) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sa: std::net::SocketAddr = addr
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad addr"))?;
    let mut last: Option<std::io::Error> = None;
    for attempt in 0..16 {
        let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        match sock.bind(&sa.into()).and_then(|_| sock.listen(128)) {
            Ok(()) => {
                if attempt > 0 {
                    crate::startup_log(&format!("control bind ok after {attempt} retries"));
                }
                return Ok(sock.into());
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("bind failed")))
}

/// 校验浏览器 Origin 是否属于受信任的本机 Tauri / Vite WebView。
fn is_trusted_webview_origin(origin: &str) -> bool {
    matches!(
        origin.trim().to_ascii_lowercase().as_str(),
        "http://localhost:1420"
            | "http://127.0.0.1:1420"
            | "tauri://localhost"
            | "http://tauri.localhost"
            | "https://tauri.localhost"
    )
}

/// 拦截外部网页发起的跨源驱动攻击（Drive-by CSRF / DNS Rebinding / SSRF）：
/// - 本机自动化测试脚本（urllib/curl）不携带 Origin 与 Sec-Fetch-Site，予以放行；
/// - Tauri WebView 内部仅允许受信任 Origin；
/// - 任何来自公网/外部网页（携带非授信 Origin，或无授信 Origin 却带 Sec-Fetch-Site: cross-site）
///   的请求一律在进入路由前以 403 拒绝，且绝不回传 Access-Control-Allow-Origin: *。
fn check_browser_security(
    headers_raw: &str,
) -> Result<Option<String>, &'static str> {
    let mut origin: Option<String> = None;
    let mut sec_fetch_site: Option<String> = None;
    for line in headers_raw.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            let k_lower = k.trim().to_ascii_lowercase();
            let v_trim = v.trim();
            if k_lower == "origin" {
                origin = Some(v_trim.to_string());
            } else if k_lower == "sec-fetch-site" {
                sec_fetch_site = Some(v_trim.to_ascii_lowercase());
            }
        }
    }

    if let Some(ref o) = origin {
        if !is_trusted_webview_origin(o) {
            return Err("blocked untrusted browser Origin");
        }
        return Ok(Some(o.clone()));
    }

    if let Some(ref sfs) = sec_fetch_site {
        // 诸如 <img src="http://127.0.0.1:17820/quit"> 或 <script src="..."> 不带 Origin，
        // 但现代浏览器均会强制附带 Sec-Fetch-Site: cross-site。
        if sfs != "same-origin" && sfs != "none" {
            return Err("blocked cross-site browser request without trusted Origin");
        }
    }

    Ok(None)
}

pub fn is_safe_log_filename(name: &str) -> bool {
    if name.is_empty() || name == "startup.log" {
        return true;
    }
    if name.len() > 128
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains(':')
        || name.chars().any(|c| c.is_control())
    {
        return false;
    }
    name.ends_with(".log") || name.ends_with(".log.gz")
}

fn handle(s: &mut TcpStream, app: &tauri::AppHandle) -> std::io::Result<()> {
    let mut buf = vec![0u8; 16384];
    let n = s.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
    let (headers_part, body_part) = match raw.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b.to_string()),
        None => (raw.as_str(), String::new()),
    };

    let allowed_origin = match check_browser_security(headers_part) {
        Ok(o) => o,
        Err(reason) => {
            crate::startup_log(&format!("control security blocked request: {reason}"));
            let msg = format!(r#"{{"ok":false,"error":"{reason}"}}"#);
            let head = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                msg.len()
            );
            s.write_all(head.as_bytes())?;
            s.write_all(msg.as_bytes())?;
            s.flush()?;
            return Ok(());
        }
    };

    let mut it = headers_part.split_whitespace();
    let method = it.next().unwrap_or("GET").to_string();
    let target = it.next().unwrap_or("/").to_string();
    let (path, query) = match target.find('?') {
        Some(i) => (target[..i].to_string(), target[i + 1..].to_string()),
        None => (target, String::new()),
    };

    if method == "OPTIONS" {
        let cors = allowed_origin
            .map(|o| format!("Access-Control-Allow-Origin: {o}\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\n"))
            .unwrap_or_default();
        let head = format!("HTTP/1.1 204 No Content\r\n{cors}Connection: close\r\n\r\n");
        s.write_all(head.as_bytes())?;
        s.flush()?;
        return Ok(());
    }

    let out = route(&method, &path, &query, &body_part, app);
    let bytes = out.into_bytes();
    let cors_header = allowed_origin
        .map(|o| format!("Access-Control-Allow-Origin: {o}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
        bytes.len(),
        cors_header
    );
    s.write_all(head.as_bytes())?;
    s.write_all(&bytes)?;
    s.flush()?;
    Ok(())
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for kv in q.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            m.insert(k.to_string(), url_decode(v));
        }
    }
    m
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn ok(v: Value) -> String {
    json!({ "ok": true, "data": v }).to_string()
}
fn err(msg: &str) -> String {
    json!({ "ok": false, "error": msg }).to_string()
}

fn win_info(app: &tauri::AppHandle, label: &str) -> Value {
    match app.get_webview_window(label) {
        Some(w) => json!({
            "exists": true,
            "visible": w.is_visible().unwrap_or(false),
            "pos": w.outer_position().ok().map(|p| json!({"x": p.x, "y": p.y})),
            "size": w.outer_size().ok().map(|s| json!({"w": s.width, "h": s.height})),
            "url": w.url().map(|u| u.to_string()).unwrap_or_default(),
        }),
        None => json!({ "exists": false }),
    }
}

fn tail_log(n: usize) -> Vec<String> {
    let p = crate::llm::config_path()
        .parent()
        .map(|d| d.join("startup.log"))
        .unwrap_or_default();
    match std::fs::read_to_string(&p) {
        Ok(s) => {
            let v: Vec<String> = s.lines().map(|l| l.to_string()).collect();
            v.into_iter().rev().take(n).rev().collect()
        }
        Err(e) => vec![format!("<read failed: {e}>")],
    }
}

fn route(method: &str, path: &str, query: &str, body: &str, app: &tauri::AppHandle) -> String {
    let q = parse_query(query);
    let g = |k: &str| q.get(k).cloned().unwrap_or_default();

    match (method, path) {
        ("GET", "/health") => ok(json!({
            "ok": true,
            "ts": now_sec(),
            "port": PORT,
            // 最近一次看护采样的资源快照（零成本，来自 20s 循环）
            "res": crate::watchdog::last(),
        })),

        // 现采一拍资源（含 WebView2 子进程树），测试侧断言用
        ("GET", "/resource") => ok(json!({
            "metrics": crate::watchdog::sample(),
            "thresholds": crate::watchdog::thresholds(),
        })),

        // 运行时调整看护阈值：/watchdog?mem_hard=1 即可在测试里验证熔断路径
        ("GET", "/watchdog") => {
            let num = |k: &str| q.get(k).and_then(|s| s.parse::<f64>().ok());
            crate::watchdog::configure(
                q.get("mem_soft").and_then(|s| s.parse().ok()),
                q.get("mem_hard").and_then(|s| s.parse().ok()),
                num("cpu_soft"),
                num("cpu_hard"),
            );
            ok(crate::watchdog::thresholds())
        },

        ("GET", "/status") => {
            let db = crate::commands::db_stats(app.state::<crate::db::Db>()).ok();
            ok(json!({
                "ts": now_sec(),
                "pet": win_info(app, "pet"),
                "panel": win_info(app, "panel"),
                "db": db,
                "remote": crate::display::is_remote_session(),
                "presenting": crate::perception::presenting(),
                "displays": crate::display::list(app).len(),
                "reported": snap().keys().cloned().collect::<Vec<_>>(),
            }))
        }

        // 显示器拓扑（多屏 / 云桌面 / DPI 适配的观测口）
        ("GET", "/display") => ok(json!({
            "displays": crate::display::list(app),
            "remote": crate::display::is_remote_session(),
            "signature": crate::display::signature(app),
        })),

        // 演示状态：GET 查询；?on=3600 手动开启 1 小时；?off=1 解除
        ("GET", "/presentation") => {
            if let Some(v) = q.get("on") {
                let secs = v.parse::<i64>().unwrap_or(3600);
                ok(crate::commands::set_presentation(app.clone(), Some(secs)))
            } else if q.contains_key("off") {
                ok(crate::commands::set_presentation(app.clone(), Some(0)))
            } else {
                ok(crate::commands::presentation_state())
            }
        }

        ("GET", "/dom") => {
            let w = if g("window").is_empty() {
                "pet".to_string()
            } else {
                g("window")
            };
            if g("refresh") == "1" {
                // 让前端重新上报一次
                let _ = app.emit_to(w.clone(), "wm:probe", "");
                std::thread::sleep(std::time::Duration::from_millis(700));
            }
            match snap().get(&w) {
                Some(r) => ok(json!(r)),
                None => err(&format!("no report from '{w}' yet")),
            }
        }

        // 在指定窗口的 WebView 里执行 JS（诊断用：直接问页面要真相，
        // 不依赖前端探针是否活着）。配合 /title 读回执行结果。
        ("GET", "/eval") => {
            let w = if g("window").is_empty() {
                "pet".to_string()
            } else {
                g("window")
            };
            let js = g("js");
            if js.is_empty() {
                return err("missing js");
            }
            match app.get_webview_window(&w) {
                Some(win) => match win.eval(&js) {
                    Ok(_) => ok(json!("evaluated")),
                    Err(e) => err(&e.to_string()),
                },
                None => err(&format!("window '{w}' missing")),
            }
        }

        // 读窗口标题：/eval 把结果写进 document.title 后从这里读回
        ("GET", "/title") => {
            let w = if g("window").is_empty() {
                "pet".to_string()
            } else {
                g("window")
            };
            match app.get_webview_window(&w) {
                Some(win) => ok(json!(win.title().unwrap_or_default())),
                None => err(&format!("window '{w}' missing")),
            }
        }

        // 求值并带回传值：Tauri 的 eval_with_callback 会把 JS 最后表达式的值
        // JSON 序列化后回调。这是从页面里读真相的唯一通道（/eval 是 fire-and-forget，
        // 连语法错误都返回 Ok，观察不到任何结果）。
        // 注意：Windows 上脚本抛异常时回调不触发 → 表现为 timeout。
        ("GET", "/evalr") => {
            let w = if g("window").is_empty() {
                "pet".to_string()
            } else {
                g("window")
            };
            let js = g("js");
            if js.is_empty() {
                return err("missing js");
            }
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            match app.get_webview_window(&w) {
                Some(win) => {
                    let tx2 = tx.clone();
                    match win.eval_with_callback(&js, move |s| {
                        let _ = tx2.send(s);
                    }) {
                        Ok(_) => match rx.recv_timeout(std::time::Duration::from_secs(4)) {
                            Ok(s) => ok(json!({ "result": s })),
                            Err(_) => err("eval callback timeout"),
                        },
                        Err(e) => err(&e.to_string()),
                    }
                }
                None => err(&format!("window '{w}' missing")),
            }
        }

        // 直接导航（不经 JS）：navigate
        ("GET", "/nav") => {
            let w = if g("window").is_empty() {
                "pet".to_string()
            } else {
                g("window")
            };
            let url = g("url");
            if url.is_empty() {
                return err("missing url");
            }
            match url.parse::<tauri::Url>() {
                Ok(u) => match app.get_webview_window(&w) {
                    Some(win) => match win.navigate(u) {
                        Ok(_) => ok(json!("navigating")),
                        Err(e) => err(&e.to_string()),
                    },
                    None => err(&format!("window '{w}' missing")),
                },
                Err(e) => err(&format!("bad url: {e}")),
            }
        }

        ("GET", "/panel/open") => match crate::commands::open_panel(app.clone()) {
            Ok(_) => ok(json!("opened")),
            Err(e) => err(&e),
        },
        ("GET", "/panel/close") => match app.get_webview_window("panel") {
            Some(w) => {
                let _ = w.hide();
                ok(json!("hidden"))
            }
            None => err("panel window missing"),
        },

        ("GET", "/pet/show") => match app.get_webview_window("pet") {
            Some(w) => {
                crate::perception::set_manual_present(0);
                crate::clear_present_hidden();
                let _ = w.show();
                ok(json!("shown"))
            }
            None => err("pet window missing"),
        },
        ("GET", "/pet/hide") => match crate::commands::hide_pet(app.clone()) {
            Ok(_) => ok(json!("hidden")),
            Err(e) => err(&e),
        },
        ("GET", "/pet/toggle") => match crate::commands::toggle_pet(app.clone()) {
            Ok(v) => ok(json!(v)),
            Err(e) => err(&e),
        },

        // 把宠物窗搬到指定物理坐标（测试多屏/越界收拢用）
        ("GET", "/pet/move") => {
            let Some(w) = app.get_webview_window("pet") else {
                return err("pet window missing");
            };
            let x = q.get("x").and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            let y = q.get("y").and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            match w.set_position(tauri::PhysicalPosition::new(x, y)) {
                Ok(_) => ok(json!({ "x": x, "y": y })),
                Err(e) => err(&e.to_string()),
            }
        }

        // 收拢到可见区（跑一次 ensure_visible）
        ("GET", "/pet/ensure") => {
            let moved = crate::display::ensure_visible(app);
            ok(json!({ "moved": moved.is_some(), "pos": moved }))
        }

        // 立即把当前位置写入 settings（跳过拖动节流）
        ("GET", "/pet/save") => {
            crate::display::save_position(app);
            ok(json!("saved"))
        }

        ("GET", "/db/stats") => match crate::commands::db_stats(app.state::<crate::db::Db>()) {
            Ok(s) => ok(serde_json::to_value(s).unwrap_or(json!(null))),
            Err(e) => err(&e),
        },
        ("GET", "/db/simulate") => {
            let n = q.get("n").and_then(|s| s.parse::<u32>().ok());
            match crate::commands::simulate_ticks(app.state::<crate::db::Db>(), n) {
                Ok(c) => ok(json!(c)),
                Err(e) => err(&e),
            }
        }
        ("GET", "/db/refresh") => {
            match crate::commands::ensure_stats_fresh_sync(app.state::<crate::db::Db>().inner()) {
                Ok(v) => ok(v),
                Err(e) => err(&e),
            }
        }

        // 分类时长（走预聚合表）：?window=86400
        ("GET", "/db/recent") => {
            let window = q.get("window").and_then(|s| s.parse::<i64>().ok()).unwrap_or(86_400);
            match crate::commands::recent_summary(app.state::<crate::db::Db>(), Some(window)) {
                Ok(v) => ok(serde_json::to_value(v).unwrap_or(json!(null))),
                Err(e) => err(&e),
            }
        }

        ("GET", "/db/reset") => {
            if q.get("confirm").map(|s| s.as_str()) != Some("1") {
                return err("需要 confirm=1 才执行（会清空 events 与 rollup）");
            }
            match crate::commands::reset_stats(app.state::<crate::db::Db>()) {
                Ok(v) => ok(v),
                Err(e) => err(&e),
            }
        }
        ("GET", "/db/rollup") => match crate::commands::rollup_now(app.state::<crate::db::Db>()) {
            Ok(c) => ok(json!(c)),
            Err(e) => err(&e),
        },
        ("GET", "/db/context") => {
            match crate::commands::current_context(app.state::<crate::db::Db>()) {
                Ok(c) => ok(serde_json::to_value(c).unwrap_or(json!(null))),
                Err(e) => err(&e),
            }
        }

        // 剪贴板类型识别（A2）：kind + 建议动作 + 预览
        ("GET", "/clipboard/info") => match crate::commands::clipboard_info() {
            Ok(v) => ok(v),
            Err(e) => err(&e),
        },

        // 自然语言待办（A3）：/todo/parse?text=.. 只解析；/todo/add?text=.. 解析并落库
        ("GET", "/todo/parse") => {
            let text = g("text");
            if text.is_empty() {
                return err("missing text");
            }
            ok(crate::commands::parse_todo(text))
        }
        ("GET", "/todo/add") => {
            let text = g("text");
            if text.is_empty() {
                return err("missing text");
            }
            match crate::commands::add_todo_sync(app.state::<crate::db::Db>().inner(), text, None) {
                Ok(v) => ok(v),
                Err(e) => err(&e),
            }
        }

        ("GET", "/llm/status") => ok(crate::commands::llm_status()),

        ("GET", "/llm/usage") => {
            let days = q.get("days").and_then(|s| s.parse::<i64>().ok());
            ok(crate::commands::llm_usage(days))
        }

        // 最后一次到点提醒的原始投递结果（不依赖气泡动画/淡出）
        ("GET", "/debug/last-reminder") => {
            let v = crate::LAST_REMINDER
                .lock()
                .map(|g| g.clone())
                .unwrap_or(serde_json::Value::Null);
            ok(v)
        }

        // AI 解析待办：/ai/task?text=... 或 /ai/task/clipboard
        ("GET", "/ai/task") => {
            let text = g("text");
            if text.is_empty() {
                return err("missing text");
            }
            match crate::commands::ai_task_sync(
                app.clone(),
                app.state::<crate::db::Db>().inner(),
                text,
            ) {
                Ok(v) => ok(v),
                Err(e) => err(&e),
            }
        }
        ("GET", "/ai/task/clipboard") => match crate::commands::ai_task_from_clipboard_sync(
            app.clone(),
            app.state::<crate::db::Db>().inner(),
        ) {
            Ok(v) => ok(v),
            Err(e) => err(&e),
        },

        ("GET", "/workload") => match crate::commands::workload_brief_sync(app.clone()) {
            Ok(v) => ok(v),
            Err(e) => err(&e),
        },

        // 清理归档日志：?days=7 保留一周，?days=0 全清
        ("GET", "/log/clear") => {
            let days = q.get("days").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let (n, bytes) = crate::logs::clear(days);
            ok(json!({ "files": n, "bytes": bytes }))
        }

        // 立即归档当前日志（测试与手动清理用）
        ("GET", "/log/rotate") => match crate::logs::rotate_if_needed() {
            Some(p) => ok(json!(p.display().to_string())),
            None => ok(json!(null)),
        },

        // 日志：列表 / 打开某个文件 / 打开目录（排查 AI 接口与报错用）
        ("GET", "/log/files") => ok(serde_json::to_value(crate::logs::list()).unwrap_or(json!(null))),

        ("GET", "/log/open") => {
            let name = g("f");
            // 只允许打开「当前日志」或 logs 目录下的 .log/.log.gz 归档，严禁路径穿越或打开可执行文件
            if !is_safe_log_filename(&name) {
                return err("非法文件名");
            }
            let target = if name.is_empty() || name == "startup.log" {
                crate::logs::current_log()
            } else {
                crate::logs::log_dir().map(|d| d.join(&name))
            };
            match target {
                Some(p) => match crate::logs::open_path(&p) {
                    Ok(_) => ok(json!(p.display().to_string())),
                    Err(e) => err(&e),
                },
                None => err("找不到日志路径"),
            }
        }

        ("GET", "/log/folder") => match crate::logs::log_dir() {
            Some(d) => match crate::logs::open_path(&d) {
                Ok(_) => ok(json!(d.display().to_string())),
                Err(e) => err(&e),
            },
            None => err("找不到日志目录"),
        },

        // 打扰调度状态（静默时段 / 预算）；?force_quiet=1 可在白天验证顺延分支
        ("GET", "/notify") => {
            // 测试钩子：?force_awake=1 忽略静默时段（用例与跑测时间无关）
            if let Some(v) = q.get("force_awake") {
                crate::notify::set_force_awake(v == "1");
            }
            if let Some(v) = q.get("force_quiet") {
                crate::notify::set_force_quiet(v == "1");
            }
            ok(crate::notify::status())
        }

        // 今日概览（A4 数据源）
        ("GET", "/today") => match crate::commands::today_summary_sync(app.state::<crate::db::Db>().inner()) {
            Ok(v) => ok(serde_json::to_value(v).unwrap_or(json!(null))),
            Err(e) => err(&e),
        },

        ("GET", "/today/blocks") => {
            let min = q.get("min").and_then(|s| s.parse::<i64>().ok());
            ok(crate::commands::today_blocks_sync(min))
        }

        // 提醒：/add_reminder?title=..&fire_at=..&level=1&kind=0 | /db/reminders | /db/reminder/done?id=
        ("GET", "/add_reminder") => {
            let title = g("title");
            if title.is_empty() {
                return err("missing title");
            }
            let fire_at = q.get("fire_at").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let level = q.get("level").and_then(|s| s.parse::<i64>().ok());
            let kind = q.get("kind").and_then(|s| s.parse::<i64>().ok());
            match crate::commands::add_reminder(app.state::<crate::db::Db>(), title, fire_at, level, kind) {
                Ok(id) => ok(json!(id)),
                Err(e) => err(&e),
            }
        }
        ("GET", "/db/reminders") => match crate::commands::list_reminders(app.state::<crate::db::Db>()) {
            Ok(v) => ok(serde_json::to_value(v).unwrap_or(json!(null))),
            Err(e) => err(&e),
        },
        ("GET", "/db/reminder/done") => {
            let id = q.get("id").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            match crate::commands::complete_reminder(app.state::<crate::db::Db>(), id) {
                Ok(v) => ok(json!(v)),
                Err(e) => err(&e),
            }
        }
        // 按标题前缀清理提醒：/db/gc?prefix=合并测试
        ("GET", "/db/gc") => {
            let prefix = g("prefix");
            match crate::commands::gc_reminders(app.state::<crate::db::Db>(), prefix) {
                Ok(n) => ok(json!(n)),
                Err(e) => err(&e),
            }
        }

        ("GET", "/db/reminder/del") => {
            let id = q.get("id").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            match crate::commands::delete_reminder(app.state::<crate::db::Db>(), id) {
                Ok(v) => ok(json!(v)),
                Err(e) => err(&e),
            }
        }

        // 设置项读写：/setting/get?k=pet.pos  /setting/set?k=pet.pos&v=...
        ("GET", "/setting/get") => {
            let k = g("k");
            if k.is_empty() {
                return err("missing k");
            }
            match crate::commands::get_setting(app.state::<crate::db::Db>(), k) {
                Ok(v) => ok(json!(v)),
                Err(e) => err(&e),
            }
        }
        ("GET", "/setting/set") => {
            let (k, v) = (g("k"), g("v"));
            if k.is_empty() {
                return err("missing k");
            }
            match crate::commands::set_setting(app.clone(), app.state::<crate::db::Db>(), k, v) {
                Ok(_) => ok(json!("saved")),
                Err(e) => err(&e),
            }
        }

        ("GET", "/llm/get") => ok(
            serde_json::to_value(crate::commands::get_llm_config()).unwrap_or(json!(null)),
        ),
        ("GET", "/llm/save") => {
            let base = g("base");
            let model = g("model");
            let key = g("key");
            match crate::commands::save_llm_config(base, model, key) {
                Ok(v) => ok(serde_json::to_value(v).unwrap_or(json!(null))),
                Err(e) => err(&e),
            }
        }
        ("GET", "/llm/test") => match crate::commands::test_llm_config_sync() {
            Ok(s) => ok(json!(s)),
            Err(e) => err(&e),
        },

        // 用指定文本问 LLM（不碰剪贴板）——验证 LLM 链路的主入口。
        // 例：/llm/chat?text=用一句话解释什么是债券久期
        ("GET", "/llm/chat") => {
            let text = g("text");
            match crate::commands::chat_text_sync(text, q.get("mode").cloned()) {
                Ok(s) => ok(json!(s)),
                Err(e) => err(&e),
            }
        },

        // 读当前剪贴板（截断到 2000 字符，防整篇文档撑爆响应）。
        // len 是完整字符数，方便判断「是不是没复制上」。
        ("GET", "/clipboard") => match crate::clipboard::read_text() {
            Ok(s) => ok(json!({
                "len": s.chars().count(),
                "text": crate::clipboard::truncate_chars(&s, 2000),
            })),
            Err(e) => err(&e),
        },

        // 完整「选中即问」链路：读剪贴板 → LLM → ai.answer 事件 → 宠物气泡。
        // 与 Ctrl+Shift+Space 走同一条命令；区别是错误直接回给 HTTP 调用方。
        ("GET", "/ask") => match crate::commands::ask_clipboard_sync(app.clone(), q.get("mode").cloned()) {
            Ok(s) => ok(json!(s)),
            Err(e) => err(&e),
        },

        // ---- 记忆层（v6）----
        // /memory/brief?days=7          —— 给 AI 的结构化摘要
        // /memory/ask?text=我最近去哪出差了 —— 自然语言问答（事实 + 模型措辞）
        // /memory/scan                  —— 立即重算风险
        // /memory/rebuild?days=7        —— 重算每日画像
        ("GET", "/memory/brief") => {
            let days = q.get("days").and_then(|s| s.parse::<i64>().ok());
            let trip = q.get("trip_days").and_then(|s| s.parse::<i64>().ok());
            ok(crate::commands::memory_brief_sync(days, trip))
        }

        ("GET", "/memory/ask") => {
            let text = g("text");
            if text.is_empty() {
                return err("missing text");
            }
            match crate::commands::memory_ask_sync(text) {
                Ok(v) => ok(v),
                Err(e) => err(&e),
            }
        }

        ("GET", "/memory/scan") => ok(crate::commands::memory_scan_sync()),

        ("GET", "/memory/rebuild") => {
            let days = q.get("days").and_then(|s| s.parse::<i64>().ok());
            ok(crate::commands::memory_rebuild_sync(days))
        }

        // 每日日记（每天一次的模型调用）：/memory/daily?day=2026-10-24&force=1
        ("GET", "/memory/daily") => ok(crate::commands::memory_daily_sync(
            q.get("day").cloned(),
            Some(q.get("force").map(|v| v == "1").unwrap_or(false)),
        )),

        // 分层抽象：/memory/rollup（月初自动跑，这里供手动触发）
        ("GET", "/memory/rollup") => ok(crate::commands::memory_rollup_sync()),

        // 读笔记：/memory/notes?scope=short|mid|long
        ("GET", "/memory/notes") => ok(crate::commands::memory_notes(
            g("scope"),
            q.get("limit").and_then(|s| s.parse::<i64>().ok()),
        )),

        // 温度提示：/memory/warm
        ("GET", "/memory/warm") => ok(json!(crate::commands::memory_warm())),

        // ---- 位置 ----
        // /loc/now                      —— 读缓存的定位（不触发采集）
        // /loc/refresh                  —— 立即采一次并落库
        // /loc/set?label=上海&home=1    —— 手填位置（可同时设为常驻地）
        // /loc/history?days=120         —— 按天的行程
        // /loc/clear                    —— 清除全部位置数据
        ("GET", "/loc/now") => ok(crate::commands::location_now()),

        ("GET", "/loc/refresh") => ok(crate::commands::location_refresh_sync()),

        ("GET", "/loc/set") => {
            let label = g("label");
            let as_home = q.get("home").map(|v| v == "1").unwrap_or(false);
            match crate::commands::location_set_sync(
                app.clone(),
                app.state::<crate::db::Db>().inner(),
                label,
                Some(as_home),
            ) {
                Ok(v) => ok(v),
                Err(e) => err(&e),
            }
        }

        // 时区查询与设置：/tz 查询；/tz?offset=480 手动固定偏移；/tz?auto=1 恢复跟随城市
        ("GET", "/tz") => {
            if q.get("auto").map(|v| v == "1").unwrap_or(false) {
                match crate::commands::tz_set(app.clone(), app.state::<crate::db::Db>(), None) {
                    Ok(v) => ok(v),
                    Err(e) => err(&e),
                }
            } else if let Some(off) = q.get("offset").and_then(|s| s.parse::<i64>().ok()) {
                match crate::commands::tz_set(app.clone(), app.state::<crate::db::Db>(), Some(off)) {
                    Ok(v) => ok(v),
                    Err(e) => err(&e),
                }
            } else {
                ok(crate::commands::tz_status())
            }
        }

        // 专注模式与打断统计：/focus 查询状态与趋势；/focus?act=start&min=50 开始专注；/focus?act=stop 结束专注
        ("GET", "/focus") => match g("act").as_str() {
            "start" => {
                let min = q.get("min").and_then(|s| s.parse::<i64>().ok());
                ok(crate::commands::focus_start(app.clone(), min))
            }
            "stop" => ok(crate::commands::focus_stop(app.clone())),
            _ => ok(crate::commands::focus_status()),
        },

        ("GET", "/loc/history") => {
            let days = q.get("days").and_then(|s| s.parse::<i64>().ok());
            ok(crate::commands::location_history(days))
        }

        ("GET", "/loc/raw") => {
            let days = q.get("days").and_then(|s| s.parse::<i64>().ok());
            ok(crate::commands::location_raw(days))
        }

        ("GET", "/loc/clear") => match crate::commands::location_clear() {
            Ok(n) => ok(json!(n)),
            Err(e) => err(&e),
        },

        ("GET", "/log") => {
            let n = q.get("n").and_then(|s| s.parse::<usize>().ok()).unwrap_or(50);
            ok(json!(tail_log(n)))
        }

        ("GET", "/quit") => {
            crate::commands::quit_app(app.clone());
            ok(json!("quitting"))
        }

        ("POST", "/report") => {
            let v: Value = serde_json::from_str(body).unwrap_or(json!({}));
            let win = v
                .get("window")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string();
            let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("").to_string();
            record(&win, &msg);
            crate::startup_log(&format!("FE | {msg}"));
            ok(json!("recorded"))
        }

        _ => err(&format!("unknown route {method} {path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_browser_security_blocks_drive_by_attacks() {
        // 1. 本机测试脚本（无 Origin、无 Sec-Fetch-Site）放行
        let cli_req = "GET /health HTTP/1.1\r\nHost: 127.0.0.1:17820\r\nUser-Agent: Python-urllib/3.11";
        assert_eq!(check_browser_security(cli_req), Ok(None));

        // 2. Tauri WebView 合法 Origin 放行（即使带 Sec-Fetch-Site: cross-site）
        let tauri_req = "POST /report HTTP/1.1\r\nHost: 127.0.0.1:17820\r\nOrigin: http://tauri.localhost\r\nSec-Fetch-Site: cross-site";
        assert_eq!(
            check_browser_security(tauri_req),
            Ok(Some("http://tauri.localhost".to_string()))
        );

        // 3. 外部恶意网页 fetch/POST（带恶意的 Origin）拦截
        let evil_fetch = "GET /eval?window=pet&js=alert(1) HTTP/1.1\r\nHost: 127.0.0.1:17820\r\nOrigin: https://evil.example.com";
        assert!(check_browser_security(evil_fetch).is_err());

        // 4. 外部恶意网页 <img src="http://127.0.0.1:17820/quit">（无 Origin，但有 Sec-Fetch-Site: cross-site）拦截
        let evil_img = "GET /quit HTTP/1.1\r\nHost: 127.0.0.1:17820\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors";
        assert!(check_browser_security(evil_img).is_err());
    }

    #[test]
    fn test_safe_log_filename() {
        assert!(is_safe_log_filename(""));
        assert!(is_safe_log_filename("startup.log"));
        assert!(is_safe_log_filename("workmate-2026-10-24.log.gz"));
        assert!(!is_safe_log_filename("../secret.log"));
        assert!(!is_safe_log_filename("C:evil.log"));
        assert!(!is_safe_log_filename("calc.exe"));
        assert!(!is_safe_log_filename("sub/startup.log"));
    }
}
