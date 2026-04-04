// SPDX-License-Identifier: MIT OR Apache-2.0

/// gRPC 接口生成代码，由 `build.rs` 通过 `tonic-prost-build` 从 `proto/relay.proto` 生成。
pub mod relay {
    tonic::include_proto!("relay");
}

use {
    crate::{
        config::{ServerConfig, TrustedProxyCidrs},
        pairing::{PairingEntry, PairingStore, encode_hex, random_bytes_32},
        session::{Session, SessionStore},
    },
    common::ServerToMobileMessage,
    dashmap::DashMap,
    futures::Stream,
    relay::{
        PingRequest,
        PingResponse,
        ServerEvent,
        SessionToken,
        SubscribeRequest,
        client_relay_server::{ClientRelay, ClientRelayServer},
    },
    std::{
        net::SocketAddr,
        sync::Arc,
        time::{Duration, Instant},
    },
    subtle::ConstantTimeEq,
    tokio::sync::mpsc,
    tokio_stream::wrappers::ReceiverStream,
    tonic::{Request, Response, Status, metadata::MetadataMap},
    tracing::{info, warn},
    uuid::Uuid,
};

/// gRPC `ClientRelay` 服务实现。
///
/// 每个 PC 客户端调用 `Subscribe` 时创建一个新的会话令牌，
/// 并返回一个长连接的服务端推送流（server-streaming）。
pub struct ClientRelayService {
    store: SessionStore,
    pairing_store: PairingStore,
    config: ServerConfig,
    trusted_proxies: TrustedProxyCidrs,
    grpc_tokens: Arc<DashMap<String, ()>>,
}

impl ClientRelayService {
    /// 创建新的服务实例，注入共享的会话存储和配置。
    pub fn new(
        store: SessionStore,
        pairing_store: PairingStore,
        config: ServerConfig,
    ) -> anyhow::Result<Self> {
        let trusted_proxies = config.trusted_proxy_ranges()?;
        Ok(Self {
            store,
            pairing_store,
            config,
            trusted_proxies,
            grpc_tokens: Arc::new(DashMap::new()),
        })
    }
}

/// `Subscribe` RPC 使用的服务端流包装类型，析构时会清理关联会话。
pub struct SessionStream {
    inner: ReceiverStream<Result<ServerEvent, Status>>,
    store: SessionStore,
    pairing_store: PairingStore,
    pairing_ttl_secs: u64,
    token: String,
    grpc_session_token: String,
    grpc_tokens: Arc<DashMap<String, ()>>,
}

impl std::fmt::Debug for SessionStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStream")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl Stream for SessionStream {
    type Item = Result<ServerEvent, Status>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for SessionStream {
    fn drop(&mut self) {
        self.grpc_tokens.remove(&self.grpc_session_token);
        cleanup_session_on_client_disconnect(
            &self.store,
            &self.pairing_store,
            self.pairing_ttl_secs,
            &self.token,
        )
    }
}

#[tonic::async_trait]
impl ClientRelay for ClientRelayService {
    type SubscribeStream = SessionStream;

    /// PC 客户端订阅事件流的入口。
    ///
    /// 每次调用会：
    /// 1. 生成唯一的 UUID v4 令牌
    /// 2. 构造手机扫码 URL（`{public_base_url}/m/{pairing_id}#ps={secret_hex}`）
    /// 3. 在 SessionStore 中注册新会话
    /// 4. 立即推送 `SessionToken` 事件（含令牌和 URL）作为第一条消息
    /// 5. 返回流，后续事件（手机连接/断开、文本）由 WebSocket 处理层通过 `client_tx` 注入
    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let display_addr = display_client_addr(
            request.remote_addr(),
            request.metadata(),
            &self.trusted_proxies,
        );
        if !bool::from(
            request
                .get_ref()
                .auth_token
                .as_bytes()
                .ct_eq(self.config.grpc_auth_token.as_bytes()),
        ) {
            warn!("Rejected unauthenticated gRPC subscribe from {display_addr}");
            return Err(Status::unauthenticated("invalid gRPC auth token"));
        }

