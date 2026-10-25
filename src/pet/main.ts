/**
 * WorkMate · 宠物窗主逻辑
 * 编制：WorkMate Contributors
 *
 * 设计要点：
 * 1) 动画全部由 CSS 承担，JS 只切状态；合成器跑 transform，主线程空转。
 * 2) 拖拽自己实现（pointer capture + setPosition），不用 startDragging：
 *    后者在「透明 + 无边框」窗口上不可靠，实测拖不动。
 * 3) 右键菜单全部自绘，屏蔽 WebView2 的默认菜单。
 * 4) 退出有多条路径：右键菜单 / 托盘 / Ctrl+Shift+Q，避免用户被关不掉的窗口困住。
 */

import { installProbe } from "../core/probe";

type PetState =
  | "idle"
  | "focus"
  | "sleepy"
  | "talk"
  | "celebrate"
  | "alert"
  | "hover"
  | "drag";

const body = document.body;
const petEl = document.getElementById("pet") as HTMLElement;
const bubble = document.getElementById("bubble") as HTMLElement;
const bubbleBody = document.getElementById("bubbleBody") as HTMLElement;
const bubbleText = document.getElementById("bubbleText") as HTMLElement;
const fx = document.getElementById("fx") as HTMLElement;
const ctxmenu = document.getElementById("ctxmenu") as HTMLElement;

const inTauri = "__TAURI_INTERNALS__" in window;

/* ------------------- 诊断：把真实 DOM 状态上报到 startup.log ------------------- */
/* 自动化侧截不到透明窗画面，靠前端主动上报确认形象是否真的加载/渲染。 */
const DIAG_TAG = "pet";
const diagReport = async () => {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const img = document.getElementById("pet-img") as HTMLImageElement | null;
    const bodyLen = document.body ? document.body.innerText.length : -1;
    await invoke("log_frontend", {
      msg: `${DIAG_TAG} | href=${location.href} | title=${document.title} | img=${img ? `${img.complete}/${img.naturalWidth}x${img.naturalHeight}` : "none"} | src=${img ? img.currentSrc.slice(-28) : "-"} | bodyLen=${bodyLen}`,
    });

    /* 探测：panel.html 到底有没有被 asset 协议服务出来（正确返回应 ≈7.9KB 且含"模型服务"）。
       注意别探测 panel/index.html —— Tauri 对未知路径会回退到根 index.html（宠物页），
       返回 200 + 1226 字节，极具误导性。 */
    try {
      const r = await fetch("panel.html");
      const t = await r.text();
      const ok = t.includes("模型服务");
      await invoke("log_frontend", {
        msg: `probe-panel | status=${r.status} | len=${t.length} | hasPanelContent=${ok} | head=${t.slice(0, 60).replace(/\s+/g, " ")}`,
      });
    } catch (e) {
      await invoke("log_frontend", { msg: `probe-panel FAIL ${String(e)}` });
    }

    /* 测试模式：走正常 invoke 路径打开面板（后台线程/直接创建都会报 0x8007139F） */
    try {
      const isTest = (await invoke("test_mode")) as boolean;
      if (isTest) {
        setTimeout(() => void invoke("open_panel").catch(() => {}), 2000);
      }
    } catch (_) {}
  } catch (_) {
    /* 非 Tauri 环境忽略 */
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
// 等一拍，确保 <img> 已经完成解码再报
window.addEventListener("load", () => void diagReport());
setTimeout(() => void diagReport(), 800);

// 向 HTTP 控制面上报 DOM，自动化侧 GET /dom?window=pet 可读回
installProbe(DIAG_TAG);

/* ------------------- 临时诊断（定位拖拽失效，验证后可删） ------------------- */
window.addEventListener("error", (ev) => {
  document.title = `WM ERR ${ev.message}`.slice(0, 110);
});
window.addEventListener("unhandledrejection", (ev) => {
  document.title = `WM REJ ${String(ev.reason)}`.slice(0, 110);
});
// capture=true：最先执行，用来判断事件到底有没有进到 webview
document.addEventListener(
  "mousedown",
  (ev) => {
    if (!document.title.startsWith("WM stage")) {
      document.title = `WM doc-down ${Math.round(ev.clientX)},${Math.round(ev.clientY)}`;
    }
  },
  true,
);

let current: PetState = "idle";
let bubbleTimer: number | undefined;
let oneshotTimer: number | undefined;
let zzzEl: HTMLElement | null = null;

/** AI 回答的气泡停留时长：答案可能几百字，给足阅读时间（普通台词 4.2s 不够） */
const AI_BUBBLE_MS = 12000;

/* ------------------------------ Tauri 桥 ------------------------------ */

async function invokeCmd<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T | undefined> {
  if (!inTauri) return undefined;
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args).catch(() => undefined);
}

