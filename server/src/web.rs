// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::{
        config::{ServerConfig, TrustedProxyCidrs},
        grpc::relay::{
            ClipboardText,
            FileReceived,
            MobileConnected,
            MobileDisconnected,
            ServerEvent,
            SessionToken,
            TransferMode,
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
        persist::PersistenceStore,
        session::{SessionStore, latest_session_token},
        upload_store::{
            AttachPipeError,
            FileStatus,
            SaveFileError,
            SendChunkResult,
            SendTerminalResult,
            StreamFrame,
            UploadLimitError,
            UploadStore,
            sanitize_file_name,
        },
    },
    axum::{
        Json,
        Router,
        body::Bytes,
        extract::{
            ConnectInfo,
            DefaultBodyLimit,
            Multipart,
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
                CONTENT_DISPOSITION,
                CONTENT_SECURITY_POLICY,
                CONTENT_TYPE,
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
const HTTP_STREAMING_CAPABILITY: u32 = 0x01;

type KeyedLimiter<K> = Arc<RateLimiter<K, DefaultKeyedStateStore<K>, DefaultClock>>;

static MOBILE_HTML: &str = include_str!("web/index.html");

#[derive(Clone)]
/// 注入到全部 HTTP 处理器中的共享服务端状态。
pub struct AppState {
    /// gRPC 会话存储，保存每次 PC 客户端 Subscribe 调用的会话状态。
    pub store: SessionStore,
    /// 配对存储，保存每个稳定配对关系的运行时状态（在线状态、活跃会话令牌等）。
    pub pairing_store: PairingStore,
    /// 浏览器会话存储，保存手机端通过 bootstrap 获得的 HttpOnly Cookie 会话。
    pub browser_session_store: BrowserSessionStore,
    /// WebSocket 票据存储，保存 ws-ticket 接口签发的短效一次性票据。
    pub ws_ticket_store: WsTicketStore,
    /// 服务端配置（不可变，Arc 共享）。
    pub config: Arc<ServerConfig>,
    /// 预计算的公开 Origin 字符串（如 `https://relay.example.com`），用于校验 HTTP `Origin` 请求头。
    pub public_origin: Arc<String>,
    /// WebSocket 全局并发连接数信号量，上限由 `config.max_ws_connections` 决定。
    pub ws_slots: Arc<Semaphore>,
    /// `/status` 接口按浏览器会话 ID 的限流器，防止状态轮询过于频繁。
    pub status_limiter: KeyedLimiter<[u8; 32]>,
    /// `/ws-ticket` 接口按浏览器会话 ID 的限流器，防止票据签发被滥用。
    pub ws_ticket_limiter: KeyedLimiter<[u8; 32]>,
    /// `POST /revoke` 接口按 pairing_id 的限流器，防止撤销接口被暴力调用。
    pub revoke_pairing_limiter: KeyedLimiter<Uuid>,
    /// `POST /api/pairing/{pairing_id}/revoke` 撤销路由中按 gRPC session token 维度的限流器，
    /// 防止同一 token 的撤销请求被暴力重试。
    pub revoke_session_limiter: KeyedLimiter<String>,
    /// 持久化存储句柄，为 `None` 时禁用持久化。
    pub persist: Option<Arc<PersistenceStore>>,
    /// 文件上传临时存储，管理上传文件的内存索引与全局并发计数器。
    pub upload_store: Arc<UploadStore>,
}

#[derive(Deserialize)]
struct BootstrapBody {
    pairing_secret: String,
}

struct BrowserAuth {
    session_id: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadDispatchMode {
    HttpStreaming,
    Streaming,
    Relay,
}

impl UploadDispatchMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::HttpStreaming => "HTTP_STREAMING",
            Self::Streaming => "STREAMING",
            Self::Relay => "RELAY",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LateAttachOutcome {
    RedirectToDownload,
    NotFound,
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

/// 绑定 HTTP 监听地址并运行 Axum 路由服务。
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    addr: SocketAddr,
    store: SessionStore,
    pairing_store: PairingStore,
    browser_session_store: BrowserSessionStore,
    ws_ticket_store: WsTicketStore,
    config: ServerConfig,
    persist: Option<Arc<PersistenceStore>>,
    upload_store: Arc<UploadStore>,
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
        persist,
        upload_store,
    )
    .await
}

// serve_inner 参数数量受 AppState 字段和泛型 key_extractor 约束，无法合理减少，豁免此 lint。
#[allow(clippy::too_many_arguments)]
async fn serve_inner<K>(
    addr: SocketAddr,
    store: SessionStore,
    pairing_store: PairingStore,
    browser_session_store: BrowserSessionStore,
    ws_ticket_store: WsTicketStore,
    config: ServerConfig,
    key_extractor: K,
    persist: Option<Arc<PersistenceStore>>,
    upload_store: Arc<UploadStore>,
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
        persist,
        upload_store,
    };

    let http_rate = config.http_rate_limit_per_ip_per_min;
    let ws_rate = config.ws_rate_limit_per_ip_per_min;
    let bootstrap_rate = (http_rate / 4).max(1);

    let http_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(http_rate)
            .methods(vec![Method::GET])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid http governor config"))?,
    );
    let bootstrap_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(bootstrap_rate)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid bootstrap governor config"))?,
    );
    let status_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(ws_rate)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid status governor config"))?,
    );
    let ws_ticket_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(ws_rate)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid ws_ticket governor config"))?,
    );
    let revoke_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(http_rate)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid revoke governor config"))?,
    );
    let ws_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(ws_rate)
            .methods(vec![Method::GET])
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid ws governor config"))?,
    );
    let upload_rate = config.upload_rate_limit_per_ip_per_min;
    let upload_limit = Arc::new(
        GovernorConfigBuilder::default()
            .period(Duration::from_secs(60))
            .burst_size(upload_rate)
            .methods(vec![Method::POST])
            .key_extractor(key_extractor)
            .finish()
            .ok_or_else(|| anyhow::anyhow!("invalid upload governor config"))?,
    );
    let max_upload_size = config.max_upload_size_bytes;

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
        .merge(
            Router::new()
                .route(
                    "/api/pairing/{pairing_id}/upload",
                    post(handle_upload).layer(GovernorLayer::new(upload_limit)),
                )
                .layer(DefaultBodyLimit::max(
                    (max_upload_size as usize).saturating_add(8192),
                )),
        )
        .route("/api/files/{file_id}", get(handle_file_download))
        .route("/api/files/{file_id}/stream", get(handle_stream_download))
        .route(
            "/api/files/{file_id}/ack",
            post(handle_file_ack).layer(GovernorLayer::new(status_limit)),
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

/// 旧版 `GET /api/pairing/{pairing_id}` 端点已废弃，固定返回 `410 Gone`。
async fn handle_deprecated_pairing_get() -> Response { error_json(StatusCode::GONE, "gone") }

/// 处理首次配对认证（`POST /api/pairing/{pairing_id}/bootstrap`）。
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
    let (session_id, epoch, online, notify_url, notify_session_token, notify_client_tx) = {
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
        if let Some(p) = state.persist.as_deref() {
            p.delete_browser_sessions_for_pairing(pairing_id);
            p.save_browser_session(
                browser_session.session_id,
                browser_session.pairing_id,
                browser_session.pairing_epoch,
                browser_session.created_at,
                browser_session.last_seen,
                browser_session.expires_at,
                browser_session.revoked,
            );
        }
        state
            .browser_session_store
            .insert(session_id, browser_session);

        entry.pairing_secret = new_secret;
        entry.last_seen = now;
        entry.expires_at = now + Duration::from_secs(state.config.pairing_ttl_secs);
        entry.revision = entry.revision.saturating_add(1);

        let active_token = entry.active_session_token.clone();
        let epoch = entry.epoch;
        let online = entry.online;
        let notify_url = format!(
            "{}/m/{}#ps={}",
            state.config.public_base_url.trim_end_matches('/'),
            pairing_id,
            encode_hex(&new_secret)
        );
        if let Some(p) = state.persist.as_deref() {
            p.save_pairing(
                entry.pairing_id,
                entry.pairing_secret,
                entry.epoch,
                entry.last_seen,
                entry.expires_at,
                entry.revision,
            );
        }
        drop(entry);

        let notify_session_token = active_token
            .as_deref()
            .filter(|t| state.store.contains_key(*t))
            .map(ToOwned::to_owned)
            .or_else(|| latest_session_token(&state.store, &state.pairing_store, pairing_id))
            .unwrap_or_default();
        let notify_client_tx = if notify_session_token.is_empty() {
            None
        } else {
            state
                .store
                .get(&notify_session_token)
                .and_then(|s| s.client_tx.clone())
        };

        (
            session_id,
            epoch,
            online,
            notify_url,
            notify_session_token,
            notify_client_tx,
        )
    };

    let pc_session_notified = !notify_session_token.is_empty();

    if let Some(client_tx) = notify_client_tx {
        let _ = client_tx
            .send(Ok(ServerEvent {
                event: Some(Event::SessionToken(SessionToken {
                    token: notify_session_token,
                    url: notify_url,
                })),
                grpc_session_token: String::new(),
            }))
            .await;
    }

    info!(
        pairing_id = %pairing_id,
        pairing_epoch = epoch,
        client_online = online,
        pc_session_notified,
        "browser bootstrap succeeded"
    );

    let mut response = Json(json!({
        "online": online,
        "pairing_epoch": epoch,
    }))
    .into_response();
    apply_common_headers(response.headers_mut());
    let cookie_str = if state.config.debug_mode {
        format!(
            "{LEGACY_BROWSER_SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000",
            encode_hex(&session_id)
        )
    } else {
        format!(
            "{BROWSER_SESSION_COOKIE}={}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=2592000",
            encode_hex(&session_id)
        )
    };
    let Ok(cookie_value) = HeaderValue::from_str(&cookie_str) else {
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    };
    response.headers_mut().append(SET_COOKIE, cookie_value);
    if state.config.debug_mode {
        // debug_mode 下写的是 legacy cookie（无 Secure）；主动清除 __Host- cookie，
        // 防止 localhost 场景下浏览器残留旧 __Host- cookie 并在读取时优先命中它。
        response.headers_mut().append(
            SET_COOKIE,
            HeaderValue::from_static(
                "__Host-qr_paste_browser_session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0",
            ),
        );
    }
    response
}

