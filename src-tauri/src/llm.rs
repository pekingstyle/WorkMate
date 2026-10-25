// WorkMate · 模型服务配置（OpenAI 兼容）
// 编制：WorkMate Contributors
//
// 设计要点：
//   1. 只依赖 OpenAI 兼容协议（/chat/completions），一套配置通吃 DeepSeek / 通义 / 智谱 / 本地 Ollama
//   2. **api_key 用 Windows DPAPI 加密后落盘**（CryptProtectData，绑定当前用户账户），
//      配置文件被拷到别的机器或别的账户都解不开；不把明文写进磁盘
//   3. 前端永远拿不到 key 明文，只能拿到「是否已配置」和掩码
//
// 配置路径：%APPDATA%\org.atomgit.workmate\llm.json

use std::path::PathBuf;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows::Win32::Foundation::{LocalFree, HLOCAL};

fn default_stream_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmFile {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// base64(DPAPI(api_key))，绝不明文落盘
    #[serde(default)]
    pub key_cipher: String,
    /// 是否开启流式输出（边生成边展示，自动过滤 <think> 思考过程；默认开启）
    #[serde(default = "default_stream_true")]
    pub stream: bool,
}

impl Default for LlmFile {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            key_cipher: String::new(),
            stream: true,
        }
    }
}

/// 返回给前端的视图对象：**不含明文 key**
#[derive(Debug, Clone, Serialize)]
pub struct LlmView {
    pub base_url: String,
    pub model: String,
    pub has_key: bool,
    pub key_masked: String,
    pub enabled: bool,
    pub stream: bool,
}

/// 配置文件路径。
///
/// 解析顺序（两个开关分工不同）：
///   1. `WORKMATE_LLM_PATH`——只覆盖 LLM 配置文件。需要「真连用户真实 Key」的
///      用例（AI 建待办、记忆问答）用它指向生产配置；要测「未配置/失败」分支的
///      用例用它指向临时文件。**这是唯一允许碰生产 llm.json 的路径**。
///   2. `WORKMATE_DATA_DIR`——整体数据目录。测试沙箱里 llm.json 自然落在沙箱内，
///      读写都碰不到生产配置。
///   3. 默认 %APPDATA%\org.atomgit.workmate\llm.json。
///
/// 历史教训：早先测试里的「清空配置」步骤直接抹掉了用户真实 Key。
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("WORKMATE_LLM_PATH") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let dir = crate::data_dir();
    std::fs::create_dir_all(&dir).ok();
    dir.join("llm.json")
}

/* ------------------------------ DPAPI ------------------------------ */

pub fn protect_str(plain: &str) -> Option<String> {
    protect(plain)
}

fn protect(plain: &str) -> Option<String> {
    unsafe {
        let bytes = plain.as_bytes();
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();

        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .ok()?;

        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let encoded = B64.encode(slice);
        let _ = LocalFree(HLOCAL(output.pbData as *mut _));
        Some(encoded)
    }
}

fn unprotect(cipher_b64: &str) -> Option<String> {
    let raw = B64.decode(cipher_b64).ok()?;
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: raw.len() as u32,
            pbData: raw.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();

        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .ok()?;

        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let text = String::from_utf8_lossy(slice).to_string();
        let _ = LocalFree(HLOCAL(output.pbData as *mut _));
        Some(text)
    }
}

/* ------------------------------ 读写 ------------------------------ */

pub fn load_file() -> LlmFile {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str::<LlmFile>(&s).ok())
        .unwrap_or_default()
}

pub fn save_file(f: &LlmFile) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(f).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

/// 取明文 key（仅内部使用，不返回给前端）
pub(crate) fn api_key(f: &LlmFile) -> Option<String> {
    if f.key_cipher.is_empty() {
        None
    } else {
        unprotect(&f.key_cipher)
    }
}

pub fn to_view(f: &LlmFile) -> LlmView {
    let key = api_key(f);
    let masked = match &key {
        Some(k) if k.len() > 8 => format!("{}****{}", &k[..4], &k[k.len() - 4..]),
        Some(_) => "****".to_string(),
        None => String::new(),
    };
    LlmView {
        base_url: f.base_url.clone(),
        model: f.model.clone(),
        has_key: key.is_some(),
        key_masked: masked,
        enabled: !f.base_url.is_empty() && !f.model.is_empty() && key.is_some(),
        stream: f.stream,
    }
}

/* ------------------------------ 连通性测试 ------------------------------ */

/// 桌面宠物助手人设（「选中即问」用）。
/// 要点：中文、直接、不客套、不重复问题——宠物气泡里每一句废话都在占屏幕。
const SYSTEM_PROMPT: &str = "你是 WorkMate，用户电脑桌上的宠物助手。用户会发来他刚选中/复制的文字，\
可能是问题、代码、术语或一段话。请直接回答或解释：用简体中文；不要复述用户的问题；\
不要「好的」「当然可以」这类开场白；不确定就明说不确定。回答控制在必要长度，能用三句话就别用十句。";

/// 动作模式（A1「解释/翻译/改写 3 版」+ A2 类型提议都收敛到这里）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Explain,
    Translate,
    Rewrite,
    Summarize,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.trim().to_ascii_lowercase().as_str() {
            "translate" | "翻译" => Mode::Translate,
            "rewrite" | "改写" => Mode::Rewrite,
            "summarize" | "summary" | "总结" => Mode::Summarize,
            _ => Mode::Explain,
        }
    }

    pub fn system_prompt(self) -> &'static str {
        match self {
            Mode::Explain => SYSTEM_PROMPT,
            Mode::Translate => {
                "你是翻译。把用户发来的文字翻译成简体中文；如果原文已经是中文，翻译成英文。\
只输出译文，不要解释、不要加引号、不要「以下是译文」这类前缀。\
代码块原样保留（只翻译注释与字符串里的自然语言）。"
            }
            Mode::Rewrite => {
                "你是文字编辑。把用户发来的文字改写得更清晰、更简洁，保持原意与语言。\
只输出改写后的文字，不要解释改了什么。若原文是代码，只做格式化注释与命名建议，不改逻辑。"
            }
            Mode::Summarize => {
                "你是摘要器。用简体中文把用户发来的内容压缩成不超过三句的要点；\
保留关键数字与结论；不要复述原文结构，不要「这篇文章讲了」这类开场。"
            }
        }
    }
}

/// 剪贴板内容类型（A2「识别类型 → 提议动作」的输入）
pub fn detect_kind(text: &str) -> &'static str {
    let t = text.trim();
    if t.is_empty() {
        return "empty";
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.") {
        return "url";
    }
    // 代码特征：出现两个以上典型语法符号，或含常见关键字
    let code_hits = ["fn ", "def ", "class ", "import ", "=>", "const ", "function ", "public ", "SELECT "]
        .iter()
        .filter(|k| text.contains(**k))
        .count();
    let brace = text.matches('{').count() + text.matches('}').count();
    let semi = text.matches(';').count();
    if code_hits >= 1 || (brace >= 2 && semi >= 1) {
        return "code";
    }
    if t.chars().all(|c| c.is_ascii_digit() || "-+.,:%".contains(c)) {
        return "number";
    }
    if t.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
        if t.chars().count() > 200 {
            return "text_long";
        }
        return "text_zh";
    }
    if t.chars().count() > 400 {
        return "text_long";
    }
    "text_en"
}

/// 按内容类型建议动作（面板/菜单展示用，保留兼容既有契约）
pub fn suggest_actions(kind: &str) -> Vec<&'static str> {
    match kind {
        "code" => vec!["explain", "summarize"],
        "url" => vec!["summarize", "translate"],
        "text_zh" | "text_en" => vec!["translate", "rewrite", "summarize"],
        "text_long" => vec!["summarize", "rewrite"],
        "number" => vec!["explain"],
        _ => vec!["explain"],
    }
}

/// 识别用户在当前剪贴板内容下面临的**真实工作场景**（不侵入外部应用，靠选中复制做上下文桥梁）：
///   - `"trip"`：出差 / 异地拜访 / 行程安排（需拆出「行程日程 + 行前准备待办」并检查行程日冲突）
///   - `"task"`：待处理任务 / 会议邀约 / 工作交办（需提取待办/会议入库，并结合当前手头待办评估负载）
///   - `"question"`：遇到问题 / 术语概念 / 报错排查 / 方案分析（需直接给出解答或可执行建议）
///   - `"empty"`：剪贴板为空
/// 返回 `(scene_code, scene_label, scene_hint)`。
pub fn detect_scene(text: &str) -> (&'static str, &'static str, &'static str) {
    let t = text.trim();
    if t.is_empty() {
        return ("empty", "剪贴板为空", "在任意应用选中文字按 Ctrl+C 即可与助手交互");
    }

    // 1) 出差与异地行程信号
    let trip_kw = [
        "出差", "机票", "航班", "高铁", "动车", "订票", "酒店", "住宿",
        "拜访", "见客户", "路演", "驻场", "飞往", "去深圳", "去上海",
        "去北京", "去广州", "去杭州", "去成都", "去南京", "去武汉", "去香港",
    ];
    let cities = [
        "深圳", "上海", "北京", "广州", "杭州", "南京", "苏州", "成都",
        "重庆", "武汉", "西安", "天津", "长沙", "郑州", "青岛", "厦门", "香港",
    ];
    let move_cues = ["去", "到", "飞", "赴", "见", "客户", "行程", "出差"];
    let has_trip_kw = trip_kw.iter().any(|k| t.contains(k));
    let has_city_move = cities.iter().any(|c| t.contains(c)) && move_cues.iter().any(|m| t.contains(m));
    if has_trip_kw || has_city_move {
        return (
            "trip",
            "出差与行程安排",
            "拆解『出差日程 + 行前准备』并核对行程日工作负载",
        );
    }

    // 2) 待办交办与会议约定信号（排除纯代码块）
    if detect_kind(t) != "code" {
        let task_kw = [
            "提醒", "待办", "之前", "发给", "提交", "周报", "对齐", "评审",
            "开会", "会议", "讨论", "跟进", "确认", "准备好", "截止", "下班前",
            "明天", "后天", "今天下午", "今晚", "下周", "点半", "分钟后", "小时后",
        ];
        let hits = task_kw.iter().filter(|k| t.contains(**k)).count();
        if hits >= 1 && t.chars().count() <= 500 {
            return (
                "task",
                "待办与工作交办",
                "提取待办/会议入库，并结合当前手头任务评估负载",
            );
        }
    }

    // 3) 默认：问题解答 / 报错排查 / 内容分析
    (
        "question",
        "问题解答与分析",
        "结合当前工作状态直接解答问题或提炼要点",
    )
}