/* ------------------------------- 状态机 ------------------------------- */

function setState(next: PetState): void {
  if (current === next) return;

  body.classList.remove(`state-${current}`);
  body.classList.add(`state-${next}`);
  current = next;

  if (next === "sleepy" && !zzzEl) {
    zzzEl = document.createElement("div");
    zzzEl.className = "zzz";
    zzzEl.innerHTML = "<span>z</span><span>z</span><span>z</span>";
    fx.appendChild(zzzEl);
  }
  if (next !== "sleepy" && zzzEl) {
    zzzEl.remove();
    zzzEl = null;
  }

  clearTimeout(oneshotTimer);
  if (next === "celebrate" || next === "alert") {
    const dur = next === "celebrate" ? 950 : 1600;
    oneshotTimer = window.setTimeout(() => {
      if (current === next) setState("idle");
    }, dur);
  }
}

/* ------------------------------- 说话 ------------------------------- */

interface PetTzInfo {
  offset_min: number;
  utc_label: string;
  city_label: string;
  machine_offset_min: number;
  machine_utc_label: string;
  machine_mismatch: boolean;
}

let petTz: PetTzInfo = {
  offset_min: 480,
  utc_label: "UTC+8",
  city_label: "中国标准时间",
  machine_offset_min: -new Date().getTimezoneOffset(),
  machine_utc_label: "",
  machine_mismatch: false,
};

/** 时间戳 → 当前生效城市时区「今天 15:00 / 明天 15:00 / 10/26 15:00」 */
function bjWhen(ts: number, offsetMin = petTz.offset_min): string {
  const OFFSET = offsetMin * 60;
  const d = new Date((ts + OFFSET) * 1000);
  const now = new Date((Math.floor(Date.now() / 1000) + OFFSET) * 1000);
  const hm = `${String(d.getUTCHours()).padStart(2, "0")}:${String(d.getUTCMinutes()).padStart(2, "0")}`;
  const dayDiff = Math.round(
    (Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate()) -
      Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate())) /
      86_400_000,
  );
  if (dayDiff === 0) return `今天 ${hm}`;
  if (dayDiff === 1) return `明天 ${hm}`;
  return `${d.getUTCMonth() + 1}/${d.getUTCDate()} ${hm}`;
}

let bubbleHovering = false;
let pendingHideMs = 4200;

function scheduleBubbleHide(ms: number): void {
  pendingHideMs = ms;
  clearTimeout(bubbleTimer);
  if (bubbleHovering) return;
  bubbleTimer = window.setTimeout(() => {
    if (bubbleHovering) return;
    bubble.classList.remove("show", "is-thinking");
    window.setTimeout(() => {
      if (!bubble.classList.contains("show")) {
        bubble.hidden = true;
      }
    }, 200);
  }, ms);
}

bubble.addEventListener("mouseenter", () => {
  bubbleHovering = true;
  clearTimeout(bubbleTimer);
});

bubble.addEventListener("mouseleave", () => {
  bubbleHovering = false;
  if (!bubble.hidden && bubble.classList.contains("show")) {
    scheduleBubbleHide(Math.max(2800, Math.min(pendingHideMs, 6000)));
  }
});

