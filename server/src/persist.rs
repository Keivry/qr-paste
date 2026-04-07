// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite 持久化层：使用 actor 模式（后台 OS 线程 + mpsc channel）处理所有写操作，
//! 避免阻塞异步运行时。读操作（初始加载）在启动时同步完成。

use {
    rusqlite::{Connection, params},
    std::{
        path::Path,
        sync::{Arc, mpsc},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
    tracing::{error, info, warn},
    uuid::Uuid,
};

// ---------------------------------------------------------------------------
// 公开数据传输结构（用于初始加载）
// ---------------------------------------------------------------------------

/// 从数据库加载的配对记录（仅包含可持久化字段）。
pub struct LoadedPairing {
    pub pairing_id: Uuid,
    pub pairing_secret: [u8; 32],
    pub epoch: u64,
    pub last_seen: Instant,
    pub expires_at: Instant,
    pub revision: u64,
}

/// 从数据库加载的浏览器会话记录。
pub struct LoadedBrowserSession {
    pub session_id: [u8; 32],
    pub pairing_id: Uuid,
    pub pairing_epoch: u64,
    pub created_at: Instant,
    pub last_seen: Instant,
    pub expires_at: Instant,
    pub revoked: bool,
}

// ---------------------------------------------------------------------------
// Actor 消息类型
// ---------------------------------------------------------------------------

/// 发送给后台写线程的持久化操作。
enum PersistTask {
    SavePairing {
        pairing_id: Uuid,
        pairing_secret: [u8; 32],
        epoch: u64,
        last_seen_unix: i64,
        expires_at_unix: i64,
        revision: u64,
    },
    DeletePairing {
        pairing_id: Uuid,
    },
    SaveBrowserSession {
        session_id: [u8; 32],
        pairing_id: Uuid,
        pairing_epoch: u64,
        created_at_unix: i64,
        last_seen_unix: i64,
        expires_at_unix: i64,
        revoked: bool,
    },
    DeleteBrowserSessionsForPairing {
        pairing_id: Uuid,
    },
    DeleteExpired {
        now_unix: i64,
    },
}

// ---------------------------------------------------------------------------
// PersistenceStore：公开句柄
// ---------------------------------------------------------------------------

/// 持久化存储句柄，通过 `Arc` 共享给各组件。所有写操作均为 fire-and-forget。
#[derive(Clone)]
pub struct PersistenceStore {
    tx: mpsc::SyncSender<PersistTask>,
}

impl PersistenceStore {
    /// 打开（或创建）SQLite 数据库，初始化 schema，加载现有数据，启动后台写线程。
    ///
    /// 返回 `(Arc<PersistenceStore>, Vec<LoadedPairing>, Vec<LoadedBrowserSession>)`。
    /// 已过期的记录在加载时直接跳过（不写回内存）。
    pub fn open(
        path: impl AsRef<Path>,
    ) -> anyhow::Result<(Arc<Self>, Vec<LoadedPairing>, Vec<LoadedBrowserSession>)> {
        let path = path.as_ref();
        let mut conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        init_schema(&mut conn)?;

        let now_unix = unix_now_secs();
        let pairings = load_pairings(&conn, now_unix)?;
        let browser_sessions = load_browser_sessions(&conn, now_unix)?;

        info!(
            path = %path.display(),
            pairings = pairings.len(),
            browser_sessions = browser_sessions.len(),
            "持久化存储已加载"
        );

        // 启动后台写线程（有界 channel，防止内存无限增长）
        let (tx, rx) = mpsc::sync_channel::<PersistTask>(1024);
        thread::Builder::new()
            .name("persist-writer".into())
            .spawn(move || writer_loop(conn, rx))?;

        Ok((Arc::new(Self { tx }), pairings, browser_sessions))
    }

    /// 持久化或更新一条配对记录（fire-and-forget）。
    pub fn save_pairing(
        &self,
        pairing_id: Uuid,
        pairing_secret: [u8; 32],
        epoch: u64,
        last_seen: Instant,
        expires_at: Instant,
        revision: u64,
    ) {
        self.send(PersistTask::SavePairing {
            pairing_id,
            pairing_secret,
            epoch,
            last_seen_unix: instant_to_unix_secs(last_seen),
            expires_at_unix: instant_to_unix_secs(expires_at),
            revision,
        });
    }

    /// 删除一条配对记录（级联删除关联的浏览器会话）。
    pub fn delete_pairing(&self, pairing_id: Uuid) {
        self.send(PersistTask::DeletePairing { pairing_id });
    }

    /// 持久化或更新一条浏览器会话记录。
    // 7 个参数均来自 BrowserSession 独立字段，无法合并为通用结构而不引入循环依赖，豁免此 lint。
    #[allow(clippy::too_many_arguments)]
    pub fn save_browser_session(
        &self,
        session_id: [u8; 32],
        pairing_id: Uuid,
        pairing_epoch: u64,
        created_at: Instant,
        last_seen: Instant,
        expires_at: Instant,
        revoked: bool,
    ) {
        self.send(PersistTask::SaveBrowserSession {
            session_id,
            pairing_id,
            pairing_epoch,
            created_at_unix: instant_to_unix_secs(created_at),
            last_seen_unix: instant_to_unix_secs(last_seen),
            expires_at_unix: instant_to_unix_secs(expires_at),
            revoked,
        });
    }

    /// 删除指定配对关联的所有浏览器会话（撤销/重新 bootstrap 时调用）。
    pub fn delete_browser_sessions_for_pairing(&self, pairing_id: Uuid) {
        self.send(PersistTask::DeleteBrowserSessionsForPairing { pairing_id });
    }

    /// 删除所有过期记录（由清理任务周期调用）。
    pub fn delete_expired(&self) {
        self.send(PersistTask::DeleteExpired {
            now_unix: unix_now_secs(),
        });
    }

    fn send(&self, task: PersistTask) {
        if let Err(e) = self.tx.try_send(task) {
            match e {
                mpsc::TrySendError::Full(_) => {
                    warn!("持久化通道已满，写操作被丢弃（背压过大）");
                }
                mpsc::TrySendError::Disconnected(_) => {
                    error!("持久化写线程已退出，写操作被丢弃");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Schema 初始化
// ---------------------------------------------------------------------------

fn init_schema(conn: &mut Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pairings (
            pairing_id      TEXT    PRIMARY KEY NOT NULL,
            pairing_secret  BLOB    NOT NULL,
            epoch           INTEGER NOT NULL DEFAULT 0,
            last_seen_unix  INTEGER NOT NULL,
            expires_at_unix INTEGER NOT NULL,
            revision        INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS browser_sessions (
            session_id      BLOB    PRIMARY KEY NOT NULL,
            pairing_id      TEXT    NOT NULL REFERENCES pairings(pairing_id) ON DELETE CASCADE,
            pairing_epoch   INTEGER NOT NULL,
            created_at_unix INTEGER NOT NULL,
            last_seen_unix  INTEGER NOT NULL,
            expires_at_unix INTEGER NOT NULL,
            revoked         INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS idx_browser_sessions_pairing_id
            ON browser_sessions(pairing_id);

        PRAGMA foreign_keys = ON;",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 初始加载
// ---------------------------------------------------------------------------

fn load_pairings(conn: &Connection, now_unix: i64) -> anyhow::Result<Vec<LoadedPairing>> {
    let mut stmt = conn.prepare(
        "SELECT pairing_id, pairing_secret, epoch, last_seen_unix, expires_at_unix, revision
         FROM pairings
         WHERE expires_at_unix > ?1",
    )?;

    let rows = stmt.query_map(params![now_unix], |row| {
        let id_str: String = row.get(0)?;
        let secret_blob: Vec<u8> = row.get(1)?;
        let epoch: i64 = row.get(2)?;
        let last_seen_unix: i64 = row.get(3)?;
        let expires_at_unix: i64 = row.get(4)?;
        let revision: i64 = row.get(5)?;
        Ok((
            id_str,
            secret_blob,
            epoch,
            last_seen_unix,
            expires_at_unix,
            revision,
        ))
    })?;

    let mut result = Vec::new();
    for row in rows {
        let (id_str, secret_blob, epoch, last_seen_unix, expires_at_unix, revision) = row?;
        let pairing_id = match Uuid::parse_str(&id_str) {
            Ok(id) => id,
            Err(_) => {
                warn!(pairing_id = %id_str, "跳过无效 pairing_id");
                continue;
            }
        };
        let secret: [u8; 32] = match secret_blob.try_into() {
            Ok(bytes) => bytes,
            Err(_) => {
                warn!(%pairing_id, "跳过 pairing_secret 长度异常的记录");
                continue;
            }
        };
        let last_seen = unix_secs_to_instant(last_seen_unix);
        let expires_at = unix_secs_to_instant(expires_at_unix);

        let (epoch_u64, revision_u64) = match (u64::try_from(epoch), u64::try_from(revision)) {
            (Ok(e), Ok(r)) => (e, r),
            _ => {
                warn!(%pairing_id, "跳过 epoch/revision 值异常的记录");
                continue;
            }
        };

        result.push(LoadedPairing {
            pairing_id,
            pairing_secret: secret,
            epoch: epoch_u64,
            last_seen,
            expires_at,
            revision: revision_u64,
        });
    }

    Ok(result)
}

fn load_browser_sessions(
    conn: &Connection,
    now_unix: i64,
) -> anyhow::Result<Vec<LoadedBrowserSession>> {
    let mut stmt = conn.prepare(
        "SELECT bs.session_id, bs.pairing_id, bs.pairing_epoch, bs.created_at_unix,
                bs.last_seen_unix, bs.expires_at_unix, bs.revoked
         FROM browser_sessions bs
         INNER JOIN pairings p ON bs.pairing_id = p.pairing_id
         WHERE bs.expires_at_unix > ?1 AND bs.revoked = 0
           AND p.expires_at_unix > ?1",
    )?;

    let rows = stmt.query_map(params![now_unix], |row| {
        let session_blob: Vec<u8> = row.get(0)?;
        let pairing_id_str: String = row.get(1)?;
        let pairing_epoch: i64 = row.get(2)?;
        let created_at_unix: i64 = row.get(3)?;
        let last_seen_unix: i64 = row.get(4)?;
        let expires_at_unix: i64 = row.get(5)?;
        let revoked: bool = row.get(6)?;
        Ok((
            session_blob,
            pairing_id_str,
            pairing_epoch,
            created_at_unix,
            last_seen_unix,
            expires_at_unix,
            revoked,
        ))
    })?;

    let mut result = Vec::new();
    for row in rows {
        let (
            session_blob,
            pairing_id_str,
            pairing_epoch,
            created_at_unix,
            last_seen_unix,
            expires_at_unix,
            revoked,
        ) = row?;

        let session_id: [u8; 32] = match session_blob.try_into() {
            Ok(bytes) => bytes,
            Err(_) => {
                warn!("跳过 session_id 长度异常的记录");
                continue;
            }
        };
        let pairing_id = match Uuid::parse_str(&pairing_id_str) {
            Ok(id) => id,
            Err(_) => {
                warn!(pairing_id = %pairing_id_str, "跳过浏览器会话中无效 pairing_id");
                continue;
            }
        };

        let pairing_epoch_u64 = match u64::try_from(pairing_epoch) {
            Ok(v) => v,
            Err(_) => {
                warn!(%pairing_id, "跳过 pairing_epoch 值异常的浏览器会话记录");
                continue;
            }
        };

        result.push(LoadedBrowserSession {
            session_id,
            pairing_id,
            pairing_epoch: pairing_epoch_u64,
            created_at: unix_secs_to_instant(created_at_unix),
            last_seen: unix_secs_to_instant(last_seen_unix),
            expires_at: unix_secs_to_instant(expires_at_unix),
            revoked,
        });
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// 后台写线程
// ---------------------------------------------------------------------------

fn writer_loop(mut conn: Connection, rx: mpsc::Receiver<PersistTask>) {
    // 启用外键约束（每个连接需独立设置）
    if let Err(err) = conn.execute_batch("PRAGMA foreign_keys = ON;") {
        error!("无法启用外键约束: {err}");
    }

    for task in rx {
        if let Err(err) = execute_task(&mut conn, task) {
            error!("持久化写操作失败: {err}");
        }
    }

    // channel 已关闭（sender 全部 drop）
    info!("持久化写线程正常退出");
}

fn execute_task(conn: &mut Connection, task: PersistTask) -> anyhow::Result<()> {
    match task {
        PersistTask::SavePairing {
            pairing_id,
            pairing_secret,
            epoch,
            last_seen_unix,
            expires_at_unix,
            revision,
        } => {
            conn.execute(
                "INSERT INTO pairings (pairing_id, pairing_secret, epoch, last_seen_unix, expires_at_unix, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(pairing_id) DO UPDATE SET
                     pairing_secret  = excluded.pairing_secret,
                     epoch           = excluded.epoch,
                     last_seen_unix  = excluded.last_seen_unix,
                     expires_at_unix = excluded.expires_at_unix,
                     revision        = excluded.revision",
                params![
                    pairing_id.to_string(),
                    pairing_secret.as_slice(),
                    epoch as i64,
                    last_seen_unix,
                    expires_at_unix,
                    revision as i64,
                ],
            )?;
        }

        PersistTask::DeletePairing { pairing_id } => {
            conn.execute(
                "DELETE FROM pairings WHERE pairing_id = ?1",
                params![pairing_id.to_string()],
            )?;
        }

        PersistTask::SaveBrowserSession {
            session_id,
            pairing_id,
            pairing_epoch,
            created_at_unix,
            last_seen_unix,
            expires_at_unix,
            revoked,
        } => {
            conn.execute(
                "INSERT INTO browser_sessions
                     (session_id, pairing_id, pairing_epoch, created_at_unix, last_seen_unix, expires_at_unix, revoked)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(session_id) DO UPDATE SET
                     pairing_epoch   = excluded.pairing_epoch,
                     last_seen_unix  = excluded.last_seen_unix,
                     expires_at_unix = excluded.expires_at_unix,
                     revoked         = excluded.revoked",
                params![
                    session_id.as_slice(),
                    pairing_id.to_string(),
                    pairing_epoch as i64,
                    created_at_unix,
                    last_seen_unix,
                    expires_at_unix,
                    revoked as i64,
                ],
            )?;
        }

        PersistTask::DeleteBrowserSessionsForPairing { pairing_id } => {
            conn.execute(
                "DELETE FROM browser_sessions WHERE pairing_id = ?1",
                params![pairing_id.to_string()],
            )?;
        }

        PersistTask::DeleteExpired { now_unix } => {
            conn.execute(
                "DELETE FROM browser_sessions WHERE expires_at_unix <= ?1",
                params![now_unix],
            )?;
            conn.execute(
                "DELETE FROM pairings WHERE expires_at_unix <= ?1",
                params![now_unix],
            )?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 时间转换工具函数
// ---------------------------------------------------------------------------

/// 获取当前 Unix 时间戳（秒）。
fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

/// 将 `Instant` 转换为 Unix 时间戳（秒）。
///
/// 通过 `SystemTime::now()` 和 `Instant::now()` 的差值进行换算。
pub fn instant_to_unix_secs(instant: Instant) -> i64 {
    let system_now = SystemTime::now();
    let instant_now = Instant::now();

    let unix_now = system_now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;

    // instant 与 now 的差值（可能为负，表示 instant 在未来）
    let delta_secs = match instant_now.checked_duration_since(instant) {
        Some(past) => -(past.as_secs() as i64),
        None => instant
            .checked_duration_since(instant_now)
            .map(|future| future.as_secs() as i64)
            .unwrap_or(0),
    };

    unix_now + delta_secs
}

/// 将 Unix 时间戳（秒）转换回 `Instant`。
///
/// 已过期的时间戳（小于当前时间）将被重建为过去的 `Instant`（可安全比较 `expires_at <= now`）。
pub fn unix_secs_to_instant(unix_secs: i64) -> Instant {
    let system_now = SystemTime::now();
    let instant_now = Instant::now();

    let unix_now = system_now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;

    if unix_secs >= unix_now {
        let future_secs = unix_secs.saturating_sub(unix_now) as u64;
        instant_now
            .checked_add(Duration::from_secs(future_secs))
            .unwrap_or(instant_now)
    } else {
        let past_secs = unix_now.saturating_sub(unix_secs) as u64;
        instant_now
            .checked_sub(Duration::from_secs(past_secs))
            .unwrap_or(instant_now)
    }
}