/// 返回当前配对及 PC 客户端的在线状态（`POST /api/pairing/{pairing_id}/status`）。
///
/// 需要有效的浏览器会话 cookie；PC 不在线时响应固定包含 `retry_after_ms: 1000`。
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

/// 签发短效 WebSocket 票据（`POST /api/pairing/{pairing_id}/ws-ticket`），有效期 15 秒。
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

    info!(
        pairing_id = %pairing_id,
        pairing_epoch = epoch,
        "websocket ticket issued"
    );

    let mut response = Json(json!({
        "ws_ticket": URL_SAFE_NO_PAD.encode(ticket_id),
        "expires_in_ms": 15000,
    }))
    .into_response();
    apply_common_headers(response.headers_mut());
    response
}

/// 撤销指定配对的所有浏览器会话（`POST /api/pairing/{pairing_id}/revoke`）。
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
        if let Some(p) = state.persist.as_deref() {
            p.save_pairing(
                entry.pairing_id,
                entry.pairing_secret,
                entry.epoch,
                entry.last_seen,
                entry.expires_at,
                entry.revision,
            );
            p.delete_browser_sessions_for_pairing(pairing_id);
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

async fn handle_upload(
    Path(pairing_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Err(status) = require_browser_origin(&state, &headers) {
        return error_json(status, "forbidden");
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
    let _ = auth;

    let upload_dir = state.config.upload_dir.clone();
    let max_upload_size = state.config.max_upload_size_bytes;
    let body_timeout = Duration::from_secs(state.config.upload_body_timeout_secs);

    let field = match multipart
        .next_field()
        .await
        .map_err(|e| SaveFileError::Io(format!("multipart 读取失败: {e}")))
    {
        Ok(Some(f)) => f,
        Ok(None) => return error_json(StatusCode::UNPROCESSABLE_ENTITY, "no_file_field"),
        Err(SaveFileError::Io(e)) => {
            warn!("multipart 读取失败 pairing_id={pairing_id}: {e}");
            return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        }
        Err(_) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };

    let raw_filename = field.file_name().unwrap_or_default().to_string();
    let mime_type = {
        let tentative_name = sanitize_file_name(&raw_filename, &Uuid::nil());
        field
            .content_type()
            .map(|ct| ct.to_string())
            .unwrap_or_else(|| {
                mime_guess::from_path(&tentative_name)
                    .first_or_octet_stream()
                    .to_string()
            })
    };

    let (file_id, file_access_token, file_name) =
        match state
            .upload_store
            .begin_upload(pairing_id, &raw_filename, mime_type.clone())
        {
            Ok(v) => v,
            Err(SaveFileError::Limit(limit_err)) => {
                let msg = match limit_err {
                    UploadLimitError::PerPairingFileLimitReached => "per_pairing_file_limit",
                    UploadLimitError::GlobalFileLimitReached => "global_file_limit",
                    UploadLimitError::GlobalByteLimitReached => "global_byte_limit",
                    UploadLimitError::PairingClosed => "pairing_closed",
                };
                return error_json(StatusCode::TOO_MANY_REQUESTS, msg);
            }
            Err(e) => {
                warn!("begin_upload 失败 pairing_id={pairing_id}: {e:?}");
                return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
            }
        };

    let client_tx = current_client_tx(&state.store, &state.pairing_store, pairing_id);
    let upload_mode = select_upload_dispatch_mode(client_tx.as_ref().map(|(_, caps)| *caps));
    info!(
        pairing_id = %pairing_id,
        file_id = %file_id,
        mode = upload_mode.as_str(),
        "upload accepted"
    );

    match (upload_mode, client_tx) {
        (UploadDispatchMode::HttpStreaming, Some((tx, _))) => {
            handle_upload_http_streaming(
                state,
                pairing_id,
                file_id,
                file_access_token,
                file_name,
                raw_filename,
                mime_type,
                field,
                tx,
                upload_dir,
                max_upload_size,
                body_timeout,
            )
            .await
        }
        (UploadDispatchMode::Streaming, Some((tx, _))) => {
            handle_upload_streaming(
                state,
                pairing_id,
                file_id,
                file_access_token,
                file_name,
                raw_filename,
                mime_type,
                field,
                tx,
                upload_dir,
                max_upload_size,
                body_timeout,
            )
            .await
        }
        (UploadDispatchMode::Relay, None) => {
            handle_upload_relay(
                state,
                pairing_id,
                file_id,
                file_access_token,
                file_name,
                raw_filename,
                mime_type,
                field,
                upload_dir,
                max_upload_size,
                body_timeout,
            )
            .await
        }
        _ => unreachable!("upload dispatch mode and client availability diverged"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_upload_http_streaming(
    state: AppState,
    pairing_id: Uuid,
    file_id: Uuid,
    file_access_token: [u8; 32],
    file_name: String,
    raw_filename: String,
    mime_type: String,
    field: axum::extract::multipart::Field<'_>,
    tx: mpsc::Sender<Result<crate::grpc::relay::ServerEvent, tonic::Status>>,
    upload_dir: std::path::PathBuf,
    max_upload_size: u64,
    body_timeout: Duration,
) -> Response {
    use {futures::StreamExt as _, sha2::Digest as _, tokio::io::AsyncWriteExt as _};

    let _permit = match state.upload_store.try_acquire_stream_permit(pairing_id) {
        Some(permit) => permit,
        None => {
            warn!("HTTP_STREAMING 并发限制 pairing_id={pairing_id}");
            return error_json(StatusCode::TOO_MANY_REQUESTS, "too_many_uploads");
        }
    };

    // 创建 StreamPipe（下载端通过 attach_stream_pipe 获取 rx）
    if let Err(crate::upload_store::CreatePipeError::AlreadyExists) =
        state.upload_store.create_stream_pipe(file_id)
    {
        warn!("HTTP_STREAMING create_stream_pipe 冲突 file_id={file_id}");
        state.upload_store.rollback_upload(file_id, pairing_id);
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    }

    let stream_url = format!(
        "{}/api/files/{file_id}/stream",
        state.config.public_base_url.trim_end_matches('/')
    );
    let event = crate::grpc::relay::ServerEvent {
        event: Some(crate::grpc::relay::server_event::Event::FileReceived(
            FileReceived {
                file_id: file_id.to_string(),
                file_name: file_name.clone(),
                mime_type: mime_type.clone(),
                size_bytes: 0,
                download_url: stream_url,
                sha256: String::new(),
                transfer_mode: TransferMode::HttpStreaming as i32,
                file_access_token: file_access_token.to_vec(),
            },
        )),
        grpc_session_token: String::new(),
    };
    if tx.send(Ok(event)).await.is_err() {
        warn!("HTTP_STREAMING FileReceived 发送失败 file_id={file_id} pairing_id={pairing_id}");
        state.upload_store.abort_stream(file_id);
        state.upload_store.rollback_upload(file_id, pairing_id);
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    }

    info!(
        pairing_id = %pairing_id,
        file_id = %file_id,
        "HTTP_STREAMING upload started"
    );

    let tmp_name = format!(".{file_id}.tmp");
    let tmp_path = upload_dir.join(&tmp_name);

    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            warn!("HTTP_STREAMING 创建临时文件失败 file_id={file_id}: {e}");
            state.upload_store.abort_stream(file_id);
            state.upload_store.rollback_upload(file_id, pairing_id);
            return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        }
    };

    let mut hasher = sha2::Sha256::new();
    let mut total_bytes: u64 = 0;
    let mut field = field;
    let mut stream_aborted = false;

    loop {
        let next = tokio::time::timeout(body_timeout, field.next()).await;
        let chunk = match next {
            Err(_) => {
                let _ = file.flush().await;
                let _ = tokio::fs::remove_file(&tmp_path).await;
                if !stream_aborted {
                    state.upload_store.abort_stream(file_id);
                }
                state.upload_store.rollback_upload(file_id, pairing_id);
                return error_json(StatusCode::REQUEST_TIMEOUT, "upload_timeout");
            }
            Ok(None) => break,
            Ok(Some(res)) => match res {
                Ok(c) => c,
                Err(e) => {
                    let _ = file.flush().await;
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    warn!("HTTP_STREAMING 读取 multipart chunk 失败 file_id={file_id}: {e}");
                    if !stream_aborted {
                        state.upload_store.abort_stream(file_id);
                    }
                    state.upload_store.rollback_upload(file_id, pairing_id);
                    return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
                }
            },
        };

        let chunk_len = chunk.len() as u64;
        total_bytes = total_bytes.saturating_add(chunk_len);
        if total_bytes > max_upload_size {
            let _ = file.flush().await;
            let _ = tokio::fs::remove_file(&tmp_path).await;
            if !stream_aborted {
                state.upload_store.abort_stream(file_id);
            }
            state.upload_store.rollback_upload(file_id, pairing_id);
            return error_json(StatusCode::PAYLOAD_TOO_LARGE, "file_too_large");
        }

        hasher.update(&chunk);

        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            warn!("HTTP_STREAMING 写盘失败 file_id={file_id}: {e}");
            if !stream_aborted {
                state.upload_store.abort_stream(file_id);
            }
            state.upload_store.rollback_upload(file_id, pairing_id);
            return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        }

        if !stream_aborted {
            let data = bytes::Bytes::copy_from_slice(&chunk);
            match state.upload_store.send_stream_chunk(file_id, data).await {
                crate::upload_store::SendStreamResult::Ok => {}
                crate::upload_store::SendStreamResult::Disconnected => {
                    warn!(
                        "HTTP_STREAMING send_stream_chunk 断连 file_id={file_id} pairing_id={pairing_id}"
                    );
                    state.upload_store.abort_stream(file_id);
                    stream_aborted = true;
                }
                crate::upload_store::SendStreamResult::Timeout => {
                    warn!(
                        "HTTP_STREAMING send_stream_chunk backpressure 超时 file_id={file_id} pairing_id={pairing_id}"
                    );
                    state.upload_store.abort_stream(file_id);
                    stream_aborted = true;
                }
                crate::upload_store::SendStreamResult::NotFound => {
                    warn!("HTTP_STREAMING send_stream_chunk pipe 丢失 file_id={file_id}");
                    stream_aborted = true;
                }
            }
        }
    }

    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        warn!("HTTP_STREAMING flush 失败 file_id={file_id}: {e}");
        if !stream_aborted {
            state.upload_store.abort_stream(file_id);
        }
        state.upload_store.rollback_upload(file_id, pairing_id);
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    }
    drop(file);

    let hash = hasher.finalize();
    let sha256_bytes: [u8; 32] = hash.into();
    let sha256_hex: String = sha256_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let final_path = upload_dir.join(file_id.to_string());
    if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        warn!("HTTP_STREAMING rename 失败 file_id={file_id}: {e}");
        if !stream_aborted {
            state.upload_store.abort_stream(file_id);
        }
        state.upload_store.rollback_upload(file_id, pairing_id);
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    }

    // finalize_stream_success 原子性更新 FileMeta 并返回 Sender；
    // 在锁外带超时地 await 发送 Done，确保背压场景下 Done 帧不被丢弃。
    let done_tx =
        state
            .upload_store
            .finalize_stream_success(file_id, final_path, total_bytes, sha256_bytes);
    if let Some(tx) = done_tx {
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            tx.send(StreamFrame::Done {
                sha256: sha256_bytes,
            }),
        )
        .await;
    }

    let final_status = state
        .upload_store
        .get_file_meta(file_id)
        .map(|meta| meta.status);
    info!(
        pairing_id = %pairing_id,
        file_id = %file_id,
        size_bytes = total_bytes,
        stream_aborted,
        final_status = ?final_status,
        "HTTP_STREAMING upload finalized"
    );

    let mut response = Json(json!({
        "file_id": file_id.to_string(),
        "file_name": raw_filename,
        "mime_type": mime_type,
        "size_bytes": total_bytes,
        "sha256": sha256_hex,
    }))
    .into_response();
    apply_common_headers(response.headers_mut());
    response
}