/// 规范化用户填写的 Base URL，并生成候选 `/chat/completions` 端点列表。
///
/// 用户实际配置模型时常遇到以下坑：
///   1) 复制时带上了首尾引号、空格或末尾 `/`；
///   2) 只填了根域名（如 `https://api.deepseek.com` 或 `http://127.0.0.1:11434`），漏了 `/v1`；
///   3) 填了完整端点 `.../v1/chat/completions`，甚至误填成 `.../v1/models`；
///   4) 部分国产网关（如智谱 `/api/paas/v4`）不带 `/v1`，而标准网关需要 `/v1`。
/// 因此生成 `[主端点, 备选端点]`，主端点遇到 HTTP 404 时自动尝试备选端点。
fn candidate_endpoints(f: &LlmFile) -> Result<Vec<String>, String> {
    let raw = f
        .base_url
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim()
        .trim_end_matches('/');
    if raw.is_empty() {
        return Err("尚未填写 Base URL".into());
    }
    if raw.ends_with("/chat/completions") {
        return Ok(vec![raw.to_string()]);
    }
    if let Some(prefix) = raw.strip_suffix("/models") {
        let prefix = prefix.trim_end_matches('/');
        return Ok(vec![format!("{prefix}/chat/completions")]);
    }

    // 判断 scheme://host[:port] 之后是否完全没有路径段
    let after_scheme = raw
        .find("://")
        .map(|i| &raw[i + 3..])
        .unwrap_or(raw);
    let has_path = after_scheme.contains('/');

    if !has_path {
        // 只有根域名（如 https://api.openai.com 或 http://localhost:11434）：优先尝试 /v1/chat/completions
        Ok(vec![
            format!("{raw}/v1/chat/completions"),
            format!("{raw}/chat/completions"),
        ])
    } else if raw.ends_with("/v1") || raw.contains("/v1/") || raw.ends_with("/v4") || raw.ends_with("/v3") {
        Ok(vec![format!("{raw}/chat/completions")])
    } else {
        Ok(vec![
            format!("{raw}/chat/completions"),
            format!("{raw}/v1/chat/completions"),
        ])
    }
}

/// 兼容单端点获取（保留内部使用与单测）
#[allow(dead_code)]
fn endpoint(f: &LlmFile) -> Result<String, String> {
    candidate_endpoints(f).map(|mut v| v.remove(0))
}

/// 当网关返回 HTTP 400 抱怨不支持 `temperature` 或 `max_tokens`（如 o1/o3/o4-mini 等推理模型要求
/// `max_completion_tokens` 且禁止自定义 `temperature`）时，自动降级请求体参数。
fn downgrade_incompatible_params(
    body: &serde_json::Value,
    err_text: &str,
) -> Option<serde_json::Value> {
    let lower = err_text.to_ascii_lowercase();
    let mut obj = body.as_object()?.clone();
    let mut changed = false;

    if (lower.contains("temperature") || lower.contains("unsupported_value") || lower.contains("unsupported parameter"))
        && obj.remove("temperature").is_some()
    {
        changed = true;
    }
    if lower.contains("max_completion_tokens") || (lower.contains("max_tokens") && lower.contains("unsupported")) {
        if let Some(mt) = obj.remove("max_tokens") {
            obj.insert("max_completion_tokens".to_string(), mt);
            changed = true;
        }
    }
    if changed {
        Some(serde_json::Value::Object(obj))
    } else {
        None
    }
}

/// 发一次 chat/completions 请求，把响应体解析成 JSON。
/// 统一处理 ureq 的两种错误形态（网络错误 / 非 2xx 状态码），错误信息截断防刷屏。
/// 一次调用的实际用量（取自响应的 usage 字段）。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// 从 chat/completions 响应里取 usage。
///
/// 有些网关不返回 usage，三个字段就是 0——请求次数仍会累加，
/// 所以「今天调了几次」永远准，「用了多少 token」可能偏小。这是能接受的方向：
/// 防滥用看的是数量级，少算不会导致误判为滥用，多算才会。
fn usage_of(v: &serde_json::Value) -> Usage {
    let u = &v["usage"];
    let prompt = u["prompt_tokens"]
        .as_i64()
        .or_else(|| u["input_tokens"].as_i64())
        .unwrap_or(0);
    let completion = u["completion_tokens"]
        .as_i64()
        .or_else(|| u["output_tokens"].as_i64())
        .unwrap_or(0);
    let total = u["total_tokens"].as_i64().unwrap_or(prompt + completion);
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
    }
}

/// 本次启动以来的累计用量（内存态，重启清零；按天的持久化在库里）。
/// 面板同时展示两个视角：今天用了多少（库）+ 本次启动以来（内存）。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct UsageTotal {
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub failed: i64,
}

fn usage_cell() -> &'static std::sync::Mutex<UsageTotal> {
    static M: std::sync::OnceLock<std::sync::Mutex<UsageTotal>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(UsageTotal::default()))
}

pub fn usage_since_start() -> UsageTotal {
    usage_cell().lock().map(|g| *g).unwrap_or_default()
}

/// 累加用量。**全程不允许抛错**：用量统计失败绝不能影响 AI 功能本身。
fn track_usage(prompt: i64, completion: i64, ok: bool) {
    if let Ok(mut g) = usage_cell().lock() {
        g.requests += 1;
        g.prompt_tokens += prompt;
        g.completion_tokens += completion;
        g.total_tokens += prompt + completion;
        g.failed += if ok { 0 } else { 1 };
    }
    let day = crate::logs::bj_date(now_sec());
    let _ = crate::db::with_conn(|c| {
        crate::db::repo::bump_usage(c, &day, prompt, completion, ok)
    });
}

/* ---------------- 网络自适应与 TUN / Fake-IP 故障自动绕行 ---------------- */

fn is_fake_ip_v4(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    // RFC 2544 基准测试网段 198.18.0.0/15，被 Clash / Mihomo / Surge 等广泛用作 TUN Fake-IP
    o[0] == 198 && (o[1] == 18 || o[1] == 19)
}

fn is_fake_ip_v6(ip: std::net::Ipv6Addr) -> bool {
    let s = ip.segments();
    // 2001:2::/48 (RFC 5180) 与 fc00::/7 常被用作 TUN Fake-IPv6
    (s[0] == 0x2001 && s[1] == 0x0002) || ((s[0] & 0xfe00) == 0xfc00)
}

/// 跳过 DNS 报文中的域名字段（支持普通标签与 0xC0 压缩指针），带步数上限防畸形包死循环。
fn skip_dns_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    for _ in 0..64 {
        let b = *buf.get(pos)?;
        if b == 0 {
            return Some(pos + 1);
        }
        if (b & 0xC0) == 0xC0 {
            return if pos + 1 < buf.len() {
                Some(pos + 2)
            } else {
                None
            };
        }
        if (b & 0xC0) != 0 {
            return None;
        }
        pos = pos.checked_add(1 + b as usize)?;
        if pos > buf.len() {
            return None;
        }
    }
    None
}

/// 解析 DNS 响应报文中的 A 记录（排除 Fake-IP）。
fn parse_dns_a_records(buf: &[u8]) -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    if buf.len() < 12 || (buf[3] & 0x0F) != 0 {
        return out;
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    let ancount = u16::from_be_bytes([buf[6], buf[7]]);
    let mut pos = 12usize;
    for _ in 0..qdcount {
        let Some(next) = skip_dns_name(buf, pos) else {
            return out;
        };
        pos = next + 4;
        if pos > buf.len() {
            return out;
        }
    }
    for _ in 0..ancount {
        let Some(next) = skip_dns_name(buf, pos) else {
            break;
        };
        pos = next;
        if pos + 10 > buf.len() {
            break;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rclass = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);
        let rdlen = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlen > buf.len() {
            break;
        }
        if rtype == 1 && rclass == 1 && rdlen == 4 {
            let ip = std::net::Ipv4Addr::new(buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]);
            if !ip.is_unspecified() && !ip.is_loopback() && !is_fake_ip_v4(ip) {
                out.push(ip);
            }
        }
        pos += rdlen;
    }
    out
}

/// 枚举本机候选物理网卡 IPv4 地址（排除环回、链路本地与 198.18.0.0/15 TUN 虚拟网卡）。
fn candidate_physical_nics() -> Vec<std::net::Ipv4Addr> {
    use std::net::{SocketAddr, ToSocketAddrs};
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    if host.is_empty() {
        return Vec::new();
    }
    let mut nics = Vec::new();
    if let Ok(addrs) = (host.as_str(), 0u16).to_socket_addrs() {
        for addr in addrs {
            if let SocketAddr::V4(v4) = addr {
                let ip = *v4.ip();
                let o = ip.octets();
                if ip.is_loopback()
                    || ip.is_unspecified()
                    || (o[0] == 169 && o[1] == 254)
                    || is_fake_ip_v4(ip)
                {
                    continue;
                }
                if !nics.contains(&ip) {
                    nics.push(ip);
                }
            }
        }
    }
    // 优先排常规局域网终端地址（192.168.* / 10.* 且末位 != 1），将 Hyper-V/WSL 的 .1 虚拟网桥排在后面
    nics.sort_by_key(|ip| {
        let o = ip.octets();
        let is_gateway_one = o[3] == 1;
        let tier = if o[0] == 192 && o[1] == 168 {
            0
        } else if o[0] == 10 {
            1
        } else {
            2
        };
        (is_gateway_one, tier)
    });
    nics
}

/// 绑定指定物理网卡 IP 向公共 DNS (223.5.5.5 / 119.29.29.29) 查询真实 A 记录，绕过 TUN Fake-IP 劫持。
fn dns_query_a_via_nic(
    domain: &str,
    nic_ip: std::net::Ipv4Addr,
) -> Option<Vec<std::net::Ipv4Addr>> {
    let mut query = Vec::with_capacity(64);
    query.extend_from_slice(&[0x57, 0x4D, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    for label in domain.trim_end_matches('.').split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return None;
        }
        query.push(bytes.len() as u8);
        query.extend_from_slice(bytes);
    }
    query.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x01]);

    for dns_srv in [
        std::net::Ipv4Addr::new(223, 5, 5, 5),
        std::net::Ipv4Addr::new(119, 29, 29, 29),
    ] {
        let Ok(sock) = std::net::UdpSocket::bind((nic_ip, 0)) else {
            continue;
        };
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(1500)));
        let _ = sock.set_write_timeout(Some(std::time::Duration::from_millis(1500)));
        if sock.send_to(&query, (dns_srv, 53)).is_err() {
            continue;
        }
        let mut buf = [0u8; 512];
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            let ips = parse_dns_a_records(&buf[..n]);
            if !ips.is_empty() {
                return Some(ips);
            }
        }
    }
    None
}

fn cached_nic_cell() -> &'static std::sync::Mutex<Option<std::net::Ipv4Addr>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<std::net::Ipv4Addr>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(None))
}

fn discover_direct_route(
    domain: &str,
) -> Option<(std::net::Ipv4Addr, Vec<std::net::Ipv4Addr>)> {
    if let Some(cached) = cached_nic_cell().lock().ok().and_then(|g| *g) {
        if let Some(ips) = dns_query_a_via_nic(domain, cached) {
            return Some((cached, ips));
        }
    }
    for nic in candidate_physical_nics() {
        if let Some(ips) = dns_query_a_via_nic(domain, nic) {
            if let Ok(mut g) = cached_nic_cell().lock() {
                *g = Some(nic);
            }
            return Some((nic, ips));
        }
    }
    None
}

fn split_netloc(netloc: &str) -> (&str, u16) {
    if let Some((h, p)) = netloc.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return (h.trim_matches(|c| c == '[' || c == ']'), port);
        }
    }
    (netloc, 443)
}

