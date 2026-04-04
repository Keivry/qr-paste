// SPDX-License-Identifier: MIT OR Apache-2.0

//! 配对状态管理：配对条目、浏览器会话、WebSocket 票据及其存储的创建、验证与清理。

use {
    crate::{config::ServerConfig, session::SessionStore},
    dashmap::DashMap,
    rand::RngExt,
    regex::Regex,
    std::{collections::HashSet, fmt, sync::Arc, time::Instant},
    subtle::ConstantTimeEq,
    tokio::{sync::mpsc, task::JoinHandle, time::Duration},
    tokio_util::sync::CancellationToken,
    uuid::Uuid,
};

#[derive(Clone)]
pub struct WsHandle {
    /// 本次 WebSocket 连接的唯一标识，用于区分同一配对的多次连接。
    pub connection_id: Uuid,
    /// 向 WebSocket 任务发送控制指令（如关闭）的无界通道发送端。
    pub control_tx: mpsc::UnboundedSender<WsControl>,
}

impl fmt::Debug for WsHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsHandle")
            .field("connection_id", &self.connection_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub enum WsControl {
    Close { code: u16, reason: &'static str },
}

pub struct PairingEntry {
    /// 配对唯一标识（UUID v4），也是 [`PairingStore`] 的 key。
    pub pairing_id: Uuid,
    /// 用于验证手机端身份的 32 字节随机密钥，通过 hex 编码后嵌入二维码 URL。
    pub pairing_secret: [u8; 32],
    /// 撤销纪元，每次 `POST /revoke` 后递增；手机端 BrowserSession/WsTicket 中记录的
    /// 旧纪元值会立即失效。
    pub epoch: u64,
    /// PC 客户端当前是否在线（持有活跃的 gRPC Subscribe 流）。
    pub online: bool,
    /// PC 客户端最近一次心跳或连接时刻，用于诊断和 TTL 续期。
    pub last_seen: Instant,
    /// 配对记录的过期时刻；离线且超过此时刻的记录会在清理任务中删除。
    pub expires_at: Instant,
    /// 当前活跃的手机端 WebSocket 连接句柄；`None` 表示手机端未连接。
    pub active_mobile_ws: Option<WsHandle>,
    /// 乐观锁版本号，在删除/修改时用于检测并发竞争。
    pub revision: u64,
}

impl fmt::Debug for PairingEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingEntry")
            .field("pairing_id", &self.pairing_id)
            .field("pairing_secret", &"[REDACTED]")
            .field("epoch", &self.epoch)
            .field("online", &self.online)
            .field("last_seen", &self.last_seen)
            .field("expires_at", &self.expires_at)
            .field("active_mobile_ws", &self.active_mobile_ws)
            .field("revision", &self.revision)
            .finish()
    }
}

pub struct BrowserSession {
    /// 32 字节随机会话标识，同时作为 [`BrowserSessionStore`] 的 key，通过 hex 编码存入 cookie。
    pub session_id: [u8; 32],
    /// 所属配对的唯一标识。
    pub pairing_id: Uuid,
    /// 创建时记录的配对纪元；纪元不匹配时视为已撤销。
    pub pairing_epoch: u64,
    /// 会话创建时刻。
    pub created_at: Instant,
    /// 最近一次请求时刻，用于诊断。
    pub last_seen: Instant,
    /// 会话过期时刻；超过后在清理任务中删除。
    pub expires_at: Instant,
    /// 是否已通过 `/revoke` 主动撤销。
    pub revoked: bool,
}

impl fmt::Debug for BrowserSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserSession")
            .field("session_id", &"[REDACTED]")
            .field("pairing_id", &self.pairing_id)
            .field("pairing_epoch", &self.pairing_epoch)
            .field("created_at", &self.created_at)
            .field("last_seen", &self.last_seen)
            .field("expires_at", &self.expires_at)
            .field("revoked", &self.revoked)
            .finish()
    }
}

pub struct WsTicket {
    /// 关联的浏览器会话标识，与 [`BrowserSession::session_id`] 对应。
    pub browser_session_id: [u8; 32],
    /// 所属配对的唯一标识。
    pub pairing_id: Uuid,
    /// 签发时记录的配对纪元；纪元不匹配时票据立即失效。
    pub pairing_epoch: u64,
    /// 票据签发时刻。
    pub issued_at: Instant,
    /// 票据过期时刻（通常为签发后 15 秒）。
    pub expires_at: Instant,
}

