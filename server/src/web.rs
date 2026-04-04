// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::{
        config::{ServerConfig, TrustedProxyCidrs},
        grpc::relay::{
            ClipboardText,
            MobileConnected,
            MobileDisconnected,
            ServerEvent,
            SessionToken,
            server_event::Event,
        },
        pairing::{
            BrowserSession,
            BrowserSessionStore,
            PairingStore,
            WsControl,
            WsHandle,
            WsTicket,
            WsTicketStore,
            constant_time_dummy_compare,
            decode_hex_32,
            encode_hex,
            random_bytes_32,
            verify_secret,
        },
        session::SessionStore,
    },
    axum::{
        Json,
        Router,
        body::Bytes,
        extract::{
            ConnectInfo,
            Path,
            State,
            ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        },
        http::{
            HeaderMap,
            HeaderValue,
            Method,
            StatusCode,
            header::{
                AUTHORIZATION,
                CACHE_CONTROL,
                CONTENT_SECURITY_POLICY,
                COOKIE,
                ORIGIN,
                REFERRER_POLICY,
                SEC_WEBSOCKET_PROTOCOL,
                SET_COOKIE,
                USER_AGENT,
                X_CONTENT_TYPE_OPTIONS,
                X_FRAME_OPTIONS,
            },
        },
        response::{Html, IntoResponse, Response},
        routing::{get, post},
    },
    base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD},
    common::{MobileMessage, ServerToMobileMessage},
    futures::{SinkExt, StreamExt},
    governor::{
        Quota,
        RateLimiter,
        clock::DefaultClock,
        state::{InMemoryState, NotKeyed, keyed::DefaultKeyedStateStore},
    },
    serde::Deserialize,
    serde_json::json,
    std::{
        collections::HashSet,
        hash::Hash,
        net::{IpAddr, SocketAddr},
        num::NonZeroU32,
        sync::Arc,
        time::{Duration, Instant},
    },
    subtle::ConstantTimeEq,
    tokio::{
        sync::{OwnedSemaphorePermit, Semaphore, mpsc},
        time::timeout,
    },
    tower_governor::{
        GovernorLayer,
        errors::GovernorError,
        governor::GovernorConfigBuilder,
        key_extractor::KeyExtractor,
    },
    tracing::{info, warn},
    uuid::Uuid,
};

const MAX_DEVICE_INFO_LEN: usize = 256;
const FALLBACK_INTERNAL_ERROR_JSON: &str =
    r#"{"type":"error","message":"服务器内部错误，请稍后重试"}"#;
const BROWSER_SESSION_COOKIE: &str = "__Host-qr_paste_browser_session";
const LEGACY_BROWSER_SESSION_COOKIE: &str = "qr_paste_browser_session";

type KeyedLimiter<K> = Arc<RateLimiter<K, DefaultKeyedStateStore<K>, DefaultClock>>;

static MOBILE_HTML: &str = include_str!("web/index.html");

#[derive(Clone)]
pub struct AppState {
    pub store: SessionStore,
    pub pairing_store: PairingStore,
    pub browser_session_store: BrowserSessionStore,
    pub ws_ticket_store: WsTicketStore,
    pub config: Arc<ServerConfig>,
    pub public_origin: Arc<String>,
    pub ws_slots: Arc<Semaphore>,
    pub status_limiter: KeyedLimiter<[u8; 32]>,
    pub ws_ticket_limiter: KeyedLimiter<[u8; 32]>,
    pub revoke_pairing_limiter: KeyedLimiter<Uuid>,
    pub revoke_session_limiter: KeyedLimiter<String>,
}

#[derive(Deserialize)]
struct BootstrapBody {
    pairing_secret: String,
}

struct BrowserAuth {
    session_id: [u8; 32],
}

#[derive(Clone)]
struct TrustedClientIpKeyExtractor {
    trusted_proxies: TrustedProxyCidrs,
}

impl TrustedClientIpKeyExtractor {
    fn new(trusted_proxies: TrustedProxyCidrs) -> Self { Self { trusted_proxies } }
}

impl KeyExtractor for TrustedClientIpKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &axum::http::Request<T>) -> Result<Self::Key, GovernorError> {
        let peer_ip = request_peer_ip(req).ok_or(GovernorError::UnableToExtractKey)?;
        Ok(self.trusted_proxies.resolve_client_ip(
            peer_ip,
            header_value(req.headers(), "x-forwarded-for"),
            header_value(req.headers(), "x-real-ip"),
        ))
    }
}

pub async fn serve(
    addr: SocketAddr,
    store: SessionStore,
    pairing_store: PairingStore,
    browser_session_store: BrowserSessionStore,
    ws_ticket_store: WsTicketStore,
    config: ServerConfig,
) -> anyhow::Result<()> {
    let trusted_proxies = config.trusted_proxy_ranges()?;
    serve_inner(
        addr,
        store,
        pairing_store,
        browser_session_store,
        ws_ticket_store,
        config,
        TrustedClientIpKeyExtractor::new(trusted_proxies),
    )
    .await
}

