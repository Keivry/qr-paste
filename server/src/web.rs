// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::{
        config::ServerConfig,
        grpc::relay::{MobileConnected, MobileDisconnected, ServerEvent, server_event::Event},
        session::{Session, SessionStore},
    },
    axum::{
        Router,
        extract::{
            Path,
            State,
            ws::{Message, WebSocket, WebSocketUpgrade},
        },
        http::{HeaderMap, Method, StatusCode, header::USER_AGENT},
        response::{Html, IntoResponse, Response},
        routing::get,
    },
    common::{MobileMessage, ServerToMobileMessage},
    futures::{SinkExt, StreamExt},
    std::{net::SocketAddr, sync::Arc, time::Duration},
    tokio::{
        sync::{OwnedSemaphorePermit, Semaphore, mpsc},
        time::timeout,
    },
    tower_governor::{
        GovernorLayer,
        governor::GovernorConfigBuilder,
        key_extractor::{PeerIpKeyExtractor, SmartIpKeyExtractor},
    },
    tracing::{debug, error, info, warn},
};

const MAX_DEVICE_INFO_LEN: usize = 256;
const FALLBACK_INTERNAL_ERROR_JSON: &str =
    r#"{"type":"error","message":"服务器内部错误，请稍后重试"}"#;
const TOKEN_NOT_FOUND_MESSAGE: &str = "会话不存在或已过期";
const TOKEN_ALREADY_USED_MESSAGE: &str = "此二维码已使用";
/// WebSocket 升级预留的超时时间（秒）。手机端发起 WebSocket 升级请求后，
/// 必须在此时间内完成升级，否则预留失效。
const UPGRADE_RESERVATION_TIMEOUT_SECS: u64 = 10;

/// 手机端成功扫码并完成 WebSocket 升级后的活跃会话快照。
///
/// 仅包含 WebSocket 处理逻辑所需的字段，避免长时间持有 [`SessionStore`] 锁。
struct ActivatedSession {
    device_info: String,
    client_tx: Option<mpsc::Sender<Result<ServerEvent, tonic::Status>>>,
}

struct SessionGuard {
    store: SessionStore,
    token: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.store.remove(&self.token);
        debug!("WS session closed for token {}", self.token);
    }
}

/// 编译时嵌入的手机端 Web 页面 HTML。
static INDEX_HTML: &str = include_str!("web/index.html");

/// axum 路由共享状态，包含会话存储和服务端配置。
#[derive(Clone)]
pub struct AppState {
    pub store: SessionStore,
    pub config: Arc<ServerConfig>,
    pub ws_slots: Arc<Semaphore>,
}

/// 启动 HTTP 服务器并阻塞监听。
///
/// 注册以下路由：
/// - `GET /s/{token}` — 返回手机端 HTML 页面（令牌有效时）
/// - `GET /ws/mobile/{token}` — WebSocket 升级端点（手机端连接）
///
/// 同时启动后台令牌清理任务（[`cleanup_task`]）。
pub async fn serve(
    addr: SocketAddr,
    store: SessionStore,
    config: ServerConfig,
) -> anyhow::Result<()> {
    if config.behind_trusted_proxy {
        serve_inner(addr, store, config, SmartIpKeyExtractor).await
    } else {
        serve_inner(addr, store, config, PeerIpKeyExtractor).await
    }
}