/// 常规解析器：优先 IPv4，过滤掉 2001:2::/48 等 TUN Fake-IPv6 地址。
struct Ipv4PreferResolver;

impl ureq::Resolver for Ipv4PreferResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        let mut addrs: Vec<std::net::SocketAddr> = netloc
            .to_socket_addrs()?
            .filter(|a| match a {
                std::net::SocketAddr::V6(v6) => !is_fake_ip_v6(*v6.ip()),
                std::net::SocketAddr::V4(_) => true,
            })
            .collect();
        addrs.sort_by_key(|a| a.is_ipv6());
        if addrs.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no usable address found",
            ));
        }
        Ok(addrs)
    }
}

/// 物理网卡直连解析器：当系统存在 TUN 虚拟网卡劫持且代理节点异常导致 TLS EOF 时启用。
struct DirectNicResolver;

impl ureq::Resolver for DirectNicResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::{IpAddr, SocketAddr};
        let (host, port) = split_netloc(netloc);
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        if let Some((_nic, ips)) = discover_direct_route(host) {
            let addrs: Vec<SocketAddr> = ips
                .into_iter()
                .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
                .collect();
            if !addrs.is_empty() {
                return Ok(addrs);
            }
        }
        Ipv4PreferResolver.resolve(netloc)
    }
}

/// 物理网卡直连 TLS 连接器：将底层 TCP 套接字显式绑定到物理网卡 IPv4，绕过异常的 TUN 默认路由。
struct DirectNicTlsConnector {
    inner: std::sync::Arc<ureq::rustls::ClientConfig>,
}

impl ureq::TlsConnector for DirectNicTlsConnector {
    fn connect(
        &self,
        dns_name: &str,
        io: Box<dyn ureq::ReadWrite>,
    ) -> Result<Box<dyn ureq::ReadWrite>, ureq::Error> {
        if let Some(old_sock) = io.socket() {
            if let Ok(remote_addr) = old_sock.peer_addr() {
                if remote_addr.is_ipv4() && !remote_addr.ip().is_loopback() {
                    let read_to = old_sock.read_timeout().ok().flatten();
                    let write_to = old_sock.write_timeout().ok().flatten();
                    let nic_opt = cached_nic_cell()
                        .lock()
                        .ok()
                        .and_then(|g| *g)
                        .or_else(|| candidate_physical_nics().into_iter().next());
                    if let Some(nic_ip) = nic_opt {
                        if let Ok(direct_sock) =
                            connect_tcp_via_nic(nic_ip, remote_addr, read_to, write_to)
                        {
                            drop(io);
                            return self.inner.connect(dns_name, Box::new(direct_sock));
                        }
                    }
                }
            }
        }
        self.inner.connect(dns_name, io)
    }
}

fn connect_tcp_via_nic(
    nic_ip: std::net::Ipv4Addr,
    remote_addr: std::net::SocketAddr,
    read_to: Option<std::time::Duration>,
    write_to: Option<std::time::Duration>,
) -> std::io::Result<std::net::TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{IpAddr, SocketAddr};

    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let bind_addr = SocketAddr::new(IpAddr::V4(nic_ip), 0);
    sock.bind(&bind_addr.into())?;
    sock.connect_timeout(&remote_addr.into(), std::time::Duration::from_secs(8))?;
    let stream: std::net::TcpStream = sock.into();
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(read_to);
    let _ = stream.set_write_timeout(write_to);
    Ok(stream)
}

fn build_rustls_config() -> std::sync::Arc<ureq::rustls::ClientConfig> {
    static CFG: std::sync::OnceLock<std::sync::Arc<ureq::rustls::ClientConfig>> =
        std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = ureq::rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        // 叠加加载 Windows 系统受信任根证书存储区（ROOT）：
        // 解决金融机构公司 VPN / 办公网安全网关（深信服、Zscaler、奇安信等）开启 SSL 流量审计（MITM）时，
        // 仅靠 Mozilla 静态 webpki_roots 不认企业自签名根证书导致 UnknownIssuer 报错的问题。
        unsafe {
            use windows::Win32::Security::Cryptography::{
                CertCloseStore, CertEnumCertificatesInStore, CertOpenSystemStoreW, CERT_CONTEXT,
                HCRYPTPROV_LEGACY,
            };
            let store_name: Vec<u16> = "ROOT\0".encode_utf16().collect();
            if let Ok(store) = CertOpenSystemStoreW(
                HCRYPTPROV_LEGACY::default(),
                windows::core::PCWSTR(store_name.as_ptr()),
            ) {
                let mut ctx: *const CERT_CONTEXT = std::ptr::null();
                let mut ders = Vec::new();
                loop {
                    let prev = if ctx.is_null() { None } else { Some(ctx) };
                    ctx = CertEnumCertificatesInStore(store, prev);
                    if ctx.is_null() {
                        break;
                    }
                    let encoded = (*ctx).pbCertEncoded;
                    let len = (*ctx).cbCertEncoded as usize;
                    if !encoded.is_null() && len > 0 && len < 65_536 {
                        let bytes = std::slice::from_raw_parts(encoded, len).to_vec();
                        ders.push(ureq::rustls::pki_types::CertificateDer::from(bytes));
                    }
                }
                let _ = roots.add_parsable_certificates(ders);
                let _ = CertCloseStore(store, 0);
            }
        }
        let cfg = ureq::rustls::ClientConfig::builder_with_provider(
            ureq::rustls::crypto::ring::default_provider().into(),
        )
        .with_protocol_versions(&[
            &ureq::rustls::version::TLS12,
            &ureq::rustls::version::TLS13,
        ])
        .expect("valid tls versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        std::sync::Arc::new(cfg)
    })
    .clone()
}

fn normal_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .resolver(Ipv4PreferResolver)
            .tls_config(build_rustls_config())
            .max_idle_connections(0)
            .timeout_connect(std::time::Duration::from_secs(8))
            .build()
    })
}

fn direct_nic_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        let tls = DirectNicTlsConnector {
            inner: build_rustls_config(),
        };
        ureq::AgentBuilder::new()
            .resolver(DirectNicResolver)
            .tls_connector(std::sync::Arc::new(tls))
            .try_proxy_from_env(false)
            .max_idle_connections(0)
            .timeout_connect(std::time::Duration::from_secs(8))
            .build()
    })
}

fn bypass_tun_hosts() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static HOSTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    HOSTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

fn host_of_url(url: &str) -> String {
    let after = url.find("://").map(|i| &url[i + 3..]).unwrap_or(url);
    let host_port = after.split('/').next().unwrap_or("");
    host_port.split(':').next().unwrap_or("").to_ascii_lowercase()
}

fn should_bypass_tun(url: &str) -> bool {
    let h = host_of_url(url);
    if h.is_empty() || h == "127.0.0.1" || h == "localhost" {
        return false;
    }
    bypass_tun_hosts()
        .lock()
        .map(|g| g.contains(&h))
        .unwrap_or(false)
}

fn mark_bypass_tun(url: &str) {
    let h = host_of_url(url);
    if !h.is_empty() && h != "127.0.0.1" && h != "localhost" {
        if let Ok(mut g) = bypass_tun_hosts().lock() {
            if g.insert(h.clone()) {
                crate::startup_log(&format!(
                    "llm: 已为域名 {h} 启用物理网卡直连模式（自动绕过异常 TUN / 代理劫持）"
                ));
            }
        }
    }
}

fn post_chat(f: &LlmFile, body: serde_json::Value, timeout_secs: u64) -> Result<serde_json::Value, String> {
    let key = api_key(f)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or("尚未配置 API Key")?;
    let urls = candidate_endpoints(f)?;
    let mut url_idx = 0usize;
    let mut cur_body = body;
    let mut downgraded_400 = false;
    let mut tried_direct_bypass = false;

    // 429 = 超出网关的 TPM/RPM 限额；404 = 根路径缺 /v1 自动切换备选端点；
    // 400 = 推理模型不接受 temperature / max_tokens 时自动降级参数重试；
    // Transport (TLS EOF / 连接重置) = 自动绕过 TUN 虚拟网卡劫持、绑定物理网卡直连重试。
    for attempt in 0..4u32 {
        let url = &urls[url_idx];
        let use_direct = tried_direct_bypass || should_bypass_tun(url);
        let agent = if use_direct {
            direct_nic_agent()
        } else {
            normal_agent()
        };
        let resp = agent
            .post(url)
            .set("Authorization", &format!("Bearer {key}"))
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .send_json(cur_body.clone());

        match resp {
            Ok(r) => {
                if tried_direct_bypass {
                    mark_bypass_tun(url);
                }
                let text = r.into_string().map_err(|e| e.to_string())?;
                let v: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|_| format!("响应不是 JSON：{}", truncate(&text)))?;
                // 有些网关 HTTP 状态码返回 200，但 JSON 顶层包了 {"error": {"message": "..."}}
                if v.get("choices").is_none() {
                    if let Some(err_msg) = v["error"]["message"]
                        .as_str()
                        .or_else(|| v["error"].as_str())
                        .or_else(|| v["msg"].as_str())
                    {
                        track_usage(0, 0, false);
                        return Err(format!("网关报错：{}", truncate(err_msg)));
                    }
                }
                let u = usage_of(&v);
                track_usage(u.prompt_tokens, u.completion_tokens, true);
                return Ok(v);
            }
            Err(ureq::Error::Status(404, r)) if url_idx + 1 < urls.len() => {
                let _ = r.into_string();
                url_idx += 1;
                crate::startup_log(&format!(
                    "llm: HTTP 404 on primary endpoint, fallback to {}",
                    urls[url_idx]
                ));
            }
            Err(ureq::Error::Status(400, r)) if !downgraded_400 => {
                let body_txt = r.into_string().unwrap_or_default();
                if let Some(next_body) = downgrade_incompatible_params(&cur_body, &body_txt) {
                    downgraded_400 = true;
                    cur_body = next_body;
                    crate::startup_log("llm: HTTP 400 参数不兼容，自动移除/替换受限参数后重试");
                    continue;
                }
                track_usage(0, 0, false);
                return Err(format!("HTTP 400：{}", truncate(&body_txt)));
            }
            Err(ureq::Error::Status(429, r)) if attempt == 0 => {
                let _ = r.into_string();
                // 失败也计一次请求：网关侧照样计费
                track_usage(0, 0, false);
                crate::startup_log("llm: HTTP 429 限流，3 秒后重试一次");
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            Err(ureq::Error::Status(code, r)) => {
                if tried_direct_bypass {
                    mark_bypass_tun(url);
                }
                let body_txt = r.into_string().unwrap_or_default();
                track_usage(0, 0, false);
                return Err(format!("HTTP {code}：{}", truncate(&body_txt)));
            }
            Err(ureq::Error::Transport(tp)) if !use_direct && host_of_url(url) != "127.0.0.1" => {
                tried_direct_bypass = true;
                crate::startup_log(&format!(
                    "llm: 默认路由传输/TLS 握手失败 ({tp})，自动切换物理网卡直连重试"
                ));
                continue;
            }
            Err(e) => {
                // 若物理网卡缓存失效（如切换了 Wi-Fi），清空缓存再试一次
                if use_direct && attempt < 2 {
                    if let Ok(mut g) = cached_nic_cell().lock() {
                        *g = None;
                    }
                    continue;
                }
                track_usage(0, 0, false);
                return Err(format!("请求失败：{e}"));
            }
        }
    }
    Err("请求失败：多次重试后仍未成功，请检查网络或代理设置".into())
}