#[allow(clippy::too_many_arguments)]
async fn handle_upload_streaming(
    state: AppState,
    pairing_id: Uuid,
    file_id: Uuid,
    file_access_token: [u8; 32],
    file_name: String,
    raw_filename: String,
    mime_type: String,
    field: axum::extract::multipart::Field<'_>,
    tx: mpsc::Sender<Result<crate::grpc::relay::ServerEvent, tonic::Status>>,
    upload_dir: std::path::PathBuf,
    max_upload_size: u64,
    body_timeout: Duration,
) -> Response {
    use {futures::StreamExt as _, sha2::Digest as _, tokio::io::AsyncWriteExt as _};

    state.upload_store.create_streaming_state(file_id);

    let download_url = format!(
        "{}/api/files/{file_id}",
        state.config.public_base_url.trim_end_matches('/')
    );
    let event = crate::grpc::relay::ServerEvent {
        event: Some(crate::grpc::relay::server_event::Event::FileReceived(
            FileReceived {
                file_id: file_id.to_string(),
                file_name: file_name.clone(),
                mime_type: mime_type.clone(),
                size_bytes: 0,
                download_url,
                sha256: String::new(),
                transfer_mode: TransferMode::Streaming as i32,
                file_access_token: file_access_token.to_vec(),
            },
        )),
        grpc_session_token: String::new(),
    };
    if tx.send(Ok(event)).await.is_err() {
        warn!("STREAMING FileReceived 发送失败 file_id={file_id} pairing_id={pairing_id}");
        state.upload_store.remove_streaming_state(file_id);
        state.upload_store.rollback_upload(file_id, pairing_id);
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    }

    info!(
        pairing_id = %pairing_id,
        file_id = %file_id,
        "STREAMING upload started"
    );

    let tmp_name = format!(".{file_id}.tmp");
    let tmp_path = upload_dir.join(&tmp_name);

    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            warn!("STREAMING 创建临时文件失败 file_id={file_id}: {e}");
            if let SendTerminalResult::Timeout = state
                .upload_store
                .send_abort(file_id, "tmp_create_failed".to_string())
                .await
            {
                warn!("STREAMING send_abort 超时（tmp_create_failed）file_id={file_id}");
            }
            state.upload_store.rollback_upload(file_id, pairing_id);
            return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        }
    };

    let mut hasher = sha2::Sha256::new();
    let mut total_bytes: u64 = 0;
    let mut field = field;
    let mut streaming_aborted = false;

    loop {
        let next = tokio::time::timeout(body_timeout, field.next()).await;
        let chunk = match next {
            Err(_) => {
                let _ = file.flush().await;
                let _ = tokio::fs::remove_file(&tmp_path).await;
                if !streaming_aborted
                    && let SendTerminalResult::Timeout = state
                        .upload_store
                        .send_abort(file_id, "upload_timeout".to_string())
                        .await
                {
                    warn!("STREAMING send_abort 超时（upload_timeout）file_id={file_id}");
                }
                state.upload_store.rollback_upload(file_id, pairing_id);
                return error_json(StatusCode::REQUEST_TIMEOUT, "upload_timeout");
            }
            Ok(None) => break,
            Ok(Some(res)) => match res {
                Ok(c) => c,
                Err(e) => {
                    let _ = file.flush().await;
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    warn!("STREAMING 读取 multipart chunk 失败 file_id={file_id}: {e}");
                    if !streaming_aborted
                        && let SendTerminalResult::Timeout = state
                            .upload_store
                            .send_abort(file_id, "read_error".to_string())
                            .await
                    {
                        warn!("STREAMING send_abort 超时（read_error）file_id={file_id}");
                    }
                    state.upload_store.rollback_upload(file_id, pairing_id);
                    return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
                }
            },
        };

        let chunk_len = chunk.len() as u64;
        total_bytes = total_bytes.saturating_add(chunk_len);
        if total_bytes > max_upload_size {
            let _ = file.flush().await;
            let _ = tokio::fs::remove_file(&tmp_path).await;
            if !streaming_aborted
                && let SendTerminalResult::Timeout = state
                    .upload_store
                    .send_abort(file_id, "too_large".to_string())
                    .await
            {
                warn!("STREAMING send_abort 超时（too_large）file_id={file_id}");
            }
            state.upload_store.rollback_upload(file_id, pairing_id);
            return error_json(StatusCode::PAYLOAD_TOO_LARGE, "file_too_large");
        }

        hasher.update(&chunk);

        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            warn!("STREAMING 写盘失败 file_id={file_id}: {e}");
            if !streaming_aborted
                && let SendTerminalResult::Timeout = state
                    .upload_store
                    .send_abort(file_id, "io_error".to_string())
                    .await
            {
                warn!("STREAMING send_abort 超时（io_error）file_id={file_id}");
            }
            state.upload_store.rollback_upload(file_id, pairing_id);
            return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        }

        if !streaming_aborted {
            let bytes = bytes::Bytes::copy_from_slice(&chunk);
            match state.upload_store.send_chunk(file_id, bytes).await {
                SendChunkResult::Ok => {}
                SendChunkResult::Disconnected => {
                    warn!("STREAMING send_chunk 断连 file_id={file_id} pairing_id={pairing_id}");
                    if let SendTerminalResult::Timeout = state
                        .upload_store
                        .send_abort(file_id, "disconnected".to_string())
                        .await
                    {
                        warn!("STREAMING send_abort 超时（disconnected）file_id={file_id}");
                    }
                    streaming_aborted = true;
                }
                SendChunkResult::Timeout => {
                    warn!(
                        "STREAMING send_chunk backpressure 超时 file_id={file_id} pairing_id={pairing_id}"
                    );
                    if let SendTerminalResult::Timeout = state
                        .upload_store
                        .send_abort(file_id, "backpressure_timeout".to_string())
                        .await
                    {
                        warn!("STREAMING send_abort 超时（backpressure_timeout）file_id={file_id}");
                    }
                    streaming_aborted = true;
                }
            }
        }
    }

    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        warn!("STREAMING flush 失败 file_id={file_id}: {e}");
        if !streaming_aborted
            && let SendTerminalResult::Timeout = state
                .upload_store
                .send_abort(file_id, "flush_error".to_string())
                .await
        {
            warn!("STREAMING send_abort 超时（flush_error）file_id={file_id}");
        }
        state.upload_store.rollback_upload(file_id, pairing_id);
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    }
    drop(file);

    let hash = hasher.finalize();
    let sha256_bytes: [u8; 32] = hash.into();
    let sha256_hex: String = sha256_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let final_path = upload_dir.join(file_id.to_string());
    if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        warn!("STREAMING rename 失败 file_id={file_id}: {e}");
        if !streaming_aborted
            && let SendTerminalResult::Timeout = state
                .upload_store
                .send_abort(file_id, "rename_failed".to_string())
                .await
        {
            warn!("STREAMING send_abort 超时（rename_failed）file_id={file_id}");
        }
        state.upload_store.rollback_upload(file_id, pairing_id);
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    }

    state
        .upload_store
        .mark_streaming_done(file_id, final_path, total_bytes, sha256_bytes);

    if !streaming_aborted {
        match state.upload_store.send_end(file_id, sha256_bytes).await {
            SendTerminalResult::Sent => {}
            SendTerminalResult::Disconnected => {
                warn!("STREAMING send_end 断连（文件已提交）file_id={file_id}");
            }
            SendTerminalResult::Timeout => {
                warn!("STREAMING send_end 超时（文件已提交）file_id={file_id}");
            }
        }
    }

    info!(
        pairing_id = %pairing_id,
        file_id = %file_id,
        size_bytes = total_bytes,
        streaming_aborted,
        "STREAMING upload finalized"
    );

    let mut response = Json(json!({
        "file_id": file_id.to_string(),
        "file_name": raw_filename,
        "mime_type": mime_type,
        "size_bytes": total_bytes,
        "sha256": sha256_hex,
    }))
    .into_response();
    apply_common_headers(response.headers_mut());
    response
}

