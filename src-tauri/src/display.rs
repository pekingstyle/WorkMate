// WorkMate · 多屏 / DPI / 云桌面适配
// 编制：WorkMate Contributors
//
// 要解决的真机问题（都是「猫不见了」的成因）：
//   ① 双屏：启动位置必须落在「此刻真实连接着」的显示器里，不能假设主屏在右边
//   ② 拔掉外接屏 / 云桌面（RDP）每次登录分辨率都可能变 → 窗口坐标落到不存在的区域
//   ③ 125% / 150% 缩放切换 → 逻辑坐标与物理像素错位，窗口压边或越界
//   ④ RDP 会话里透明窗 + 置顶的合成开销更高，需要降级（前端读 /status 的 remote 字段）
//
// 策略（简单且可验证）：
//   · 位置存 settings['pet.pos'] = "x,y"（物理像素），**恢复前校验**该点仍在某台屏内
//   · 每 10s 轮询显示器拓扑签名（EnumDisplayMonitors 级成本，微秒量级），变化即收拢
//   · 窗口中心跑出所有屏幕 → 拉回主屏右下角（保证用户永远找得到）
//   · 云端会话用 SM_REMOTESESSION 判断，只做降级提示，不改变行为

use tauri::{Manager, PhysicalPosition};

#[derive(Debug, Clone, serde::Serialize)]
pub struct DisplayInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub scale: f64,
    pub primary: bool,
}