        let token = Uuid::new_v4().to_string();
        let grpc_session_token = Uuid::new_v4().to_string();
        let pairing_id = Uuid::parse_str(&request.get_ref().pairing_id)
            .map_err(|_| Status::invalid_argument("pairing_id must be a valid UUID"))?;
        let (url, _) = upsert_pairing(&self.pairing_store, &self.config, pairing_id, &token)?;

        let (tx, rx) = mpsc::channel(32);

        let session = Session {
            token: token.clone(),
            created_at: std::time::Instant::now(),
            scanned: false,
            upgrade_reserved_at: None,
            ws_active: false,
            client_tx: Some(tx.clone()),
            mobile_control_tx: None,
            device_info: None,
            pairing_id: Some(pairing_id),
            grpc_session_token: grpc_session_token.clone(),
        };

        self.store.insert(token.clone(), session);
        info!("Created session token for gRPC client {display_addr}");

        let first_event = ServerEvent {
            event: Some(relay::server_event::Event::SessionToken(SessionToken {
                token: token.clone(),
                url,
            })),
            grpc_session_token: grpc_session_token.clone(),
        };

        tx.send(Ok(first_event))
            .await
            .map_err(|_| Status::internal("channel send failed"))?;

        self.grpc_tokens.insert(grpc_session_token.clone(), ());

        let stream = SessionStream {
            inner: ReceiverStream::new(rx),
            store: self.store.clone(),
            pairing_store: self.pairing_store.clone(),
            pairing_ttl_secs: self.config.pairing_ttl_secs,
            token,
            grpc_session_token,
            grpc_tokens: Arc::clone(&self.grpc_tokens),
        };

        Ok(Response::new(stream))
    }

    async fn ping(&self, request: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        let token = request.into_inner().grpc_session_token;
        if token.is_empty() {
            return Err(Status::unauthenticated("missing grpc_session_token"));
        }
        if !self.grpc_tokens.contains_key(&token) {
            return Err(Status::unauthenticated("invalid grpc_session_token"));
        }
        Ok(Response::new(PingResponse {}))
    }
}

fn cleanup_session_on_client_disconnect(
    store: &SessionStore,
    pairing_store: &PairingStore,
    pairing_ttl_secs: u64,
    token: &str,
) {
    let Some((_, session)) = store.remove(token) else {
        return;
    };

    let successor_token = session
        .pairing_id
        .and_then(|pairing_id| latest_session_token(store, pairing_store, pairing_id));

    if let Some(pairing_id) = session.pairing_id
        && let Some(mut entry) = pairing_store.get_mut(&pairing_id)
    {
        entry.active_session_token = successor_token.clone();
    }

    if let Some(successor_token) = successor_token {
        if let Some(mut successor) = store.get_mut(&successor_token)
            && successor.mobile_control_tx.is_none()
        {
            successor.ws_active = session.ws_active;
            successor.mobile_control_tx = session.mobile_control_tx.clone();
        }
        return;
    }

    if let Some(pairing_id) = session.pairing_id
        && let Some(mut entry) = pairing_store.get_mut(&pairing_id)
    {
        let now = Instant::now();
        entry.online = false;
        entry.last_seen = now;
        entry.expires_at = now + Duration::from_secs(pairing_ttl_secs);
        entry.revision = entry.revision.saturating_add(1);
    }

    let message = serde_json::to_string(&ServerToMobileMessage::ClientDisconnected)
        .unwrap_or_else(|_| r#"{"type":"client_disconnected"}"#.to_string());

    if let Some(control_tx) = session.mobile_control_tx {
        let _ = control_tx.send(message);
    }
}

fn latest_session_token(
    store: &SessionStore,
    pairing_store: &PairingStore,
    pairing_id: Uuid,
) -> Option<String> {
    pairing_store
        .get(&pairing_id)
        .and_then(|entry| entry.active_session_token.clone())
        .filter(|token| store.contains_key(token))
        .or_else(|| {
            store
                .iter()
                .filter_map(|session| {
                    (session.pairing_id == Some(pairing_id))
                        .then(|| (session.key().clone(), session.created_at))
                })
                .max_by_key(|(_, created_at)| *created_at)
                .map(|(token, _)| token)
        })
}

/// 启动 gRPC 服务器并阻塞监听。
///
/// 配置 HTTP/2 keepalive 以维持 PC 客户端的长连接稳定性。
pub async fn serve(
    addr: SocketAddr,
    store: SessionStore,
    pairing_store: PairingStore,
    config: ServerConfig,
) -> anyhow::Result<()> {
    let keepalive_interval = Duration::from_secs(config.grpc_keepalive_interval_secs);
    let keepalive_timeout = Duration::from_secs(config.grpc_keepalive_timeout_secs);

    let svc = ClientRelayServer::new(ClientRelayService::new(store, pairing_store, config)?);

    tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(keepalive_interval))
        .http2_keepalive_timeout(Some(keepalive_timeout))
        .add_service(svc)
        .serve(addr)
        .await?;

    Ok(())
}