/// AI 从一段文字里抽出的待办草稿。
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct TaskDraft {
    pub title: String,
    /// 距现在多少分钟；-1 表示原文没提时间
    pub in_minutes: i64,
    /// 预估用时（分钟），0 表示没估
    pub duration_min: i64,
    /// 1=高 2=中 3=低
    pub priority: i64,
    /// 涉及的对象（人 / 团队 / 系统）：「发给张总」里的「张总」。
    /// 规则解析器最常错的就是这个字段，所以单独抽出来。
    pub who: String,
    pub note: String,
}

/// 让模型把一段话抽成结构化待办。
///
/// 为什么用「从现在起多少分钟」而不是绝对时间：
///   绝对时间要求模型自己换算时区与日期，错一次就整条错；
///   相对分钟数由本地加回时间戳，时区问题在源头消失（本机时区还是美区，更容易错）。
///
/// 输出解析做了三层兜底：去 ```json 包裹 → 截取首个 { 到末个 } → 失败即报错（不猜）。
pub fn extract_task(f: &LlmFile, text: &str) -> Result<TaskDraft, String> {
    let src = text.trim();
    if src.is_empty() {
        return Err("没有可解析的内容".into());
    }
    if f.model.trim().is_empty() {
        return Err("尚未填写模型名".into());
    }

    let tz_ctx = crate::tz::prompt_time_context();
    let system = format!(
        "你是待办抽取器。用户会给一段话（聊天记录、邮件、会议纪要里的一句都有可能）。\
         从中抽出一个待办，**只输出 JSON**，不要代码块、不要解释：\
         {{\"title\":\"不超过 20 字的动作短语（要说清楚做什么，含对象和交付物）\",\
         \"in_minutes\":从现在起多少分钟后提醒(整数；原文没提时间就填 -1；\
         说\"明天早晨\"\"明天中午\"\"明天上午10点\"这类请严格按用户所在城市时区换算成相对现在的分钟数),\
         \"duration_min\":预估需要多少分钟(没提就 0),\
         \"priority\":1或2或3(1=高 2=中 3=低),\
         \"who\":\"涉及的人、团队或系统，例如 张总 / 产品组 / 风控部；没有就填空串\",\
         \"note\":\"原文里的补充信息；没有就填空串\"}}\
         时间换算基准：{tz_ctx}\
         口语时段缺省时刻（若只说时段未说具体几点）：清晨=07:30、早晨/早上=08:30、上午=10:00、中午=12:00、下午=15:00、傍晚/下班前=18:00、晚上=20:00、睡前=22:30、凌晨=01:00。\
         「in_minutes」必须基于上述基准时间精确换算，不要凭空给值。"
    );

    let raw = chat_with_budget(f, &system, src, 500, 0.1)?;
    parse_task_json(&raw)
}

/* -------------------------- 多条目抽取（面板待办用） -------------------------- */

/// 一条被拆出来的待办或日程。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskItem {
    /// "event"（有明确发生时刻：出差/开会/拜访）或 "task"（要动手做的事）
    pub kind: String,
    pub title: String,
    /// 绝对北京时间 "YYYY-MM-DD HH:MM"；空串表示原文没给时间
    pub when: String,
    pub duration_min: i64,
    pub priority: i64,
    pub who: String,
    pub location: String,
    pub note: String,
}

/// 把一句话拆成多条待办/日程。
///
/// 与 `extract_task`（剪贴板建单条）的区别——这里用**绝对时间**：
///   ① 「在此之前要订机票」是跨条目的相对关系，单一 in_minutes 表达不了。
///      实测：模型对这类句子直接返回 -1，兜底成「30 分钟后」，完全答非所问。
///   ② 已经给了当前北京时间，让它输出绝对时间反而更稳。
///   ③ Rust 侧只按固定格式解析，不再做时区换算。
pub fn extract_tasks(f: &LlmFile, text: &str) -> Result<Vec<TaskItem>, String> {
    let src = text.trim();
    if src.is_empty() {
        return Err("没有可解析的内容".into());
    }
    if f.model.trim().is_empty() {
        return Err("尚未填写模型名".into());
    }

    let tz_ctx = crate::tz::prompt_time_context();
    let system = format!(
        "你是待办与日程抽取器。用户会给一句话，里面可能有多件事\
         （例如一个行程，加上出行前必须完成的准备）。\
         请拆成 JSON，只输出 JSON，不要代码块、不要解释：\
         {{\"items\":[{{\
         \"type\":\"event 或 task\",\
         \"title\":\"不超过 20 字、能独立看懂的动作短语\",\
         \"when\":\"YYYY-MM-DD HH:MM（按用户当前所在城市当地时间；原文没提时间就填空串）\",\
         \"duration_min\":预估需要多少分钟(整数，0=不知道),\
         \"priority\":1或2或3(1=高 2=中 3=低),\
         \"who\":\"涉及的人或机构，没有就填空串\",\
         \"location\":\"城市名，没有就填空串\",\
         \"note\":\"补充信息，没有就填空串\"\
         }}]}}。\
         规则：\
         1) 有明确发生时刻的（出差、开会、拜访、聚餐）用 event；需要动手完成的用 task。\
         2) 「在此之前」「提前」「先」这类前置要求必须单独拆成 task，\
            且它的 when 要早于对应的 event（一般提前一天）。\
         3) 口语时段缺省时刻（若只说时段未说具体几点）：清晨=07:30、早晨/早上/明早=08:30、上午=10:00、中午=12:00、下午=15:00、傍晚/下班前=18:00、晚上/今晚/明晚=20:00、睡前=22:30、凌晨=01:00。\
         4) 只说日期没说任何时段或时刻：event 用当天 09:00，task 用当天 10:00。\
         5) 同一件事不要拆成重复条目；确实只有一件事就只输出一个元素。\
         时间换算基准：{tz_ctx}\
         when 必须基于上述用户所在城市的基准时间换算，且必须是完整日期（YYYY-MM-DD HH:MM），不能写「后天」「明天」这类相对词。"
    );

    let raw = chat_with_budget(f, &system, src, 900, 0.1)?;
    parse_tasks_json(&raw)
}

/// 剥离模型直接混在 `content` 里的 `<think>...</think>` 或 `<thinking>...</thinking>` 思考块。
///
/// 常见于本地 Ollama / vLLM / DeepSeek-R1 蒸馏版 / Qwen3 等模型：
///   1) 若不剥离，宠物气泡会直接把几百字的内心独白吐给用户；
///   2) 若 `<think>` 内部举例写了 `{` 或 `[`（例如思考 JSON 格式），`raw.find('{')`
///      会误命中思考过程里的花括号，导致 JSON 解析必败；
///   3) 若模型在 `<think>` 还没闭合时就被 `max_tokens` 截断，剩余正文应视为空，
///      触发预算扩容重试，而不是把半截 `<think>` 当成回答返回。
pub fn strip_inline_thinking(raw: &str) -> (String, String) {
    let mut text = raw.to_string();
    let mut thoughts: Vec<String> = Vec::new();

    for (open_tag, close_tag) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        loop {
            let lower = text.to_ascii_lowercase();
            let Some(start) = lower.find(open_tag) else {
                break;
            };
            let after_open = start + open_tag.len();
            if let Some(rel_end) = lower[after_open..].find(close_tag) {
                let end = after_open + rel_end;
                let inner = text[after_open..end].trim().to_string();
                if !inner.is_empty() {
                    thoughts.push(inner);
                }
                let close_end = end + close_tag.len();
                text = format!("{}{}", &text[..start], &text[close_end..]);
            } else {
                // 只有 <think> 没有 </think>：说明模型在思考中途被 length 截断
                let inner = text[after_open..].trim().to_string();
                if !inner.is_empty() {
                    thoughts.push(inner);
                }
                text = text[..start].to_string();
                break;
            }
        }
    }

    (text.trim().to_string(), thoughts.join("\n"))
}

/// 清理小模型在生成 JSON 时常见的尾随逗号（如 `{"a": 1, }` 或 `[1, 2, ]`）。
fn remove_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut in_str = false;
    let mut escaped = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 从模型文本中定位并解析 JSON：
///   1) 先剥离 `<think>...</think>` 思考过程；
///   2) 优先提取 ```json ... ``` 代码块内的文本（防止模型在代码块外寒暄里带了 `{}`）；
///   3) 标准解析失败时，自动去尾逗号再试一次。
fn extract_and_parse_json(raw: &str, allow_array: bool) -> Result<serde_json::Value, String> {
    let (cleaned, _) = strip_inline_thinking(raw);
    let source = if cleaned.is_empty() { raw } else { &cleaned };

    // 若包含 ``` 代码块，优先在代码块内找 JSON
    let candidate_region = if let Some(fence_start) = source.find("```") {
        let after_tick = &source[fence_start + 3..];
        let body_start = after_tick.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after_tick[body_start..];
        if let Some(fence_end) = body.find("```") {
            &body[..fence_end]
        } else {
            body
        }
    } else {
        source
    };

    let find_bounds = |s: &str| -> Option<(usize, usize)> {
        let start = if allow_array {
            s.find(|c| c == '{' || c == '[')
        } else {
            s.find('{')
        };
        let end = if allow_array {
            s.rfind(|c| c == '}' || c == ']')
        } else {
            s.rfind('}')
        };
        match (start, end) {
            (Some(a), Some(b)) if b > a => Some((a, b)),
            _ => None,
        }
    };

    let (target_str, (a, b)) = if let Some(bounds) = find_bounds(candidate_region) {
        (candidate_region, bounds)
    } else if let Some(bounds) = find_bounds(source) {
        (source, bounds)
    } else {
        return Err(format!("模型没返回 JSON：{}", truncate(source)));
    };

    let slice = &target_str[a..=b];
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
        return Ok(v);
    }
    let fixed = remove_trailing_commas(slice);
    serde_json::from_str::<serde_json::Value>(&fixed)
        .map_err(|e| format!("解析 JSON 失败（{e}）：{}", truncate(slice)))
}

/// 宽容读取整数字段：兼容 `30`、`30.0`、`"30"`、`"30分钟"`。
fn flex_i64(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return Some(f.round() as i64);
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        if let Ok(n) = s.parse::<i64>() {
            return Some(n);
        }
        // 提取前导带符号数字（如 "40分钟" -> 40）
        let num_part: String = s
            .chars()
            .skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        if let Ok(n) = num_part.parse::<i64>() {
            return Some(n);
        }
    }
    None
}

/// 宽容读取优先级（1=高 2=中 3=低）：兼容整数、数字字符串、以及中英文枚举词。
fn flex_priority(v: &serde_json::Value) -> i64 {
    if let Some(n) = flex_i64(v) {
        return n.clamp(1, 3);
    }
    if let Some(s) = v.as_str() {
        let lower = s.trim().to_ascii_lowercase();
        if lower.contains('高') || lower == "high" || lower == "urgent" || lower == "p0" || lower == "p1" {
            return 1;
        }
        if lower.contains('低') || lower == "low" || lower == "p3" {
            return 3;
        }
        if lower.contains('中') || lower == "medium" || lower == "normal" || lower == "p2" {
            return 2;
        }
    }
    2
}

