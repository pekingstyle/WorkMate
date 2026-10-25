/**
 * WorkMate · 面板逻辑
 * 编制：WorkMate Contributors
 *
 * P0 阶段只做数据层自检：读 stats、触发模拟数据、跑一次聚合、展示分布。
 * 真实数据在 P2 由感知模块（M5）写入。
 */

import { installProbe } from "../core/probe";

const inTauri = "__TAURI_INTERNALS__" in window;

/* ------------------- 诊断：把真实 DOM 状态上报到 startup.log ------------------- */
/* 自动化侧看不到画面（ImageGrab / PrintWindow 对本透明窗都是黑），
   只能靠前端主动上报，才能确认面板到底有没有加载、渲染出来。 */
const DIAG_TAG = "panel";
const diagReport = async () => {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const h1 = (document.querySelector("h1") as HTMLElement | null)?.innerText ?? "NULL";
    const inputs = document.querySelectorAll("input").length;
    const bodyLen = document.body ? document.body.innerText.length : -1;
    const bg = getComputedStyle(document.body).backgroundColor;
    await invoke("log_frontend", {
      msg: `${DIAG_TAG} | href=${location.href} | title=${document.title} | h1=${h1} | bodyLen=${bodyLen} | inputs=${inputs} | bg=${bg}`,
    });
  } catch (_) {
    /* 非 Tauri 环境（浏览器里调试）忽略 */
  }
};
window.addEventListener("error", (ev) => {
  void (async () => {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("log_frontend", { msg: `${DIAG_TAG} JS-ERR ${ev.message}` });
    } catch (_) {}
  })();
});
void diagReport();

// 向 HTTP 控制面上报 DOM，自动化侧 GET /dom?window=panel 可读回
installProbe(DIAG_TAG);

interface DbStats {
  schema_version: number;
  events: number;
  rollup_5m: number;
  facts: number;
  reminders: number;
  size_bytes: number;
  path: string;
}

interface CurrentContext {
  ts: number;
  idle_s: number;
  app_cat: number;
  app_label: string;
  focus_min: number;
}

interface Reminder {
  id: number;
  fire_at: number;
  level: number;
  title: string;
  status: number;
  kind: number;
  /** 1=高 2=中 3=低 */
  priority: number;
  /** 预估用时（分钟，0 未估） */
  duration_min: number;
  note: string;
  location?: string;
  tz_offset_min?: number;
}

interface TzInfo {
  offset_min: number;
  offset_secs: number;
  utc_label: string;
  city_label: string;
  source: string;
  now_text: string;
  now_hm: string;
  machine_offset_min: number;
  machine_utc_label: string;
  machine_now_text: string;
  machine_mismatch: boolean;
}

interface CategoryRow {
  app_cat: number;
  /** 采样点数（诊断用，不展示给用户） */
  cnt: number;
  /** 活跃秒数（每个 5s 采样点活跃则 +5） */
  focus_sec: number;
}

const CAT_LABEL: Record<number, string> = {
  0: "IDE / 编码",
  1: "Paper Work",
  2: "浏览器",
  3: "会议",
  4: "即时通讯",
  5: "媒体",
  6: "其他",
  7: "AI Agent",
};

const $ = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;

async function invokeCmd<T>(cmd: string, args?: Record<string, unknown>): Promise<T | undefined> {
  if (!inTauri) return undefined;
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args).catch((e) => {
    toast(`调用失败：${e}`);
    return undefined;
  });
}

/** 与 invokeCmd 的区别：**把错误原文交回调用方**，而不是只弹个 toast。
 *  用户反馈「连接失败，看上面的提示」——但上面根本没有提示，
 *  根因就是错误被 invokeCmd 的 catch 吞掉了。 */
async function invokeResult<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<{ ok: boolean; data?: T; err?: string }> {
  if (!inTauri) return { ok: false, err: "非 Tauri 环境" };
  const { invoke } = await import("@tauri-apps/api/core");
  try {
    return { ok: true, data: await invoke<T>(cmd, args) };
  } catch (e) {
    return { ok: false, err: String(e) };
  }
}

function toast(msg: string): void {
  const el = $("toast");
  el.textContent = msg;
  window.setTimeout(() => {
    if (el.textContent === msg) el.textContent = "";
  }, 3200);
}

/* ------------------------------ 时间（底层城市时区驱动，默认中国 UTC+8） ------------------------------ */
// 用户电脑的系统时区可能被设为美国时区（如 UTC-7），但日常工作和口语（「明天早晨」「明天中午」）
// 均按所在城市时间（默认中国标准时间 UTC+8，出国切换城市时自动跟随新城市时区）。
const pad2 = (n: number): string => String(n).padStart(2, "0");

function fmtUtcOffsetLabel(offsetMin: number): string {
  const sign = offsetMin >= 0 ? "+" : "-";
  const abs = Math.abs(offsetMin);
  const h = Math.floor(abs / 60);
  const m = abs % 60;
  return m === 0 ? `UTC${sign}${h}` : `UTC${sign}${pad2(h)}:${pad2(m)}`;
}

const browserOffsetMin = -new Date().getTimezoneOffset();

let activeTz: TzInfo = {
  offset_min: 480,
  offset_secs: 480 * 60,
  utc_label: "UTC+8",
  city_label: "中国标准时间",
  source: "default_cn",
  now_text: "",
  now_hm: "",
  machine_offset_min: browserOffsetMin,
  machine_utc_label: fmtUtcOffsetLabel(browserOffsetMin),
  machine_now_text: "",
  machine_mismatch: browserOffsetMin !== 480,
};

function applyTzInfo(tz: TzInfo | undefined): void {
  if (!tz) return;
  activeTz = tz;
  renderTzBanner();
}

function renderTzBanner(): void {
  const el = document.getElementById("rm-tz-text");
  const nowSec = Math.floor(Date.now() / 1000);
  const cityHm = bjHM(nowSec, activeTz.offset_min);
  const machHm = bjHM(nowSec, activeTz.machine_offset_min);
  if (el) {
    if (activeTz.machine_mismatch) {
      el.innerHTML = `🌏 计时基准：<b>${escapeHtml(activeTz.city_label)} (${escapeHtml(activeTz.utc_label)}) ${cityHm}</b> · <span class="tz-warn">本机系统 ${escapeHtml(activeTz.machine_utc_label)} (${machHm}) 已自动校准</span>`;
    } else {
      el.innerHTML = `🌏 计时基准：<b>${escapeHtml(activeTz.city_label)} (${escapeHtml(activeTz.utc_label)}) ${cityHm}</b>（跟随所在城市自动识别与转换）`;
    }
  }
  const locTzEl = document.getElementById("loc-tz");
  if (locTzEl) {
    locTzEl.textContent = `${activeTz.city_label}（${activeTz.utc_label} · 当前 ${cityHm}）${
      activeTz.machine_mismatch ? ` ｜ 本机系统 ${activeTz.machine_utc_label} (${machHm})` : ""
    }`;
  }
  const rmTime = document.getElementById("rm-time") as HTMLInputElement | null;
  if (rmTime) {
    rmTime.title = `按 ${activeTz.city_label} (${activeTz.utc_label}) 选择提醒时刻`;
  }
}

async function syncTzStatus(): Promise<void> {
  const tz = await invokeCmd<TzInfo>("tz_status");
  if (tz) applyTzInfo(tz);
}

function bjDate(tsSec: number, offsetMin = activeTz.offset_min): Date {
  return new Date(tsSec * 1000 + offsetMin * 60 * 1000);
}