function say(text: string, ms = 4200, keepScroll = false): void {
  bubble.classList.remove("is-thinking");
  bubbleText.textContent = text;
  // 诊断面包屑：气泡会自动隐藏，测试侧断言时多半已经晚了；
  // 这里把最后一句留在 body.dataset 上，供 /evalr 回读（不影响 UI）。
  document.body.dataset.lastSay = text;
  bubble.hidden = false;
  // 必须先归零滚动：上一条如果是长回答被滚到中段，下一条短句会从中间开始显示，
  // 看上去就像"顶部被裁掉"（截图里踩到过）。流式增量完成时则保留当前位置。
  if (!keepScroll) {
    bubbleBody.scrollTop = 0;
  }
  requestAnimationFrame(() => bubble.classList.add("show"));

  // 按字数自适应延长阅读时间（显式短提示如 2200ms 除外）
  const effectiveMs =
    ms <= 3000 ? ms : Math.max(ms, Math.min(18_000, 4500 + text.length * 60));
  scheduleBubbleHide(effectiveMs);

  setState("talk");
  clearTimeout(oneshotTimer);
  oneshotTimer = window.setTimeout(() => {
    if (current === "talk") setState("idle");
  }, Math.min(effectiveMs, 2600));
}

function addFx(kind: "heart" | "bang", ms = 2000): void {
  const el = document.createElement("div");
  el.className = kind;
  if (kind === "bang") el.style.marginTop = "-26px";
  fx.appendChild(el);
  window.setTimeout(() => el.remove(), ms);
}

/* ------------------------------- 台词 ------------------------------- */

const IDLE_LINES = [
  "行吧，给你看一眼。",
  "这个我记下了。",
  "先忙你的，我在。",
  "要不要歇会儿？",
  "别熬太晚。",
];

function pick<T>(arr: T[]): T {
  return arr[Math.floor(Math.random() * arr.length)];
}

/* ------------------------------ 拖拽 ------------------------------ */
// 用 pointer capture 自己搬窗口：startDragging() 在透明无边框窗口上不生效。
// 位移用 e.screenX（CSS px）算，再乘 scale 换成物理像素。

// 拖拽交给系统：.pet 上的 data-tauri-drag-region 会让 Tauri 在 WM_NCHITTEST
// 里返回 HTCAPTION，由 Windows 自己搬窗口 —— 最可靠，不依赖 JS 事件。
// 之前手写 setPosition 的方案实测收不到 mousedown（事件没进 webview），已废弃。

/* ------------------------------ 点击 / 双击 ------------------------------ */

let lastTapTs = 0;
let tapTimer: number | undefined;

function onTap(): void {
  const now = Date.now();
  if (now - lastTapTs < 300) {
    // 双击
    clearTimeout(tapTimer);
    tapTimer = undefined;
    lastTapTs = 0;
    hideMenu();
    void invokeCmd("open_panel");
    return;
  }
  lastTapTs = now;
  tapTimer = window.setTimeout(() => {
    tapTimer = undefined;
    say(pick(IDLE_LINES));
  }, 260);
}

/* ------------------------------ 右键菜单 ------------------------------ */

function clampMenuPos(x: number, y: number): void {
  const r = ctxmenu.getBoundingClientRect();
  const W = window.innerWidth;
  const H = window.innerHeight;
  const nx = x + r.width > W - 4 ? Math.max(4, W - r.width - 4) : x;
  const ny = y + r.height > H - 4 ? Math.max(4, H - r.height - 4) : y;
  ctxmenu.style.left = `${nx}px`;
  ctxmenu.style.top = `${ny}px`;
}

let focusActive = false;

