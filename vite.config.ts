import { defineConfig } from "vite";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const projectRoot = fileURLToPath(new URL(".", import.meta.url));
const srcRoot = resolve(projectRoot, "src");

// 三入口：宠物窗（pet）、面板窗（panel）与任务提示弹出窗（alert）。
// root 设为 src/，产物直接落到 dist/ 根目录，与 tauri.conf.json 里的窗口 url 对齐。
export default defineConfig({
  root: srcRoot,
  base: "./",
  publicDir: false,
  build: {
    outDir: resolve(projectRoot, "dist"),
    emptyOutDir: true,
    target: "chrome110",
    rollupOptions: {
      // pet 作为根 index.html：Tauri 的 frontendDist 要求根目录有入口文件，
      // 否则资源不会被嵌入（症状：exe 体积不增长、窗口里显示"无法访问此页面"）。
      input: {
        index: resolve(srcRoot, "index.html"),
        // panel / alert 放根目录（src/panel.html -> dist/panel.html），不用子目录：
        // 实测子目录 panel/index.html 在 Tauri 的 asset 协议下取不到，面板会白屏。
        panel: resolve(srcRoot, "panel.html"),
        alert: resolve(srcRoot, "alert.html"),
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
  },
  clearScreen: false,
});