/// 解析 items。兼容三种形状：
///   ① `{"items":[...]}`（当前提示词）② `[...]`（模型直接给数组）
///   ③ `{"title":...}`（旧版单任务格式，mock 与旧用例仍会产生）
fn parse_tasks_json(raw: &str) -> Result<Vec<TaskItem>, String> {
    let v = extract_and_parse_json(raw, true)?;

    let arr: Vec<serde_json::Value> = if v.is_array() {
        v.as_array().cloned().unwrap_or_default()
    } else if let Some(a) = v
        .get("items")
        .or_else(|| v.get("tasks"))
        .or_else(|| v.get("todos"))
        .and_then(|x| x.as_array())
    {
        a.clone()
    } else {
        // 旧格式：整个对象就是一条
        vec![v.clone()]
    };

    let mut out = Vec::new();
    for it in arr {
        let title = it["title"]
            .as_str()
            .or_else(|| it["task"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let raw_kind = it["type"]
            .as_str()
            .or_else(|| it["kind"].as_str())
            .unwrap_or("task")
            .trim()
            .to_ascii_lowercase();
        let kind = if raw_kind == "event" || raw_kind == "日程" || raw_kind == "schedule" {
            "event"
        } else {
            "task"
        };
        // 兼容旧格式：模型（或 mock）只给了「从现在起多少分钟」而没有绝对时间。
        // 直接换算成绝对时间，别让调用方再维护两套分支。
        let mut when = it["when"]
            .as_str()
            .or_else(|| it["time"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if when.is_empty() {
            if let Some(m) = flex_i64(&it["in_minutes"]) {
                if m >= 0 {
                    when = crate::notify::bj_text(now_sec() + m * 60);
                }
            }
        }
        out.push(TaskItem {
            kind: kind.to_string(),
            title,
            when,
            duration_min: flex_i64(&it["duration_min"]).unwrap_or(0).max(0),
            priority: flex_priority(&it["priority"]),
            who: it["who"].as_str().unwrap_or("").trim().to_string(),
            location: it["location"].as_str().unwrap_or("").trim().to_string(),
            note: it["note"].as_str().unwrap_or("").trim().to_string(),
        });
    }
    if out.is_empty() {
        return Err(format!("模型没给出任何条目：{}", truncate(raw)));
    }
    Ok(out)
}

/// 从模型输出里抠出 JSON（容忍 `<think>`、```json 包裹、前后废话、尾逗号、字符串数值）
fn parse_task_json(raw: &str) -> Result<TaskDraft, String> {
    let v = extract_and_parse_json(raw, false)?;

    let title = v["title"]
        .as_str()
        .or_else(|| v["task"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if title.is_empty() {
        return Err(format!("模型没给出标题：{}", truncate(raw)));
    }
    Ok(TaskDraft {
        title,
        in_minutes: flex_i64(&v["in_minutes"]).unwrap_or(-1),
        duration_min: flex_i64(&v["duration_min"]).unwrap_or(0).max(0),
        priority: flex_priority(&v["priority"]),
        // 对象缺字段是常态（模型经常省掉空字段），空串即可，不是错误
        who: v["who"].as_str().unwrap_or("").trim().to_string(),
        note: v["note"].as_str().unwrap_or("").trim().to_string(),
    })
}

/* --------------------- 推理模型（Reasoning）自动检测与自适应 --------------------- */

fn reasoning_models_cache() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// 记录某模型在运行时表现出了推理行为（返回了 `reasoning` / `reasoning_content` / `<think>` / `reasoning_tokens`）。
fn mark_reasoning_model(model: &str) {
    let key = model.trim().to_ascii_lowercase();
    if key.is_empty() {
        return;
    }
    if let Ok(mut set) = reasoning_models_cache().lock() {
        if set.insert(key.clone()) {
            crate::startup_log(&format!("llm: 自动识别并标记推理型模型 model={key}"));
        }
    }
}

/// 判断当前模型是否为推理型模型（结合「运行时自动学习缓存」与「常见推理模型命名特征」）。
pub fn is_reasoning_model(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    if let Ok(set) = reasoning_models_cache().lock() {
        if set.contains(&lower) {
            return true;
        }
    }
    let reasoning_patterns = [
        "reasoner",
        "deepseek-r1",
        "-r1",
        "/r1",
        "qwq",
        "thinking",
        "glm-z1",
        "sensenova-6.8",
        "qwen3",
        "o1-",
        "o3-",
        "o4-mini",
        "marco-o1",
        "skywork-o1",
    ];
    reasoning_patterns.iter().any(|p| lower.contains(p))
        || lower == "o1"
        || lower == "o3"
        || lower == "r1"
}

#[derive(Debug, Clone, Default)]
struct PickedMessage {
    content: String,
    reasoning: String,
    finish_reason: String,
    has_reasoning: bool,
}

/// 从 chat/completions 响应里提取正文、推理过程与结束原因，并**自动检测推理模型**。
///
/// 兼容不同厂商/网关的五种返回形态：
///   1) `message.reasoning`（商汤 SenseNova / OpenRouter 等）
///   2) `message.reasoning_content`（DeepSeek 官方 / 硅基流动 / 火山方舟 / 百炼 / vLLM）
///   3) `message.thought`（部分 Gemini 代理网关）
///   4) `message.content` 内嵌 `<think>...</think>` 或 `<thinking>...</thinking>`（Ollama / 本地模型）
///   5) `message.content` 为分段数组 `[{"type":"thinking","thinking":"..."},{"type":"text","text":"..."}]`
/// 同时检查 `usage.completion_tokens_details.reasoning_tokens`。一旦发现任何推理痕迹，
/// 立即调用 `mark_reasoning_model` 将该模型记入推理模型集合，后续请求自动升级 Token 预算。
fn pick_message_ex(v: &serde_json::Value, model_hint: &str) -> PickedMessage {
    let choice = &v["choices"][0];
    let msg = &choice["message"];

    let mut raw_content = String::new();
    let mut part_reasoning = String::new();

    if let Some(s) = msg["content"].as_str() {
        raw_content = s.to_string();
    } else if let Some(arr) = msg["content"].as_array() {
        let mut text_parts = Vec::new();
        let mut think_parts = Vec::new();
        for item in arr {
            let ty = item["type"].as_str().unwrap_or("text");
            if ty == "thinking" || ty == "reasoning" {
                if let Some(t) = item["thinking"].as_str().or_else(|| item["text"].as_str()) {
                    think_parts.push(t.trim().to_string());
                }
            } else if let Some(t) = item["text"].as_str().or_else(|| item["content"].as_str()) {
                text_parts.push(t.to_string());
            }
        }
        raw_content = text_parts.join("\n");
        part_reasoning = think_parts.join("\n");
    } else if let Some(s) = choice["text"].as_str() {
        raw_content = s.to_string();
    }

    let (clean_content, inline_thinking) = strip_inline_thinking(&raw_content);

    let field_reasoning = msg["reasoning_content"]
        .as_str()
        .or_else(|| msg["reasoning"].as_str())
        .or_else(|| msg["thought"].as_str())
        .unwrap_or("")
        .trim();

    let mut reasoning_pieces = Vec::new();
    if !field_reasoning.is_empty() {
        reasoning_pieces.push(field_reasoning.to_string());
    }
    if !part_reasoning.is_empty() {
        reasoning_pieces.push(part_reasoning);
    }
    if !inline_thinking.is_empty() {
        reasoning_pieces.push(inline_thinking);
    }
    let reasoning = reasoning_pieces.join("\n");

    let reasoning_tokens = v["usage"]["completion_tokens_details"]["reasoning_tokens"]
        .as_i64()
        .or_else(|| v["usage"]["reasoning_tokens"].as_i64())
        .unwrap_or(0);

    let has_reasoning = !reasoning.is_empty() || reasoning_tokens > 0;
    if has_reasoning && !model_hint.is_empty() {
        mark_reasoning_model(model_hint);
    }

    let finish_reason = choice["finish_reason"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    PickedMessage {
        content: clean_content,
        reasoning,
        finish_reason,
        has_reasoning,
    }
}

/// 带推理模型自动识别与预算自适应的一次对话。
///
/// 策略：
///   1) 若当前模型已被识别为推理型模型（通过命名特征或前序请求自动学习），
///      首次请求直接将预算放大到 `(base_tokens * 4).clamp(2048, 4096)`，省去必败的小预算试探；
///   2) 若首次请求按 `base_tokens` 发出后触发 `finish_reason == "length"`，或正文为空但包含推理过程，
///      自动用 4 倍预算（至少 2048 tokens）重试一次；
///   3) 若重试后 `content` 仍为空，但模型把最终 JSON 写在了 `reasoning` 末尾，自动尝试从 `reasoning` 尾部挽救 JSON。
fn chat_with_budget(
    f: &LlmFile,
    system: &str,
    user: &str,
    base_tokens: u64,
    temperature: f64,
) -> Result<String, String> {
    let model = f.model.trim();
    let known_reasoning = is_reasoning_model(model);
    let initial_tokens = if known_reasoning {
        (base_tokens.saturating_mul(4)).clamp(2048, 4096)
    } else {
        base_tokens
    };

    let p1 = chat_once(f, system, user, initial_tokens, temperature)?;

    let needs_retry = p1.finish_reason == "length"
        || (p1.content.is_empty() && p1.has_reasoning);

    if needs_retry {
        let bigger = (initial_tokens.saturating_mul(4)).clamp(2048, 4096);
        if bigger > initial_tokens {
            let p2 = chat_once(f, system, user, bigger, temperature)?;
            if !p2.content.is_empty() && p2.content.chars().count() >= p1.content.chars().count() {
                crate::startup_log(&format!(
                    "llm: reasoning/length retry ({initial_tokens} -> {bigger} tokens, {} -> {} chars)",
                    p1.content.chars().count(),
                    p2.content.chars().count()
                ));
                return Ok(p2.content);
            }
            if p1.content.is_empty() && !p2.reasoning.is_empty() {
                if let Some(rescued) = rescue_json_from_reasoning(&p2.reasoning, system) {
                    return Ok(rescued);
                }
            }
        }
    }

    if !p1.content.is_empty() {
        return Ok(p1.content);
    }

    // 兜底：部分推理模型在完成思考时直接把最终 JSON 块放在了 reasoning 末尾而未填 content
    if !p1.reasoning.is_empty() {
        if let Some(rescued) = rescue_json_from_reasoning(&p1.reasoning, system) {
            return Ok(rescued);
        }
        return Err("模型只输出了推理过程就被截断：已自动标记为推理模型，请重试或换用非推理模型".into());
    }

    Err("模型返回了空内容".into())
}

/// 当提示词明确要求输出 JSON，而模型把 JSON 写在了 `reasoning` 末尾、`content` 为空时，
/// 尝试从 `reasoning` 的最后一个 `{...}` 提取合法 JSON。
fn rescue_json_from_reasoning(reasoning: &str, system_prompt: &str) -> Option<String> {
    if !system_prompt.contains("JSON") {
        return None;
    }
    let end = reasoning.rfind('}')?;
    let start = reasoning[..=end].rfind('{')?;
    let candidate = &reasoning[start..=end];
    if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
        crate::startup_log("llm: 从 reasoning 尾部成功挽救出结构化 JSON");
        Some(candidate.to_string())
    } else {
        None
    }
}

fn chat_once(
    f: &LlmFile,
    system: &str,
    user: &str,
    max_tokens: u64,
    temperature: f64,
) -> Result<PickedMessage, String> {
    let model = f.model.trim();
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "max_tokens": max_tokens,
        "temperature": temperature
    });
    let v = post_chat(f, body, 90)?;
    Ok(pick_message_ex(&v, model))
}

/// 发一条最小请求验证配置。成功返回模型回复片段，失败返回可读的错误。
///
/// 预算 200，**并且自动检测推理型模型**：
/// 推理型模型光思考就要一两百 token，若返回了 `reasoning` / `reasoning_content` / `<think>`，
/// 自动将其登记到推理模型集合中（后续正式请求直接启用大预算），并向用户明确反馈已识别为推理模型。
pub fn test_connection(f: &LlmFile) -> Result<String, String> {
    let model = f.model.trim();
    if model.is_empty() {
        return Err("尚未填写模型名".into());
    }

    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "回复一个词：ok" }],
        "max_tokens": 200,
        "temperature": 0
    });

    let v = post_chat(f, body, 30)?;
    let picked = pick_message_ex(&v, model);
    if !picked.content.is_empty() {
        if picked.has_reasoning {
            return Ok(format!("{}（已自动识别为推理模型）", picked.content));
        }
        return Ok(picked.content);
    }
    if picked.has_reasoning || picked.finish_reason == "length" {
        mark_reasoning_model(model);
        return Ok("已连通（已自动识别为推理型模型，后续调用将自动扩容 Token 预算）".into());
    }
    Err("模型返回了空内容".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_json() {
        let d = parse_task_json(r#"{"title":"发周报","in_minutes":90,"duration_min":20,"priority":1,"note":"给张总"}"#).unwrap();
        assert_eq!(d.title, "发周报");
        assert_eq!(d.in_minutes, 90);
        assert_eq!(d.priority, 1);
        assert_eq!(d.note, "给张总");
    }

    #[test]
    fn parse_fenced_json() {
        let raw = "```json\n{\"title\":\"评审设计\",\"in_minutes\":-1,\"priority\":9}\n```";
        let d = parse_task_json(raw).unwrap();
        assert_eq!(d.title, "评审设计");
        assert_eq!(d.in_minutes, -1);
        assert_eq!(d.priority, 3, "越界优先级要夹到 3");
    }

    #[test]
    fn parse_with_chatter() {
        let raw = "好的，这是结果：{\"title\":\"回电话\",\"in_minutes\":30} 需要我调整吗？";
        assert_eq!(parse_task_json(raw).unwrap().title, "回电话");
    }

    #[test]
    fn parse_with_think_tags_and_trailing_commas() {
        // 思考过程里含有干扰性的 { "title": "错的" }，且最终 JSON 含尾逗号与中文优先级、字符串数字
        let raw = "<think>用户说要发周报，是不是 {\"title\":\"错的\"}？不对，重来。</think>\n```json\n{\"title\":\"发最终周报\",\"in_minutes\":\"45分钟\",\"duration_min\":\"25\",\"priority\":\"高\",}\n```";
        let d = parse_task_json(raw).unwrap();
        assert_eq!(d.title, "发最终周报");
        assert_eq!(d.in_minutes, 45);
        assert_eq!(d.duration_min, 25);
        assert_eq!(d.priority, 1);
    }

    #[test]
    fn strip_unclosed_think_tag_returns_empty_content() {
        let (clean, thought) = strip_inline_thinking("<think>模型还在思考中就被 max_tokens 截断了...");
        assert!(clean.is_empty(), "未闭合的 <think> 不应把半截思考当正文返回");
        assert!(thought.contains("模型还在思考中"));
    }

    #[test]
    fn pick_message_auto_detects_reasoning_variants() {
        // 1) DeepSeek / SiliconFlow 风格的 reasoning_content
        let v1 = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "reasoning_content": "先分析一下时间...",
                    "content": "OK"
                },
                "finish_reason": "stop"
            }]
        });
        let p1 = pick_message_ex(&v1, "custom-model-alpha");
        assert!(p1.has_reasoning);
        assert_eq!(p1.content, "OK");
        assert!(is_reasoning_model("custom-model-alpha"));

        // 2) Ollama / Qwen3 风格的内嵌 <think>
        let v2 = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "<think>一步步思考</think>最终结论"
                },
                "finish_reason": "stop"
            }]
        });
        let p2 = pick_message_ex(&v2, "local-custom-beta");
        assert!(p2.has_reasoning);
        assert_eq!(p2.content, "最终结论");
        assert!(is_reasoning_model("local-custom-beta"));
    }

    #[test]
    fn candidate_endpoints_handles_common_user_url_mistakes() {
        let f_root = LlmFile {
            base_url: " \"https://api.deepseek.com/\" ".into(),
            model: "deepseek-chat".into(),
            key_cipher: String::new(),
            stream: true,
        };
        let urls = candidate_endpoints(&f_root).unwrap();
        assert_eq!(urls[0], "https://api.deepseek.com/v1/chat/completions");
        assert_eq!(urls[1], "https://api.deepseek.com/chat/completions");

        let f_v1 = LlmFile {
            base_url: "https://token.sensenova.cn/v1".into(),
            model: "sensenova-6.8-flash-lite".into(),
            key_cipher: String::new(),
            stream: true,
        };
        let urls_v1 = candidate_endpoints(&f_v1).unwrap();
        assert_eq!(urls_v1, vec!["https://token.sensenova.cn/v1/chat/completions"]);

        let f_models = LlmFile {
            base_url: "http://127.0.0.1:11434/v1/models".into(),
            model: "qwen3".into(),
            key_cipher: String::new(),
            stream: true,
        };
        assert_eq!(
            endpoint(&f_models).unwrap(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn reject_non_json() {
        assert!(parse_task_json("抱歉，我无法理解").is_err());
        assert!(parse_task_json("").is_err());
    }

    #[test]
    fn parse_multi_items() {
        let raw = r#"{"items":[
            {"type":"event","title":"深圳出差","when":"2026-10-27 09:00","who":"一创投行","location":"深圳"},
            {"type":"task","title":"订机票酒店","when":"2026-10-26 18:00","duration_min":40,"priority":1}
        ]}"#;
        let v = parse_tasks_json(raw).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].kind, "event");
        assert_eq!(v[0].location, "深圳");
        assert_eq!(v[1].kind, "task");
        assert_eq!(v[1].priority, 1);
        assert_eq!(v[1].when, "2026-10-26 18:00");
    }

    #[test]
    fn parse_bare_array() {
        let raw = r#"[{"title":"A","when":"2026-10-26 10:00"}]"#;
        assert_eq!(parse_tasks_json(raw).unwrap().len(), 1);
    }

    #[test]
    fn parse_legacy_single_object() {
        // 旧格式（无 type / when，只有 in_minutes）仍要能吃下：
        // mock 与老用例还在产出这种，且要换算成绝对时间而不是丢掉。
        let raw = r#"{"title":"回电话","in_minutes":30}"#;
        let v = parse_tasks_json(raw).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, "task");
        assert!(!v[0].when.is_empty(), "in_minutes 应被换算成绝对时间");
        assert!(crate::notify::parse_bj_datetime(&v[0].when).is_some());
    }

    #[test]
    fn parse_empty_items_is_error() {
        assert!(parse_tasks_json(r#"{"items":[]}"#).is_err());
    }

    #[test]
    fn detect_scene_identifies_trip_task_question() {
        assert_eq!(
            detect_scene("后天可能需要出差去深圳，见一创投行客户，提前订机票和酒店").0,
            "trip"
        );
        assert_eq!(
            detect_scene("明天下午三点提醒我把周报发给张总").0,
            "task"
        );
        assert_eq!(
            detect_scene("什么是债券久期与凸性？").0,
            "question"
        );
        assert_eq!(detect_scene("   ").0, "empty");
    }

    #[test]
    fn stream_think_filter_strips_split_think_tags_and_reasoning() {
        let mut f = StreamThinkFilter::new();

        // 1) 首包只收到半截 "<thi"
        let (p1, v1) = f.push_delta("", "<thi");
        assert_eq!(p1, "thinking");
        assert!(v1.is_empty());

        // 2) 后续包进入思考内部
        let (p2, v2) = f.push_delta("", "nk>先一步步推导久期公式...");
        assert_eq!(p2, "thinking");
        assert!(v2.is_empty());

        // 3) 闭合标签 "</think>" 也跨了两个包
        let (p3, v3) = f.push_delta("", "继续推导</th");
        assert_eq!(p3, "thinking");
        assert!(v3.is_empty());

        let (p4, v4) = f.push_delta("", "ink>\n\n久期衡量债券价格对利率变动的敏感度。");
        assert_eq!(p4, "streaming");
        assert_eq!(v4, "久期衡量债券价格对利率变动的敏感度。");
        assert_eq!(f.finish(), "久期衡量债券价格对利率变动的敏感度。");

        // 4) DeepSeek 风格独立 reasoning_content 字段
        let mut f2 = StreamThinkFilter::new();
        let (rp1, rv1) = f2.push_delta("正在思考中...", "");
        assert_eq!(rp1, "thinking");
        assert!(rv1.is_empty());
        let (rp2, rv2) = f2.push_delta("", "直接给出答案");
        assert_eq!(rp2, "streaming");
        assert_eq!(rv2, "直接给出答案");
    }
}

/// 每日日记：把当天的事实口袋写成一段自然语言，并给出「明天的关照」。
///
/// 这是**后端每天唯一一次**主动调用模型的地方（提醒、问答都是被动触发）。
/// 要求模型同时输出 care：一句对明天的贴心提示，存进当天记忆，
/// 后续提醒直接复用——避免每次提醒都调一次模型。
///
/// 返回 (日记正文, care)。
pub fn daily_note(f: &LlmFile, facts: &str) -> Result<(String, String), String> {
    let started = std::time::Instant::now();
    let model = f.model.trim().to_string();
    let res = daily_inner(f, facts);
    record(Mode::Summarize, &model, started, &res.as_ref().map(|(s, _)| s.clone()).map_err(|e| e.clone()));
    res
}

const DAILY_SYSTEM: &str = "你是 WorkMate 的日记助手。用户是一线金融 IT 从业者，工作强度大、\
经常出差。下面是他某一天的**客观数据**。请写成一段 60–120 字的日记：\
用第二人称「你」，语气平实不煽情；说清做了多少事、专注得怎么样、去过哪里、大概在忙什么；\
不要写流水账，也不要臆测数据里没有的事。\n\
【语言规范】用平实规范的书面中文。不要生造词、不要用网络口语或俚语\
（例如「很松」「很顶」「绷不住」「拿捏」「绝了」都不允许）；\
不要用形容词堆情绪，有话直说。宁可不写，也不要编一个听起来像行话但实际讲不通的说法。\n\
【数据缺失】如果事实里出现「没有采集到任何采样」或「样本不足」，\
就**不要评价忙闲、专注、作息**，只写任务相关的事实，并明确说明当天没有活动数据。\
把「没有数据」说成「你没干活」是严重错误。\n\
同时输出一句**明天的关照**（字段 care）：基于今天的状态（是否熬夜、是否连轴转、\
任务是否堆积、是否在出差）给一句具体、可执行的提醒，不超过 25 字，同样必须是规范中文。\n\
只输出 JSON：{\"summary\":\"...\",\"care\":\"...\"}";

fn daily_inner(f: &LlmFile, facts: &str) -> Result<(String, String), String> {
    if f.model.trim().is_empty() {
        return Err("尚未填写模型名".into());
    }
    let raw = chat_with_budget(f, DAILY_SYSTEM, facts, 800, 0.4)?;
    let (clean_raw, _) = strip_inline_thinking(&raw);
    let fallback_text = if clean_raw.is_empty() { &raw } else { &clean_raw };
    if fallback_text.trim().is_empty() {
        return Err("模型返回了空内容".into());
    }
    match extract_and_parse_json(fallback_text, false) {
        Ok(obj) => {
            let summary = obj["summary"].as_str().unwrap_or("").trim().to_string();
            if summary.is_empty() {
                return Ok((truncate(fallback_text), String::new()));
            }
            let care = obj["care"].as_str().unwrap_or("").trim().to_string();
            Ok((summary, care))
        }
        Err(_) => {
            // 兜底：不少小模型对「只输出 JSON」这条指令遵循得不好，
            // 直接吐一段纯文本。这时整段当正文用——日记写得不规范，
            // 也远好过当天完全没有记录（断档后补不回来）。
            Ok((truncate(fallback_text), String::new()))
        }
    }
}

/// 分层抽象：把一批短期记录压成中期/长期印象。
///
/// `hint` 决定压缩到什么粒度——mid 丢掉时间戳、long 压到两句话。
/// 同一套调用服务两个层级，避免为每种窗口各写一个函数。
pub fn abstract_chat(f: &LlmFile, hint: &str, input: &str) -> Result<String, String> {
    let started = std::time::Instant::now();
    let model = f.model.trim().to_string();
    let res = abstract_inner(f, hint, input);
    record(Mode::Summarize, &model, started, &res);
    res
}

fn abstract_inner(f: &LlmFile, hint: &str, input: &str) -> Result<String, String> {
    if f.model.trim().is_empty() {
        return Err("尚未填写模型名".into());
    }
    if input.trim().is_empty() {
        return Err("没有可压缩的内容".into());
    }
    let s = chat_with_budget(f, hint, input, 600, 0.3)?;
    if s.is_empty() {
        Err("模型返回了空内容".into())
    } else {
        Ok(s)
    }
}

/// 记忆问答：把「事实清单」作为唯一依据交给模型，让它翻译成自然语言。
///
/// 为什么不让模型直接查库（function calling / 自己编 SQL）：
///   ① 它会编造没去过的城市、没发生的逾期，而用户无从分辨；
///   ② 事实由 SQL 产出是可复现、可测试的；模型的措辞错了不影响结论。
/// 因此提示词里写死三条纪律：只用清单、没提就说不知道、不臆测未来。
pub fn grounded_chat(f: &LlmFile, question: &str, facts: &str) -> Result<String, String> {
    let started = std::time::Instant::now();
    let model = f.model.trim().to_string();
    let res = grounded_inner(f, question, facts);
    record(Mode::Explain, &model, started, &res);
    res
}

const GROUNDED_SYSTEM: &str = "你是 WorkMate 的记忆助手。用户问的是关于他自己的问题（去过哪里、\
有什么待办、工作习惯、当前风险）。下面【事实】是系统从本地数据库里查出来的**唯一依据**。\
纪律：1) 只能使用事实里的信息，禁止补充、推测、编造任何事实里没有的内容；\
2) 事实里没有的就明确回答「没有记录」，不要猜；3) 不要复述问题，不要客套；\
4) 用简体中文，控制在三到五句话；5) 涉及日期要说清是哪一天。";

fn grounded_inner(f: &LlmFile, question: &str, facts: &str) -> Result<String, String> {
    if question.trim().is_empty() {
        return Err("问题是空的".into());
    }
    if f.model.trim().is_empty() {
        return Err("尚未填写模型名".into());
    }
    chat_with_budget(
        f,
        GROUNDED_SYSTEM,
        &format!("【事实】\n{facts}\n\n【问题】{question}"),
        700,
        0.2,
    )
}

/// 最近一次调用结果（面板「助手」页展示接入状态、排查失败原因用）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CallStatus {
    pub ts: i64,
    pub ok: bool,
    pub ms: u64,
    pub mode: String,
    pub model: String,
    /// 成功：回答字符数；失败：错误摘要
    pub detail: String,
}