function showMenu(x: number, y: number): void {
  ctxmenu.hidden = false;
  clampMenuPos(x, y);

  // 同步当前专注状态，动态切换「开始专注 / 结束专注」按钮文案
  void (async () => {
    const fst = await invokeCmd<{
      active: boolean;
      active_secs: number;
      target_secs: number;
      im_interrupts: number;
    }>("focus_status");
    const focusBtn = document.getElementById("ctx-focus");
    if (fst && focusBtn) {
      focusActive = Boolean(fst.active);
      if (fst.active) {
        const mins = Math.floor((fst.active_secs || 0) / 60);
        focusBtn.textContent = `⏹ 结束专注 (${mins}m · 断${fst.im_interrupts || 0}次)`;
      } else {
        focusBtn.textContent = "🎯 开始专注";
      }
      if (!ctxmenu.hidden) clampMenuPos(x, y);
    }
  })();

  // 异步探测剪贴板场景（A2）：理解用户此刻面临的是「出差行程 / 待办交办 / 问题解答」，
  // 自动在菜单顶部提示场景并高亮推荐动作。
  void (async () => {
    const info = await invokeCmd<{
      scene?: string;
      scene_label?: string;
      recommended_act?: string;
      chars: number;
    }>("clipboard_info");
    const banner = document.getElementById("ctx-scene");
    const askBtn = document.getElementById("ctx-ask");
    const taskBtn = document.getElementById("ctx-task");
    if (!info || !info.chars || info.scene === "empty") {
      if (banner) banner.hidden = true;
      askBtn?.classList.remove("rec");
      taskBtn?.classList.remove("rec");
      return;
    }
    const icon = info.scene === "trip" ? "🧳" : info.scene === "task" ? "📌" : "💡";
    if (banner) {
      banner.textContent = `${icon} 剪贴板：${info.scene_label || "已就绪"}`;
      banner.hidden = false;
    }
    askBtn?.classList.toggle("rec", info.recommended_act === "ask");
    taskBtn?.classList.toggle("rec", info.recommended_act === "task");
    if (!ctxmenu.hidden) clampMenuPos(x, y);
  })();
}

function hideMenu(): void {
  ctxmenu.hidden = true;
}

async function onMenuAction(act: string): Promise<void> {
  hideMenu();
  switch (act) {
    case "focus": {
      const fst = await invokeCmd<{ active: boolean }>("focus_status");
      const isRunning = fst ? Boolean(fst.active) : focusActive;
      if (isRunning) {
        await invokeCmd("focus_stop");
      } else {
        await invokeCmd("focus_start", { minutes: 50 });
      }
      break;
    }
    case "panel":
      await invokeCmd("open_panel");
      break;
    case "ask": {
      // 选中即问的手动入口（与 Ctrl+Shift+Space 同一条链路），自动结合当前工作负载与场景作答。
      say("让我看看…", 30000);
      try {
        const { invoke } = await import("@tauri-apps/api/core");
        await invoke("ask_clipboard");
      } catch (e) {
        setState("alert");
        addFx("bang", 2000);
        say(String(e), 6000);
      }
      break;
    }
    case "task": {
      // 「选中一段话 → 理解场景并提取待办/出差行程 + 评估工作负载」（与 Ctrl+Shift+T 同一条链路）。
      say("正在提取待办与行程…", 30000);
      try {
        const { invoke } = await import("@tauri-apps/api/core");
        await invoke("ai_task_from_clipboard");
      } catch (e) {
        setState("alert");
        addFx("bang", 2000);
        say(String(e), 6000);
      }
      break;
    }
    case "workload": {
      say("正在盘点手头负载…", 10000);
      try {
        const { invoke } = await import("@tauri-apps/api/core");
        await invoke("workload_brief");
      } catch (e) {
        say(String(e), 5000);
      }
      break;
    }
    case "say":
      say(pick(IDLE_LINES));
      break;
    case "present": {
      // 窗口化共享屏幕无法被全屏检测识别，这里让用户主动声明。
      // 先说话再躲：set_presentation 会立刻隐藏窗口，气泡要抢在它之前展示。
      say("好，我先躲起来，一小时后再出来。", 2200);
      window.setTimeout(() => {
        void invokeCmd("set_presentation", { seconds: 3600 });
      }, 1600);
      break;
    }
    case "hide":
      await invokeCmd("hide_pet");
      break;
    case "quit":
      await invokeCmd("quit_app");
      break;
    default:
      break;
  }
}

/* ------------------------------ 事件绑定 ------------------------------ */

petEl.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  // 拖拽由 data-tauri-drag-region 交给系统，这里不做任何搬窗口操作
});

petEl.addEventListener("mouseup", (e) => {
  if (e.button !== 0) return;
  onTap();
});

petEl.addEventListener("mouseenter", () => {
  if (current === "idle") setState("hover");
});
petEl.addEventListener("mouseleave", () => {
  // 原先引用了未定义的 dragging（每次移出宠物都抛 ReferenceError）。
  // 拖拽由 data-tauri-drag-region 交给系统处理，不经过 JS；
  // current 为 "drag" 时本分支本就不会命中，无需额外判断。
  if (current === "hover") setState("idle");
});