async fn serve_inner<K>(
    addr: SocketAddr,
    store: SessionStore,
    pairing_store: PairingStore,
    browser_session_store: BrowserSessionStore,
    ws_ticket_store: WsTicketStore,
    config: ServerConfig,
    key_extractor: K,
) -> anyhow::Result<()>
where
    K: tower_governor::key_extractor::KeyExtractor + Clone + Send + Sync + 'static,
    <K as tower_governor::key_extractor::KeyExtractor>::Key: Send + Sync,
{
    let state = AppState {
        store,
        pairing_store,
        browser_session_store,
        ws_ticket_store,
        config: Arc::new(config.clone()),
        public_origin: Arc::new(config.normalized_public_origin()?),
        ws_slots: Arc::new(Semaphore::new(config.max_ws_connections)),
        status_limiter: keyed_limiter(30),
        ws_ticket_limiter: keyed_limiter(12),
        revoke_pairing_limiter: keyed_limiter(5),
        revoke_session_limiter: keyed_limiter(10),
    };

    let http_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(20)
            .methods(vec![Method::GET])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid http governor config"))?,
    );
    let bootstrap_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(5)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid bootstrap governor config"))?,
    );
    let status_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(10)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid status governor config"))?,
    );
    let ws_ticket_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(10)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid ws_ticket governor config"))?,
    );
    let revoke_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(20)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid revoke governor config"))?,
    );
    let ws_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(10)
            .methods(vec![Method::GET])
            .key_extractor(key_extractor)
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid ws governor config"))?,
    );

    let router = Router::new()
        .route(
            "/m/{pairing_id}",
            get(handle_mobile_page).layer(GovernorLayer::new(http_limit.clone())),
        )
        .route(
            "/api/pairing/{pairing_id}",
            get(handle_deprecated_pairing_get).layer(GovernorLayer::new(http_limit)),
        )
        .route(
            "/api/pairing/{pairing_id}/bootstrap",
            post(handle_bootstrap).layer(GovernorLayer::new(bootstrap_limit)),
        )
        .route(
            "/api/pairing/{pairing_id}/status",
            post(handle_status).layer(GovernorLayer::new(status_limit.clone())),
        )
        .route(
            "/api/pairing/{pairing_id}/ws-ticket",
            post(handle_ws_ticket).layer(GovernorLayer::new(ws_ticket_limit)),
        )
        .route(
            "/api/pairing/{pairing_id}/revoke",
            post(handle_revoke).layer(GovernorLayer::new(revoke_limit)),
        )
        .route(
            "/ws/mobile/{id}",
            get(handle_ws_upgrade).layer(GovernorLayer::new(ws_limit)),
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

fn keyed_limiter<K>(limit_per_minute: u32) -> KeyedLimiter<K>
where
    K: Clone + Eq + Hash,
{
    let burst = NonZeroU32::new(limit_per_minute.max(1)).expect("non-zero limit");
    Arc::new(RateLimiter::keyed(Quota::per_minute(burst)))
}

fn request_peer_ip<T>(req: &axum::http::Request<T>) -> Option<IpAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|addr| addr.0.ip())
        .or_else(|| req.extensions().get::<SocketAddr>().map(|addr| addr.ip()))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// 提供手机端配对页面（`GET /m/:pairing_id`）。