impl fmt::Debug for WsTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsTicket")
            .field("ticket_id", &"[REDACTED]")
            .field("browser_session_id", &"[REDACTED]")
            .field("pairing_id", &self.pairing_id)
            .field("pairing_epoch", &self.pairing_epoch)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

pub type PairingStore = Arc<DashMap<Uuid, PairingEntry>>;
pub type BrowserSessionStore = Arc<DashMap<[u8; 32], BrowserSession>>;
pub type WsTicketStore = Arc<DashMap<[u8; 32], WsTicket>>;

/// 创建一个空的配对条目存储。
pub fn new_pairing_store() -> PairingStore { Arc::new(DashMap::new()) }

/// 创建一个空的浏览器会话存储。
pub fn new_browser_session_store() -> BrowserSessionStore { Arc::new(DashMap::new()) }

/// 创建一个空的 WebSocket 票据存储。
pub fn new_ws_ticket_store() -> WsTicketStore { Arc::new(DashMap::new()) }

/// 使用常量时间比较验证配对密钥，防止时序攻击。
///
/// `provided_hex` 必须是 64 个小写十六进制字符，否则直接返回 `false`。
pub fn verify_secret(entry: &PairingEntry, provided_hex: &str) -> bool {
    if !secret_regex().is_match(provided_hex) {
        return false;
    }

    let Ok(decoded) = decode_hex_32(provided_hex) else {
        return false;
    };

    bool::from(entry.pairing_secret.ct_eq(&decoded))
}

/// 在配对 ID 不存在时执行一次无意义的常量时间比较，使响应时间与"ID 存在但密钥错误"
/// 的情况保持一致，避免通过时序差异枚举有效配对 ID。
pub fn constant_time_dummy_compare(provided_hex: &str) {
    let Ok(decoded) = decode_hex_32(provided_hex) else {
        return;
    };
    let _ = bool::from(decoded.ct_eq(&[0_u8; 32]));
}

/// 返回用于验证 64 字符小写十六进制密钥格式的编译后正则表达式（惰性初始化单例）。
pub fn secret_regex() -> &'static Regex {
    static REGEX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[0-9a-f]{64}$").expect("valid regex"))
}

/// 生成 32 个加密安全的随机字节。
pub fn random_bytes_32() -> anyhow::Result<[u8; 32]> {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes[..]);
    Ok(bytes)
}

/// 将字节切片编码为小写十六进制字符串，输出长度为 `bytes.len() * 2`。
pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// 将 64 字符小写十六进制字符串解码为 32 字节数组。
///
/// 输入长度不为 64 或包含非十六进制字符时返回 `Err(())`。
pub fn decode_hex_32(value: &str) -> Result<[u8; 32], ()> {
    if value.len() != 64 {
        return Err(());
    }

    let mut output = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (decode_hex_nibble(chunk[0])? << 4) | decode_hex_nibble(chunk[1])?;
    }
    Ok(output)
}

fn decode_hex_nibble(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(()),
    }
}

/// 启动后台清理任务，按 `config.token_cleanup_interval_secs` 周期性删除：
/// 已过期的会话令牌、离线且过期的配对条目，以及对应的浏览器会话和 WS 票据。
///
/// 收到 `cancellation` 信号后退出。
pub fn spawn_cleanup_task(
    config: Arc<ServerConfig>,
    session_store: SessionStore,
    pairing_store: PairingStore,
    browser_session_store: BrowserSessionStore,
    ws_ticket_store: WsTicketStore,
    cancellation: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(
            config.token_cleanup_interval_secs.max(1),
        ));
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = interval.tick() => {
                    cleanup_old_sessions(&session_store, config.token_expiry_secs);
                    cleanup_pairings(
                        &pairing_store,
                        &browser_session_store,
                        &ws_ticket_store,
                    );
                }
            }
        }
    })
}