// 屏蔽 WebView2 默认右键菜单，改用自绘菜单
document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  showMenu(e.clientX, e.clientY);
});

ctxmenu.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest("button");
  if (!btn) return;
  void onMenuAction(btn.dataset.act || "");
});

document.addEventListener("mousedown", (e) => {
  if (!ctxmenu.hidden && !ctxmenu.contains(e.target as Node)) hideMenu();
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideMenu();
});

// 窗口不可见时暂停动画
document.addEventListener("visibilitychange", () => {
  body.classList.toggle("is-hidden", document.hidden);
});

/* --------------------------- 感知驱动（M5 → M1/M3） --------------------------- */
// 宠物状态不再由测试轮播决定，而是跟随真实感知：
//   离开 > 5 分钟 → 睡；在 IDE/办公 且专注 → 专注态；久坐 → 冒气泡劝休息

interface ContextTick {
  ts: number;
  idleS: number;
  appCat: number;
  appLabel: string;
  focusMin: number;
  flags: number;
}

let lastSleptFor = 0;

function applyContext(p: ContextTick): void {
  // 正在说话/拖拽时不打断
  if (current === "talk" || current === "drag") return;

  // 离开（5 分钟无输入）→ 打瞌睡
  if (p.idleS >= 300) {
    if (current !== "sleepy") setState("sleepy");
    lastSleptFor = p.ts;
    return;
  }
  // 回来了 → 醒
  if (current === "sleepy") {
    setState("idle");
    if (p.ts - lastSleptFor > 900) {
      say("回来了？刚才那会儿我眯了一会儿。", 3800);
      return;
    }
  }

  // 专注：IDE / 办公 且确实在动（空闲 < 60s）
  const working = p.appCat === 0 || p.appCat === 1;
  if (working && p.idleS < 60) {
    if (current !== "focus") setState("focus");
  } else if (current === "focus") {
    setState("idle");
  }

  // 喝水 / 站起的提醒改由后端统一调度（Rust: maybe_schedule_body_reminders → reminders 表）。
  // 前端不再自己喊，否则会绕过静默时段与打扰预算，跟任务提醒撞在一起变成噪音。
}