///
/// 验证 pairing_id 存在后渲染内嵌 HTML，注入随机 nonce 并设置严格的 CSP 头。
async fn handle_mobile_page(
    Path(pairing_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let Ok(pairing_id) = Uuid::parse_str(&pairing_id) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_pairing_id");
    };
    if !state.pairing_store.contains_key(&pairing_id) {
        return error_json(StatusCode::NOT_FOUND, "pairing_not_found");
    }

    let nonce = match random_bytes_32() {
        Ok(bytes) => URL_SAFE_NO_PAD.encode(bytes),
        Err(_) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };
    let html = MOBILE_HTML
        .replace("__PAIRING_ID__", &pairing_id.to_string())
        .replace("__NONCE__", &nonce);
    let mut response = Html(html).into_response();
    apply_common_headers(response.headers_mut());
    let Ok(csp_value) = HeaderValue::from_str(&format!(
        "default-src 'self'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; worker-src 'none'"
    )) else {
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    };
    response
        .headers_mut()
        .insert(CONTENT_SECURITY_POLICY, csp_value);
    response
        .headers_mut()
        .insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// 旧版 `GET /pair/:id` 端点已废弃，固定返回 `410 Gone`。
async fn handle_deprecated_pairing_get() -> Response { error_json(StatusCode::GONE, "gone") }

/// 处理首次配对认证（`POST /pair/:pairing_id/bootstrap`）。
///
/// 手机端提交 pairing_secret 后，创建浏览器会话并在响应中写入 session cookie。
async fn handle_bootstrap(
    Path(pairing_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(status) = require_browser_origin(&state, &headers) {
        return error_json(status, "forbidden");
    }
    if body.len() > 1024 {
        return error_json(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }

    let Ok(pairing_id) = Uuid::parse_str(&pairing_id) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_pairing_id");
    };
    let Ok(payload) = serde_json::from_slice::<BootstrapBody>(&body) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_format");
    };

    if !payload.pairing_secret.as_bytes().iter().all(|byte| {
        byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte.is_ascii_hexdigit())
    }) || payload.pairing_secret.len() != 64
    {
        return error_json(StatusCode::BAD_REQUEST, "invalid_format");
    }

    let now = Instant::now();
    let (session_id, epoch, online, notify_url) = {
        let Some(mut entry) = state.pairing_store.get_mut(&pairing_id) else {
            constant_time_dummy_compare(&payload.pairing_secret);
            return error_json(StatusCode::NOT_FOUND, "pairing_not_found_or_invalid");
        };

        if !verify_secret(&entry, &payload.pairing_secret) {
            return error_json(StatusCode::NOT_FOUND, "pairing_not_found_or_invalid");
        }

        state
            .browser_session_store
            .retain(|_, session| session.pairing_id != pairing_id);
        state
            .ws_ticket_store
            .retain(|_, ticket| ticket.pairing_id != pairing_id);
        if let Some(handle) = entry.active_mobile_ws.take() {
            let _ = handle.control_tx.send(WsControl::Close {
                code: 4003,
                reason: "superseded",
            });
        }

        let session_id = match random_bytes_32() {
            Ok(value) => value,
            Err(_) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        let new_secret = match random_bytes_32() {
            Ok(value) => value,
            Err(_) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };

        let browser_session = BrowserSession {
            session_id,
            pairing_id,
            pairing_epoch: entry.epoch,
            created_at: now,
            last_seen: now,
            expires_at: now + Duration::from_secs(30 * 24 * 60 * 60),
            revoked: false,
        };
        state
            .browser_session_store
            .insert(session_id, browser_session);

        entry.pairing_secret = new_secret;
        entry.last_seen = now;
        entry.expires_at = now + Duration::from_secs(state.config.pairing_ttl_secs);
        entry.revision = entry.revision.saturating_add(1);

        (
            session_id,
            entry.epoch,
            entry.online,
            format!(
                "{}/m/{}#ps={}",
                state.config.public_base_url.trim_end_matches('/'),
                pairing_id,
                encode_hex(&new_secret)
            ),
        )
    };

    if let Some(client_tx) = current_client_tx(&state.store, pairing_id) {
        let _ = client_tx
            .send(Ok(ServerEvent {
                event: Some(Event::SessionToken(SessionToken {
                    token: current_session_token(&state.store, pairing_id).unwrap_or_default(),
                    url: notify_url,
                })),
                grpc_session_token: String::new(),
            }))
            .await;
    }

    let mut response = Json(json!({
        "online": online,
        "pairing_epoch": epoch,
    }))
    .into_response();
    apply_common_headers(response.headers_mut());
    let Ok(cookie_value) = HeaderValue::from_str(&format!(
        "{BROWSER_SESSION_COOKIE}={}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=2592000",
        encode_hex(&session_id)
    )) else {
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    };
    response
        .headers_mut()
        .append(SET_COOKIE, cookie_value);
    response
}

/// 返回当前配对及 PC 客户端的在线状态（`POST /pair/:pairing_id/status`）。
///
/// 需要有效的浏览器会话 cookie；PC 不在线时响应中包含轮询建议间隔。
async fn handle_status(
    Path(pairing_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(status) = require_browser_origin(&state, &headers) {
        return error_json(status, "forbidden");
    }
    if body.len() > 256 {
        return error_json(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    let Ok(pairing_id) = Uuid::parse_str(&pairing_id) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_pairing_id");
    };
    let auth = match authenticate_browser_session(&state, pairing_id, &headers) {
        Ok(auth) => auth,
        Err(StatusCode::UNAUTHORIZED) => return reauth_required_response(),
        Err(StatusCode::NOT_FOUND) => return pairing_not_found_response(),
        Err(other) => return error_json(other, "unauthorized"),
    };
    if state.status_limiter.check_key(&auth.session_id).is_err() {
        return error_json(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }

    let Some(entry) = state.pairing_store.get(&pairing_id) else {
        return pairing_not_found_response();
    };
    let online = entry.online;
    let epoch = entry.epoch;
    drop(entry);

    if let Some(mut session) = state.browser_session_store.get_mut(&auth.session_id) {
        session.last_seen = Instant::now();
    }

    let mut response = Json(json!({
        "online": online,
        "pairing_epoch": epoch,
        "retry_after_ms": 1000,
    }))
    .into_response();
    apply_common_headers(response.headers_mut());
    response
}

/// 签发短效 WebSocket 票据（`POST /pair/:pairing_id/ws-ticket`），有效期 15 秒。
///
/// 票据通过 `Sec-WebSocket-Protocol` 头在握手时一次性使用，之后立即失效。
async fn handle_ws_ticket(
    Path(pairing_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(status) = require_browser_origin(&state, &headers) {
        return error_json(status, "forbidden");
    }
    if body.len() > 256 {
        return error_json(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    let Ok(pairing_id) = Uuid::parse_str(&pairing_id) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_pairing_id");
    };
    let auth = match authenticate_browser_session(&state, pairing_id, &headers) {
        Ok(auth) => auth,
        Err(StatusCode::UNAUTHORIZED) => return reauth_required_response(),
        Err(StatusCode::NOT_FOUND) => return pairing_not_found_response(),
        Err(other) => return error_json(other, "unauthorized"),
    };
    if state.ws_ticket_limiter.check_key(&auth.session_id).is_err() {
        return error_json(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }

    let Some(entry) = state.pairing_store.get(&pairing_id) else {
        return pairing_not_found_response();
    };
    let epoch = entry.epoch;
    drop(entry);

    let ticket_id = match random_bytes_32() {
        Ok(value) => value,
        Err(_) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };
    let now = Instant::now();
    state.ws_ticket_store.insert(
        ticket_id,
        WsTicket {
            browser_session_id: auth.session_id,
            pairing_id,
            pairing_epoch: epoch,
            issued_at: now,
            expires_at: now + Duration::from_secs(15),
        },
    );

    let mut response = Json(json!({
        "ws_ticket": URL_SAFE_NO_PAD.encode(ticket_id),
        "expires_in_ms": 15000,
    }))
    .into_response();
    apply_common_headers(response.headers_mut());
    response
}

/// 撤销指定配对的所有浏览器会话（`POST /pair/:pairing_id/revoke`）。
///
/// 需要 PC 客户端的 gRPC Bearer 令牌认证；通过递增 epoch 使现有会话 cookie 全部失效。
async fn handle_revoke(
    Path(pairing_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > 256 {
        return error_json(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    let Ok(pairing_id) = Uuid::parse_str(&pairing_id) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_pairing_id");
    };

    let Some(grpc_token) = bearer_token(&headers) else {
        return error_json(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    if state.revoke_pairing_limiter.check_key(&pairing_id).is_err()
        || state.revoke_session_limiter.check_key(&grpc_token).is_err()
    {
        return error_json(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }

    let Some(session_token) = grpc_session_token(&state.store, &grpc_token) else {
        return error_json(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let Some(session) = state.store.get(&session_token) else {
        return error_json(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    if session.pairing_id != Some(pairing_id) {
        return error_json(StatusCode::FORBIDDEN, "forbidden");
    }
    let client_tx = session.client_tx.clone();
    drop(session);

    let now = Instant::now();
    let (epoch, secret) = {
        let Some(mut entry) = state.pairing_store.get_mut(&pairing_id) else {
            return error_json(StatusCode::NOT_FOUND, "pairing_not_found");
        };
        let new_secret = match random_bytes_32() {
            Ok(value) => value,
            Err(_) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        entry.pairing_secret = new_secret;
        entry.epoch = entry.epoch.saturating_add(1);
        entry.last_seen = now;
        entry.expires_at = now + Duration::from_secs(state.config.pairing_ttl_secs);
        entry.revision = entry.revision.saturating_add(1);
        if let Some(handle) = entry.active_mobile_ws.take() {
            let _ = handle.control_tx.send(WsControl::Close {
                code: 4002,
                reason: "revoked",
            });
        }
        (entry.epoch, new_secret)
    };

    if let Some(client_tx) = client_tx {
        let _ = client_tx
            .send(Ok(pairing_session_token_event(
                session_token,
                format!(
                    "{}/m/{}#ps={}",
                    state.config.public_base_url.trim_end_matches('/'),
                    pairing_id,
                    encode_hex(&secret)
                ),
            )))
            .await;
    }

    let mut response = Json(json!({ "pairing_epoch": epoch })).into_response();
    apply_common_headers(response.headers_mut());
    response
}

/// 将 HTTP 连接升级为 WebSocket（`GET /ws/:id`）。
///
/// 根据是否存在 `Sec-WebSocket-Protocol` 票据头，分别走手机端配对通道或
/// 遗留会话通道；升级后进入 [`handle_pairing_ws`] 消息循环。
async fn handle_ws_upgrade(
    Path(id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let max_msg_size = state.config.max_message_size_bytes;
    let device_info = extract_device_info(&headers);
    if let Some(protocol_header) = headers.get(SEC_WEBSOCKET_PROTOCOL)
        && let Ok(pairing_id) = Uuid::parse_str(&id)
    {
        if let Err(status) = require_browser_origin(&state, &headers) {
            return error_json(status, "forbidden");
        }
        let ticket_id = match parse_ws_protocol_ticket(protocol_header) {
            Ok(ticket_id) => ticket_id,
            Err(status) => return error_json(status, "bad_request"),
        };
        let now = Instant::now();
        let Some((_, ticket)) = state
            .ws_ticket_store
            .remove_if(&ticket_id, |_, ticket| ticket.expires_at > now)
        else {
            return unauthorized_ws_response();
        };

        if ticket.pairing_id != pairing_id {
            return unauthorized_ws_response();
        }
        let Some(browser_session) = state.browser_session_store.get(&ticket.browser_session_id)
        else {
            return unauthorized_ws_response();
        };
        if browser_session.revoked || browser_session.pairing_id != pairing_id {
            return unauthorized_ws_response();
        }
        let browser_session_id = browser_session.session_id;
        let browser_epoch = browser_session.pairing_epoch;
        drop(browser_session);

        let Some(entry) = state.pairing_store.get(&pairing_id) else {
            return unauthorized_ws_response();
        };
        if browser_epoch != entry.epoch || ticket.pairing_epoch != entry.epoch {
            return unauthorized_ws_response();
        }
        drop(entry);

        // Ticket校验通过后再竞争 semaphore，避免未授权请求消耗连接槽
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

        return ws
            .protocols(["qr-paste.v1"])
            .max_message_size(max_msg_size)
            .max_frame_size(max_msg_size)
            .on_upgrade(move |socket| {
                handle_pairing_ws(
                    socket,
                    pairing_id,
                    browser_session_id,
                    state,
                    permit,
                    device_info,
                )
            });
    }

    error_json(
        StatusCode::BAD_REQUEST,
        "missing_ws_protocol_or_invalid_pairing_id",
    )
}

async fn handle_pairing_ws(
    socket: WebSocket,
    pairing_id: Uuid,
    browser_session_id: [u8; 32],
    state: AppState,
    _permit: OwnedSemaphorePermit,
    device_info: Option<String>,
) {
    let connection_id = Uuid::new_v4();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let (mobile_control_tx, mut mobile_control_rx) = mpsc::unbounded_channel();
    let handle = WsHandle {
        connection_id,
        control_tx,
    };

    {
        let Some(mut entry) = state.pairing_store.get_mut(&pairing_id) else {
            return;
        };
        if let Some(old) = entry.active_mobile_ws.replace(handle.clone()) {
            let _ = old.control_tx.send(WsControl::Close {
                code: 4001,
                reason: "replaced",
            });
        }
    }

    let (mut sender, mut receiver) = socket.split();
    let session_token = mark_mobile_ws_connected(&state.store, pairing_id, &mobile_control_tx);
    if let Some(client_tx) = current_client_tx(&state.store, pairing_id) {
        let _ = client_tx
            .send(Ok(ServerEvent {
                event: Some(Event::MobileConnected(MobileConnected {
                    device_info: device_info.unwrap_or_default(),
                })),
                grpc_session_token: String::new(),
            }))
            .await;
    }

    // C5-03: per-connection Ping rate limit — 1 Ping / 30s, burst 2.
    // First excess: silently ignore; sustained excess: close(1008).
    let ping_limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock> = RateLimiter::direct(
        Quota::with_period(Duration::from_secs(30))
            .expect("non-zero period")
            .allow_burst(NonZeroU32::new(2).expect("non-zero burst")),
    );
    let mut ping_rate_exceeded = false;

    let idle_timeout = Duration::from_secs(state.config.ws_idle_timeout_secs);
    loop {
        tokio::select! {
            control = control_rx.recv() => {
                if let Some(WsControl::Close { code, reason }) = control {
                    let _ = sender.send(Message::Close(Some(CloseFrame {
                        code,
                        reason: reason.into(),
                    }))).await;
                }
                break;
            }
            control_message = mobile_control_rx.recv() => {
                let Some(control_message) = control_message else {
                    break;
                };
                if sender.send(Message::Text(control_message.into())).await.is_err() {
                    break;
                }
            }
            result = timeout(idle_timeout, receiver.next()) => {
                match result {
                    Err(_) => break,
                    Ok(None) => break,
                    Ok(Some(Err(_))) => break,
                    Ok(Some(Ok(msg))) => {
                        let text = match msg {
                            Message::Text(text) => text,
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        if text.len() > state.config.max_message_size_bytes {
                            break;
                        }

                        if let Some(code) = validate_pairing_ws(&state, pairing_id, browser_session_id, connection_id) {
                            let reason = match code {
                                4001 => "replaced",
                                4002 => "revoked",
                                4003 => "session_revoked",
                                _ => "superseded",
                            };
                            let _ = sender.send(Message::Close(Some(CloseFrame {
                                code,
                                reason: reason.into(),
                            }))).await;
                            break;
                        }

                        let mobile_msg: MobileMessage = match serde_json::from_str(&text) {
                            Ok(msg) => msg,
                            Err(_) => {
                                let error = serialize_mobile_message(&ServerToMobileMessage::Error {
                                    message: "消息格式无效，请重试。".to_string(),
                                });
                                let _ = sender.send(Message::Text(error.into())).await;
                                continue;
                            }
                        };
                        let content = match mobile_msg {
                            MobileMessage::ClipboardText { content } => content,
                            MobileMessage::Ping => {
                                if ping_limiter.check().is_ok() {
                                    ping_rate_exceeded = false;
                                    let pong = serialize_mobile_message(&ServerToMobileMessage::Pong);
                                    let _ = sender.send(Message::Text(pong.into())).await;
                                } else if ping_rate_exceeded {
                                    let _ = sender.send(Message::Close(Some(CloseFrame {
                                        code: 1008,
                                        reason: "ping_rate_exceeded".into(),
                                    }))).await;
                                    break;
                                } else {
                                    ping_rate_exceeded = true;
                                }
                                continue;
                            }
                        };
                        if content.is_empty() {
                            continue;
                        }
                        let Some(client_tx) = current_client_tx(&state.store, pairing_id) else {
                            let message = serialize_mobile_message(&ServerToMobileMessage::ClientDisconnected);
                            let _ = sender.send(Message::Text(message.into())).await;
                            break;
                        };
                        if client_tx.send(Ok(ServerEvent {
                            event: Some(Event::ClipboardText(ClipboardText { content })),
                            grpc_session_token: String::new(),
                        })).await.is_err() {
                            let message = serialize_mobile_message(&ServerToMobileMessage::ClientDisconnected);
                            let _ = sender.send(Message::Text(message.into())).await;
                            break;
                        }
                    }
                }
            }
        }
    }

    clear_mobile_ws_state(&state.store, session_token.as_deref(), &mobile_control_tx);

    if let Some(mut entry) = state.pairing_store.get_mut(&pairing_id)
        && entry
            .active_mobile_ws
            .as_ref()
            .is_some_and(|current| current.connection_id == connection_id)
    {
        entry.active_mobile_ws = None;
        entry.revision = entry.revision.saturating_add(1);
    }

    if let Some(client_tx) = current_client_tx(&state.store, pairing_id) {
        let _ = client_tx
            .send(Ok(ServerEvent {
                event: Some(Event::MobileDisconnected(MobileDisconnected {})),
                grpc_session_token: String::new(),
            }))
            .await;
    }
}

fn validate_pairing_ws(
    state: &AppState,
    pairing_id: Uuid,
    browser_session_id: [u8; 32],
    connection_id: Uuid,
) -> Option<u16> {
    let entry = state.pairing_store.get(&pairing_id)?;
    if entry
        .active_mobile_ws
        .as_ref()
        .is_some_and(|handle| handle.connection_id != connection_id)
    {
        return Some(4001);
    }
    let session = state.browser_session_store.get(&browser_session_id)?;
    if session.revoked {
        return Some(4003);
    }
    if session.pairing_epoch != entry.epoch {
        return Some(4002);
    }
    None
}

fn authenticate_browser_session(
    state: &AppState,
    pairing_id: Uuid,
    headers: &HeaderMap,
) -> Result<BrowserAuth, StatusCode> {
    let Some(cookie_value) = read_browser_session_cookie(headers) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Ok(session_id) = decode_hex_32(cookie_value) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Some(session) = state.browser_session_store.get(&session_id) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if session.expires_at <= Instant::now() || session.revoked {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if session.pairing_id != pairing_id {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let Some(entry) = state.pairing_store.get(&pairing_id) else {
        // 401 vs 404 distinction: reaching here means the caller's cookie is valid and bound to
        // this pairing_id (credential checks above all passed). The pairing entry itself is gone
        // (deleted/expired), which is a resource-not-found condition, not a credential failure.
        return Err(StatusCode::NOT_FOUND);
    };
    if session.pairing_epoch != entry.epoch {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(BrowserAuth { session_id })
}

fn require_browser_origin(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        return Err(StatusCode::FORBIDDEN);
    };
    if origin.is_empty() || origin == "null" {
        return Err(StatusCode::FORBIDDEN);
    }
    let Ok(url) = url::Url::parse(origin) else {
        return Err(StatusCode::FORBIDDEN);
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let scheme = url.scheme().to_ascii_lowercase();
    let normalized = match (scheme.as_str(), url.port()) {
        ("http", None | Some(80)) | ("https", None | Some(443)) => format!("{scheme}://{host}"),
        (_, Some(port)) => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    };
    if normalized != state.public_origin.as_str() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

fn parse_ws_protocol_ticket(header: &HeaderValue) -> Result<[u8; 32], StatusCode> {
    let Ok(value) = header.to_str() else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let tokens: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.len() != 2 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let unique: HashSet<&str> = tokens.iter().copied().collect();
    if unique.len() != 2 || !unique.contains("qr-paste.v1") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let Some(ticket_token) = tokens
        .iter()
        .copied()
        .find(|token| token.starts_with("ticket."))
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let opaque = ticket_token.trim_start_matches("ticket.");
    if opaque.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(opaque) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    <[u8; 32]>::try_from(decoded.as_slice()).map_err(|_| StatusCode::BAD_REQUEST)
}

fn current_client_tx(
    store: &SessionStore,
    pairing_id: Uuid,
) -> Option<mpsc::Sender<Result<ServerEvent, tonic::Status>>> {
    let token = latest_session_token(store, pairing_id)?;
    store
        .get(&token)
        .and_then(|session| session.client_tx.clone())
}

fn current_session_token(store: &SessionStore, pairing_id: Uuid) -> Option<String> {
    latest_session_token(store, pairing_id)
}

fn latest_session_token(store: &SessionStore, pairing_id: Uuid) -> Option<String> {
    store
        .iter()
        .filter_map(|session| {
            (session.pairing_id == Some(pairing_id))
                .then(|| (session.key().clone(), session.created_at))
        })
        .max_by_key(|(_, created_at)| *created_at)
        .map(|(token, _)| token)
}

fn mark_mobile_ws_connected(
    store: &SessionStore,
    pairing_id: Uuid,
    mobile_control_tx: &mpsc::UnboundedSender<String>,
) -> Option<String> {
    let token = latest_session_token(store, pairing_id)?;
    if let Some(mut session) = store.get_mut(&token) {
        session.ws_active = true;
        session.mobile_control_tx = Some(mobile_control_tx.clone());
    }
    Some(token)
}

fn clear_mobile_ws_state(
    store: &SessionStore,
    session_token: Option<&str>,
    mobile_control_tx: &mpsc::UnboundedSender<String>,
) {
    let Some(session_token) = session_token else {
        return;
    };
    let Some(mut session) = store.get_mut(session_token) else {
        return;
    };
    if session
        .mobile_control_tx
        .as_ref()
        .is_some_and(|current| current.same_channel(mobile_control_tx))
    {
        session.ws_active = false;
        session.mobile_control_tx = None;
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(ToOwned::to_owned)
}

fn grpc_session_token(store: &SessionStore, bearer_token: &str) -> Option<String> {
    let token_bytes = bearer_token.as_bytes();
    store.iter().find_map(|session| {
        let stored = session.grpc_session_token.as_bytes();
        (stored.len() == token_bytes.len() && bool::from(stored.ct_eq(token_bytes)))
            .then(|| session.key().clone())
    })
}

fn pairing_session_token_event(token: String, url: String) -> ServerEvent {
    ServerEvent {
        event: Some(Event::SessionToken(SessionToken { token, url })),
        grpc_session_token: String::new(),
    }
}

fn read_browser_session_cookie(headers: &HeaderMap) -> Option<&str> {
    let cookie_header = headers.get(COOKIE)?.to_str().ok()?;
    let mut legacy_value = None;
    for item in cookie_header.split(';').map(str::trim) {
        let Some((name, value)) = item.split_once('=') else {
            continue;
        };
        if name == BROWSER_SESSION_COOKIE {
            return Some(value);
        }
        if name == LEGACY_BROWSER_SESSION_COOKIE {
            legacy_value = Some(value);
        }
    }
    legacy_value
}

fn reauth_required_response() -> Response {
    let mut response = error_json(StatusCode::UNAUTHORIZED, "reauth_required");
    clear_browser_session_cookies(response.headers_mut());
    response
}

fn pairing_not_found_response() -> Response {
    let mut response = error_json(StatusCode::NOT_FOUND, "pairing_not_found");
    clear_browser_session_cookies(response.headers_mut());
    response
}

fn unauthorized_ws_response() -> Response { error_json(StatusCode::UNAUTHORIZED, "unauthorized") }

fn error_json(status: StatusCode, error: &str) -> Response {
    let mut response = (status, Json(json!({ "error": error }))).into_response();
    apply_common_headers(response.headers_mut());
    response
}

fn apply_common_headers(headers: &mut HeaderMap) {
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
}

fn clear_browser_session_cookies(headers: &mut HeaderMap) {
    headers.append(
        SET_COOKIE,
        HeaderValue::from_static(
            "__Host-qr_paste_browser_session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0",
        ),
    );
    headers.append(
        SET_COOKIE,
        HeaderValue::from_static(
            "qr_paste_browser_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        ),
    );
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
        crate::session::{Session, new_store},
        tokio::sync::mpsc,
    };

    fn test_session(token: &str, pairing_id: Uuid, created_at: Instant) -> Session {
        Session {
            token: token.to_string(),
            created_at,
            scanned: false,
            upgrade_reserved_at: None,
            ws_active: false,
            client_tx: None,
            mobile_control_tx: None,
            device_info: None,
            pairing_id: Some(pairing_id),
            grpc_session_token: format!("grpc-{token}"),
        }
    }

    #[test]
    fn trusted_client_ip_extractor_uses_peer_ip_for_untrusted_proxy() {
        let extractor = TrustedClientIpKeyExtractor::new(
            TrustedProxyCidrs::parse(&["127.0.0.1/32".to_string()])
                .expect("trusted proxies should parse"),
        );
        let mut request = axum::http::Request::builder()
            .uri("/api/pairing/123/ws-ticket")
            .body(())
            .expect("request should build");
        request.headers_mut().insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.10, 203.0.113.7"),
        );
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 5], 8080))));

        let ip = extractor
            .extract(&request)
            .expect("peer IP should be extracted");
        assert_eq!(ip, IpAddr::from([10, 0, 0, 5]));
    }

    #[test]
    fn trusted_client_ip_extractor_uses_rightmost_xff_for_trusted_proxy() {
        let extractor = TrustedClientIpKeyExtractor::new(
            TrustedProxyCidrs::parse(&["127.0.0.1/32".to_string()])
                .expect("trusted proxies should parse"),
        );
        let mut request = axum::http::Request::builder()
            .uri("/api/pairing/123/ws-ticket")
            .body(())
            .expect("request should build");
        request.headers_mut().insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.10, 203.0.113.7"),
        );
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 8080))));

        let ip = extractor
            .extract(&request)
            .expect("trusted client IP should be extracted");
        assert_eq!(ip, IpAddr::from([203, 0, 113, 7]));
    }

    #[test]
    fn read_browser_session_cookie_prefers_host_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static(
                "qr_paste_browser_session=legacy; __Host-qr_paste_browser_session=current",
            ),
        );

        assert_eq!(read_browser_session_cookie(&headers), Some("current"));
    }

    #[test]
    fn read_browser_session_cookie_falls_back_to_legacy_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("foo=bar; qr_paste_browser_session=legacy"),
        );

        assert_eq!(read_browser_session_cookie(&headers), Some("legacy"));
    }

    #[test]
    fn mark_mobile_ws_connected_updates_latest_session() {
        let store = new_store();
        let pairing_id = Uuid::new_v4();
        store.insert(
            "older".to_string(),
            test_session(
                "older",
                pairing_id,
                Instant::now() - Duration::from_secs(10),
            ),
        );
        store.insert(
            "newer".to_string(),
            test_session("newer", pairing_id, Instant::now()),
        );
        let (mobile_control_tx, _mobile_control_rx) = mpsc::unbounded_channel();

        let session_token = mark_mobile_ws_connected(&store, pairing_id, &mobile_control_tx);

        assert_eq!(session_token.as_deref(), Some("newer"));
        assert!(
            !store
                .get("older")
                .expect("older session should exist")
                .ws_active
        );
        let newer = store.get("newer").expect("newer session should exist");
        assert!(newer.ws_active);
        assert!(
            newer
                .mobile_control_tx
                .as_ref()
                .is_some_and(|current| current.same_channel(&mobile_control_tx))
        );
    }

    #[test]
    fn clear_mobile_ws_state_only_clears_matching_connection() {
        let store = new_store();
        let pairing_id = Uuid::new_v4();
        let mut session = test_session("session", pairing_id, Instant::now());
        let (active_tx, _active_rx) = mpsc::unbounded_channel();
        session.ws_active = true;
        session.mobile_control_tx = Some(active_tx.clone());
        store.insert("session".to_string(), session);
        let (other_tx, _other_rx) = mpsc::unbounded_channel();

        clear_mobile_ws_state(&store, Some("session"), &other_tx);

        let after_mismatch = store
            .get("session")
            .expect("session should remain after mismatched clear");
        assert!(after_mismatch.ws_active);
        assert!(
            after_mismatch
                .mobile_control_tx
                .as_ref()
                .is_some_and(|current| current.same_channel(&active_tx))
        );
        drop(after_mismatch);

        clear_mobile_ws_state(&store, Some("session"), &active_tx);

        let cleared = store.get("session").expect("session should still exist");
        assert!(!cleared.ws_active);
        assert!(cleared.mobile_control_tx.is_none());
    }

    #[test]
    fn grpc_session_token_resolves_session_key_from_bearer_token() {
        let store = new_store();
        let pairing_id = Uuid::new_v4();
        store.insert(
            "session-a".to_string(),
            Session {
                token: "session-a".to_string(),
                created_at: Instant::now(),
                scanned: false,
                upgrade_reserved_at: None,
                ws_active: false,
                client_tx: None,
                mobile_control_tx: None,
                device_info: None,
                pairing_id: Some(pairing_id),
                grpc_session_token: "grpc-a".to_string(),
            },
        );
        store.insert(
            "session-b".to_string(),
            Session {
                token: "session-b".to_string(),
                created_at: Instant::now(),
                scanned: false,
                upgrade_reserved_at: None,
                ws_active: false,
                client_tx: None,
                mobile_control_tx: None,
                device_info: None,
                pairing_id: Some(pairing_id),
                grpc_session_token: "grpc-b".to_string(),
            },
        );

        assert_eq!(
            grpc_session_token(&store, "grpc-b").as_deref(),
            Some("session-b")
        );
        assert!(grpc_session_token(&store, "session-b").is_none());
        assert!(grpc_session_token(&store, "missing").is_none());
    }

    #[test]
    fn pairing_session_token_event_preserves_session_token() {
        let event = pairing_session_token_event(
            "session-token".to_string(),
            "https://example.com/m/123#ps=secret".to_string(),
        );

        match event.event {
            Some(Event::SessionToken(token)) => {
                assert_eq!(token.token, "session-token");
                assert_eq!(token.url, "https://example.com/m/123#ps=secret");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(event.grpc_session_token.is_empty());
    }
}