fn cleanup_old_sessions(session_store: &SessionStore, token_expiry_secs: u64) {
    let now = Instant::now();
    session_store.retain(|_, session| {
        let reservation_active = session
            .upgrade_reserved_at
            .is_some_and(|reserved_at| now.duration_since(reserved_at).as_secs() <= 10);
        session.ws_active
            || session.client_tx.is_some()
            || reservation_active
            || now.duration_since(session.created_at).as_secs() <= token_expiry_secs
    });
}

fn cleanup_pairings(
    pairing_store: &PairingStore,
    browser_session_store: &BrowserSessionStore,
    ws_ticket_store: &WsTicketStore,
) {
    let now = Instant::now();
    let mut removed_pairings = HashSet::new();

    let observed: Vec<(Uuid, u64)> = pairing_store
        .iter()
        .filter(|entry| {
            entry.expires_at <= now && !entry.online && entry.active_mobile_ws.is_none()
        })
        .map(|entry| (*entry.key(), entry.revision))
        .collect();

    for (pairing_id, revision) in observed {
        if pairing_store
            .remove_if(&pairing_id, |_, value| {
                value.revision == revision
                    && value.expires_at <= now
                    && !value.online
                    && value.active_mobile_ws.is_none()
            })
            .is_some()
        {
            removed_pairings.insert(pairing_id);
        }
    }

    if !removed_pairings.is_empty() {
        browser_session_store.retain(|_, session| !removed_pairings.contains(&session.pairing_id));
        ws_ticket_store.retain(|_, ticket| !removed_pairings.contains(&ticket.pairing_id));
    }

    browser_session_store.retain(|_, session| {
        session.expires_at > now && pairing_store.contains_key(&session.pairing_id)
    });
    ws_ticket_store.retain(|_, ticket| {
        ticket.expires_at > now && pairing_store.contains_key(&ticket.pairing_id)
    });
}

#[cfg(test)]
mod tests {
    use {
        super::{
            PairingEntry,
            cleanup_old_sessions,
            cleanup_pairings,
            new_browser_session_store,
            new_pairing_store,
            new_ws_ticket_store,
        },
        crate::session::{Session, new_store},
        std::time::{Duration, Instant},
        tokio::sync::mpsc,
        uuid::Uuid,
    };

    #[test]
    fn cleanup_old_sessions_keeps_active_websocket_sessions_past_ttl() {
        let store = new_store();
        store.insert(
            "active".to_string(),
            Session {
                token: "active".to_string(),
                created_at: Instant::now() - Duration::from_secs(600),
                scanned: true,
                upgrade_reserved_at: None,
                ws_active: true,
                client_tx: None,
                mobile_control_tx: None,
                device_info: None,
                pairing_id: None,
                grpc_session_token: "grpc-token".to_string(),
            },
        );

        cleanup_old_sessions(&store, 300);

        assert!(store.contains_key("active"));
    }

    #[test]
    fn cleanup_old_sessions_keeps_active_grpc_sessions_past_ttl() {
        let store = new_store();
        let (client_tx, _client_rx) = mpsc::channel(4);
        store.insert(
            "grpc-active".to_string(),
            Session {
                token: "grpc-active".to_string(),
                created_at: Instant::now() - Duration::from_secs(600),
                scanned: true,
                upgrade_reserved_at: None,
                ws_active: false,
                client_tx: Some(client_tx),
                mobile_control_tx: None,
                device_info: None,
                pairing_id: None,
                grpc_session_token: "grpc-token".to_string(),
            },
        );

        cleanup_old_sessions(&store, 300);

        assert!(store.contains_key("grpc-active"));
    }

    #[test]
    fn cleanup_pairings_keeps_online_pairings_past_ttl() {
        let pairing_store = new_pairing_store();
        let browser_session_store = new_browser_session_store();
        let ws_ticket_store = new_ws_ticket_store();
        let pairing_id = Uuid::new_v4();
        pairing_store.insert(
            pairing_id,
            PairingEntry {
                pairing_id,
                pairing_secret: [1_u8; 32],
                epoch: 0,
                online: true,
                last_seen: Instant::now() - Duration::from_secs(600),
                expires_at: Instant::now() - Duration::from_secs(10),
                active_mobile_ws: None,
                revision: 0,
            },
        );

        cleanup_pairings(&pairing_store, &browser_session_store, &ws_ticket_store);

        assert!(pairing_store.contains_key(&pairing_id));
    }
}