#[allow(clippy::too_many_arguments)]
async fn handle_upload_relay(
    state: AppState,
    pairing_id: Uuid,
    file_id: Uuid,
    file_access_token: [u8; 32],
    file_name: String,
    raw_filename: String,
    _mime_type: String,
    field: axum::extract::multipart::Field<'_>,
    upload_dir: std::path::PathBuf,
    max_upload_size: u64,
    body_timeout: Duration,
) -> Response {
    let save_result = state
        .upload_store
        .save_file_from_field(
            &upload_dir,
            file_id,
            pairing_id,
            field,
            max_upload_size,
            body_timeout,
        )
        .await;

    match save_result {
        Err(SaveFileError::Timeout) => {
            return error_json(StatusCode::REQUEST_TIMEOUT, "upload_timeout");
        }
        Err(SaveFileError::TooLarge) => {
            return error_json(StatusCode::PAYLOAD_TOO_LARGE, "file_too_large");
        }
        Err(SaveFileError::Limit(limit_err)) => {
            let msg = match limit_err {
                UploadLimitError::PerPairingFileLimitReached => "per_pairing_file_limit",
                UploadLimitError::GlobalFileLimitReached => "global_file_limit",
                UploadLimitError::GlobalByteLimitReached => "global_byte_limit",
                UploadLimitError::PairingClosed => "pairing_closed",
            };
            return error_json(StatusCode::TOO_MANY_REQUESTS, msg);
        }
        Err(SaveFileError::Io(err)) => {
            warn!("RELAY 上传文件 IO 错误 pairing_id={pairing_id}: {err}");
            return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        }
        Ok(()) => {}
    }

    let Some(meta) = state.upload_store.get_file_meta(file_id) else {
        warn!("RELAY 写盘后 FileMeta 丢失 file_id={file_id}");
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    };

    if state.upload_store.begin_notifying(file_id, pairing_id) {
        let download_url = format!(
            "{}/api/files/{file_id}",
            state.config.public_base_url.trim_end_matches('/')
        );
        let event = ServerEvent {
            event: Some(Event::FileReceived(FileReceived {
                file_id: file_id.to_string(),
                file_name: file_name.clone(),
                mime_type: meta.mime_type.clone(),
                size_bytes: meta.size_bytes,
                download_url,
                sha256: meta.sha256.clone(),
                transfer_mode: TransferMode::Relay as i32,
                file_access_token: file_access_token.to_vec(),
            })),
            grpc_session_token: String::new(),
        };
        let client_tx = current_client_tx(&state.store, &state.pairing_store, pairing_id);
        if let Some((relay_tx, _)) = client_tx {
            match relay_tx.send(Ok(event)).await {
                Ok(()) => state.upload_store.mark_notified(file_id),
                Err(_) => {
                    let store = state.upload_store.clone();
                    tokio::spawn(async move {
                        store.mark_notify_failed(file_id);
                    });
                    warn!(
                        "RELAY gRPC 推送失败（接收端已关闭） file_id={file_id} pairing_id={pairing_id}"
                    );
                }
            }
        } else {
            warn!("RELAY 上传完成但无活跃 gRPC 订阅 file_id={file_id} pairing_id={pairing_id}");
            state.upload_store.mark_notify_failed(file_id);
        }
    }

    info!(
        pairing_id = %pairing_id,
        file_id = %file_id,
        size_bytes = meta.size_bytes,
        "RELAY upload stored"
    );

    let mut response = Json(json!({
        "file_id": file_id.to_string(),
        "file_name": raw_filename,
        "mime_type": meta.mime_type,
        "size_bytes": meta.size_bytes,
        "sha256": meta.sha256,
    }))
    .into_response();
    apply_common_headers(response.headers_mut());
    response
}