/// 主屏 + 全部显示器（物理坐标）
pub fn list(app: &tauri::AppHandle) -> Vec<DisplayInfo> {
    let primary_pos = app
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| *m.position());
    app.available_monitors()
        .map(|v| {
            v.into_iter()
                .map(|m| {
                    let p = *m.position();
                    let s = *m.size();
                    DisplayInfo {
                        name: m.name().cloned().unwrap_or_default(),
                        x: p.x,
                        y: p.y,
                        w: s.width,
                        h: s.height,
                        scale: m.scale_factor(),
                        primary: primary_pos == Some(p),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 拓扑签名：分辨率/位置/缩放任一变化都会变，用于 10s 轮询比对
pub fn signature(app: &tauri::AppHandle) -> String {
    let mut v: Vec<String> = list(app)
        .into_iter()
        .map(|d| format!("{}x{}@{},{}*{:.2}", d.w, d.h, d.x, d.y, d.scale))
        .collect();
    v.sort();
    v.join("|")
}

/// 是否运行在远程桌面（云桌面）会话里
pub fn is_remote_session() -> bool {
    // SM_REMOTESESSION = 0x1000，windows crate 已导出类型化常量
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_REMOTESESSION,
        ) != 0
    }
}

/// 某个物理点是否落在任一显示器内
fn point_on_screen(app: &tauri::AppHandle, x: i32, y: i32) -> bool {
    list(app)
        .iter()
        .any(|d| x >= d.x && x < d.x + d.w as i32 && y >= d.y && y < d.y + d.h as i32)
}

/// 桌宠透明宿主窗口逻辑尺寸（与 tauri.conf.json 保持一致）
pub const PET_LOGICAL_W: f64 = 280.0;
pub const PET_LOGICAL_H: f64 = 360.0;

/// 主屏右下角默认位（物理坐标，留 24 逻辑像素边距）
fn default_pos(app: &tauri::AppHandle) -> Option<PhysicalPosition<i32>> {
    let m = app.primary_monitor().ok().flatten()?;
    let p = *m.position();
    let s = *m.size();
    let scale = m.scale_factor();
    let margin = (24.0 * scale) as i32;
    let w = (PET_LOGICAL_W * scale) as i32;
    let h = (PET_LOGICAL_H * scale) as i32;
    Some(PhysicalPosition::new(
        p.x + s.width as i32 - w - margin,
        p.y + s.height as i32 - h - margin,
    ))
}

/// 把窗口收拢到可见区域。返回 Some(新位置) 表示移动了。
///
/// 判定用「窗口中心是否在某台屏内」：比检查整窗交叠更严格，
/// 用户把猫拖到半个屏幕外没问题，拖到完全不可见时会被拉回来。
pub fn ensure_visible(app: &tauri::AppHandle) -> Option<(i32, i32)> {
    let w = app.get_webview_window("pet")?;
    let pos = w.outer_position().ok()?;
    let size = w.outer_size().ok()?;
    let cx = pos.x + size.width as i32 / 2;
    let cy = pos.y + size.height as i32 / 2;
    if point_on_screen(app, cx, cy) {
        return None;
    }
    let target = default_pos(app)?;
    let _ = w.set_position(target);
    crate::startup_log(&format!(
        "display: pet off-screen at ({},{}), moved to ({},{})",
        pos.x, pos.y, target.x, target.y
    ));
    Some((target.x, target.y))
}

/// 保存当前窗口位置到 settings（物理像素）
pub fn save_position(app: &tauri::AppHandle) {
    let Some(db) = app.try_state::<crate::db::Db>() else {
        return;
    };
    let Some(w) = app.get_webview_window("pet") else {
        return;
    };
    if let Ok(p) = w.outer_position() {
        let v = format!("{},{}", p.x, p.y);
        let _ = db.with(|c| crate::db::repo::set_setting(c, "pet.pos", &v));
    }
}

/// 启动时定位：优先恢复上次位置（校验仍在屏内），否则主屏右下角。
pub fn restore_position(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("pet") else {
        crate::startup_log("display: pet window missing");
        return;
    };

    let saved = app
        .try_state::<crate::db::Db>()
        .and_then(|db| db.with(|c| crate::db::repo::get_setting(c, "pet.pos")).ok().flatten());

    if let Some(s) = &saved {
        let nums: Vec<i32> = s
            .split(',')
            .filter_map(|p| p.trim().parse::<i32>().ok())
            .collect();
        if nums.len() == 2 {
            // 恢复前必须校验：上次可能在外接屏上，这次外接屏没了
            let size = w
                .outer_size()
                .unwrap_or(tauri::PhysicalSize::new(PET_LOGICAL_W as u32, PET_LOGICAL_H as u32));
            let cx = nums[0] + size.width as i32 / 2;
            let cy = nums[1] + size.height as i32 / 2;
            if let Some(d) = list(app)
                .into_iter()
                .find(|d| cx >= d.x && cx < d.x + d.w as i32 && cy >= d.y && cy < d.y + d.h as i32)
            {
                let max_x = (d.x + d.w as i32 - size.width as i32).max(d.x);
                let max_y = (d.y + d.h as i32 - size.height as i32).max(d.y);
                let nx = nums[0].clamp(d.x, max_x);
                let ny = nums[1].clamp(d.y, max_y);
                let _ = w.set_position(PhysicalPosition::new(nx, ny));
                crate::startup_log(&format!("display: restored pet.pos ({nx},{ny})"));
                return;
            }
            crate::startup_log(&format!(
                "display: saved pos ({},{}) not on any screen, falling back",
                nums[0], nums[1]
            ));
        }
    }

    if let Some(p) = default_pos(app) {
        let _ = w.set_position(p);
        crate::startup_log(&format!(
            "display: default pos ({},{}) remote={} displays={}",
            p.x,
            p.y,
            is_remote_session(),
            list(app).len()
        ));
    } else {
        crate::startup_log("display: no primary monitor");
    }
}

/// 拓扑看护：每 10s 比对一次签名，变化即收拢窗口。
/// 覆盖：拔插外接屏、云桌面重连换分辨率、缩放变更、显示器摆位调整。
pub fn start_watcher(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut sig = signature(&app);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(10));
            let now = signature(&app);
            if now != sig {
                crate::startup_log(&format!("display: topology changed\n  old={sig}\n  new={now}"));
                sig = now;
                // 拓扑变化后窗口可能越界：收拢 + 立刻持久化新位置
                if ensure_visible(&app).is_some() {
                    save_position(&app);
                }
            }
        }
    });
}
