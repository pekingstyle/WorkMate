/**
 * WorkMate · 前端探针（配合 Rust 侧 HTTP 控制面 127.0.0.1:17820）
 * 编制：WorkMate Contributors
 *
 * 桌面助手是透明窗 + skipTaskbar，自动化侧既截不到画面、也模拟不了输入。
 * 所以前端主动把「真实 DOM 状态」POST 给本机控制面，自动化再从 GET /dom 读回并断言。
 *
 * 这比截屏可靠得多：不依赖合成器，也不受 DPI / 透明层影响。
 */
export const CONTROL = "http://127.0.0.1:17820";

export async function report(tag: string, msg: string): Promise<void> {
  try {
    // 用 text/plain：跨域但属于简单请求，不会触发 CORS 预检，服务端能正常收到
    await fetch(`${CONTROL}/report`, {
      method: "POST",
      headers: { "Content-Type": "text/plain" },
      body: JSON.stringify({ window: tag, msg }),
    });
  } catch (_) {
    /* 控制面没起来时静默忽略，不影响正常运行 */
  }
}

export function domMsg(tag: string): string {
  const h1 = (document.querySelector("h1") as HTMLElement | null)?.innerText ?? "-";
  const img = document.getElementById("pet-img") as HTMLImageElement | null;
  const bodyLen = document.body ? document.body.innerText.length : -1;
  const inputs = document.querySelectorAll("input").length;
  const cards = document.querySelectorAll(".card").length;
  const bg = getComputedStyle(document.body).backgroundColor;
  return (
    `${tag} | href=${location.href} | title=${document.title} | h1=${h1} | ` +
    `bodyLen=${bodyLen} | inputs=${inputs} | cards=${cards} | bg=${bg} | ` +
    `img=${img ? `${img.complete}/${img.naturalWidth}x${img.naturalHeight}` : "-"}`
  );
}

export function installProbe(tag: string): void {
  const send = () => void report(tag, domMsg(tag));

  window.addEventListener("load", send);
  window.addEventListener("error", (e) => void report(tag, `${tag} JS-ERR ${e.message}`));
  window.addEventListener("unhandledrejection", (e) =>
    void report(tag, `${tag} REJ ${String(e.reason)}`),
  );
  setTimeout(send, 800);

  // 响应控制面的刷新请求：GET /dom?window=xxx&refresh=1
  void (async () => {
    try {
      const { listen } = await import("@tauri-apps/api/event");
      await listen("wm:probe", () => send());
    } catch (_) {}
  })();
}