fn last_call_cell() -> &'static std::sync::Mutex<CallStatus> {
    static M: std::sync::OnceLock<std::sync::Mutex<CallStatus>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(CallStatus::default()))
}

pub fn last_call() -> CallStatus {
    last_call_cell().lock().map(|g| g.clone()).unwrap_or_default()
}

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 记一次调用（含失败），并写一行日志——排查 AI 接口问题时日志里必须有痕迹。
fn record(mode: Mode, model: &str, started: std::time::Instant, res: &Result<String, String>) {
    let ms = started.elapsed().as_millis() as u64;
    let st = CallStatus {
        ts: now_sec(),
        ok: res.is_ok(),
        ms,
        mode: format!("{mode:?}").to_lowercase(),
        model: model.to_string(),
        detail: match res {
            Ok(s) => format!("{} 字", s.chars().count()),
            Err(e) => crate::clipboard::truncate_chars(e, 160),
        },
    };
    crate::startup_log(&format!(
        "llm: model={} mode={} {}ms ok={} {}",
        st.model, st.mode, st.ms, st.ok, st.detail
    ));
    if let Ok(mut g) = last_call_cell().lock() {
        *g = st;
    }
}

/// 流式 `<think>` / `reasoning_content` 实时净滤状态机。
///
/// 解决问题：
///   1) 推理模型在流式输出时可能先通过 `delta.reasoning_content` 或 `<think>...</think>` 吐出几百字思考链；
///   2) `<think>` 或 `</think>` 标签经常被切碎在两个相邻的 SSE chunk 中（如 chunk1=`"<thi"`, chunk2=`"nk>"`）；
///   3) 若直接把原始流推给前端，桌宠气泡会被思考过程撑大并暴露冗余推理独白。
///
/// 本状态机累积原始流并实时剥离思考块：
///   - 思考阶段返回 `phase = "thinking"`, `visible = ""`（前端仅展示单行 `💭 正在思考…`）；
///   - 正文阶段返回 `phase = "streaming"`, `visible = "<干净正文>"`。
#[derive(Debug, Default, Clone)]
pub struct StreamThinkFilter {
    raw_content: String,
    pub saw_reasoning: bool,
}

