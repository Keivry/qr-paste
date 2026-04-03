// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::{config::ServerConfig, session::SessionStore},
    dashmap::DashMap,
    rand::RngExt,
    regex::Regex,
    std::{fmt, sync::Arc, time::Instant},
    subtle::ConstantTimeEq,
    tokio::{sync::mpsc, task::JoinHandle, time::Duration},
    tokio_util::sync::CancellationToken,
    uuid::Uuid,
};

#[derive(Clone)]
pub struct WsHandle {
    pub connection_id: Uuid,
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
    pub pairing_id: Uuid,
    pub pairing_secret: [u8; 32],
    pub epoch: u64,
    pub online: bool,
    pub last_seen: Instant,
    pub expires_at: Instant,
    pub active_mobile_ws: Option<WsHandle>,
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
    pub session_id: [u8; 32],
    pub pairing_id: Uuid,
    pub pairing_epoch: u64,
    pub created_at: Instant,
    pub last_seen: Instant,
    pub expires_at: Instant,
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
    pub browser_session_id: [u8; 32],
    pub pairing_id: Uuid,
    pub pairing_epoch: u64,
    pub issued_at: Instant,
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

pub fn new_pairing_store() -> PairingStore { Arc::new(DashMap::new()) }

pub fn new_browser_session_store() -> BrowserSessionStore { Arc::new(DashMap::new()) }

pub fn new_ws_ticket_store() -> WsTicketStore { Arc::new(DashMap::new()) }

pub fn verify_secret(entry: &PairingEntry, provided_hex: &str) -> bool {
    if !secret_regex().is_match(provided_hex) {
        return false;
    }

    let Ok(decoded) = decode_hex_32(provided_hex) else {
        return false;
    };

    bool::from(entry.pairing_secret.ct_eq(&decoded))
}

pub fn constant_time_dummy_compare(provided_hex: &str) {
    let Ok(decoded) = decode_hex_32(provided_hex) else {
        return;
    };
    let _ = bool::from(decoded.ct_eq(&[0_u8; 32]));
}

pub fn secret_regex() -> &'static Regex {
    static REGEX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[0-9a-f]{64}$").expect("valid regex"))
}

pub fn random_bytes_32() -> anyhow::Result<[u8; 32]> {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes[..]);
    Ok(bytes)
}

pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

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
    let mut removed_pairings = Vec::new();

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
            removed_pairings.push(pairing_id);
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