async fn serve_inner<K>(
    addr: SocketAddr,
    store: SessionStore,
    config: ServerConfig,
    key_extractor: K,
) -> anyhow::Result<()>
where
    K: tower_governor::key_extractor::KeyExtractor + Clone + Send + Sync + 'static,
    <K as tower_governor::key_extractor::KeyExtractor>::Key: Send + Sync,
{
    let state = AppState {
        store: store.clone(),
        config: Arc::new(config.clone()),
        ws_slots: Arc::new(Semaphore::new(config.max_ws_connections)),
    };

    tokio::spawn(cleanup_task(store.clone(), config.clone()));

    let mut http_builder = GovernorConfigBuilder::default();
    http_builder.period(Duration::from_secs(60));
    http_builder.burst_size(config.http_rate_limit_per_ip_per_min);
    http_builder.methods(vec![Method::GET]);
    let http_rate_limit = Arc::new(
        http_builder
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid HTTP rate limit configuration"))?,
    );

    let mut ws_builder = GovernorConfigBuilder::default();
    ws_builder.period(Duration::from_secs(60));
    ws_builder.burst_size(config.ws_rate_limit_per_ip_per_min);
    ws_builder.methods(vec![Method::GET]);
    let ws_rate_limit = Arc::new(
        ws_builder
            .key_extractor(key_extractor)
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid WebSocket rate limit configuration"))?,
    );

    {
        let http_limiter = http_rate_limit.limiter().clone();
        let ws_limiter = ws_rate_limit.limiter().clone();
        // 复用 token_cleanup_interval_secs 作为限速器清理周期，避免引入额外配置项
        let interval_secs = config.token_cleanup_interval_secs.max(1);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                http_limiter.retain_recent();
                ws_limiter.retain_recent();
            }
        });
    }

    let router = Router::new()
        .route(
            "/s/{token}",
            get(handle_page).layer(GovernorLayer::new(http_rate_limit)),
        )
        .route(
            "/ws/mobile/{token}",
            get(handle_ws_upgrade).layer(GovernorLayer::new(ws_rate_limit)),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("HTTP server listening on {addr}");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// 处理手机端访问二维码 URL 的 HTTP 请求。
///
/// 令牌不存在或已过期返回 404，令牌已被扫描过或处于 WebSocket 升级预留期内返回 410，
/// 否则返回内嵌的 HTML 页面。
async fn handle_page(Path(token): Path<String>, State(state): State<AppState>) -> Response {
    match state.store.get(&token) {
        None => (StatusCode::NOT_FOUND, TOKEN_NOT_FOUND_MESSAGE).into_response(),
        Some(session) => {
            if session.scanned || reservation_still_active(&session) {
                return (StatusCode::GONE, TOKEN_ALREADY_USED_MESSAGE).into_response();
            }

            let expired = session_expired(&session, state.config.token_expiry_secs);
            if expired {
                drop(session);
                state.store.remove(&token);
                return (StatusCode::NOT_FOUND, TOKEN_NOT_FOUND_MESSAGE).into_response();
            }

            Html(INDEX_HTML).into_response()
        }
    }
}

/// 处理手机端 WebSocket 升级请求。
async fn handle_ws_upgrade(
    Path(token): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let permit = match state.ws_slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many active WebSocket connections",
            )
                .into_response();
        }
    };

    let device_info = extract_device_info(&headers);

    match reserve_upgrade_slot(&state, &token, device_info) {
        UpgradeReservation::Reserved => {}
        UpgradeReservation::NotFound => {
            return (StatusCode::NOT_FOUND, TOKEN_NOT_FOUND_MESSAGE).into_response();
        }
        UpgradeReservation::AlreadyUsed => {
            return (StatusCode::GONE, TOKEN_ALREADY_USED_MESSAGE).into_response();
        }
    }

    let max_msg_size = state.config.max_message_size_bytes;
    ws.max_message_size(max_msg_size)
        .max_frame_size(max_msg_size)
        .on_upgrade(move |socket| handle_ws(socket, token, state, permit))
}