async function initPerception(): Promise<void> {
  if (!inTauri) return;
  try {
    const tzInit = await invokeCmd<PetTzInfo>("tz_status");
    if (tzInit) petTz = tzInit;
    const { listen } = await import("@tauri-apps/api/event");
    await listen<PetTzInfo>("wm:tz", (ev) => {
      if (ev.payload) petTz = ev.payload;
    });
    await listen<ContextTick>("wm:tick", (ev) => {
      applyContext(ev.payload);
    });

    // 到点提醒：抖动 + 惊叹号 + 气泡。
    // 载荷是多条合并后的（{ids,title,count,level,kinds}）——同一时刻到点的几条
    // 会拼成一句「A · B」，避免连珠炮。
    await listen<{
      ids: number[];
      title: string;
      count: number;
      level: number;
      meta?: string;
      warm?: string;
      digest?: boolean;
      tz?: PetTzInfo;
    }>("wm:reminder", (ev) => {
      if (ev.payload.tz) petTz = ev.payload.tz;
      setState("alert");
      addFx("bang", 2400);
      const n = ev.payload.count ?? 1;
      // 单独留一份面包屑：用户可能立刻点一下猫（会触发闲聊台词覆盖气泡），
      // 但「上次提醒内容」必须可回读，否则自动化断言会误判成没弹。
      document.body.dataset.lastReminder = ev.payload.title;
      // 第二行是「这件事有多急/要多久/找谁」（已逾期 2 天 · 约 30 分钟 · 对象 产品组），
      // 第三行是记忆来的温度提示。顺序不能反：先讲事，再讲关心。
      const meta = (ev.payload.meta ?? "").trim();
      const warm = (ev.payload.warm ?? "").trim();
      document.body.dataset.lastWarm = warm;
      document.body.dataset.lastMeta = meta;
      // 逾期摘要用另一种图标，避免和「到点提醒」混淆
      const icon = ev.payload.digest ? "📋" : "⏰";
      const text = [`${icon} ${ev.payload.title}`, meta, warm]
        .filter((s) => s && s.length > 0)
        .join("\n");
      say(text, 9000 + (n - 1) * 1500);
    });

    // 流式输出：思考阶段只展示单行「💭 正在思考…」，正文阶段自适应长高并平滑跟随
    let activeStreamReqId = 0;
    await listen<{
      req_id: number;
      phase: "thinking" | "streaming" | "done";
      text: string;
    }>("wm:answer_stream", (ev) => {
      const p = ev.payload;
      if (!p || !p.text) return;
      activeStreamReqId = p.req_id || 0;
      if (p.phase === "thinking") {
        bubble.classList.add("is-thinking");
        bubbleText.textContent = p.text;
        bubble.hidden = false;
        bubbleBody.scrollTop = 0;
        requestAnimationFrame(() => bubble.classList.add("show"));
        scheduleBubbleHide(30_000);
        return;
      }
      const nearBottom =
        bubbleBody.scrollHeight - bubbleBody.scrollTop - bubbleBody.clientHeight < 28;
      bubble.classList.remove("is-thinking");
      bubbleText.textContent = p.text;
      document.body.dataset.lastSay = p.text;
      document.body.dataset.lastAnswer = p.text;
      bubble.hidden = false;
      requestAnimationFrame(() => {
        bubble.classList.add("show");
        if (nearBottom) {
          bubbleBody.scrollTop = bubbleBody.scrollHeight;
        }
      });
      if (p.phase === "done") {
        say(p.text, AI_BUBBLE_MS, true);
      } else {
        scheduleBubbleHide(25_000);
      }
    });

    // 选中即问：Rust 侧（热键线程 / 右键菜单）把答案或错误推过来，气泡负责展示。
    // 窗口此时多半是隐藏的，事件机制不依赖窗口焦点，照常送达。
    await listen<string>("wm:answer", (ev) => {
      document.body.dataset.lastAnswer = ev.payload;
      // 若刚刚已由流式 done 渲染过完全一致的文本，保持滚动位置不跳回顶部
      const keepScroll = activeStreamReqId > 0 && bubbleText.textContent === ev.payload;
      say(ev.payload, AI_BUBBLE_MS, keepScroll);
    });
    // AI 建待办/行程成功：气泡确认「记下了什么、什么时候、优先级/地点」+「当前工作负载衔接评估」
    await listen<{
      title: string;
      fire_at: number;
      duration_min: number;
      priority: number;
      note: string;
      has_time: boolean;
      count?: number;
      tz?: PetTzInfo;
      cross_hint?: string;
      items?: Array<{
        id: number;
        title: string;
        fire_at: number;
        kind: string;
        location?: string;
        duration_min?: number;
        priority?: number;
        cross_hint?: string;
      }>;
      workload_line?: string;
    }>("wm:task", (ev) => {
      const p = ev.payload;
      if (p.tz) petTz = p.tz;
      document.body.dataset.lastTask = JSON.stringify(p);
      const wl = (p.workload_line ?? "").trim();
      const items = p.items ?? [];
      if (items.length > 1) {
        const lines = items.map((it) => {
          const tag = it.kind === "event" ? "日程" : "待办";
          const loc = it.location ? `·${it.location}` : "";
          return `· ${tag}「${it.title}」${bjWhen(it.fire_at)} (${petTz.utc_label})${loc}`;
        });
        const body = [`🧳 已拆解 ${items.length} 项日程与待办：`, ...lines, wl]
          .filter(Boolean)
          .join("\n");
        say(body, 9500);
        return;
      }
      const pri = p.priority === 1 ? "高" : p.priority === 3 ? "低" : "中";
      const dur = p.duration_min > 0 ? ` · 约 ${p.duration_min} 分钟` : "";
      const tzHint = p.cross_hint ? ` · ${p.cross_hint}` : ` (${petTz.utc_label})`;
      const body = [
        `✅ 记下了：${p.title}`,
        `${bjWhen(p.fire_at)}${tzHint} · ${pri}优先级${dur}`,
        wl,
      ]
        .filter(Boolean)
        .join("\n");
      say(body, 8000);
    });
    await listen<string>("wm:error", (ev) => {
      document.body.dataset.lastError = ev.payload;
      setState("alert");
      addFx("bang", 2200);
      say(ev.payload, 6000);
    });
    // 专注模式事件：开始专注 / 被 IM 打断提示 / 满 50 分钟提醒走走喝水 / 结束专注。
    // 不改变宠物形象，仅通过气泡提示。
    await listen<{
      kind: string;
      message: string;
      status?: {
        active: boolean;
        active_secs: number;
        target_secs: number;
        im_interrupts: number;
      };
    }>("wm:focus", (ev) => {
      const p = ev.payload;
      if (!p) return;
      if (p.status) {
        focusActive = Boolean(p.status.active);
      }
      document.body.dataset.lastFocus = JSON.stringify(p);
      if (p.message) {
        say(p.message, 8500);
      }
    });
    // 健康信号：五个事件全部注册成功才写。排障时先看 dataset.perc 在不在。
    document.body.dataset.perc = "tick,reminder,answer,task,error,focus";
  } catch (e) {
    // 以前这里是空 catch，事件注册失败会静默——「气泡从来不动」这类问题
    // 只能靠盲猜。现在把原因留在 dataset 与日志里。
    document.body.dataset.percErr = String(e);
    /* 感知不可用时保持静态待机 */
  }
}