/** 时间戳 → 指定时区（默认当前生效城市时区） "HH:mm" */
function bjHM(tsSec: number, offsetMin = activeTz.offset_min): string {
  const d = bjDate(tsSec, offsetMin);
  return `${pad2(d.getUTCHours())}:${pad2(d.getUTCMinutes())}`;
}

/** 时间戳 → 指定时区（默认当前生效城市时区） "M/D HH:mm" */
function bjMDHM(tsSec: number, offsetMin = activeTz.offset_min): string {
  const d = bjDate(tsSec, offsetMin);
  return `${d.getUTCMonth() + 1}/${d.getUTCDate()} ${bjHM(tsSec, offsetMin)}`;
}

/** datetime-local 字符串按**当前生效城市时区**解释 → 秒级时间戳 */
function bjInputToTs(v: string): number {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})/.exec(v);
  if (!m) return 0;
  return Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5]) / 1000 - activeTz.offset_min * 60;
}

/** 生成跨时区或本机时区不一致时的辅助提示标签 */
function crossTzMeta(fireAt: number, recordedTzMin?: number): string {
  const rec = recordedTzMin ?? activeTz.offset_min;
  if (rec !== activeTz.offset_min) {
    const origHm = bjHM(fireAt, rec);
    return ` · <span class="tz-sub">原 ${fmtUtcOffsetLabel(rec)} ${origHm} → 已换算 ${activeTz.utc_label}</span>`;
  }
  if (activeTz.machine_mismatch) {
    const machMdhm = bjMDHM(fireAt, activeTz.machine_offset_min);
    return ` · <span class="tz-sub">折合本机 ${activeTz.machine_utc_label} ${machMdhm}</span>`;
  }
  return "";
}

