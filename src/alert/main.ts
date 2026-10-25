import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface AlertItem {
  id: number;
  title: string;
  meta?: string;
  priority?: number;
  kind?: number;
}

interface AlertPayload {
  ids?: number[];
  title?: string;
  count?: number;
  level?: number;
  badge?: string;
  meta?: string;
  warm?: string;
  items?: AlertItem[];
  digest?: boolean;
  focus_digest?: boolean;
  tz?: {
    city_label?: string;
    utc_label?: string;
  };
}

const cardEl = document.getElementById("alertCard")!;
const badgeEl = document.getElementById("alertBadge")!;
const tzEl = document.getElementById("alertTz")!;
const listEl = document.getElementById("alertList")!;
const warmEl = document.getElementById("alertWarm")!;
const btnDone = document.getElementById("btnDone") as HTMLButtonElement;
const btnSnooze15 = document.getElementById("btnSnooze15") as HTMLButtonElement;
const btnSnooze60 = document.getElementById("btnSnooze60") as HTMLButtonElement;
const btnOpenPanel = document.getElementById("btnOpenPanel") as HTMLButtonElement;
const btnClose = document.getElementById("btnClose") as HTMLButtonElement;
const progressBar = document.getElementById("alertProgressBar")!;

let currentItems: AlertItem[] = [];
let totalDurationMs = 20_000;
let remainingMs = 20_000;
let hovering = false;
let tickTimer: number | undefined;

function startCountdown(durationMs: number): void {
  totalDurationMs = durationMs;
  remainingMs = durationMs;
  progressBar.style.width = "100%";
  if (tickTimer !== undefined) {
    clearInterval(tickTimer);
  }
  tickTimer = window.setInterval(() => {
    if (hovering) return;
    remainingMs -= 150;
    const pct = Math.max(0, Math.min(100, (remainingMs / totalDurationMs) * 100));
    progressBar.style.width = `${pct}%`;
    if (remainingMs <= 0) {
      clearInterval(tickTimer);
      tickTimer = undefined;
      void invoke("alert_close");
    }
  }, 150);
}

function renderAlert(p: AlertPayload): void {
  if (!p) return;

  const isUrgent = p.level === 1;
  cardEl.classList.toggle("is-urgent", isUrgent);

  badgeEl.textContent = p.badge || (isUrgent ? "🔔 重要待办 / 日程到点" : "🔔 待办提醒");

  if (p.tz && (p.tz.city_label || p.tz.utc_label)) {
    tzEl.textContent = `${p.tz.city_label || ""} ${p.tz.utc_label || ""}`.trim();
    tzEl.hidden = false;
  } else {
    tzEl.hidden = true;
  }

  // 构建条目列表：优先使用结构化 items，兼容旧单条字段
  if (Array.isArray(p.items) && p.items.length > 0) {
    currentItems = p.items.map((x) => ({
      id: Number(x.id || 0),
      title: String(x.title || ""),
      meta: x.meta ? String(x.meta) : "",
      priority: Number(x.priority || 2),
      kind: Number(x.kind || 0),
    }));
  } else {
    const ids = Array.isArray(p.ids) ? p.ids : [];
    currentItems = [
      {
        id: ids[0] || 0,
        title: p.title || "待办事项提醒",
        meta: p.meta || "",
        priority: isUrgent ? 1 : 2,
      },
    ];
  }

  renderItemsList(p.meta);

  if (p.warm && p.warm.trim()) {
    warmEl.textContent = `💡 ${p.warm.trim()}`;
    warmEl.hidden = false;
  } else {
    warmEl.hidden = true;
  }

  // 高优提醒或专注结束汇总停留 30 秒，普通软提醒停留 20 秒
  const holdMs = isUrgent || p.focus_digest || p.digest ? 30_000 : 20_000;
  startCountdown(holdMs);
}

function renderItemsList(fallbackMeta?: string): void {
  listEl.innerHTML = "";
  const actionableIds = currentItems.map((i) => i.id).filter((id) => id > 0);
  const multi = currentItems.length > 1;

  btnDone.textContent = multi ? `✅ 全部完成 (${actionableIds.length})` : "✅ 完成";
  btnDone.disabled = actionableIds.length === 0;
  btnSnooze15.disabled = actionableIds.length === 0;
  btnSnooze60.disabled = actionableIds.length === 0;

  for (const item of currentItems) {
    const row = document.createElement("div");
    row.className = "alert-item";

    const main = document.createElement("div");
    main.className = "alert-item-main";

    const titleRow = document.createElement("div");
    titleRow.className = "alert-item-title-row";

    if (item.priority === 1) {
      const pri = document.createElement("span");
      pri.className = "pri-tag";
      pri.textContent = "重要";
      titleRow.appendChild(pri);
    }

    const titleSpan = document.createElement("span");
    titleSpan.className = "alert-item-title";
    titleSpan.textContent = item.title;
    titleRow.appendChild(titleSpan);
    main.appendChild(titleRow);

    const metaText = item.meta || (!multi ? fallbackMeta : "") || "";
    if (metaText) {
      const metaDiv = document.createElement("div");
      metaDiv.className = "alert-item-meta";
      metaDiv.textContent = metaText;
      main.appendChild(metaDiv);
    }

    row.appendChild(main);

    if (multi && item.id > 0) {
      const miniBtn = document.createElement("button");
      miniBtn.type = "button";
      miniBtn.className = "item-done-mini";
      miniBtn.textContent = "✓ 完成";
      miniBtn.addEventListener("click", async () => {
        await invoke("alert_complete", { ids: [item.id] });
        currentItems = currentItems.filter((x) => x.id !== item.id);
        if (currentItems.length === 0) {
          await invoke("alert_close");
        } else {
          renderItemsList(fallbackMeta);
        }
      });
      row.appendChild(miniBtn);
    }

    listEl.appendChild(row);
  }
}

cardEl.addEventListener("mouseenter", () => {
  hovering = true;
});

cardEl.addEventListener("mouseleave", () => {
  hovering = false;
});

btnClose.addEventListener("click", () => {
  void invoke("alert_close");
});

btnDone.addEventListener("click", async () => {
  const ids = currentItems.map((i) => i.id).filter((id) => id > 0);
  if (ids.length > 0) {
    await invoke("alert_complete", { ids });
  }
  await invoke("alert_close");
});

btnSnooze15.addEventListener("click", async () => {
  const ids = currentItems.map((i) => i.id).filter((id) => id > 0);
  if (ids.length > 0) {
    await invoke("alert_snooze", { ids, minutes: 15 });
  }
  await invoke("alert_close");
});

btnSnooze60.addEventListener("click", async () => {
  const ids = currentItems.map((i) => i.id).filter((id) => id > 0);
  if (ids.length > 0) {
    await invoke("alert_snooze", { ids, minutes: 60 });
  }
  await invoke("alert_close");
});

btnOpenPanel.addEventListener("click", async () => {
  await invoke("open_panel");
  await invoke("alert_close");
});

// 监听实时推送的任务提示事件，并在初始化时拉取最近一次挂起的提醒数据
void (async () => {
  await listen<AlertPayload>("wm:alert", (ev) => {
    if (ev.payload) {
      renderAlert(ev.payload);
    }
  });
  try {
    const initial = await invoke<AlertPayload | null>("alert_current");
    if (initial && (initial.title || (initial.items && initial.items.length > 0))) {
      renderAlert(initial);
    }
  } catch {
    // ignore
  }
})();