/// 处理文件下载（`GET /api/files/{file_id}`）。
///
/// 客户端（PC）通过 Bearer token（file_access_token 的十六进制）进行常量时间验证后，
/// 流式返回文件内容，并携带 RFC5987 编码的 Content-Disposition 头。
async fn handle_file_download(
    Path(file_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let Ok(file_id) = Uuid::parse_str(&file_id) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_file_id");
    };

    let Some(token_hex) = bearer_token(&headers) else {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    };
    let Ok(provided_token) = decode_hex_32(&token_hex) else {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    };

    let Some(meta) = state.upload_store.get_file_meta(file_id) else {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    };
    if provided_token.ct_ne(&meta.file_access_token).into() {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    }

    let file = match tokio::fs::File::open(&meta.file_path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return error_json(StatusCode::NOT_FOUND, "file_not_found");
        }
        Err(e) => {
            warn!("打开文件失败 file_id={file_id}: {e}");
            return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        }
    };

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    // RFC5987 编码的 Content-Disposition，兼容非 ASCII 文件名
    let filename_escaped = meta
        .file_name
        .chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                vec![c]
            } else {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                encoded
                    .bytes()
                    .flat_map(|b| {
                        let hi = "0123456789ABCDEF".as_bytes()[usize::from(b >> 4)];
                        let lo = "0123456789ABCDEF".as_bytes()[usize::from(b & 0x0f)];
                        vec!['%', hi as char, lo as char]
                    })
                    .collect::<Vec<_>>()
            }
        })
        .collect::<String>();
    let content_disposition = format!("attachment; filename*=UTF-8''{filename_escaped}");
    let Ok(cd_value) = HeaderValue::from_str(&content_disposition) else {
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    };
    let Ok(ct_value) = HeaderValue::from_str(&meta.mime_type) else {
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    };

    info!(
        file_id = %file_id,
        size_bytes = meta.size_bytes,
        file_status = ?meta.status,
        "normal file download started"
    );

    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(CONTENT_DISPOSITION, cd_value);
    response.headers_mut().insert(CONTENT_TYPE, ct_value);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store, private"));
    if matches!(
        meta.status,
        FileStatus::StoredUnnotified | FileStatus::Notified
    ) && !meta.sha256.is_empty()
        && let Ok(v) = HeaderValue::from_str(&meta.sha256)
    {
        response.headers_mut().insert(
            axum::http::header::HeaderName::from_static("x-file-sha256"),
            v,
        );
    }
    response
}