fn display_client_addr(
    remote_addr: Option<SocketAddr>,
    metadata: &MetadataMap,
    trusted_proxies: &TrustedProxyCidrs,
) -> String {
    let Some(peer_ip) = remote_addr.map(|addr| addr.ip()) else {
        return format!("{remote_addr:?}");
    };

    trusted_proxies
        .resolve_client_ip(
            peer_ip,
            metadata
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok()),
            metadata
                .get("x-real-ip")
                .and_then(|value| value.to_str().ok()),
        )
        .to_string()
}

fn upsert_pairing(
    pairing_store: &PairingStore,
    config: &ServerConfig,
    pairing_id: Uuid,
    session_token: &str,
) -> Result<(String, [u8; 32]), Status> {
    let now = std::time::Instant::now();
    let public_base_url = config.public_base_url.trim_end_matches('/');
    let ttl = Duration::from_secs(config.pairing_ttl_secs);

    if let Some(mut entry) = pairing_store.get_mut(&pairing_id) {
        entry.online = true;
        entry.last_seen = now;
        entry.expires_at = now + ttl;
        entry.active_session_token = Some(session_token.to_string());
        entry.revision = entry.revision.saturating_add(1);
        let secret = entry.pairing_secret;
        let url = format!(
            "{public_base_url}/m/{pairing_id}#ps={}",
            encode_hex(&secret)
        );
        return Ok((url, secret));
    }

    let secret = random_bytes_32().map_err(|_| Status::internal("failed to generate secret"))?;
    let url = format!(
        "{public_base_url}/m/{pairing_id}#ps={}",
        encode_hex(&secret)
    );
    pairing_store.insert(
        pairing_id,
        PairingEntry {
            pairing_id,
            pairing_secret: secret,
            epoch: 0,
            online: true,
            last_seen: now,
            expires_at: now + ttl,
            active_session_token: Some(session_token.to_string()),
            active_mobile_ws: None,
            revision: 0,
        },
    );
    Ok((url, secret))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{config::ServerConfig, session::new_store},
        tokio::sync::mpsc,
        tokio_stream::StreamExt,
    };

    fn test_config() -> ServerConfig {
        ServerConfig {
            public_base_url: "https://example.com".to_string(),
            grpc_auth_token: "shared-secret-xx".to_string(),
            grpc_port: 50051,
            http_port: 8080,
            http_bind_host: std::net::IpAddr::from([127, 0, 0, 1]),
            grpc_bind_host: std::net::IpAddr::from([127, 0, 0, 1]),
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
        }
    }

    const TEST_PAIRING_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[tokio::test]
    async fn subscribe_rejects_invalid_auth_token() {
        let service = ClientRelayService::new(
            new_store(),
            crate::pairing::new_pairing_store(),
            test_config(),
        )
        .expect("test config should be valid");
        let request = Request::new(SubscribeRequest {
            auth_token: "wrong-token".to_string(),
            pairing_id: TEST_PAIRING_ID.to_string(),
        });

        let error = service
            .subscribe(request)
            .await
            .expect_err("invalid token should be rejected");

        assert_eq!(error.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn subscribe_rejects_invalid_pairing_id() {
        let service = ClientRelayService::new(
            new_store(),
            crate::pairing::new_pairing_store(),
            test_config(),
        )
        .expect("test config should be valid");
        let request = Request::new(SubscribeRequest {
            auth_token: "shared-secret-xx".to_string(),
            pairing_id: String::new(),
        });

        let error = service
            .subscribe(request)
            .await
            .expect_err("empty pairing_id should be rejected");

        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn subscribe_creates_session_and_sends_first_event() {
        let store = new_store();
        let pairing_store = crate::pairing::new_pairing_store();
        let service = ClientRelayService::new(store.clone(), pairing_store, test_config())
            .expect("test config should be valid");
        let request = Request::new(SubscribeRequest {
            auth_token: "shared-secret-xx".to_string(),
            pairing_id: TEST_PAIRING_ID.to_string(),
        });

        let response = service
            .subscribe(request)
            .await
            .expect("valid subscribe should succeed");

        assert_eq!(store.len(), 1);

        let mut stream = response.into_inner();
        let first = stream
            .next()
            .await
            .expect("stream should yield first event")
            .expect("first event should be ok");

        match first.event {
            Some(relay::server_event::Event::SessionToken(token)) => {
                assert!(!token.token.is_empty());
                // URL 格式：{public_base_url}/m/{pairing_id}#ps={secret_hex}
                assert!(
                    token.url.contains(&format!("/m/{TEST_PAIRING_ID}#ps=")),
                    "URL should use new pairing path, got: {}",
                    token.url
                );
                assert!(store.contains_key(&token.token));
            }
            other => panic!("unexpected first event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dropping_grpc_stream_removes_session() {
        let store = new_store();
        let pairing_store = crate::pairing::new_pairing_store();
        let service = ClientRelayService::new(store.clone(), pairing_store.clone(), test_config())
            .expect("test config should be valid");
        let request = Request::new(SubscribeRequest {
            auth_token: "shared-secret-xx".to_string(),
            pairing_id: TEST_PAIRING_ID.to_string(),
        });

        let response = service
            .subscribe(request)
            .await
            .expect("valid subscribe should succeed");

        let mut stream = response.into_inner();
        let first = stream
            .next()
            .await
            .expect("stream should yield first event")
            .expect("first event should be ok");

        let token = match first.event {
            Some(relay::server_event::Event::SessionToken(token)) => token.token,
            other => panic!("unexpected first event: {other:?}"),
        };

        drop(stream);
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(!store.contains_key(&token));
    }

    #[test]
    fn cleanup_session_on_client_disconnect_notifies_mobile_peer() {
        let store = new_store();
        let (client_tx, _client_rx) = mpsc::channel(4);
        let (mobile_control_tx, mut mobile_control_rx) = mpsc::unbounded_channel();

        store.insert(
            "token".to_string(),
            Session {
                token: "token".to_string(),
                created_at: std::time::Instant::now(),
                scanned: true,
                upgrade_reserved_at: None,
                ws_active: true,
                client_tx: Some(client_tx),
                mobile_control_tx: Some(mobile_control_tx),
                device_info: Some("Phone".to_string()),
                pairing_id: None,
                grpc_session_token: "test-grpc-token".to_string(),
            },
        );

        cleanup_session_on_client_disconnect(
            &store,
            &crate::pairing::new_pairing_store(),
            300,
            "token",
        );

        assert!(!store.contains_key("token"));
        assert_eq!(
            mobile_control_rx
                .try_recv()
                .expect("mobile peer should be notified"),
            r#"{"type":"client_disconnected"}"#
        );
    }

    #[test]
    fn display_client_addr_uses_proxy_headers_only_for_trusted_peer() {
        let trusted = TrustedProxyCidrs::parse(&["127.0.0.1/32".to_string()])
            .expect("trusted proxies should parse");
        let mut metadata = MetadataMap::new();
        metadata.insert(
            "x-forwarded-for",
            "198.51.100.10, 203.0.113.7".parse().unwrap(),
        );

        let trusted_display = display_client_addr(
            Some(SocketAddr::from(([127, 0, 0, 1], 50051))),
            &metadata,
            &trusted,
        );
        let untrusted_display = display_client_addr(
            Some(SocketAddr::from(([10, 0, 0, 5], 50051))),
            &metadata,
            &trusted,
        );

        assert_eq!(trusted_display, "203.0.113.7");
        assert_eq!(untrusted_display, "10.0.0.5");
    }

    #[test]
    fn cleanup_session_on_client_disconnect_resets_pairing_expiry_from_disconnect_time() {
        let store = new_store();
        let pairing_store = crate::pairing::new_pairing_store();
        let pairing_id = Uuid::parse_str(TEST_PAIRING_ID).expect("pairing id should parse");
        let old_expires_at = Instant::now() - Duration::from_secs(60);
        pairing_store.insert(
            pairing_id,
            PairingEntry {
                pairing_id,
                pairing_secret: [7_u8; 32],
                epoch: 0,
                online: true,
                last_seen: Instant::now() - Duration::from_secs(120),
                expires_at: old_expires_at,
                active_session_token: Some("token".to_string()),
                active_mobile_ws: None,
                revision: 3,
            },
        );
        store.insert(
            "token".to_string(),
            Session {
                token: "token".to_string(),
                created_at: Instant::now(),
                scanned: true,
                upgrade_reserved_at: None,
                ws_active: false,
                client_tx: None,
                mobile_control_tx: None,
                device_info: None,
                pairing_id: Some(pairing_id),
                grpc_session_token: "grpc-token".to_string(),
            },
        );

        let before = Instant::now();
        cleanup_session_on_client_disconnect(&store, &pairing_store, 300, "token");
        let after = Instant::now();

        let entry = pairing_store
            .get(&pairing_id)
            .expect("pairing should remain");
        assert!(!entry.online);
        assert!(entry.expires_at >= before + Duration::from_secs(300));
        assert!(entry.expires_at <= after + Duration::from_secs(300));
        assert_eq!(entry.active_session_token, None);
        assert!(entry.revision >= 4);
    }

    #[test]
    fn cleanup_session_on_client_disconnect_keeps_pairing_online_when_newer_session_exists() {
        let store = new_store();
        let pairing_store = crate::pairing::new_pairing_store();
        let pairing_id = Uuid::parse_str(TEST_PAIRING_ID).expect("pairing id should parse");
        let (mobile_control_tx, mut mobile_control_rx) = mpsc::unbounded_channel();
        pairing_store.insert(
            pairing_id,
            PairingEntry {
                pairing_id,
                pairing_secret: [9_u8; 32],
                epoch: 0,
                online: true,
                last_seen: Instant::now(),
                expires_at: Instant::now() + Duration::from_secs(300),
                active_session_token: Some("older".to_string()),
                active_mobile_ws: None,
                revision: 0,
            },
        );
        store.insert(
            "older".to_string(),
            Session {
                token: "older".to_string(),
                created_at: Instant::now() - Duration::from_secs(5),
                scanned: true,
                upgrade_reserved_at: None,
                ws_active: true,
                client_tx: None,
                mobile_control_tx: Some(mobile_control_tx.clone()),
                device_info: None,
                pairing_id: Some(pairing_id),
                grpc_session_token: "grpc-older".to_string(),
            },
        );
        store.insert(
            "newer".to_string(),
            Session {
                token: "newer".to_string(),
                created_at: Instant::now(),
                scanned: true,
                upgrade_reserved_at: None,
                ws_active: false,
                client_tx: None,
                mobile_control_tx: None,
                device_info: None,
                pairing_id: Some(pairing_id),
                grpc_session_token: "grpc-newer".to_string(),
            },
        );

        cleanup_session_on_client_disconnect(&store, &pairing_store, 300, "older");

        assert!(!store.contains_key("older"));
        let newer = store.get("newer").expect("newer session should remain");
        assert!(newer.ws_active);
        assert!(
            newer
                .mobile_control_tx
                .as_ref()
                .is_some_and(|current| current.same_channel(&mobile_control_tx))
        );
        drop(newer);
        let entry = pairing_store
            .get(&pairing_id)
            .expect("pairing should remain online");
        assert!(entry.online);
        assert_eq!(entry.active_session_token.as_deref(), Some("newer"));
        assert_eq!(
            mobile_control_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
    }
}