/** 秒 → 人类可读时长：<60s 显示秒，<60min 显示分钟，否则 x 小时 y 分 */
function fmtDur(sec: number): string {
  if (sec < 60) return `${sec} 秒`;
  const m = Math.round(sec / 60);
  if (m < 60) return `${m} 分钟`;
  const h = Math.floor(m / 60);
  const rest = m % 60;
  return rest ? `${h} 小时 ${rest} 分` : `${h} 小时`;
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(2)} MB`;
}

function renderContext(ctx: CurrentContext | null | undefined): void {
  if (!ctx) {
    $("ctx-app").textContent = "尚无采样";
    return;
  }
  $("ctx-app").textContent = ctx.app_label;
  $("ctx-idle").textContent = ctx.idle_s < 60 ? `${ctx.idle_s} 秒` : `${Math.floor(ctx.idle_s / 60)} 分钟`;
  $("ctx-focus").textContent = `${ctx.focus_min} 分钟`;
  const d = bjDate(ctx.ts);
  $("ctx-ts").textContent = `${pad2(d.getUTCHours())}:${pad2(d.getUTCMinutes())}:${pad2(d.getUTCSeconds())}`;
}

async function refreshContext(): Promise<void> {
  const ctx = await invokeCmd<CurrentContext | null>("current_context");
  renderContext(ctx);
}

/** 补齐聚合再刷新：切到「今日」页/打开面板时调用，避免看到空图却要手动点聚合 */
async function ensureFresh(): Promise<void> {
  await invokeCmd<{ stale: boolean; rolled: number }>("ensure_stats_fresh");
}

async function refresh(ensure = false): Promise<void> {
  if (ensure) await ensureFresh();
  const st = await invokeCmd<DbStats>("db_stats");
  if (!st) {
    $("schema").textContent = "（非 Tauri 环境）";
    return;
  }
  $("schema").textContent = `v${st.schema_version}`;
  $("events").textContent = st.events.toLocaleString();
  $("rollup").textContent = st.rollup_5m.toLocaleString();
  $("facts").textContent = st.facts.toLocaleString();
  $("size").textContent = fmtBytes(st.size_bytes);
  $("path").textContent = st.path;

  // 首屏时 refreshToday 可能还没回来，先补一次，否则空状态文案会误判成「今天没数据」
  if (todayEvents === 0) await refreshToday();
  const rows = (await invokeCmd<CategoryRow[]>("recent_summary", { windowSec: 86400 })) ?? [];
  const dayEvents = refTodayEvents();
  renderBars(rows, dayEvents);
  await refreshContext();
}

function renderBars(rows: CategoryRow[], eventsToday = 0): void {
  const box = $("bars");
  if (!rows.length) {
    // 「暂无数据」这句话以前指向技术页的生成按钮，放在这里已经说不通了。
    // 真正要告诉用户的是：为什么没有数据。
    box.innerHTML =
      eventsToday > 0
        ? `<div class="empty">今天采到 ${eventsToday} 条记录，但都在闲置时段（键鼠离开超过 1 分钟不计入专注）。</div>`
        : '<div class="empty">今天还没有感知数据。WorkMate 在后台每 5 秒采样一次，正常用几分钟就会自动出现。</div>';
    return;
  }
  // 以「活跃时长」为唯一指标：条数只是采样点数，对用户没有意义（用户当场质疑过）。
  // 条宽必须按**占总量比例**画：早先按"最大值"归一，15% 的项也画满整条，
  // 七条一样长，完全看不出差异（截图里被抓到）。
  const total = rows.reduce((a, r) => a + r.focus_sec, 0);
  $("bars-total").textContent = total ? `合计 ${fmtDur(total)}` : "";
  box.innerHTML = rows
    .map((r, i) => {
      const label = CAT_LABEL[r.app_cat] ?? `类别 ${r.app_cat}`;
      const share = total ? (r.focus_sec / total) * 100 : 0;
      // 排名越靠后越浅，既能区分又不至于七彩
      const alpha = Math.max(0.45, 0.92 - i * 0.07);
      return `<div class="bar-item">
        <div class="bar-head"><span>${escapeHtml(label)}</span><span>${fmtDur(r.focus_sec)} · ${share.toFixed(0)}%</span></div>
        <div class="bar-track"><div class="bar-fill" style="width:${share.toFixed(1)}%;opacity:${alpha}"></div></div>
      </div>`;
    })
    .join("");
}

/* ------------------------------ 模型服务配置 ------------------------------ */

interface LlmView {
  base_url: string;
  model: string;
  has_key: boolean;
  key_masked: string;
  enabled: boolean;
  stream?: boolean;
}

function setLlmStatus(msg: string, cls = ""): void {
  const el = $("llm-status");
  el.textContent = msg;
  el.className = `llm-status ${cls}`;
}

// 表单脏标记：用户改过、或焦点还在输入框里，就不许定时刷新覆盖。
// 之前每 20s 无条件回填，用户正输入一半就被换回旧值（反馈里的"焦点来回切换后丢失"）。
let llmDirty = false;
const llmInputs = ["llm-base", "llm-model", "llm-key"];
llmInputs.forEach((id) => {
  const el = $(id) as HTMLInputElement;
  el.addEventListener("input", () => {
    llmDirty = true;
  });
});

async function loadLlmConfig(force = false): Promise<void> {
  const v = await invokeCmd<LlmView>("get_llm_config");
  if (!v) return;
  const keyEl = $("llm-key") as HTMLInputElement;
  const streamEl = document.getElementById("opt-stream") as HTMLInputElement | null;
  if (streamEl && typeof v.stream === "boolean") {
    streamEl.checked = v.stream;
  }
  const editing =
    llmDirty || llmInputs.some((id) => document.activeElement === $(id));
  if (force || !editing) {
    ($("llm-base") as HTMLInputElement).value = v.base_url;
    ($("llm-model") as HTMLInputElement).value = v.model;
  }
  keyEl.placeholder = v.has_key ? `已配置 ${v.key_masked}（留空不改）` : "粘贴你的 API Key";
  // 正在编辑时不覆盖状态文案，免得把"连接失败：xxx"刷掉
  if (!editing) setLlmStatus(v.enabled ? "已就绪" : "未配置完整", v.enabled ? "ok" : "");
}

$("llm-save").addEventListener("click", async () => {
  const base = ($("llm-base") as HTMLInputElement).value;
  const model = ($("llm-model") as HTMLInputElement).value;
  const key = ($("llm-key") as HTMLInputElement).value;
  const streamEl = document.getElementById("opt-stream") as HTMLInputElement | null;
  const v = await invokeCmd<LlmView>("save_llm_config", {
    baseUrl: base,
    model,
    apiKey: key,
    stream: streamEl ? streamEl.checked : true,
  });
  if (v) {
    llmDirty = false;
    ($("llm-key") as HTMLInputElement).value = "";
    ($("llm-key") as HTMLInputElement).placeholder = v.has_key
      ? `已配置 ${v.key_masked}（留空不改）`
      : "粘贴你的 API Key";
    setLlmStatus(v.enabled ? "已保存，配置完整" : "已保存，但配置不完整", v.enabled ? "ok" : "err");
    toast("配置已保存");
  }
});

$("llm-test").addEventListener("click", async () => {
  setLlmStatus("测试中…");
  const r = await invokeResult<string>("test_llm_config");
  if (r.ok) {
    setLlmStatus(`连接正常，模型回复：${r.data}`, "ok");
  } else {
    // 直接展示 Rust 返回的错误原文（含 HTTP 状态码与响应体片段），
    // 不要再说"看上面的提示"——上面没有提示。
    setLlmStatus(`连接失败：${r.err}`, "err");
  }
  await loadLlmStatus();
});

/* ------------------------------ 今日概览（A4） ------------------------------ */

interface TodaySummary {
  day_start: number;
  now: number;
  focus_min: number;
  categories: CategoryRow[];
  rm_pending: number;
  rm_done: number;
  water_count: number;
  standup_count: number;
  sit_streak_min: number;
  events: number;
}

let todayEvents = 0;
function refTodayEvents(): number {
  return todayEvents;
}

interface WorkBlockView {
  start: number;
  end: number;
  secs: number;
  app_cat: number;
  label: string;
}
interface BlocksView {
  day_start?: number;
  now?: number;
  total_sec: number;
  deep_sec: number;
  deep_count: number;
  switches: number;
  block_count: number;
  blocks: WorkBlockView[];
}

/** 每个应用类别对应的语义色（图例与纵向时间序列色块共用，省去重复文字标签） */
const CAT_COLOR: Record<number, string> = {
  0: "#5B5CE2", // IDE / 编码（靛蓝）
  7: "#8B5CF6", // AI Agent（紫罗兰）
  1: "#0EA5E9", // 办公 / 文档（天青）
  2: "#10B981", // 浏览器（翠绿）
  3: "#F59E0B", // 会议（琥珀橙）
  4: "#EC4899", // 即时通讯（玫瑰粉）
  5: "#F97316", // 媒体（珊瑚橙）
  6: "#94A3B8", // 其他（冷灰蓝）
};

/** 今天的工作块纵向彩色时间序列图：
 *  顶部用彩色图例标识今天打开/使用了哪些类别及累计时长，
 *  下方纵向按活跃小时展开，横向在 0–60 分钟刻度内用对应颜色块标注使用时段，避免冗长重复的文本列表。 */
async function refreshBlocks(): Promise<void> {
  const v = await invokeCmd<BlocksView>("today_blocks", { minSecs: 60 });
  if (!v) return;
  const total = v.total_sec || 0;
  $("blk-sum").textContent = total
    ? `深度 ${fmtDur(v.deep_sec)} · 切换 ${v.switches} 次`
    : "";
  const box = $("today-blocks");
  const list = v.blocks || [];
  if (!list.length) {
    box.innerHTML =
      v.block_count > 0
        ? `<div class="empty">今天有 ${v.block_count} 段活动，但每段都不到 1 分钟——切得太碎，没形成完整的工作块。</div>`
        : '<div class="empty">还没有工作记录。用几分钟就会自动出现。</div>';
    return;
  }

  // 1) 汇总今日出现过的类别图例（按时长倒序）
  const catTotals = new Map<number, number>();
  for (const b of list) {
    catTotals.set(b.app_cat, (catTotals.get(b.app_cat) || 0) + b.secs);
  }
  const legendHtml = Array.from(catTotals.entries())
    .sort((a, b) => b[1] - a[1])
    .map(([cat, secs]) => {
      const label = CAT_LABEL[cat] ?? `类别 ${cat}`;
      const color = CAT_COLOR[cat] ?? "#94A3B8";
      return `<span class="tl-leg-item"><i class="tl-dot" style="background:${color}"></i><span>${escapeHtml(label)}</span><b>${fmtDur(secs)}</b></span>`;
    })
    .join("");

  // 2) 按当前城市时区的自然小时切片（0..23），只渲染有活动的活跃小时行
  const tzSec = activeTz.offset_min * 60;
  interface HourSlice {
    startMin: number;
    endMin: number;
    activeSecs: number;
    cat: number;
    label: string;
    color: string;
    rawStart: number;
    rawEnd: number;
    rawSecs: number;
  }
  const hourMap = new Map<number, { hourOfDay: number; activeSecs: number; slices: HourSlice[] }>();

  for (const b of list) {
    const startWall = b.start + tzSec;
    const endWall = Math.max(startWall + Math.max(b.secs, 60), b.end + tzSec);
    const spanWall = Math.max(1, endWall - startWall);
    const firstHourBucket = Math.floor(startWall / 3600) * 3600;
    const lastHourBucket = Math.floor((endWall - 1) / 3600) * 3600;
    const label = CAT_LABEL[b.app_cat] ?? b.label;
    const color = CAT_COLOR[b.app_cat] ?? "#94A3B8";

    for (let hb = firstHourBucket; hb <= lastHourBucket; hb += 3600) {
      const segStart = Math.max(startWall, hb);
      const segEnd = Math.min(endWall, hb + 3600);
      if (segEnd <= segStart) continue;
      const ratio = (segEnd - segStart) / spanWall;
      const sliceActiveSecs = Math.max(15, Math.round(b.secs * ratio));
      const hourOfDay = ((hb / 3600) % 24 + 24) % 24;
      let row = hourMap.get(hb);
      if (!row) {
        row = { hourOfDay, activeSecs: 0, slices: [] };
        hourMap.set(hb, row);
      }
      row.activeSecs += sliceActiveSecs;
      row.slices.push({
        startMin: (segStart - hb) / 60,
        endMin: (segEnd - hb) / 60,
        activeSecs: sliceActiveSecs,
        cat: b.app_cat,
        label,
        color,
        rawStart: b.start,
        rawEnd: Math.max(b.end, b.start + b.secs),
        rawSecs: b.secs,
      });
    }
  }

  const sortedHours = Array.from(hourMap.entries()).sort((a, b) => a[0] - b[0]);
  const nowBucket = Math.floor((Math.floor(Date.now() / 1000) + tzSec) / 3600) * 3600;

  const spineRows: string[] = [];
  let prevBucket: number | null = null;
  for (const [hb, row] of sortedHours) {
    if (prevBucket !== null && hb - prevBucket > 3600) {
      const gapHours = Math.round((hb - prevBucket) / 3600) - 1;
      if (gapHours > 0) {
        spineRows.push(
          `<div class="tl-gap"><span class="tl-gap-dots">⋮</span><span>间隔 ${gapHours} 小时未用电脑</span></div>`,
        );
      }
    }
    prevBucket = hb;
    const hrLabel = `${String(row.hourOfDay).padStart(2, "0")}:00`;
    const isNow = hb === nowBucket ? " active-now" : "";
    const segsHtml = row.slices
      .map((s) => {
        const leftPct = Math.max(0, Math.min(97.5, (s.startMin / 60) * 100));
        const widthPct = Math.max(2.5, Math.min(100 - leftPct, ((s.endMin - s.startMin) / 60) * 100));
        const mins = Math.round(s.activeSecs / 60);
        const inlineTxt = widthPct >= 14 && mins >= 5 ? `${mins}m` : "";
        const tip = `${bjHM(s.rawStart)}–${bjHM(s.rawEnd)} · ${s.label} · 活跃 ${fmtDur(s.rawSecs)}`;
        return `<span class="tl-seg" style="left:${leftPct.toFixed(1)}%;width:${widthPct.toFixed(1)}%;background:${s.color}" data-tip="${escapeHtml(tip)}" data-color="${s.color}" title="${escapeHtml(tip)}">${inlineTxt}</span>`;
      })
      .join("");

    spineRows.push(
      `<div class="tl-row${isNow}">
        <span class="tl-hr">${hrLabel}</span>
        <span class="tl-node"></span>
        <span class="tl-track">${segsHtml}</span>
        <span class="tl-dur">${fmtDur(Math.min(3600, row.activeSecs))}</span>
      </div>`,
    );
  }

  const last = list[list.length - 1];
  const lastLabel = CAT_LABEL[last.app_cat] ?? last.label;
  const lastColor = CAT_COLOR[last.app_cat] ?? "#94A3B8";
  const defaultInspect = `<i class="tl-dot" style="background:${lastColor}"></i><span>最新时段：${bjHM(last.start)}–${bjHM(Math.max(last.end, last.start + last.secs))} · <b>${escapeHtml(lastLabel)}</b> · 活跃 ${fmtDur(last.secs)}</span>`;

  box.innerHTML = `
    <div class="tl-legend">${legendHtml}</div>
    <div class="tl-ruler"><span>00分</span><span>15分</span><span>30分</span><span>45分</span><span>60分</span></div>
    <div class="tl-spine">${spineRows.join("")}</div>
    <div class="tl-inspect" id="tl-inspect">${defaultInspect}</div>
  `;

  const inspectEl = document.getElementById("tl-inspect");
  if (inspectEl) {
    box.querySelectorAll<HTMLElement>(".tl-seg").forEach((seg) => {
      seg.addEventListener("mouseenter", () => {
        const tip = seg.dataset.tip || "";
        const col = seg.dataset.color || "#8B5CF6";
        inspectEl.innerHTML = `<i class="tl-dot" style="background:${col}"></i><span>${escapeHtml(tip)}</span>`;
      });
      seg.addEventListener("mouseleave", () => {
        inspectEl.innerHTML = defaultInspect;
      });
    });
  }
}

interface FocusStatusView {
  active: boolean;
  started_at: number;
  active_secs: number;
  uninterrupted_secs: number;
  target_secs: number;
  im_interrupts: number;
  trend?: {
    today_count: number;
    yesterday_count?: number;
    recent_avg: number;
    prev_avg: number;
    direction: string;
    delta_pct?: number;
    summary: string;
  };
}

let panelFocusActive = false;

async function refreshFocusStatus(): Promise<void> {
  const fst = await invokeCmd<FocusStatusView>("focus_status");
  if (!fst) return;
  panelFocusActive = Boolean(fst.active);
  const trendEl = $("td-intr-trend");
  const btn = $("btn-focus-toggle") as HTMLButtonElement | null;
  const trendText = fst.trend?.summary || `今日被打断 ${fst.trend?.today_count ?? 0} 次`;
  if (fst.active) {
    const mins = Math.floor((fst.active_secs || 0) / 60);
    const targetMins = Math.floor((fst.target_secs || 3000) / 60);
    if (trendEl) {
      trendEl.innerHTML = `🎯 <b>正在专注：${mins} / ${targetMins} 分钟</b>（本轮 IM 打断 ${fst.im_interrupts || 0} 次）<br /><span style="color:var(--muted);font-size:11px">📈 ${escapeHtml(trendText)}</span>`;
    }
    if (btn) btn.textContent = "结束专注";
  } else {
    if (trendEl) {
      trendEl.innerHTML = `🎯 <b>专注模式未开启</b>（目标 50 分钟，IM 打断自动提醒）<br /><span style="color:var(--muted);font-size:11px">📈 ${escapeHtml(trendText)}</span>`;
    }
    if (btn) btn.textContent = "开始专注";
  }
}

$("btn-focus-toggle")?.addEventListener("click", async () => {
  if (panelFocusActive) {
    await invokeCmd("focus_stop");
    toast("已结束本轮专注");
  } else {
    await invokeCmd("focus_start", { minutes: 50 });
    toast("已开始 50 分钟专注");
  }
  await refreshFocusStatus();
});

async function refreshToday(): Promise<void> {
  const t = await invokeCmd<TodaySummary>("today_summary");
  if (!t) return;
  todayEvents = t.events;
  $("td-focus").textContent = String(t.focus_min);
  $("td-pending").textContent = String(t.rm_pending);
  $("td-done").textContent = String(t.rm_done);
  $("td-water").textContent = String(t.water_count);
  $("td-stand").textContent = String(t.standup_count);

  await refreshFocusStatus();

  const box = $("td-cats");
  if (!t.categories.length) {
    box.innerHTML =
      t.events > 0
        ? '<span class="chip">今天只有闲置时段</span>'
        : '<span class="chip">今天还没有感知数据</span>';
  } else {
    box.innerHTML = t.categories
      .map(
        (r) =>
          `<span class="chip">${escapeHtml(CAT_LABEL[r.app_cat] ?? `类别${r.app_cat}`)} <b>${fmtDur(r.focus_sec)}</b></span>`,
      )
      .join("");
  }

  // 调度状态：静默时段与打扰预算对用户可见，才谈得上「贴心」
  const d = await invokeCmd<{ beijing_hm: string; quiet: boolean; budget_ok: boolean; tz?: TzInfo }>("notify_status");
  if (d) {
    if (d.tz) applyTzInfo(d.tz);
    const parts = [`当前 ${d.beijing_hm}（${activeTz.city_label} ${activeTz.utc_label}）`];
    parts.push(d.quiet ? "静默时段：只放硬提醒" : "可提醒");
    parts.push(d.budget_ok ? "打扰预算充足" : "刚提醒过，软提醒会顺延");
    if (t.sit_streak_min > 0) parts.push(`连续专注 ${t.sit_streak_min} 分钟`);
    $("td-sched").textContent = parts.join(" · ");
  }

  // 时间轴同步刷新，确保打开面板或刷新概览时工作块立即就绪
  await refreshBlocks();
}

/* ------------------------------ 偏好：流式输出 & 低功耗模式 ------------------------------ */

const streamToggle = document.getElementById("opt-stream") as HTMLInputElement | null;
streamToggle?.addEventListener("change", async () => {
  const base = ($("llm-base") as HTMLInputElement).value;
  const model = ($("llm-model") as HTMLInputElement).value;
  const v = await invokeCmd<LlmView>("save_llm_config", {
    baseUrl: base,
    model,
    apiKey: "",
    stream: streamToggle.checked,
  });
  if (v) {
    toast(streamToggle.checked ? "已开启流式输出（自动隐藏思考过程）" : "已切换为完整生成后一次性输出");
  }
});

const lpToggle = $("opt-lowpower") as HTMLInputElement;

async function loadLowPower(): Promise<void> {
  const v = await invokeCmd<string | null>("get_setting", { key: "pet.low_power" });
  lpToggle.checked = v === "1";
}

lpToggle.addEventListener("change", async () => {
  const v = lpToggle.checked ? "1" : "0";
  // set_setting 在 Rust 侧会推 wm:lowpower，宠物窗立刻换形象
  await invokeCmd("set_setting", { key: "pet.low_power", value: v });
  toast(lpToggle.checked ? "已切到低功耗静态形象" : "已恢复动画形象");
});

/* ------------------------------ 偏好：逾期摘要 / 自适应静默 ------------------------------ */

const digestToggle = $("opt-digest") as HTMLInputElement;
const adaptiveToggle = $("opt-adaptive-quiet") as HTMLInputElement;

async function loadRemindPrefs(): Promise<void> {
  // 默认都是开（未配置时按开处理），所以只有显式 "0" 才取消勾选
  const d = await invokeCmd<string | null>("get_setting", { key: "remind.digest" });
  digestToggle.checked = d !== "0";
  const a = await invokeCmd<string | null>("get_setting", { key: "remind.adaptive_quiet" });
  adaptiveToggle.checked = a !== "0";
}

digestToggle.addEventListener("change", async () => {
  await invokeCmd("set_setting", {
    key: "remind.digest",
    value: digestToggle.checked ? "1" : "0",
  });
  toast(digestToggle.checked ? "每天提醒一次逾期事项" : "已关闭逾期摘要");
});

adaptiveToggle.addEventListener("change", async () => {
  await invokeCmd("set_setting", {
    key: "remind.adaptive_quiet",
    value: adaptiveToggle.checked ? "1" : "0",
  });
  toast(
    adaptiveToggle.checked
      ? "深夜还在用电脑时不按静默处理"
      : "静默时段固定为 21:30–08:30 / 12:00–13:30",
  );
});

/* ------------------------------ 待办提醒 ------------------------------ */

function fmtRemain(fireAt: number): string {
  const diff = fireAt - Math.floor(Date.now() / 1000);
  if (diff <= 0) return "已到点";
  if (diff < 60) return `${diff} 秒后`;
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟后`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时后`;
  return bjMDHM(fireAt);
}

async function refreshReminders(): Promise<void> {
  const list = (await invokeCmd<Reminder[]>("list_reminders")) ?? [];
  const box = $("rm-list");
  if (!list.length) {
    box.innerHTML = '<div class="rm-empty">还没有待办。输入内容点「添加」，默认 30 分钟后提醒。</div>';
    return;
  }
  box.innerHTML = list
    .map((r) => {
      const cls = r.status === 2 ? "done" : r.status === 1 ? "fired" : "";
      const hh = bjMDHM(r.fire_at);
      const status = r.status === 2 ? "已完成" : r.status === 1 ? "已提醒" : fmtRemain(r.fire_at);
      const tzSub = crossTzMeta(r.fire_at, r.tz_offset_min);
      return `<div class="rm-item ${cls}" data-id="${r.id}">
        <div class="rm-main">
          <div class="rm-title">${escapeHtml(r.title)}</div>
          <div class="rm-meta">${hh} (${escapeHtml(activeTz.utc_label)}) · ${status}${tzSub}${
            r.priority === 1 ? ' · <b class="pri-hi">高</b>' : r.priority === 3 ? " · 低" : ""
          }${r.duration_min > 0 ? ` · ${r.duration_min} 分钟` : ""}</div>
        </div>
        ${r.status !== 2 ? `<button class="rm-act" data-act="done">完成</button>` : ""}
        <button class="rm-act" data-act="del">删</button>
      </div>`;
    })
    .join("");
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string,
  );
}

interface ParsedTodo {
  ok: boolean;
  fire_at: number;
  title: string;
  matched: string;
  tz_offset_min?: number;
  city_mdhm?: string;
  machine_mdhm?: string;
  cross_hint?: string;
  tz?: TzInfo;
}

/** 自然语言预览：输入「明天早晨9点提醒我发周报」「明天中午对齐需求」时实时告诉用户会定在什么时候 */
let parseTimer: number | undefined;
function schedulePreview(): void {
  clearTimeout(parseTimer);
  parseTimer = window.setTimeout(async () => {
    const input = $("rm-title") as HTMLInputElement;
    const hint = $("rm-parse");
    const text = input.value.trim();
    if (!text) {
      hint.textContent = `支持按 ${activeTz.city_label} (${activeTz.utc_label}) 识别「明天早晨」「明天中午」「今晚8点」「30分钟后」`;
      return;
    }
    const p = await invokeCmd<ParsedTodo>("parse_todo", { text });
    if (p?.tz) applyTzInfo(p.tz);
    if (p?.ok) {
      const cityTime = p.city_mdhm || bjMDHM(p.fire_at);
      const machPart = activeTz.machine_mismatch
        ? `（折合本机 ${activeTz.machine_utc_label} ${p.machine_mdhm || bjMDHM(p.fire_at, activeTz.machine_offset_min)}）`
        : "";
      hint.textContent = `已识别「${p.matched}」→ ${activeTz.city_label} (${activeTz.utc_label}) ${cityTime}${machPart} · ${p.title}`;
    } else {
      hint.textContent = `未识别时间；不选时间则默认 30 分钟后（按 ${activeTz.utc_label} 簿记）提醒`;
    }
  }, 220);
}

async function addReminder(minutes: number | null): Promise<void> {
  const input = $("rm-title") as HTMLInputElement;
  const title = input.value.trim();
  if (!title) {
    toast("先写点要记的事");
    input.focus();
    return;
  }

  const timeEl = $("rm-time") as HTMLInputElement;
  const timeInput = timeEl.value;

  // 用户显式选了时间（快捷按钮 / 日期控件）→ 尊重显式选择；
  // 否则先做自然语言解析，解析不出再按 30 分钟兜底。
  if (minutes === null && !timeInput) {
    const p = await invokeCmd<{
      id: number;
      fire_at: number;
      title: string;
      parsed: boolean;
      source: string;
      who_in_note: boolean;
      count: number;
      tz?: TzInfo;
      cross_hint?: string;
      items: Array<{ id: number; title: string; fire_at: number; kind: string; who_in_note: boolean }>;
      workload_line?: string;
    }>("add_todo", { text: title });
    if (p) {
      if (p.tz) applyTzInfo(p.tz);
      input.value = "";
      const src = p.source === "ai" ? "AI 解析" : "规则解析";
      const items = p.items ?? [];
      const wlSuffix = p.workload_line ? ` ｜ ${p.workload_line}` : "";
      const machPart = activeTz.machine_mismatch
        ? `（本机 ${activeTz.machine_utc_label} ${bjMDHM(p.fire_at, activeTz.machine_offset_min)}）`
        : "";
      if (items.length > 1) {
        // 一句话拆成多件时逐条列出：用户要能一眼核对有没有被拆错、时间锚得对不对
        const lines = items.map((it) => {
          const tag = it.kind === "event" ? "日程" : "待办";
          return `${tag}「${it.title}」→ ${bjMDHM(it.fire_at)} (${activeTz.utc_label})`;
        });
        $("rm-parse").textContent = `[${src}] 拆成 ${items.length} 条：${lines.join("；")}${wlSuffix}`;
        toast(`已记下 ${items.length} 条`);
      } else {
        const whoTag = p.who_in_note ? "（含对象）" : "";
        $("rm-parse").textContent = (p.parsed
          ? `[${src}] 已记下「${p.title}」→ ${activeTz.city_label} (${activeTz.utc_label}) ${bjMDHM(p.fire_at)}${machPart}${whoTag}`
          : `[${src}] 未识别时间，已按 30 分钟后记下「${p.title}」→ ${bjMDHM(p.fire_at)} (${activeTz.utc_label})`) + wlSuffix;
        toast(`已记下：${p.title}`);
      }
      await refreshReminders();
      await refreshToday();
    }
    return;
  }

  let fireAt: number;
  if (minutes !== null) {
    fireAt = Math.floor(Date.now() / 1000) + minutes * 60;
  } else {
    fireAt = bjInputToTs(timeInput);
  }

  const id = await invokeCmd<number>("add_reminder", { title, fireAt, level: 1 });
  if (id) {
    input.value = "";
    timeEl.value = "";
    toast(`已记下：${title}（${activeTz.utc_label} ${bjMDHM(fireAt)}）`);
    await refreshReminders();
    await refreshToday();
  }
}

$("rm-add").addEventListener("click", () => void addReminder(null));
$("rm-title").addEventListener("input", schedulePreview);
$("rm-title").addEventListener("keydown", (e) => {
  if ((e as KeyboardEvent).key === "Enter") void addReminder(null);
});
document.querySelectorAll<HTMLButtonElement>(".quick button").forEach((b) => {
  b.addEventListener("click", () => void addReminder(Number(b.dataset.min)));
});

$("rm-list").addEventListener("click", async (e) => {
  const btn = (e.target as HTMLElement).closest("button");
  if (!btn) return;
  const item = btn.closest(".rm-item") as HTMLElement;
  const id = Number(item.dataset.id);
  if (btn.dataset.act === "done") await invokeCmd("complete_reminder", { id });
  if (btn.dataset.act === "del") await invokeCmd("delete_reminder", { id });
  await refreshReminders();
});

$("btn-gen").addEventListener("click", async () => {
  const n = await invokeCmd<number>("simulate_ticks", { count: 48 });
  if (n !== undefined) toast(`已写入 ${n} 条模拟快照`);
  await refresh();
});

$("btn-rollup").addEventListener("click", async () => {
  const n = await invokeCmd<number>("rollup_now");
  if (n !== undefined) toast(`聚合完成，影响 ${n} 个桶`);
  await refresh();
});

$("btn-refresh").addEventListener("click", () => {
  void refresh(true);
});

// 模拟数据/压测数据会污染「今日概览」，给一个明确的归零入口
$("btn-reset").addEventListener("click", async () => {
  if (!confirm("清空所有感知统计（明细 + 聚合桶）？提醒与设置不受影响。")) return;
  const r = await invokeCmd<{ events: number; rollup: number }>("reset_stats");
  if (r) {
    toast(`已清空 ${r.events} 条明细、${r.rollup} 个聚合桶`);
    await refresh();
    await refreshToday();
  }
});

// 感知每 5 秒推一次，面板跟着刷新"此刻"；同时监听面板唤起事件立即刷新数据
if (inTauri) {
  void import("@tauri-apps/api/event").then(({ listen }) => {
    void listen("wm:tick", () => {
      void refreshContext();
    });
    void listen("wm:panel-open", () => {
      void syncTzStatus();
      void refresh(true);
      void refreshToday();
    });
    void listen<TzInfo>("wm:tz", (ev) => {
      applyTzInfo(ev.payload);
      void refreshReminders();
      void refreshToday();
    });
    void listen("wm:focus", () => {
      void refreshFocusStatus();
      void refreshBrief();
    });
    void listen("wm:reminders-changed", () => {
      void refreshReminders();
      void refreshToday();
      void refreshBrief();
    });
  });
}

/* ------------------------------ Tab 切换 ------------------------------ */
// 把「看 / 做 / 配 / 调」分开：一打开面板不该先看到模型参数（用户明确要求）
const TAB_KEY = "wm.panel.tab";

function showTab(name: string): void {
  document.querySelectorAll<HTMLButtonElement>(".tabs button").forEach((b) => {
    b.classList.toggle("on", b.dataset.tab === name);
  });
  document.querySelectorAll<HTMLElement>("[data-pane]").forEach((sec) => {
    sec.hidden = sec.dataset.pane !== name;
  });
  sessionStorage.setItem(TAB_KEY, name);
  if (name === "tech") void refreshLogs();
  if (name === "assistant") {
    void loadLlmStatus();
    void loadLlmUsage();
  }
  // 记忆页同样是「动作触发」：切过去才查，避免每次开面板都白跑一轮 SQL
  if (name === "memory") {
    void refreshLocation();
    void refreshBrief();
    void refreshNotes();
  }
  // 切到「今日」就补一次聚合，用户不该为了看到数字去点按钮
  if (name === "today") {
    void refresh(true);
    void refreshToday();
  }
}

document.querySelectorAll<HTMLButtonElement>(".tabs button").forEach((b) => {
  b.addEventListener("click", () => showTab(b.dataset.tab || "today"));
});

/* ------------------------------ 模型接入状态 ------------------------------ */

interface LlmLast {
  ts: number;
  ok: boolean;
  ms: number;
  mode: string;
  model: string;
  detail: string;
}
interface LlmStatus {
  configured: boolean;
  model: string;
  base_host: string;
  has_key: boolean;
  last: LlmLast;
}

async function loadLlmStatus(): Promise<void> {
  const st = await invokeCmd<LlmStatus>("llm_status");
  if (!st) return;
  const badge = $("llm-badge");
  badge.textContent = st.configured ? "已接入" : "未配置";
  badge.className = `badge ${st.configured ? "ok" : "err"}`;
  $("llm-model-name").textContent = st.configured
    ? `${st.model} · ${st.base_host}`
    : "展开下方「接入设置」填写即可";

  const l = st.last;
  if (!l || !l.ts) {
    $("llm-last").textContent = "尚无调用";
    $("llm-last-detail").textContent = "";
    return;
  }
  $("llm-last").textContent = `${bjHM(l.ts)} · ${l.ms} ms · ${l.ok ? "成功" : "失败"}`;
  $("llm-last-detail").textContent = `${l.mode} · ${l.model} · ${l.detail}`;
}

/* ------------------------------ 大模型用量（v8） ------------------------------ */

interface UsageDay {
  day: string;
  requests: number;
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  failed: number;
}
interface UsageView {
  today: UsageDay;
  total30: UsageDay;
  since_start: { requests: number; total_tokens: number; failed: number };
  days: UsageDay[];
}

/** 大数字压成 k / w：防滥用看的是数量级，精确到个位没意义还占宽度 */
function fmtNum(n: number): string {
  if (n >= 10000) return `${(n / 10000).toFixed(1)}w`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

async function loadLlmUsage(): Promise<void> {
  const v = await invokeCmd<UsageView>("llm_usage", { days: 14 });
  if (!v) return;
  const t = v.today || { requests: 0, total_tokens: 0 };
  const m = v.total30 || { requests: 0, total_tokens: 0 };
  $("use-today-req").textContent = String(t.requests);
  $("use-today-tok").textContent = fmtNum(t.total_tokens);
  $("use-30-req").textContent = String(m.requests);
  $("use-30-tok").textContent = fmtNum(m.total_tokens);

  const s = v.since_start || { requests: 0, total_tokens: 0, failed: 0 };
  $("use-since").textContent =
    `${s.requests} 次 · ${fmtNum(s.total_tokens)} tokens` +
    (s.failed ? ` · ${s.failed} 次失败` : "");

  // 最近 14 天：只画有调用的日子，空档不画（否则一条条 0 把图拉成锯齿）
  const days = (v.days || []).filter((d) => d.requests > 0);
  if (!days.length) {
    $("use-days").innerHTML = '<div class="empty">还没有调用记录。</div>';
    return;
  }
  const max = Math.max(...days.map((d) => d.total_tokens), 1);
  const todayStr = (v.today || {}).day || "";
  $("use-days").innerHTML = days
    .slice()
    .reverse()
    .map((d) => {
      const pct = Math.max(2, (d.total_tokens / max) * 100);
      const mmdd = d.day.slice(5);
      const isToday = d.day === todayStr ? " today" : "";
      return `<div class="uday${isToday}">
        <span class="ud">${mmdd}</span>
        <span class="utrack"><span class="ufill" style="width:${pct.toFixed(1)}%"></span></span>
        <span class="un">${d.requests} 次 · ${fmtNum(d.total_tokens)}</span>
      </div>`;
    })
    .join("");
}

/* ------------------------------ 记忆 / 位置（v6） ------------------------------ */

interface LocNow {
  label: string;
  source: string;
  home: string;
  trip: boolean;
  lat: number | null;
  lon: number | null;
}
interface TripDay {
  day: string;
  place: string;
  trip: boolean;
}
interface Risk {
  level: number;
  kind: string;
  title: string;
  detail: string;
}
interface Brief {
  habits: {
    samples: number;
    avg_start?: string;
    avg_end?: string;
    avg_active_min?: number;
    top_categories?: { cat_id?: number; cat: string; share_pct: number }[];
    interrupt_trend?: {
      today_count: number;
      yesterday_count?: number;
      recent_avg: number;
      prev_avg: number;
      direction: string;
      delta_pct?: number;
      summary: string;
    };
  };
  tasks?: {
    open?: number;
    overdue?: number;
    deferred?: number;
    done_7d?: number;
    avg_response_min?: number | null;
  };
  risks?: Risk[];
}
interface AskResult {
  intent: string;
  facts_text: string;
  result: { ok: boolean; answer?: string; error?: string; by?: string };
}

const SRC_LABEL: Record<string, string> = {
  geo: "Windows 定位",
  ip: "IP 粗定位",
  manual: "手填",
  mock: "测试注入",
  unavailable: "未取到",
};

async function refreshLocation(): Promise<void> {
  await syncTzStatus();
  const now = await invokeCmd<LocNow>("location_now");
  $("loc-now").textContent = now?.label ? now.label : "—";
  $("loc-src").textContent = now
    ? `${SRC_LABEL[now.source] ?? now.source}${now.trip ? " · 出差中" : ""}`
    : "—";
  $("loc-home").textContent = now?.home || "未设置";

  const hist = await invokeCmd<{ days: TripDay[] }>("location_history", { days: 120 });
  const days = hist?.days ?? [];
  const box = $("loc-trips");
  if (!days.length) {
    box.innerHTML = '<div class="rm-empty">还没有位置记录。自动定位或手填一次即可开始积累。</div>';
    return;
  }
  // 行程只展示「有位置记录的天」，出差的天打标
  box.innerHTML = days
    .slice(0, 40)
    .map(
      (d) =>
        `<div class="rm-item"><div class="rm-main"><div class="rm-title">${escapeHtml(d.day)}　${escapeHtml(d.place)}${
          d.trip ? '<span class="tag">出差</span>' : ""
        }</div></div></div>`,
    )
    .join("");
}

async function refreshBrief(): Promise<void> {
  const b = await invokeCmd<Brief>("memory_brief", { days: 14, tripDays: 120 });
  if (!b) return;
  const h = b.habits ?? ({} as Brief["habits"]);
  const samples = h.samples ?? 0;
  $("hb-samples").textContent = samples > 0 ? `${samples} 天` : "—";
  $("hb-span").textContent =
    samples > 0 && h.avg_start && h.avg_end ? `${h.avg_start} → ${h.avg_end}` : "—";
  const activeMin = h.avg_active_min ?? 0;
  $("hb-active").textContent = samples > 0 ? `${activeMin} 分钟` : "—";

  const cats = h.top_categories ?? [];
  const catsEl = $("hb-cats");
  if (!cats.length) {
    catsEl.innerHTML = '<span class="hb-chip">暂无分类记录</span>';
  } else {
    catsEl.innerHTML = cats
      .map((c) => {
        const col = (c.cat_id != null ? CAT_COLOR[c.cat_id] : undefined) ?? "#64748B";
        return `<span class="hb-chip"><i style="background:${col}"></i>${escapeHtml(c.cat)} <b>${c.share_pct ?? 0}%</b></span>`;
      })
      .join("");
  }

  const intrEl = $("hb-intr-trend");
  if (intrEl) {
    const tr = h.interrupt_trend;
    const dir = tr?.direction ?? "new";
    const tagMap: Record<string, { cls: string; text: string }> = {
      up: { cls: "up", text: "📈 上升" },
      down: { cls: "down", text: "📉 下降" },
      flat: { cls: "", text: "➡️ 持平" },
      new: { cls: "", text: "🌱 积累中" },
    };
    const tag = tagMap[dir] ?? tagMap.new;
    const summary = tr?.summary || "暂无样本";
    intrEl.innerHTML = `<span class="hb-trend-tag ${tag.cls}">${tag.text}</span><span>${escapeHtml(summary)}</span>`;
  }

  const t = b.tasks ?? {};
  const openCnt = t.open ?? 0;
  const overdueCnt = t.overdue ?? 0;
  const deferredCnt = t.deferred ?? 0;
  const done7dCnt = t.done_7d ?? 0;
  const avgResp = t.avg_response_min;
  const taskChips: string[] = [
    `<span class="hb-chip">未完成 <b>${openCnt}</b></span>`,
    `<span class="hb-chip${overdueCnt > 0 ? " warn" : ""}">逾期 <b>${overdueCnt}</b></span>`,
    `<span class="hb-chip">顺延 <b>${deferredCnt}</b></span>`,
    `<span class="hb-chip${done7dCnt > 0 ? " ok" : ""}">近 7 天完成 <b>${done7dCnt}</b></span>`,
  ];
  if (avgResp != null) {
    taskChips.push(`<span class="hb-chip">平均响应 <b>${avgResp} 分钟</b></span>`);
  }
  $("hb-tasks").innerHTML = taskChips.join("");

  const risks = b.risks ?? [];
  $("risk-list").innerHTML = risks.length
    ? risks
        .map(
          (r) =>
            `<div class="rm-item"><div class="rm-main"><div class="rm-title risk-l${r.level}">${escapeHtml(
              r.title,
            )}</div><div class="rm-meta">${escapeHtml(r.detail ?? "")}</div></div></div>`,
        )
        .join("")
    : '<div class="rm-empty">当前没有需要留意的风险</div>';
}

async function askMemory(q: string): Promise<void> {
  if (!q.trim()) return;
  const box = $("mem-answer");
  box.textContent = "查询中…";
  const r = await invokeResult<AskResult>("memory_ask", { question: q });
  if (!r.ok || !r.data) {
    box.textContent = `出错了：${r.err ?? "未知原因"}`;
    return;
  }
  const d = r.data;
  $("mem-facts").textContent = d.facts_text || "—";
  // 模型不可用时也要给出答案：事实清单本身就是可用的回答
  box.textContent = d.result?.answer ?? d.facts_text ?? "没有查到相关记录";
  if (d.result?.by === "fallback") {
    box.textContent += "（模型未接入，以上为原始事实）";
  }
}

$("loc-save").addEventListener("click", async () => {
  const v = ($("loc-input") as HTMLInputElement).value.trim();
  if (!v) return toast("先填一个城市名");
  const r = await invokeResult("location_set", { label: v, asHome: false });
  toast(r.ok ? `已设为当前位置：${v}` : `失败：${r.err}`);
  await refreshLocation();
});
$("loc-ashome").addEventListener("click", async () => {
  const cur = await invokeCmd<LocNow>("location_now");
  if (!cur?.label) return toast("当前没有位置，先定位或手填");
  await invokeResult("location_set", { label: cur.label, asHome: true });
  toast(`常驻地已设为 ${cur.label}`);
  await refreshLocation();
});
$("loc-refresh").addEventListener("click", async () => {
  $("loc-now").textContent = "定位中…";
  const r = await invokeResult<{ ok: boolean; label?: string }>("location_refresh");
  toast(r.ok && r.data?.ok ? `已记录：${r.data.label}` : "没取到位置（可手填）");
  await refreshLocation();
});
$("loc-clear").addEventListener("click", async () => {
  if (!confirm("清除全部位置轨迹？（待办上的位置标记会保留）")) return;
  const r = await invokeResult<number>("location_clear");
  toast(r.ok ? `已清除 ${r.data ?? 0} 条位置记录` : `失败：${r.err}`);
  await refreshLocation();
});
$("mem-ask").addEventListener("click", () => {
  void askMemory(($("mem-q") as HTMLInputElement).value);
});
$("mem-q").addEventListener("keydown", (e) => {
  if ((e as KeyboardEvent).key === "Enter") void askMemory(($("mem-q") as HTMLInputElement).value);
});
document.querySelectorAll<HTMLButtonElement>("#mem-quick button").forEach((b) => {
  b.addEventListener("click", () => {
    (($("mem-q") as HTMLInputElement).value) = b.dataset.q ?? "";
    void askMemory(b.dataset.q ?? "");
  });
});
interface Note {
  period: string;
  ts: number;
  source: string;
  summary: string;
  weight: number;
}

async function refreshNotes(): Promise<void> {
  const warm = await invokeCmd<string>("memory_warm");
  $("mem-warm").textContent = warm || "—";

  for (const scope of ["short", "mid", "long"]) {
    const r = await invokeCmd<{ notes: Note[] }>("memory_notes", { scope, limit: 20 });
    const box = $(`notes-${scope}`);
    if (!box) continue;
    const list = r?.notes ?? [];
    box.innerHTML = list.length
      ? list
          .map(
            (n) =>
              `<div class="mem-note"><span class="when">${escapeHtml(n.period)}${
                n.source === "ai" ? " · AI" : " · 规则"
              }</span>${escapeHtml(n.summary)}</div>`,
          )
          .join("")
      : '<div class="rm-empty">还没有记录</div>';
  }
}

$("mem-daily").addEventListener("click", async () => {
  $("mem-warm").textContent = "生成中…";
  const today = new Date(Date.now() + activeTz.offset_min * 60 * 1000)
    .toISOString()
    .slice(0, 10);
  const r = await invokeResult<{ source?: string; summary?: string }>("memory_daily", {
    day: today,
    force: true,
  });
  toast(r.ok ? `已写入（${r.data?.source === "ai" ? "模型" : "规则"}）` : `失败：${r.err}`);
  await refreshNotes();
});
$("mem-rollup").addEventListener("click", async () => {
  const r = await invokeResult("memory_rollup");
  toast(r.ok ? "已做一次分层抽象" : `失败：${r.err}`);
  await refreshNotes();
});

$("mem-scan").addEventListener("click", async () => {
  const r = await invokeResult<{ count: number }>("memory_scan");
  toast(r.ok ? `已重算，当前 ${r.data?.count ?? 0} 条风险` : `失败：${r.err}`);
  await refreshBrief();
});
$("mem-rebuild").addEventListener("click", async () => {
  const r = await invokeResult<{ days: number }>("memory_rebuild", { days: 14 });
  toast(r.ok ? `已重算 ${r.data?.days ?? 0} 天画像` : `失败：${r.err}`);
  await refreshBrief();
});

/* ------------------------------ 日志 ------------------------------ */

interface LogEntry {
  name: string;
  size: number;
  mtime: number;
  current: boolean;
}

async function refreshLogs(): Promise<void> {
  const list = await invokeCmd<LogEntry[]>("log_files");
  if (!list) return;
  const box = $("log-list");
  if (!list.length) {
    box.innerHTML = '<div class="rm-empty">还没有日志</div>';
    return;
  }
  box.innerHTML = list
    .map(
      (e) => `<div class="log-item">
        <a data-log="${escapeHtml(e.name)}">${e.current ? "当前日志 startup.log" : escapeHtml(e.name)}</a>
        <span class="log-meta">${fmtBytes(e.size)} · ${bjMDHM(e.mtime)}</span>
      </div>`,
    )
    .join("");
  box.querySelectorAll<HTMLAnchorElement>("[data-log]").forEach((a) => {
    a.addEventListener("click", () => {
      void invokeCmd("log_open", { name: a.dataset.log || "" });
    });
  });
}

$("btn-log-open").addEventListener("click", () => {
  void invokeCmd("log_open", { name: "" });
});
$("btn-log-dir").addEventListener("click", () => {
  void invokeCmd("log_folder");
});
$("btn-log-clear").addEventListener("click", async () => {
  if (!confirm("清理 7 天前的归档日志？（当前日志不会被删除）")) return;
  const r = await invokeCmd<{ files: number; bytes: number }>("log_clear", { days: 7 });
  if (r) {
    const n = r.files ?? 0;
    const mb = ((r.bytes ?? 0) / 1048576).toFixed(1);
    toast(n ? `已清理 ${n} 个归档，释放 ${mb}MB` : "7 天内没有可清理的归档");
    await refreshLogs();
  }
});

$("btn-log-rotate").addEventListener("click", async () => {
  const r = await invokeResult<string | null>("log_rotate");
  if (r.ok) {
    toast(r.data ? "已归档到 logs/" : "今天的日志无需归档");
    await refreshLogs();
  }
});

/* ------------------------------ 启动 ------------------------------ */

void syncTzStatus();
void loadLlmConfig();
void loadLlmStatus();
showTab(sessionStorage.getItem(TAB_KEY) || "today");
// 面板窗口常驻（只是隐藏），重新显示时页面不会重新加载 —— 必须自己刷新
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) {
    void syncTzStatus();
    void refresh(true);
    void refreshToday();
  }
});
void refreshReminders();
void refreshToday();
void loadLowPower();
void loadRemindPrefs();
window.setInterval(() => {
  void syncTzStatus();
  void loadLlmConfig();
  void loadLlmStatus();
  void refreshReminders();
  void refreshToday();
}, 20000);

void refresh();
