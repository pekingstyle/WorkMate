// WorkMate · 本地存储
// 编制：WorkMate Contributors
//
// 设计依据见规划 §3：两轮真机基准得出的三条纪律——
//   ① 一切统计走 rollup 预聚合表（明细表区间聚合 105ms，预聚合表 0.76ms）
//   ② 快照拆数值列，禁 JSON payload（280 B/行 vs 33 B/行）
//   ③ 保留策略用分段表 DROP，不用 DELETE + VACUUM
//
// 并发模型：单连接 + Mutex。桌面应用单进程、单写多读，WAL 下读不会阻塞写，
// 不需要连接池（省一个依赖，也省去池化带来的生命周期复杂度）。

pub mod repo;
pub mod schema;

use rusqlite::Connection;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

thread_local! {
    /// 同线程重入检测：std::sync::Mutex 不可重入，若在 db.with(|c| ...) 闭包内
    /// 间接触发 db::with_conn(...) 会永久死锁。通过线程局部标记提前拦截并报错。
    static IN_DB_LOCK: Cell<bool> = const { Cell::new(false) };
}

struct LockReentrancyGuard;

impl LockReentrancyGuard {
    fn enter() -> Result<Self, &'static str> {
        IN_DB_LOCK.with(|flag| {
            if flag.get() {
                Err("db lock re-entered on the same thread (deadlock prevented)")
            } else {
                flag.set(true);
                Ok(Self)
            }
        })
    }
}

impl Drop for LockReentrancyGuard {
    fn drop(&mut self) {
        IN_DB_LOCK.with(|flag| flag.set(false));
    }
}

/// 可克隆的库句柄。
///
/// 为什么 conn 改成 Arc：感知/提醒/定位这些后台线程要写库，但它们拿不到
/// `tauri::State<Db>`（State 只在命令调用期间有效）。早先的方案是把状态
/// 塞进全局变量再泄漏，代价是生命周期不可控；改成 Arc 后，
/// `app.manage(d.clone())` 与 `db::set_global(d)` 各拿一份引用，谁先退出都不悬空。
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

/// 进程级句柄，供后台线程使用（命令层仍走 tauri::State）
static GLOBAL: OnceLock<Db> = OnceLock::new();

pub fn set_global(d: Db) {
    let _ = GLOBAL.set(d);
}

/// 后台线程入口。返回 rusqlite::Result 以便与 repo 层的 `?` 直接组合。
pub fn with_conn<R>(
    f: impl FnOnce(&Connection) -> rusqlite::Result<R>,
) -> rusqlite::Result<R> {
    let d = GLOBAL
        .get()
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("db 尚未就绪".into()))?;
    let _reentrant = LockReentrancyGuard::enter()
        .map_err(|msg| rusqlite::Error::InvalidParameterName(msg.into()))?;
    let c = d
        .conn
        .lock()
        .map_err(|_| rusqlite::Error::InvalidParameterName("db 锁被污染".into()))?;
    f(&c)
}

impl Db {
    pub fn open(dir: &Path) -> rusqlite::Result<Self> {
        std::fs::create_dir_all(dir).ok();
        let path = dir.join("workmate.db");
        let conn = Connection::open(&path)?;

        // WAL + NORMAL：实测单条 commit 0.123ms；FULL 要 5.05ms，性价比不足
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;
             PRAGMA wal_autocheckpoint=1000;",
        )?;

        schema::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    /// 所有读写入口。锁粒度是单条语句级别，够用且不易死锁。
    pub fn with<R>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> Result<R, String> {
        let _reentrant = LockReentrancyGuard::enter().map_err(|e| e.to_string())?;
        let conn = self.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
        f(&conn).map_err(|e| e.to_string())
    }

    /// 需要 &mut 的场景（事务批量写入）
    pub fn with_mut<R>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<R>,
    ) -> Result<R, String> {
        let _reentrant = LockReentrancyGuard::enter().map_err(|e| e.to_string())?;
        let mut conn = self.conn.lock().map_err(|_| "db lock poisoned".to_string())?;
        f(&mut conn).map_err(|e| e.to_string())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 磁盘占用（含 WAL），用于面板展示与压测采样
    pub fn size_bytes(&self) -> u64 {
        let main = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let wal = std::fs::metadata(self.path.with_extension("db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        main + wal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reentrant_lock_detected_without_deadlock() {
        let g1 = LockReentrancyGuard::enter();
        assert!(g1.is_ok());
        let g2 = LockReentrancyGuard::enter();
        assert!(g2.is_err());
        drop(g1);
        let g3 = LockReentrancyGuard::enter();
        assert!(g3.is_ok());
    }
}
