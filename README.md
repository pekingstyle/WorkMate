# WorkMate

> **Lightweight Desktop Companion & Focus Assistant** built with **Tauri 2 (Rust + TypeScript)**.
>
> ⚠️ **License & Usage Notice**: This project is open-sourced under the **[PolyForm Noncommercial License 1.0.0](./LICENSE)** strictly for **personal study, technical research, and educational exchange**. **Commercial use is strictly prohibited.**

---

## ✨ Key Features

### 1. 🎯 Lightweight Focus Mode & IM Interruption Analytics
- **One-Click 50-Minute Focus**: Right-click the desktop companion and click **`🎯 开始专注`** to start a 50-minute focus session.
- **IM Interruption Detection**: Automatically detects when the foreground switches to an Instant Messaging app (WeChat, WeCom, Feishu/Lark, DingTalk, QQ, Slack, Telegram, etc.) during a focus session and gently reminds you via a speech bubble (`专注被打断了一次`).
- **50-Minute Rest & Hydration Prompt**: Upon completing a 50-minute focus session, WorkMate prompts you to stand up, stretch, and drink water. You can then manually start the next focus round whenever ready.
- **14-Day Interruption Trend**: Periodically tracks daily focus interruptions and analyzes whether your recent interruption frequency is **rising (`↑`)**, **falling (`↓`)**, or **stable (`→`)**.

### 2. 📊 Vertical Color-Coded Time-Series Activity Chart
- **Privacy-First Perception**: Samples keyboard/mouse idle seconds and foreground application category every 5 seconds. **Never records window titles, screen captures, or keystrokes.**
- **Compact Vertical Time-Series UI**: Displays today's work blocks using a semantic **color legend** (`IDE / 编码`, `AI Agent`, `办公 / 文档`, `浏览器`, `会议`, `即时通讯`, `媒体`, `其他`) paired with a compact **vertical hourly time-series chart** (`00–60 min` proportional colored segments per active hour), avoiding long, repetitive text logs.

### 3. 🌏 City-Driven Timezone Engine & Smart Todo / Trip Extraction
- **Unified City Timezone Anchor**: Resolves timezone automatically from your current/home city (defaulting to China Standard Time `UTC+8` when unconfigured).
- **Cross-Timezone Consistency**: Whether your host OS is set to US time or another timezone, natural language time expressions (`明天早晨`, `明天中午`, `今晚8点`, `下周三下午3点`) are parsed and booked in your active city's timezone, and automatically converted when delivering reminders across timezones.
- **Clipboard Scene Understanding**:
  - `Ctrl+Shift+Space`: Ask LLM about the selected/copied text in context.
  - `Ctrl+Shift+T`: Extract actionable todos or multi-step business trip itineraries from copied chat/email text.

### 4. ⚡ Non-Blocking Async Architecture & Local Security
- **Zero UI Freezing**: All LLM network requests, geolocation lookups, and SQLite aggregations run on Tauri's background thread pool (`tauri::async_runtime::spawn_blocking`), keeping the Win32 UI thread responsive at all times.
- **Local-Only Storage & DPAPI Encryption**:
  - Data is stored locally in SQLite (`WAL` mode) with 5-minute rollups (`rollup_5m`) and layered memory (`short` / `mid` / `long`).
  - LLM API keys are encrypted locally using **Windows DPAPI (`CryptProtectData`)** bound to the current Windows user account.
  - Automatic full-screen presentation detection hides the companion window during screen sharing or slide presentations.

---

## 🛠️ Tech Stack

| Layer | Technology |
| :--- | :--- |
| **Desktop Framework** | [Tauri 2](https://v2.tauri.app/) |
| **Backend** | Rust (`rusqlite` bundled SQLite, `ureq`, `windows-rs` Win32 & DPAPI) |
| **Frontend** | Vanilla TypeScript + HTML5/CSS3 + Vite 6 |
| **Platform** | Windows 10 / 11 (x64) |

---

## 🚀 Getting Started

### Prerequisites
- **Windows 10 / 11 (x64)** with WebView2 Runtime (pre-installed on Windows 10/11)
- **Node.js** `>= 18`
- **Rust** stable toolchain (`x86_64-pc-windows-msvc`) + Visual Studio C++ Build Tools

### Install Dependencies
```bash
npm install
```

### Run Unit Tests
```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

### Build Frontend & Release Binary
```bash
# Build frontend assets only
npm run build

# Build full Tauri release executable & NSIS installer
npx tauri build
```

The compiled executable will be generated at:
- `src-tauri/target/x86_64-pc-windows-msvc/release/workmate.exe`

---

## 🔒 Privacy & Security Design

1. **No Telemetry / No Cloud Sync**: WorkMate does not upload any usage data to any server. Only text explicitly triggered by the user (or daily local summary statistics if an LLM API key is configured by the user) is sent to the user's configured OpenAI-compatible endpoint.
2. **Process-Category Only**: Foreground perception maps process names to 8 coarse categories (`IDE`, `Office`, `Browser`, `Meeting`, `IM`, `Media`, `AI Agent`, `Other`) in memory and discards the process name before writing to SQLite.
3. **DPAPI Key Protection**: API keys are encrypted via Windows DPAPI and masked in the UI.
4. **Loopback CSRF Protection**: The local control/diagnostic port (`127.0.0.1`) validates `Origin` and `Sec-Fetch-Site` headers to block cross-site browser requests.

---

## 📜 License & Disclaimer

This repository is licensed under the **[PolyForm Noncommercial License 1.0.0](./LICENSE)**.

- **Permitted**: Personal study, self-learning, academic research, and non-commercial technical exchange.
- **Prohibited**: Any commercial use, commercial distribution, paid service integration, or enterprise production deployment.
- **No Warranty**: Provided `"AS IS"` without warranty of any kind. See [LICENSE](./LICENSE) for full legal terms.
