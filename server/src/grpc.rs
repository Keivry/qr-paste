// SPDX-License-Identifier: MIT OR Apache-2.0

/// gRPC 接口生成代码，由 `build.rs` 通过 `tonic-prost-build` 从 `proto/relay.proto` 生成。
pub mod relay {
    tonic::include_proto!("relay");
}

use {
    crate::{
        config::ServerConfig,
        session::{Session, SessionStore},
    },
    common::ServerToMobileMessage,
    futures::Stream,
    relay::{
        ServerEvent,
        SessionToken,
        SubscribeRequest,
        client_relay_server::{ClientRelay, ClientRelayServer},
    },
    std::{net::SocketAddr, time::Duration},
    subtle::ConstantTimeEq,
    tokio::sync::mpsc,
    tokio_stream::wrappers::ReceiverStream,
    tonic::{Request, Response, Status},
    tracing::{info, warn},
    uuid::Uuid,
};

/// gRPC `ClientRelay` 服务实现。
///
/// 每个 PC 客户端调用 `Subscribe` 时创建一个新的会话令牌，
/// 并返回一个长连接的服务端推送流（server-streaming）。
pub struct ClientRelayService {
    store: SessionStore,
    config: ServerConfig,
}

impl ClientRelayService {
    /// 创建新的服务实例，注入共享的会话存储和配置。
    pub fn new(store: SessionStore, config: ServerConfig) -> Self { Self { store, config } }
}

pub struct SessionStream {
    inner: ReceiverStream<Result<ServerEvent, Status>>,
    store: SessionStore,
    token: String,
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
    fn drop(&mut self) { cleanup_session_on_client_disconnect(&self.store, &self.token) }
}

#[tonic::async_trait]
impl ClientRelay for ClientRelayService {
    type SubscribeStream = SessionStream;

    /// PC 客户端订阅事件流的入口。
    ///
    /// 每次调用会：
    /// 1. 生成唯一的 UUID v4 令牌
    /// 2. 构造手机扫码 URL（`{public_base_url}/s/{token}`）
    /// 3. 在 SessionStore 中注册新会话
    /// 4. 立即推送 `SessionToken` 事件（含令牌和 URL）作为第一条消息
    /// 5. 返回流，后续事件（手机连接/断开、文本）由 WebSocket 处理层通过 `client_tx` 注入
    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let remote_addr = request.remote_addr();
        let display_addr: String = if self.config.behind_trusted_proxy {
            let xff = request
                .metadata()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(',').next())
                .map(str::trim);
            let xri = request
                .metadata()
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(str::trim);
            xff.or(xri)
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| format!("{remote_addr:?}"))
        } else {
            format!("{remote_addr:?}")
        };
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
        let url = format!("{}/s/{}", self.config.public_base_url, token);

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
        };

        self.store.insert(token.clone(), session);
        info!("Created session token for gRPC client {display_addr}");

        let first_event = ServerEvent {
            event: Some(relay::server_event::Event::SessionToken(SessionToken {
                token: token.clone(),
                url,
            })),
        };

        tx.send(Ok(first_event))
            .await
            .map_err(|_| Status::internal("channel send failed"))?;

        let stream = SessionStream {
            inner: ReceiverStream::new(rx),
            store: self.store.clone(),
            token,
        };

        Ok(Response::new(stream))
    }
}

fn cleanup_session_on_client_disconnect(store: &SessionStore, token: &str) {
    let Some((_, session)) = store.remove(token) else {
        return;
    };

    let message = serde_json::to_string(&ServerToMobileMessage::ClientDisconnected)
        .unwrap_or_else(|_| r#"{"type":"client_disconnected"}"#.to_string());

    if let Some(control_tx) = session.mobile_control_tx {
        let _ = control_tx.send(message);
    }
}

/// 启动 gRPC 服务器并阻塞监听。
///
/// 配置 HTTP/2 keepalive 以维持 PC 客户端的长连接稳定性。
pub async fn serve(
    addr: SocketAddr,
    store: SessionStore,
    config: ServerConfig,
) -> anyhow::Result<()> {
    let keepalive_interval = Duration::from_secs(config.grpc_keepalive_interval_secs);
    let keepalive_timeout = Duration::from_secs(config.grpc_keepalive_timeout_secs);

    let svc = ClientRelayServer::new(ClientRelayService::new(store, config));

    tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(keepalive_interval))
        .http2_keepalive_timeout(Some(keepalive_timeout))
        .add_service(svc)
        .serve(addr)
        .await?;

    Ok(())
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

    #[tokio::test]
    async fn subscribe_rejects_invalid_auth_token() {
        let service = ClientRelayService::new(new_store(), test_config());
        let request = Request::new(SubscribeRequest {
            auth_token: "wrong-token".to_string(),
        });

        let error = service
            .subscribe(request)
            .await
            .expect_err("invalid token should be rejected");

        assert_eq!(error.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn subscribe_creates_session_and_sends_first_event() {
        let store = new_store();
        let service = ClientRelayService::new(store.clone(), test_config());
        let request = Request::new(SubscribeRequest {
            auth_token: "shared-secret".to_string(),
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
                assert!(token.url.ends_with(&format!("/s/{}", token.token)));
                assert!(store.contains_key(&token.token));
            }
            other => panic!("unexpected first event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dropping_grpc_stream_removes_session() {
        let store = new_store();
        let service = ClientRelayService::new(store.clone(), test_config());
        let request = Request::new(SubscribeRequest {
            auth_token: "shared-secret".to_string(),
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
            },
        );

        cleanup_session_on_client_disconnect(&store, "token");

        assert!(!store.contains_key("token"));
        assert_eq!(
            mobile_control_rx
                .try_recv()
                .expect("mobile peer should be notified"),
            r#"{"type":"client_disconnected"}"#
        );
    }
}