impl StreamThinkFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个 SSE delta 的 `reasoning_content`（如果有）与 `content`（如果有），
    /// 返回 `(phase, visible_text)`，其中 `phase` 为 `"thinking"` 或 `"streaming"`。
    pub fn push_delta(&mut self, reasoning_delta: &str, content_delta: &str) -> (&'static str, String) {
        if !reasoning_delta.is_empty() {
            self.saw_reasoning = true;
        }
        if !content_delta.is_empty() {
            self.raw_content.push_str(content_delta);
        }

        let (visible, in_think, had_inline_think) = Self::extract_visible(&self.raw_content);
        if had_inline_think {
            self.saw_reasoning = true;
        }
        if in_think || (visible.is_empty() && self.saw_reasoning) {
            ("thinking", visible)
        } else {
            ("streaming", visible)
        }
    }

    /// 流结束时获取最终干净正文
    pub fn finish(&self) -> String {
        let (visible, _, _) = Self::extract_visible(&self.raw_content);
        visible.trim().to_string()
    }

    /// 从累积的 `raw_content` 中剥离已闭合的 `<think>...</think>`、遮蔽未闭合的 `<think>...` 以及末尾半截标签前缀。
    /// 返回 `(visible_text, is_currently_inside_think, saw_any_think_tag)`。
    fn extract_visible(raw: &str) -> (String, bool, bool) {
        let mut text = raw.to_string();
        let mut saw_think = false;
        let mut in_unclosed_think = false;

        for (open_tag, close_tag) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
            loop {
                let lower = text.to_ascii_lowercase();
                let Some(start) = lower.find(open_tag) else {
                    break;
                };
                saw_think = true;
                let after_open = start + open_tag.len();
                if let Some(rel_end) = lower[after_open..].find(close_tag) {
                    let close_end = after_open + rel_end + close_tag.len();
                    text = format!("{}{}", &text[..start], &text[close_end..]);
                } else {
                    // 处于尚未闭合的 <think> 块内部：截掉 <think> 及其后面的思考内容
                    text = text[..start].to_string();
                    in_unclosed_think = true;
                    break;
                }
            }
        }

        // 检查开头或末尾是否正好是半截 "<think>" / "<thinking>" 的前缀（例如首包只到了 "<thi"）
        let trimmed_start = text.trim_start();
        let lower_trim = trimmed_start.to_ascii_lowercase();
        for tag in ["<thinking>", "<think>"] {
            if !lower_trim.is_empty()
                && lower_trim.len() < tag.len()
                && tag.starts_with(&lower_trim)
            {
                return (String::new(), true, true);
            }
        }

        let visible = trimmed_start.to_string();
        (visible, in_unclosed_think, saw_think)
    }
}