/// 处理 HTTP_STREAMING 文件流式下载（`GET /api/files/{file_id}/stream`）。
///
/// 客户端通过 Bearer token（file_access_token 的十六进制）验证后，将正在上传的文件内容
/// 以 chunked transfer 实时推送，实现上传与下载并行（总耗时 = max(T_upload, T_download)）。
///
/// 协议约定（见 design.md D5）：
/// - 响应体末尾附加 32 字节原始 SHA-256；客户端通过末尾 32 字节校验完整性
/// - 收到 `Abort` 帧时返回 HTTP 200 但 body 截断（不发 HTTP 500）
/// - token 校验失败返回 HTTP 404
/// - late attach：重试 10×50ms（500ms），超时后按文件状态返回 302 或 404
async fn handle_stream_download(
    Path(file_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    use {
        axum::body::Body,
        futures::stream,
        tokio::time::sleep,
        tokio_util::bytes::Bytes as TBytes,
    };

    let Ok(file_id) = Uuid::parse_str(&file_id) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_file_id");
    };

    // Token 验证
    let Some(token_hex) = bearer_token(&headers) else {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    };
    let Ok(provided_token) = decode_hex_32(&token_hex) else {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    };
    let Some(meta) = state.upload_store.get_file_meta(file_id) else {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    };
    if provided_token.ct_ne(&meta.file_access_token).into() {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    }

    // Late attach：重试 10×50ms
    const MAX_RETRIES: u32 = 10;
    const RETRY_INTERVAL_MS: u64 = 50;

    let mut rx = None;
    for _ in 0..MAX_RETRIES {
        match state.upload_store.attach_stream_pipe(file_id) {
            Ok(receiver) => {
                rx = Some(receiver);
                break;
            }
            Err(AttachPipeError::AlreadyAttached) => {
                // 已有消费者，不允许重复 attach
                return error_json(StatusCode::CONFLICT, "stream_already_attached");
            }
            Err(AttachPipeError::NotFound) => {
                // pipe 尚未创建，等待上传方初始化
                sleep(Duration::from_millis(RETRY_INTERVAL_MS)).await;
            }
        }
    }

    let rx = match rx {
        Some(r) => {
            info!(file_id = %file_id, "HTTP_STREAMING stream attached");
            r
        }
        None => {
            // 超时：按文件状态决定 302/404
            let Some(snap) = state.upload_store.get_file_meta(file_id) else {
                return error_json(StatusCode::NOT_FOUND, "file_not_found");
            };
            match resolve_late_attach_outcome(Some(snap.status.clone())) {
                LateAttachOutcome::RedirectToDownload => {
                    let Ok(sha_val) = HeaderValue::from_str(&snap.sha256) else {
                        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
                    };
                    if sha_val.is_empty() {
                        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
                    }
                    let redirect_url = format!(
                        "{}/api/files/{file_id}",
                        state.config.public_base_url.trim_end_matches('/')
                    );
                    let Ok(location) = HeaderValue::from_str(&redirect_url) else {
                        return error_json(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
                    };
                    info!(
                        file_id = %file_id,
                        file_status = ?snap.status,
                        "HTTP_STREAMING late attach redirected to normal download"
                    );
                    let mut resp = StatusCode::FOUND.into_response();
                    resp.headers_mut()
                        .insert(axum::http::header::LOCATION, location);
                    resp.headers_mut().insert("x-file-sha256", sha_val);
                    return resp;
                }
                LateAttachOutcome::NotFound => {
                    return error_json(StatusCode::NOT_FOUND, "file_not_found");
                }
            }
        }
    };

    // 将 mpsc::Receiver<StreamFrame> 转为 async stream
    let body_stream = stream::unfold(rx, |mut receiver| async move {
        match receiver.recv().await {
            Some(StreamFrame::Chunk(data)) => {
                Some((Ok::<TBytes, std::convert::Infallible>(data), receiver))
            }
            Some(StreamFrame::Done { sha256 }) => {
                // 追加 32 字节原始 SHA-256 后结束 stream
                let sha_bytes = TBytes::copy_from_slice(&sha256);
                Some((Ok(sha_bytes), {
                    // 发送一个哨兵使 stream 在下次 poll 结束
                    // 用一个已关闭的 dummy receiver
                    let (_, dummy_rx) = tokio::sync::mpsc::channel(1);
                    dummy_rx
                }))
            }
            Some(StreamFrame::Abort) | None => None,
        }
    });

    let body = Body::from_stream(body_stream);
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store, private"));
    response
}

#[derive(serde::Deserialize)]
struct AckBody {
    success: bool,
}

/// 处理 PC 客户端对文件接收的 ACK（`POST /api/files/{file_id}/ack`）。
///
/// 解析 `{"success": bool}` 后调用 `remove_file()`，不存在时直接 200（幂等）。
async fn handle_file_ack(
    Path(file_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > 256 {
        return error_json(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    let Ok(file_id) = Uuid::parse_str(&file_id) else {
        return error_json(StatusCode::BAD_REQUEST, "invalid_file_id");
    };

    let Some(token_hex) = bearer_token(&headers) else {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    };
    let Ok(provided_token) = decode_hex_32(&token_hex) else {
        return error_json(StatusCode::NOT_FOUND, "file_not_found");
    };

    if let Some(meta) = state.upload_store.get_file_meta(file_id) {
        if provided_token.ct_ne(&meta.file_access_token).into() {
            return error_json(StatusCode::NOT_FOUND, "file_not_found");
        }
        let ack: AckBody = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return error_json(StatusCode::BAD_REQUEST, "invalid_format"),
        };
        if ack.success {
            state.upload_store.remove_file(file_id);
        }
    }

    let mut response = Json(json!({"ok": true})).into_response();
    apply_common_headers(response.headers_mut());
    response
}

/// 将 HTTP 连接升级为 WebSocket（`GET /ws/mobile/{id}`）。
///
/// 浏览器需通过 `Sec-WebSocket-Protocol` 携带短效票据完成鉴权；升级后进入
/// [`handle_pairing_ws`] 消息循环。
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
        if browser_session.revoked
            || browser_session.expires_at <= now
            || browser_session.pairing_id != pairing_id
        {
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
    let has_device_info = device_info.as_ref().is_some_and(|value| !value.is_empty());
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
    let session_token = mark_mobile_ws_connected(
        &state.store,
        &state.pairing_store,
        pairing_id,
        &mobile_control_tx,
    );
    info!(
        pairing_id = %pairing_id,
        connection_id = %connection_id,
        has_device_info,
        "mobile websocket connected"
    );
    if let Some((client_tx, _)) = current_client_tx(&state.store, &state.pairing_store, pairing_id)
    {
        let _ = client_tx
            .send(Ok(ServerEvent {
                event: Some(Event::MobileConnected(MobileConnected {
                    device_info: device_info.clone().unwrap_or_default(),
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
                        let Some((client_tx, _)) =
                            current_client_tx(&state.store, &state.pairing_store, pairing_id)
                        else {
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

    if let Some((client_tx, _)) = current_client_tx(&state.store, &state.pairing_store, pairing_id)
    {
        let _ = client_tx
            .send(Ok(ServerEvent {
                event: Some(Event::MobileDisconnected(MobileDisconnected {})),
                grpc_session_token: String::new(),
            }))
            .await;
    }

    info!(
        pairing_id = %pairing_id,
        connection_id = %connection_id,
        "mobile websocket disconnected"
    );
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
    if session.expires_at <= Instant::now() {
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

/// 从 `Sec-WebSocket-Protocol` 头解析一次性 WS 票据。
///
/// 头部应包含两个以逗号分隔的子协议值：`v1` 和 `ticket.<base64url>` 。
/// 成功时返回 32 字节原始票据，格式或长度不符时返回 `400 Bad Request`。
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
    pairing_store: &PairingStore,
    pairing_id: Uuid,
) -> Option<(mpsc::Sender<Result<ServerEvent, tonic::Status>>, u32)> {
    let token = latest_session_token(store, pairing_store, pairing_id)?;
    store.get(&token).and_then(|session| {
        session
            .client_tx
            .clone()
            .map(|tx| (tx, session.client_capabilities))
    })
}

fn select_upload_dispatch_mode(client_capabilities: Option<u32>) -> UploadDispatchMode {
    match client_capabilities {
        Some(capabilities) if capabilities & HTTP_STREAMING_CAPABILITY != 0 => {
            UploadDispatchMode::HttpStreaming
        }
        Some(_) => UploadDispatchMode::Streaming,
        None => UploadDispatchMode::Relay,
    }
}

fn resolve_late_attach_outcome(status: Option<FileStatus>) -> LateAttachOutcome {
    match status {
        Some(FileStatus::StoredUnnotified | FileStatus::Notified) => {
            LateAttachOutcome::RedirectToDownload
        }
        Some(FileStatus::Uploading | FileStatus::Notifying | FileStatus::Acked) | None => {
            LateAttachOutcome::NotFound
        }
    }
}

fn mark_mobile_ws_connected(
    store: &SessionStore,
    pairing_store: &PairingStore,
    pairing_id: Uuid,
    mobile_control_tx: &mpsc::UnboundedSender<String>,
) -> Option<String> {
    let token = latest_session_token(store, pairing_store, pairing_id)?;
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

/// 通过恒定时间比较将 Bearer 令牌映射到对应的 `SessionStore` 键。
///
/// 用于防止基于时序的令牌枚举攻击（与 `authenticate_browser_session` 中的做法一致）。
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
        crate::{
            pairing::{new_browser_session_store, new_pairing_store, new_ws_ticket_store},
            session::{Session, new_store},
            upload_store::new_upload_store,
        },
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
            client_capabilities: 0,
        }
    }

    fn test_server_config(upload_dir: std::path::PathBuf) -> ServerConfig {
        ServerConfig {
            public_base_url: "https://example.com".to_string(),
            grpc_auth_token: "0123456789abcdef".to_string(),
            grpc_port: 50051,
            http_port: 8080,
            http_bind_host: IpAddr::from([127, 0, 0, 1]),
            grpc_bind_host: IpAddr::from([127, 0, 0, 1]),
            token_expiry_secs: 300,
            pairing_ttl_secs: 86_400,
            token_cleanup_interval_secs: 60,
            ws_rate_limit_per_ip_per_min: 10,
            http_rate_limit_per_ip_per_min: 20,
            max_ws_connections: 100,
            max_message_size_bytes: 65_536,
            ws_idle_timeout_secs: 90,
            grpc_keepalive_interval_secs: 60,
            grpc_keepalive_timeout_secs: 20,
            log_level: "info".to_string(),
            trusted_proxy_cidrs: Vec::new(),
            persistence_path: "qr-paste-state.db".to_string(),
            upload_dir,
            max_upload_size_bytes: 52_428_800,
            upload_file_retention_secs: 3_600,
            upload_cleanup_interval_secs: 300,
            upload_rate_limit_per_ip_per_min: 6,
            upload_body_timeout_secs: 30,
            max_pending_upload_files_per_pairing: 20,
            max_pending_upload_files_global: 500,
            max_pending_upload_bytes_global: 2_147_483_648,
            debug_mode: false,
            http_stream_pipe_capacity: 8,
            http_stream_backpressure_timeout_secs: 5,
            max_concurrent_http_stream_uploads_per_pairing: 5,
        }
    }

    fn test_app_state() -> AppState {
        let upload_dir = std::env::temp_dir().join(format!("qr-paste-web-test-{}", Uuid::new_v4()));
        let config = test_server_config(upload_dir);
        let public_origin = config
            .normalized_public_origin()
            .expect("test public origin should normalize");
        AppState {
            store: new_store(),
            pairing_store: new_pairing_store(),
            browser_session_store: new_browser_session_store(),
            ws_ticket_store: new_ws_ticket_store(),
            config: Arc::new(config),
            public_origin: Arc::new(public_origin),
            ws_slots: Arc::new(Semaphore::new(100)),
            status_limiter: keyed_limiter(30),
            ws_ticket_limiter: keyed_limiter(12),
            revoke_pairing_limiter: keyed_limiter(5),
            revoke_session_limiter: keyed_limiter(10),
            persist: None,
            upload_store: new_upload_store(52_428_800, 20, 500, 2_147_483_648, 8, 5, 5),
        }
    }

    fn bearer_headers(token: [u8; 32]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {}", encode_hex(&token));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&bearer).expect("bearer header should be valid"),
        );
        headers
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
        let pairing_store = new_pairing_store();
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

        let session_token =
            mark_mobile_ws_connected(&store, &pairing_store, pairing_id, &mobile_control_tx);

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
                client_capabilities: 0,
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
                client_capabilities: 0,
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

    #[test]
    fn upload_dispatch_mode_prefers_http_streaming_when_capability_present() {
        assert_eq!(
            select_upload_dispatch_mode(Some(HTTP_STREAMING_CAPABILITY)),
            UploadDispatchMode::HttpStreaming
        );
        assert_eq!(
            select_upload_dispatch_mode(Some(HTTP_STREAMING_CAPABILITY | 0x10)),
            UploadDispatchMode::HttpStreaming
        );
    }

    #[test]
    fn upload_dispatch_mode_falls_back_for_legacy_and_offline_clients() {
        assert_eq!(
            select_upload_dispatch_mode(Some(0)),
            UploadDispatchMode::Streaming
        );
        assert_eq!(select_upload_dispatch_mode(None), UploadDispatchMode::Relay);
    }

    #[test]
    fn late_attach_outcome_redirects_only_for_finalized_statuses() {
        assert_eq!(
            resolve_late_attach_outcome(Some(FileStatus::StoredUnnotified)),
            LateAttachOutcome::RedirectToDownload
        );
        assert_eq!(
            resolve_late_attach_outcome(Some(FileStatus::Notified)),
            LateAttachOutcome::RedirectToDownload
        );
    }

    #[test]
    fn late_attach_outcome_returns_not_found_for_non_finalized_statuses() {
        assert_eq!(
            resolve_late_attach_outcome(Some(FileStatus::Uploading)),
            LateAttachOutcome::NotFound
        );
        assert_eq!(
            resolve_late_attach_outcome(Some(FileStatus::Notifying)),
            LateAttachOutcome::NotFound
        );
        assert_eq!(
            resolve_late_attach_outcome(Some(FileStatus::Acked)),
            LateAttachOutcome::NotFound
        );
        assert_eq!(
            resolve_late_attach_outcome(None),
            LateAttachOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn stream_download_late_attach_redirects_to_normal_download() {
        let state = test_app_state();
        let pairing_id = Uuid::new_v4();
        let (file_id, token, _file_name) = state
            .upload_store
            .begin_upload(pairing_id, "report.txt", "text/plain".to_string())
            .expect("upload reservation should succeed");
        let final_path = state.config.upload_dir.join(file_id.to_string());
        let sha256 = [0xAB; 32];

        let sender = state
            .upload_store
            .finalize_stream_success(file_id, final_path, 123, sha256);
        assert!(sender.is_none());

        let response = handle_stream_download(
            Path(file_id.to_string()),
            State(state.clone()),
            bearer_headers(token),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers().get(axum::http::header::LOCATION),
            Some(
                &HeaderValue::from_str(&format!(
                    "{}/api/files/{file_id}",
                    state.config.public_base_url
                ))
                .expect("location header should be valid")
            )
        );
        assert_eq!(
            response.headers().get("x-file-sha256"),
            Some(&HeaderValue::from_str(&encode_hex(&sha256)).expect("sha header should be valid"))
        );
    }

    #[tokio::test]
    async fn stream_download_rejects_second_attach_with_conflict() {
        let state = test_app_state();
        let pairing_id = Uuid::new_v4();
        let (file_id, token, _file_name) = state
            .upload_store
            .begin_upload(pairing_id, "report.txt", "text/plain".to_string())
            .expect("upload reservation should succeed");
        state
            .upload_store
            .create_stream_pipe(file_id)
            .expect("stream pipe should be created");
        let _first_receiver = state
            .upload_store
            .attach_stream_pipe(file_id)
            .expect("first attach should succeed");

        let response = handle_stream_download(
            Path(file_id.to_string()),
            State(state),
            bearer_headers(token),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