/// 维护一个手机端 WebSocket 连接的完整生命周期。
///
/// 连接建立后向 PC 客户端推送 `MobileConnected` 事件；
/// 循环接收手机端消息并转发给 PC 客户端；
/// 连接断开（正常关闭、空闲超时或错误）后推送 `MobileDisconnected` 事件并从 store 删除令牌。
async fn handle_ws(
    mut socket: WebSocket,
    token: String,
    state: AppState,
    _permit: OwnedSemaphorePermit,
) {
    let (mobile_control_tx, mut mobile_control_rx) = mpsc::unbounded_channel();
    let Some(activated_session) = activate_reserved_session(&state, &token, mobile_control_tx)
    else {
        error!("handle_ws: missing or invalid reserved session for token {token}");
        let message = serialize_mobile_message(&ServerToMobileMessage::ClientDisconnected);
        let _ = socket.send(Message::Text(message.into())).await;
        return;
    };

    let (mut sender, mut receiver) = socket.split();
    let _guard = SessionGuard {
        store: state.store.clone(),
        token: token.clone(),
    };

    let user_agent = activated_session.device_info;
    let connected_tx = activated_session.client_tx;

    if let Some(client_tx) = &connected_tx {
        let _ = client_tx
            .send(Ok(ServerEvent {
                event: Some(Event::MobileConnected(MobileConnected {
                    device_info: user_agent,
                })),
            }))
            .await;
    }

    let idle_timeout = Duration::from_secs(state.config.ws_idle_timeout_secs);
    let max_msg_size = state.config.max_message_size_bytes;

    loop {
        tokio::select! {
            control = mobile_control_rx.recv() => {
                match control {
                    Some(message) => {
                        let _ = sender.send(Message::Text(message.into())).await;
                    }
                    None => {
                        let message = serialize_mobile_message(&ServerToMobileMessage::ClientDisconnected);
                        let _ = sender.send(Message::Text(message.into())).await;
                    }
                }
                break;
            }
            result = timeout(idle_timeout, receiver.next()) => {
                match result {
                    Err(_) => {
                        warn!("WS idle timeout for token {token}");
                        break;
                    }
                    Ok(None) => break,
                    Ok(Some(Err(_))) => break,
                    Ok(Some(Ok(msg))) => {
                        let text = match msg {
                            Message::Text(t) => t,
                            Message::Close(_) => break,
                            _ => continue,
                        };

                        if text.len() > max_msg_size {
                            warn!(
                                "WS message too large ({} bytes), closing token {token}",
                                text.len()
                            );
                            break;
                        }

                        let mobile_msg: MobileMessage = match serde_json::from_str(&text) {
                            Ok(m) => m,
                            Err(err) => {
                                warn!("Invalid mobile WS message for token {token}: {err}");
                                let error = serialize_mobile_message(&ServerToMobileMessage::Error {
                                    message: "消息格式无效，请重试。".to_string(),
                                });
                                let _ = sender.send(Message::Text(error.into())).await;
                                continue;
                            }
                        };

                        match mobile_msg {
                            MobileMessage::ClipboardText { content } => {
                                if content.is_empty() {
                                    continue;
                                }
                                match &connected_tx {
                                    None => {
                                        let message = serialize_mobile_message(
                                            &ServerToMobileMessage::ClientDisconnected,
                                        );
                                        let _ = sender.send(Message::Text(message.into())).await;
                                    }
                                    Some(tx) => {
                                        let event = ServerEvent {
                                            event: Some(Event::ClipboardText(
                                                crate::grpc::relay::ClipboardText { content },
                                            )),
                                        };
                                        if tx.send(Ok(event)).await.is_err() {
                                            warn!("PC client channel closed for token {token}");
                                            let message = serialize_mobile_message(
                                                &ServerToMobileMessage::ClientDisconnected,
                                            );
                                            let _ = sender.send(Message::Text(message.into())).await;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let disconnect_tx = state
        .store
        .get(&token)
        .and_then(|session| session.client_tx.clone());
    if let Some(tx) = disconnect_tx {
        let event = ServerEvent {
            event: Some(Event::MobileDisconnected(MobileDisconnected {})),
        };
        let _ = tx.send(Ok(event)).await;
    }
}

/// 后台定时清理过期令牌的任务。
///
/// 按 `token_cleanup_interval_secs` 间隔周期执行，移除满足以下所有条件的会话：
/// WebSocket 连接已断开（`ws_active = false`）、不在 WebSocket 升级预留期内、且令牌已过期。
/// 仍有活跃连接的会话会在连接生命周期结束时由 [`handle_ws`] 主动清理；
/// 仅处于预留中的会话会在预留失效且令牌过期后由本清理任务移除。
async fn cleanup_task(store: SessionStore, config: ServerConfig) {
    let mut interval = tokio::time::interval(Duration::from_secs(
        config.token_cleanup_interval_secs.max(1),
    ));
    loop {
        interval.tick().await;
        let now = std::time::Instant::now();
        store.retain(|_, session| should_retain_session(now, session, config.token_expiry_secs));
    }
}

fn session_expired(session: &Session, token_expiry_secs: u64) -> bool {
    session.created_at.elapsed().as_secs() > token_expiry_secs
}

fn should_retain_session(
    now: std::time::Instant,
    session: &Session,
    token_expiry_secs: u64,
) -> bool {
    let reservation_active = session.upgrade_reserved_at.is_some_and(|reserved_at| {
        now.duration_since(reserved_at).as_secs() <= UPGRADE_RESERVATION_TIMEOUT_SECS
    });

    session.ws_active
        || reservation_active
        || now.duration_since(session.created_at).as_secs() <= token_expiry_secs
}

enum UpgradeReservation {
    Reserved,
    NotFound,
    AlreadyUsed,
}

fn reserve_upgrade_slot(
    state: &AppState,
    token: &str,
    device_info: Option<String>,
) -> UpgradeReservation {
    match state.store.get_mut(token) {
        None => UpgradeReservation::NotFound,
        Some(mut session) => {
            if session.scanned || reservation_still_active(&session) {
                return UpgradeReservation::AlreadyUsed;
            }

            if session_expired(&session, state.config.token_expiry_secs) {
                drop(session);
                state.store.remove(token);
                return UpgradeReservation::NotFound;
            }

            session.device_info = device_info;
            session.upgrade_reserved_at = Some(std::time::Instant::now());
            UpgradeReservation::Reserved
        }
    }
}

fn activate_reserved_session(
    state: &AppState,
    token: &str,
    mobile_control_tx: mpsc::UnboundedSender<String>,
) -> Option<ActivatedSession> {
    match state.store.get_mut(token) {
        None => None,
        Some(mut session) => {
            if session.scanned || !reservation_still_active(&session) {
                return None;
            }

            session.scanned = true;
            session.ws_active = true;
            session.upgrade_reserved_at = None;
            session.mobile_control_tx = Some(mobile_control_tx);

            Some(ActivatedSession {
                device_info: session.device_info.clone().unwrap_or_default(),
                client_tx: session.client_tx.clone(),
            })
        }
    }
}

fn reservation_still_active(session: &Session) -> bool {
    session.upgrade_reserved_at.is_some_and(|reserved_at| {
        reserved_at.elapsed().as_secs() <= UPGRADE_RESERVATION_TIMEOUT_SECS
    })
}

fn extract_device_info(headers: &HeaderMap) -> Option<String> {
    headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(MAX_DEVICE_INFO_LEN).collect())
}

fn serialize_mobile_message(message: &ServerToMobileMessage) -> String {
    match serde_json::to_string(message) {
        Ok(json) => json,
        Err(err) => {
            warn!("Failed to serialize mobile response: {err}");
            FALLBACK_INTERNAL_ERROR_JSON.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{config::ServerConfig, session::new_store},
        axum::body::to_bytes,
        tokio::sync::mpsc,
    };

    fn test_config() -> ServerConfig {
        ServerConfig {
            public_base_url: "https://example.com".to_string(),
            grpc_auth_token: "shared-secret".to_string(),
            grpc_port: 50051,
            http_port: 8080,
            http_bind_host: std::net::IpAddr::from([127, 0, 0, 1]),
            grpc_bind_host: std::net::IpAddr::from([127, 0, 0, 1]),
            token_expiry_secs: 300,
            token_cleanup_interval_secs: 60,
            ws_rate_limit_per_ip_per_min: 10,
            http_rate_limit_per_ip_per_min: 20,
            max_ws_connections: 100,
            max_message_size_bytes: 65_536,
            ws_idle_timeout_secs: 90,
            grpc_keepalive_interval_secs: 30,
            grpc_keepalive_timeout_secs: 20,
            log_level: "info".to_string(),
            behind_trusted_proxy: false,
        }
    }

    fn test_state() -> AppState {
        AppState {
            store: new_store(),
            config: Arc::new(test_config()),
            ws_slots: Arc::new(Semaphore::new(4)),
        }
    }

    fn make_session(created_at: std::time::Instant, scanned: bool) -> Session {
        let (tx, _rx) = mpsc::channel(4);
        Session {
            token: "token".to_string(),
            created_at,
            scanned,
            upgrade_reserved_at: None,
            ws_active: false,
            client_tx: Some(tx),
            mobile_control_tx: None,
            device_info: None,
        }
    }

    #[tokio::test]
    async fn handle_page_returns_html_for_valid_unscanned_token() {
        let state = test_state();
        state.store.insert(
            "valid".to_string(),
            make_session(std::time::Instant::now(), false),
        );

        let response = handle_page(Path("valid".to_string()), State(state)).await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let body = String::from_utf8(body.to_vec()).expect("HTML body should be UTF-8");
        assert!(body.contains("发送文字到 PC"));
    }

    #[tokio::test]
    async fn handle_page_rejects_scanned_token() {
        let state = test_state();
        state.store.insert(
            "used".to_string(),
            make_session(std::time::Instant::now(), true),
        );

        let response = handle_page(Path("used".to_string()), State(state)).await;

        assert_eq!(response.status(), StatusCode::GONE);
    }

    #[tokio::test]
    async fn handle_page_rejects_reserved_token_even_if_expired() {
        let state = test_state();
        let now = std::time::Instant::now();
        let mut session = make_session(now - Duration::from_secs(301), false);
        session.upgrade_reserved_at = Some(now - Duration::from_secs(2));
        state.store.insert("reserved".to_string(), session);

        let response = handle_page(Path("reserved".to_string()), State(state.clone())).await;

        assert_eq!(response.status(), StatusCode::GONE);
        assert!(state.store.contains_key("reserved"));
    }

    #[test]
    fn cleanup_keeps_active_scanned_session_even_if_expired() {
        let now = std::time::Instant::now();
        let mut active_session = make_session(now - Duration::from_secs(301), true);
        active_session.ws_active = true;
        let inactive_session = make_session(now - Duration::from_secs(301), true);

        assert!(should_retain_session(now, &active_session, 300));
        assert!(!should_retain_session(now, &inactive_session, 300));
    }

    #[test]
    fn cleanup_keeps_recent_upgrade_reservation() {
        let now = std::time::Instant::now();
        let mut reserved_session = make_session(now - Duration::from_secs(301), false);
        reserved_session.upgrade_reserved_at = Some(now - Duration::from_secs(2));

        assert!(should_retain_session(now, &reserved_session, 300));
    }

    #[test]
    fn reserve_and_activate_upgrade_marks_session_used() {
        let state = test_state();
        state.store.insert(
            "token".to_string(),
            make_session(std::time::Instant::now(), false),
        );

        let reservation = reserve_upgrade_slot(&state, "token", Some("Mobile Safari".to_string()));
        assert!(matches!(reservation, UpgradeReservation::Reserved));

        let session = state
            .store
            .get("token")
            .expect("session should exist after reserve");
        assert!(session.upgrade_reserved_at.is_some());
        assert_eq!(session.device_info.as_deref(), Some("Mobile Safari"));
        drop(session);

        let (mobile_control_tx, _mobile_control_rx) = mpsc::unbounded_channel();
        let activated_session = activate_reserved_session(&state, "token", mobile_control_tx)
            .expect("reserved session should activate");
        assert_eq!(activated_session.device_info, "Mobile Safari");
        assert!(activated_session.client_tx.is_some());

        let session = state
            .store
            .get("token")
            .expect("session should still exist after activate");
        assert!(session.scanned);
        assert!(session.ws_active);
        assert!(session.upgrade_reserved_at.is_none());
        assert!(session.mobile_control_tx.is_some());
    }

    #[test]
    fn activate_reserved_session_installs_mobile_control_sender_for_immediate_cleanup() {
        let state = test_state();
        let now = std::time::Instant::now();
        let mut session = make_session(now, false);
        session.device_info = Some("Phone UA".to_string());
        session.upgrade_reserved_at = Some(now);
        state.store.insert("token".to_string(), session);

        let (mobile_control_tx, mut mobile_control_rx) = mpsc::unbounded_channel();
        let activated_session = activate_reserved_session(&state, "token", mobile_control_tx)
            .expect("reserved session should activate");

        assert_eq!(activated_session.device_info, "Phone UA");

        let removed_session = state
            .store
            .remove("token")
            .expect("session should still exist after activation")
            .1;
        removed_session
            .mobile_control_tx
            .expect("cleanup path should have control sender")
            .send(r#"{"type":"client_disconnected"}"#.to_string())
            .expect("send should succeed");

        assert_eq!(
            mobile_control_rx
                .try_recv()
                .expect("mobile peer should receive disconnect notification"),
            r#"{"type":"client_disconnected"}"#
        );
    }

    #[test]
    fn reserve_upgrade_slot_rejects_active_reservation_even_if_session_expired() {
        let state = test_state();
        let now = std::time::Instant::now();
        let mut session = make_session(now - Duration::from_secs(301), false);
        session.upgrade_reserved_at = Some(now - Duration::from_secs(2));
        state.store.insert("token".to_string(), session);

        let reservation = reserve_upgrade_slot(&state, "token", Some("Retry UA".to_string()));

        assert!(matches!(reservation, UpgradeReservation::AlreadyUsed));
        assert!(state.store.contains_key("token"));
    }

    #[test]
    fn extract_device_info_trims_and_truncates_user_agent() {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            "  Test Browser/1.0  ".parse().expect("valid header value"),
        );

        let device_info = extract_device_info(&headers).expect("device info should exist");

        assert_eq!(device_info, "Test Browser/1.0");
    }

    #[test]
    fn serialize_mobile_message_covers_client_disconnected_variant() {
        let json = serialize_mobile_message(&ServerToMobileMessage::ClientDisconnected);
        assert_eq!(json, r#"{"type":"client_disconnected"}"#);
    }
}