/// 发送流式 `/chat/completions` 请求（`stream: true`），实时通过 `StreamThinkFilter` 净滤思考过程并回调 `on_progress`。
/// 若网关不支持 SSE 而直接返回了普通 JSON，也能透明兼容解析。
pub fn post_chat_stream<F>(
    f: &LlmFile,
    mut body: serde_json::Value,
    timeout_secs: u64,
    model_hint: &str,
    mut on_progress: F,
) -> Result<String, String>
where
    F: FnMut(&str, &str),
{
    use std::io::{BufRead, BufReader};

    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    let key = api_key(f)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or("尚未配置 API Key")?;
    let urls = candidate_endpoints(f)?;
    let mut url_idx = 0usize;
    let mut cur_body = body;
    let mut downgraded_400 = false;
    let mut tried_direct_bypass = false;

    for attempt in 0..4u32 {
        let url = &urls[url_idx];
        let use_direct = tried_direct_bypass || should_bypass_tun(url);
        let agent = if use_direct {
            direct_nic_agent()
        } else {
            normal_agent()
        };
        let resp = agent
            .post(url)
            .set("Authorization", &format!("Bearer {key}"))
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream, application/json")
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .send_json(cur_body.clone());

        match resp {
            Ok(r) => {
                if tried_direct_bypass {
                    mark_bypass_tun(url);
                }
                let content_type = r.header("content-type").unwrap_or("").to_ascii_lowercase();
                // 兼容部分网关忽略 stream:true 直接返回 application/json 的情况
                if content_type.contains("application/json") && !content_type.contains("event-stream") {
                    let text = r.into_string().map_err(|e| e.to_string())?;
                    let v: serde_json::Value = serde_json::from_str(&text)
                        .map_err(|_| format!("响应不是 JSON：{}", truncate(&text)))?;
                    let u = usage_of(&v);
                    track_usage(u.prompt_tokens, u.completion_tokens, true);
                    let picked = pick_message_ex(&v, model_hint);
                    if !picked.content.is_empty() {
                        on_progress("done", &picked.content);
                        return Ok(picked.content);
                    }
                    return Err("模型返回了空内容".into());
                }

                let reader = BufReader::new(r.into_reader());
                let mut filter = StreamThinkFilter::new();
                let mut usage = Usage::default();
                let mut last_emit = std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_secs(1))
                    .unwrap_or_else(std::time::Instant::now);
                let mut notified_thinking = false;
                let mut raw_non_sse = String::new();

                for line_res in reader.lines() {
                    let line = match line_res {
                        Ok(l) => l,
                        Err(e) => {
                            // 若已经收到了有效正文，网络尾包断开时直接返回已有正文
                            let cur = filter.finish();
                            if !cur.is_empty() {
                                break;
                            }
                            track_usage(0, 0, false);
                            return Err(format!("读取流式响应失败：{e}"));
                        }
                    };
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with(':') {
                        continue;
                    }
                    let Some(data_str) = trimmed.strip_prefix("data:") else {
                        if raw_non_sse.len() < 8192 {
                            raw_non_sse.push_str(trimmed);
                        }
                        continue;
                    };
                    let payload = data_str.trim();
                    if payload == "[DONE]" {
                        break;
                    }
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
                        continue;
                    };
                    if v.get("usage").is_some() {
                        let u = usage_of(&v);
                        if u.total_tokens > 0 || u.prompt_tokens > 0 || u.completion_tokens > 0 {
                            usage = u;
                        }
                    }
                    if let Some(err_msg) = v["error"]["message"]
                        .as_str()
                        .or_else(|| v["error"].as_str())
                    {
                        track_usage(0, 0, false);
                        return Err(format!("网关报错：{}", truncate(err_msg)));
                    }

                    let choice = &v["choices"][0];
                    let delta = if choice.get("delta").is_some() {
                        &choice["delta"]
                    } else {
                        &choice["message"]
                    };
                    let r_delta = delta["reasoning_content"]
                        .as_str()
                        .or_else(|| delta["reasoning"].as_str())
                        .or_else(|| delta["thought"].as_str())
                        .unwrap_or("");
                    let c_delta = delta["content"].as_str().unwrap_or("");

                    let (phase, visible) = filter.push_delta(r_delta, c_delta);
                    if phase == "thinking" {
                        if !notified_thinking {
                            notified_thinking = true;
                            on_progress("thinking", "💭 正在思考…");
                        }
                    } else if !visible.is_empty() && last_emit.elapsed().as_millis() >= 55 {
                        last_emit = std::time::Instant::now();
                        on_progress("streaming", &visible);
                    }
                }

                // 若网关未设置 Content-Type 但实际返回了整包 JSON
                if filter.finish().is_empty() && !raw_non_sse.is_empty() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw_non_sse) {
                        let u = usage_of(&v);
                        track_usage(u.prompt_tokens, u.completion_tokens, true);
                        let picked = pick_message_ex(&v, model_hint);
                        if !picked.content.is_empty() {
                            on_progress("done", &picked.content);
                            return Ok(picked.content);
                        }
                    }
                }

                if filter.saw_reasoning && !model_hint.is_empty() {
                    mark_reasoning_model(model_hint);
                }

                let final_text = filter.finish();
                let est_completion = if usage.completion_tokens > 0 {
                    usage.completion_tokens
                } else {
                    (final_text.chars().count() as i64 / 2).max(1)
                };
                track_usage(usage.prompt_tokens, est_completion, !final_text.is_empty());

                if !final_text.is_empty() {
                    on_progress("done", &final_text);
                    return Ok(final_text);
                }

                // 如果流式期间模型只输出了 <think> 就被截断，回退走带预算扩容的 chat_with_budget
                return Err("STREAM_EMPTY_AFTER_THINK".into());
            }
            Err(ureq::Error::Status(404, r)) if url_idx + 1 < urls.len() => {
                let _ = r.into_string();
                url_idx += 1;
            }
            Err(ureq::Error::Status(400, r)) if !downgraded_400 => {
                let body_txt = r.into_string().unwrap_or_default();
                if let Some(next_body) = downgrade_incompatible_params(&cur_body, &body_txt) {
                    downgraded_400 = true;
                    cur_body = next_body;
                    continue;
                }
                track_usage(0, 0, false);
                return Err(format!("HTTP 400：{}", truncate(&body_txt)));
            }
            Err(ureq::Error::Status(429, r)) if attempt == 0 => {
                let _ = r.into_string();
                track_usage(0, 0, false);
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            Err(ureq::Error::Status(code, r)) => {
                if tried_direct_bypass {
                    mark_bypass_tun(url);
                }
                let body_txt = r.into_string().unwrap_or_default();
                track_usage(0, 0, false);
                return Err(format!("HTTP {code}：{}", truncate(&body_txt)));
            }
            Err(ureq::Error::Transport(_)) if !use_direct && host_of_url(url) != "127.0.0.1" => {
                tried_direct_bypass = true;
                continue;
            }
            Err(e) => {
                if use_direct && attempt < 2 {
                    if let Ok(mut g) = cached_nic_cell().lock() {
                        *g = None;
                    }
                    continue;
                }
                track_usage(0, 0, false);
                return Err(format!("请求失败：{e}"));
            }
        }
    }
    Err("请求失败：多次重试后仍未成功".into())
}

fn chat_with_optional_stream<F>(
    f: &LlmFile,
    system: &str,
    user: &str,
    base_tokens: u64,
    temperature: f64,
    mut on_progress: F,
) -> Result<String, String>
where
    F: FnMut(&str, &str),
{
    if !f.stream {
        let res = chat_with_budget(f, system, user, base_tokens, temperature)?;
        on_progress("done", &res);
        return Ok(res);
    }

    let model = f.model.trim();
    let max_tokens = if is_reasoning_model(model) {
        (base_tokens.saturating_mul(4)).clamp(2048, 4096)
    } else {
        base_tokens
    };
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "max_tokens": max_tokens,
        "temperature": temperature,
        "stream": true
    });

    match post_chat_stream(f, body, 90, model, &mut on_progress) {
        Ok(text) => Ok(text),
        Err(e) if e == "STREAM_EMPTY_AFTER_THINK" => {
            // 流式在思考阶段耗尽预算，自动用非流式大预算重试兜底
            let res = chat_with_budget(f, system, user, base_tokens.saturating_mul(4).max(2048), temperature)?;
            on_progress("done", &res);
            Ok(res)
        }
        Err(e) => Err(e),
    }
}

/// 支持流式回调的剪贴板场景化问答（当 `f.stream == true` 时流式回调 `on_progress`）。
pub fn chat_clipboard_scene_with_stream<F>(
    f: &LlmFile,
    user_text: &str,
    scene: &str,
    workload_ctx: &str,
    on_progress: F,
) -> Result<String, String>
where
    F: FnMut(&str, &str),
{
    let started = std::time::Instant::now();
    let model = f.model.trim().to_string();
    let text = user_text.trim();
    if text.is_empty() {
        return Err("问题是空的".into());
    }
    if model.is_empty() {
        return Err("尚未填写模型名".into());
    }

    let scene_guide = match scene {
        "trip" => "当前内容涉及【出差/异地行程】。请帮用户梳理：1) 核心行程时间与目的地/拜访对象；2) 行前必须完成的准备事项（如订票、材料）；3) 结合下方【当前工作负载】指出是否有日程冲突或行前需清理的待办，并提示用户可按 Ctrl+Shift+T（或右键「提取待办/出差行程」）一键入库。",
        "task" => "当前内容涉及【工作交办/待处理事项/会议】。请帮用户梳理：1) 要交付的核心动作、截止时间与对接人；2) 结合下方【当前工作负载】评估优先级与时间安排建议，并提示可按 Ctrl+Shift+T（或右键「提取待办/出差行程」）直接记入待办。",
        _ => "当前内容是用户选中的【问题/概念/代码或文档片段】。请用简体中文直接解答或给出可执行建议；不要复述问题，不要客套开场白，控制在三五句话内说清重点。",
    };

    let sys = format!(
        "{SYSTEM_PROMPT}\n\n【场景理解】{scene_guide}\n【用户当前工作负载】{workload_ctx}"
    );
    let res = chat_with_optional_stream(f, &sys, text, 1400, 0.3, on_progress).and_then(|c| {
        if c.is_empty() {
            Err("模型返回了空内容".into())
        } else {
            Ok(c)
        }
    });
    record(Mode::Explain, &model, started, &res);
    res
}

/// 支持流式回调的对话模式（供 `chat_text` 等调用）。
pub fn chat_mode_with_stream<F>(
    f: &LlmFile,
    user_text: &str,
    mode: Mode,
    on_progress: F,
) -> Result<String, String>
where
    F: FnMut(&str, &str),
{
    let started = std::time::Instant::now();
    let model = f.model.trim().to_string();
    let text = user_text.trim();
    if text.is_empty() {
        return Err("问题是空的".into());
    }
    if model.is_empty() {
        return Err("尚未填写模型名".into());
    }
    let res = chat_with_optional_stream(
        f,
        mode.system_prompt(),
        text,
        1400,
        if mode == Mode::Translate { 0.1 } else { 0.3 },
        on_progress,
    )
    .and_then(|c| {
        if c.is_empty() {
            Err("模型返回了空内容".into())
        } else {
            Ok(c)
        }
    });
    record(mode, &model, started, &res);
    res
}

/// 带动作模式的对话。A1 的「解释/翻译/改写」与 A2 的类型提议最终都走这里。
pub fn chat_mode(f: &LlmFile, user_text: &str, mode: Mode) -> Result<String, String> {
    let started = std::time::Instant::now();
    let model = f.model.trim().to_string();
    let res = chat_inner(f, user_text, mode);
    record(mode, &model, started, &res);
    res
}

fn chat_inner(f: &LlmFile, user_text: &str, mode: Mode) -> Result<String, String> {
    let text = user_text.trim();
    if text.is_empty() {
        return Err("问题是空的".into());
    }
    if f.model.trim().is_empty() {
        return Err("尚未填写模型名".into());
    }

    let content = chat_with_budget(
        f,
        mode.system_prompt(),
        text,
        1400,
        if mode == Mode::Translate { 0.1 } else { 0.3 },
    )?;
    if content.is_empty() {
        Err("模型返回了空内容".into())
    } else {
        Ok(content)
    }
}

fn truncate(s: &str) -> String {
    let t: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        format!("{t}…")
    } else {
        t
    }
}

// 让 PWSTR 的导入不报未使用（DPAPI 的第二个参数是可选输出名）
#[allow(dead_code)]
fn _unused(_p: PWSTR) {}