/* --------------------------- 演示模式 --------------------------- */

const DEMO_DEFAULT = false;

function runDemo(): void {
  const script: Array<[PetState, string?]> = [
    ["idle", undefined],
    ["hover", undefined],
    ["talk", "复制选中的文字，按 Ctrl+Shift+Space 问我。"],
    ["celebrate", undefined],
    ["sleepy", undefined],
    ["idle", undefined],
    ["alert", "三点有个会，别忘了。"],
  ];
  let i = 0;
  const tick = (): void => {
    const [s, line] = script[i % script.length];
    i++;
    if (line) say(line, 3000);
    else {
      setState(s);
      if (s === "sleepy") window.setTimeout(() => setState("idle"), 4500);
    }
    window.setTimeout(tick, 2800);
  };
  tick();
}

if (DEMO_DEFAULT || new URLSearchParams(location.search).has("demo")) {
  runDemo();
}

/* --------------------------- 低功耗模式（静态形象） --------------------------- */
// APNG 每秒要解码 ~7.5 帧（296×296 RGBA），稳态 CPU 主要花在这。
// 笔记本/远程桌面上可以切成静态首帧：CPU 降到接近 0，形象仍完整。
// 静态图的最终 URL 从隐藏 img 上取（Vite 只会重写 HTML 里的资源引用）。
// 动画 URL 必须在模块加载时**只取一次**：切到静态后 pet-img.src 会被覆盖，
// 再读它就会把静态图当成动画源，切不回来。
const PET_ANIM_URL = (document.getElementById("pet-img") as HTMLImageElement | null)?.src ?? "";
function petStaticUrl(): string {
  const holder = document.getElementById("pet-static-src") as HTMLImageElement | null;
  return holder?.src || PET_ANIM_URL;
}

async function applyLowPower(on: boolean): Promise<void> {
  const img = document.getElementById("pet-img") as HTMLImageElement | null;
  if (!img) return;
  const want = on ? petStaticUrl() : PET_ANIM_URL;
  if (img.src !== want) {
    img.src = want;
  }
  body.classList.toggle("low-power", on);
  document.body.dataset.lowPower = on ? "1" : "0";
}

async function initLowPower(): Promise<void> {
  if (!inTauri) return;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const v = (await invoke("get_setting", { key: "pet.low_power" })) as string | null;
    await applyLowPower(v === "1");
    const { listen } = await import("@tauri-apps/api/event");
    await listen<string>("wm:lowpower", (ev) => void applyLowPower(ev.payload === "1"));
  } catch {
    /* 设置不可用时保持动画形象 */
  }
}

/* ---------------------------- 对外 API ---------------------------- */

declare global {
  interface Window {
    WorkMate?: {
      setState: (s: PetState) => void;
      say: (text: string, ms?: number) => void;
      addFx: (kind: "heart" | "bang", ms?: number) => void;
      getState: () => PetState;
    };
  }
}

window.WorkMate = {
  setState,
  say,
  addFx,
  getState: () => current,
};

requestAnimationFrame(() => setState("idle"));
void initPerception();
void initLowPower();
