// WorkMate · 日志归档与打开
// 编制：WorkMate Contributors
//
// 为什么要单独做：
//   ① 排查「AI 接口 401 / 超时 / 返回空」时，用户需要一个能点开就看到的地方；
//      日志全在一个不断增长的 startup.log 里，三天后就没法看了。
//   ② 每日归档 + gzip：一天一个文件，压缩后通常只剩几十 KB，
//      既保留现场又不至于把用户磁盘占满。
//
// 约定：
//   · 当前日志：%APPDATA%/org.atomgit.workmate/startup.log（保持原路径，兼容既有脚本）
//   · 归档目录：%APPDATA%/org.atomgit.workmate/logs/2026-10-24.log.gz
//   · 触发时机：启动时 + 每日维护（跨天，或当前日志 > 8MB）
//   · 日期一律按**北京时间**判定（机器时区是美区，用本地日期会错一天）

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;

fn app_dir() -> Option<PathBuf> {
    Some(crate::data_dir())
}

/// 当前日志文件
pub fn current_log() -> Option<PathBuf> {
    app_dir().map(|d| d.join("startup.log"))
}

/// 归档目录
pub fn log_dir() -> Option<PathBuf> {
    let d = app_dir()?.join("logs");
    fs::create_dir_all(&d).ok();
    Some(d)
}

/// 时间戳 → 当前生效城市时区日期 "2026-10-24"（默认中国时区 UTC+8）
pub(crate) fn bj_date(ts: i64) -> String {
    crate::tz::date_str(ts)
}

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 需要归档就归档。返回归档文件路径（若发生）。
pub fn rotate_if_needed() -> Option<PathBuf> {
    let cur = current_log()?;
    let meta = fs::metadata(&cur).ok()?;
    if meta.len() == 0 {
        return None;
    }

    // 文件的「北京日期」用最后修改时间判定
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(now_sec);
    let file_day = bj_date(mtime);
    let today = bj_date(now_sec());

    // 跨天或超过 8MB 才归档；同日且不大就继续追加
    if file_day == today && meta.len() < 8 * 1024 * 1024 {
        return None;
    }

    let dir = log_dir()?;
    let mut target = dir.join(format!("{file_day}.log.gz"));
    let mut n = 2;
    while target.exists() {
        target = dir.join(format!("{file_day}-{n}.log.gz"));
        n += 1;
    }

    // 先把当前日志挪开，避免归档过程中还有新日志写进来
    let tmp = dir.join(format!("{file_day}.log.tmp"));
    if fs::rename(&cur, &tmp).is_err() {
        return None;
    }

    let mut src = File::open(&tmp).ok()?;
    let out = File::create(&target).ok()?;
    let mut enc = GzEncoder::new(BufWriter::new(out), Compression::default());
    // 流式分块压缩（8KB 栈缓冲），避免将数 MB 的日志整块读入堆内存
    let copy_ok = std::io::copy(&mut src, &mut enc).is_ok() && enc.finish().is_ok();
    drop(src);
    let _ = fs::remove_file(&tmp);
    if !copy_ok {
        let _ = fs::remove_file(&target);
        return None;
    }

    crate::startup_log(&format!("log rotated: {}", target.display()));
    Some(target)
}

/// 清理归档日志。`keep_days = 0` 表示全清。
/// 返回 (删除文件数, 释放字节数)。
pub fn clear(keep_days: i64) -> (usize, u64) {
    let Some(dir) = log_dir() else {
        return (0, 0);
    };
    let now = now_sec();
    let cutoff = if keep_days <= 0 { i64::MAX } else { now - keep_days * 86_400 };

    let mut n = 0;
    let mut bytes = 0u64;
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let is_gz = p.extension().and_then(|s| s.to_str()) == Some("gz");
            if !is_gz {
                continue;
            }
            let mtime = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(now);
            // 用**文件名里的日期**判断，不用 mtime：
            // 今天归档的可能是 40 天前的日志（归档动作发生在今天），
            // 按 mtime 会永远判成"刚生成"，清理永远清不掉（真机抓到）。
            let name_date = p
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.get(0..10))
                .unwrap_or("")
                .to_string();
            let expired = if keep_days <= 0 {
                true
            } else if name_date.len() == 10 {
                let cutoff_date = bj_date(cutoff);
                name_date.as_str() < cutoff_date.as_str()
            } else {
                mtime < cutoff
            };
            if expired {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                if fs::remove_file(&p).is_ok() {
                    n += 1;
                    bytes += size;
                }
            }
        }
    }
    crate::startup_log(&format!("log clear: keep_days={keep_days} files={n} bytes={bytes}"));
    (n, bytes)
}

#[derive(Debug, serde::Serialize)]
pub struct LogEntry {
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub current: bool,
}

/// 列出当前日志 + 归档（按时间倒序）
pub fn list() -> Vec<LogEntry> {
    let mut out = Vec::new();
    if let Some(cur) = current_log() {
        if let Ok(m) = fs::metadata(&cur) {
            out.push(LogEntry {
                name: "startup.log".into(),
                size: m.len(),
                mtime: m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                current: true,
            });
        }
    }
    if let Some(dir) = log_dir() {
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("gz") {
                    continue;
                }
                if let Ok(m) = e.metadata() {
                    out.push(LogEntry {
                        name: p.file_name().unwrap_or_default().to_string_lossy().to_string(),
                        size: m.len(),
                        mtime: m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                        current: false,
                    });
                }
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.mtime));
    out
}

/// 用系统默认程序打开一个文件/目录。
///
/// 用途：面板「查看日志」直接弹出记事本/资源管理器，不用让用户去 %APPDATA% 里翻。
pub fn open_path(p: &Path) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    if !p.exists() {
        return Err(format!("文件不存在：{}", p.display()));
    }
    let file = HSTRING::from(p.as_os_str());
    let verb = HSTRING::from("open");
    unsafe {
        let h = ShellExecuteW(
            None,
            &verb,
            &file,
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        // 返回值 > 32 才算成功（WinAPI 历史约定）
        if h.0 as isize <= 32 {
            return Err(format!("打开失败（code {}）", h.0 as isize));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_conversion() {
        // 2026-10-24 00:00 北京 = 2026-10-23 16:00 UTC = 1792771200
        assert_eq!(bj_date(1_792_771_200), "2026-10-24");
        assert_eq!(bj_date(0), "1970-01-01"); // 1970-01-01 08:00 北京
    }
}
