// SPDX-License-Identifier: MIT OR Apache-2.0

/// gRPC 接口生成代码，由 `build.rs` 通过 `tonic-prost-build` 从 `proto/relay.proto` 生成。
pub mod relay {
    tonic::include_proto!("relay");
}

use {
    egui::Context,
    relay::{
        PingRequest,
        ServerEvent,
        SubscribeRequest,
        client_relay_client::ClientRelayClient,
        server_event::Event,
    },
    std::{cmp, sync::mpsc, time::Duration},
    tokio_util::sync::CancellationToken,
    tonic::{
        Code,
        Status,
        transport::{Channel, ClientTlsConfig, Endpoint},
    },
    tracing::{info, warn},
};

const CONNECT_TIMEOUT_SECS: u64 = 10;
const SUBSCRIBE_TIMEOUT_SECS: u64 = 10;
const FIRST_EVENT_TIMEOUT_SECS: u64 = 10;
const HEARTBEAT_RETRY_INITIAL_SECS: u64 = 1;
const HEARTBEAT_RETRY_MAX_SECS: u64 = 60;
const HEARTBEAT_UNHEALTHY_THRESHOLD: u32 = 5;

#[derive(Debug, Clone)]
/// 启动 gRPC 客户端连接时使用的参数集合。
pub struct StartOptions {
    pub host: String,
    pub port: u16,
    pub auth_token: String,
    pub pairing_id: String,
    pub heartbeat_interval_secs: u64,
}

/// 客户端从 gRPC 流接收到的事件，传递给 UI 线程处理。
#[derive(Debug)]
pub enum ClientEvent {
    /// 本轮连接尝试的目标地址（格式为「正在连接 <address> ...」）。
    ConnectingTarget {
        message: String,
    },
    /// 当前连接阶段性进度描述。
    ConnectingStatus {
        message: String,
    },
    /// 服务端分配了新的会话令牌和对应的手机扫码 URL。
    SessionToken {
        #[allow(dead_code)]
        token: String,
        url: String,
    },
    GrpcSessionToken {
        token: String,
    },
    /// 手机端已通过 WebSocket 连接到服务端并与本次会话绑定。
    MobileConnected {
        device_info: String,
    },
    /// 手机端 WebSocket 连接已断开。
    MobileDisconnected,
    /// 手机端发送了剪贴板文本内容。
    ClipboardText {
        content: String,
    },
    /// gRPC 连接发生不可恢复的错误。
    Error {
        message: String,
    },
}

/// 在独立线程中启动 gRPC 客户端，进行单次连接尝试，将接收到的事件发送至 `tx`。
///
/// 内部使用独立的 tokio 单线程 runtime 运行异步 gRPC 客户端。
/// 连接关闭或出错后函数返回，`tx` 随之丢弃；UI 通过检测通道断开来感知连接结束。
///
/// 当 `tx` 的接收端已关闭（UI 退出）时，函数立即返回。
pub fn start(options: StartOptions, tx: mpsc::Sender<ClientEvent>, repaint_ctx: Context) {
    std::thread::spawn(move || {
        let endpoint_config = match build_endpoint_config(&options.host, options.port) {
            Ok(config) => config,
            Err(err) => {
                let _ = send_event(&tx, &repaint_ctx, ClientEvent::Error { message: err });
                return;
            }
        };

        let rt = match tokio::runtime::Builder::new_current_thread()
            .thread_name("qr-paste-grpc")
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                let _ = send_event(
                    &tx,
                    &repaint_ctx,
                    ClientEvent::Error {
                        message: format!("客户端异步运行时初始化失败：{err}"),
                    },
                );
                return;
            }
        };

        rt.block_on(async move {
            let connecting_message = format!("正在连接 {} ...", endpoint_config.endpoint);
            if !send_event(
                &tx,
                &repaint_ctx,
                ClientEvent::ConnectingTarget {
                    message: connecting_message.clone(),
                },
            ) {
                return;
            }
            info!("{connecting_message}");

            let mut client = match connect_client(&endpoint_config, &tx, &repaint_ctx).await {
                Err(e) => {
                    let message = format!("连接服务端失败：{e}");
                    warn!("{message}");
                    let _ = send_event(
                        &tx,
                        &repaint_ctx,
                        ClientEvent::ConnectingStatus {
                            message: message.clone(),
                        },
                    );
                    return;
                }
                Ok(client) => client,
            };

            match tokio::time::timeout(
                Duration::from_secs(SUBSCRIBE_TIMEOUT_SECS),
                client.subscribe(SubscribeRequest {
                    auth_token: options.auth_token.clone(),
                    pairing_id: options.pairing_id.clone(),
                }),
            )
            .await
            {
                Err(_) => {
                    let message =
                        format!("订阅 gRPC 流超时（>{SUBSCRIBE_TIMEOUT_SECS} 秒）。");
                    warn!("{message}");
                    let _ = send_event(
                        &tx,
                        &repaint_ctx,
                        ClientEvent::ConnectingStatus { message },
                    );
                }
                Ok(Err(e)) if e.code() == Code::Unauthenticated => {
                    let _ = send_event(
                        &tx,
                        &repaint_ctx,
                        ClientEvent::Error {
                            message: "gRPC 鉴权失败，请检查 client.toml 中的 grpc_auth_token"
                                .to_string(),
                        },
                    );
                }
                Ok(Err(e)) => {
                    let message = format!("订阅 gRPC 流失败：{e}");
                    warn!("{message}");
                    let _ = send_event(
                        &tx,
                        &repaint_ctx,
                        ClientEvent::ConnectingStatus { message },
                    );
                }
                Ok(Ok(response)) => {
                    let mut stream = response.into_inner();
                    let first_event_result: Result<(ClientEvent, String), ()> = loop {
                        match tokio::time::timeout(
                            Duration::from_secs(FIRST_EVENT_TIMEOUT_SECS),
                            stream.message(),
                        )
                        .await
                        {
                            Err(_) => {
                                let message = format!(
                                    "等待服务端首条 gRPC 事件超时（>{FIRST_EVENT_TIMEOUT_SECS} 秒）。"
                                );
                                warn!("{message}");
                                let _ = send_event(
                                    &tx,
                                    &repaint_ctx,
                                    ClientEvent::ConnectingStatus { message: message.clone() },
                                );
                                break Err(());
                            }
                            Ok(Err(e)) => {
                                let message = format!("gRPC 数据流中断：{e}");
                                warn!("{message}");
                                let _ = send_event(
                                    &tx,
                                    &repaint_ctx,
                                    ClientEvent::ConnectingStatus { message },
                                );
                                break Err(());
                            }
                            Ok(Ok(None)) => {
                                info!("gRPC 连接已关闭。");
                                break Err(());
                            }
                            Ok(Ok(Some(ServerEvent { event: Some(ev), grpc_session_token }))) => {
                                if let Some(client_event) = map_server_event(ev) {
                                    break Ok((client_event, grpc_session_token));
                                }
                            }
                            Ok(Ok(Some(ServerEvent { event: None, .. }))) => {}
                        }
                    };

                    if let Ok((first_event, grpc_session_token)) = first_event_result {
                        if !grpc_session_token.is_empty()
                            && !send_event(
                                &tx,
                                &repaint_ctx,
                                ClientEvent::GrpcSessionToken {
                                    token: grpc_session_token.clone(),
                                },
                            )
                        {
                            return;
                        }

                        if !send_event(&tx, &repaint_ctx, first_event) {
                            return;
                        }

                        let heartbeat_cancellation = (options.heartbeat_interval_secs > 0
                            && !grpc_session_token.is_empty())
                        .then(|| {
                            spawn_heartbeat_task(
                                client.clone(),
                                grpc_session_token.clone(),
                                options.heartbeat_interval_secs,
                            )
                        });

                        loop {
                            match stream.message().await {
                                Err(e) => {
                                    let message = format!("gRPC 数据流中断：{e}");
                                    warn!("{message}");
                                    let _ = send_event(
                                        &tx,
                                        &repaint_ctx,
                                        ClientEvent::ConnectingStatus { message },
                                    );
                                    break;
                                }
                                Ok(None) => {
                                    info!("gRPC 连接已关闭。");
                                    break;
                                }
                                Ok(Some(ServerEvent { event: Some(ev), .. })) => {
                                    let Some(client_event) = map_server_event(ev) else {
                                        continue;
                                    };
                                    if !send_event(&tx, &repaint_ctx, client_event) {
                                        return;
                                    }
                                }
                                Ok(Some(ServerEvent { event: None, .. })) => {}
                            }
                        }

                        if let Some(cancellation) = heartbeat_cancellation {
                            cancellation.cancel();
                        }
                    }
                }
            }
        });
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrpcEndpointConfig {
    endpoint: String,
    tls_domain: Option<String>,
}

fn build_endpoint_config(host: &str, port: u16) -> Result<GrpcEndpointConfig, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("client.toml 中的 server_host 不能为空。".to_string());
    }

    let (scheme, authority) = if let Some(rest) = host.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = host.strip_prefix("http://") {
        ("http", rest)
    } else {
        if host.contains("://") {
            return Err(
                "client.toml 中的 server_host 仅支持 http:// 或 https:// 协议。".to_string(),
            );
        }
        ("http", host)
    };

    let authority = authority.trim_end_matches('/');
    if authority.is_empty() {
        return Err("client.toml 中的 server_host 缺少主机名。".to_string());
    }
    if authority.contains('/') || authority.contains('?') || authority.contains('#') {
        return Err(
            "client.toml 中的 server_host 只能填写主机名、IP，或不带路径的 http(s):// 地址。"
                .to_string(),
        );
    }

    let authority_with_port = if has_explicit_port(authority) {
        authority.to_string()
    } else {
        format!("{}:{}", format_authority_host(authority), port)
    };

    let tls_domain = if scheme == "https" {
        Some(extract_tls_domain(authority)?)
    } else {
        None
    };

    Ok(GrpcEndpointConfig {
        endpoint: format!("{scheme}://{authority_with_port}"),
        tls_domain,
    })
}

fn has_explicit_port(authority: &str) -> bool {
    if authority.starts_with('[') {
        return authority.contains(":]") || authority.contains("]:");
    }

    authority
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && !host.contains(':') && !port.is_empty())
}

fn format_authority_host(authority: &str) -> String {
    if authority.contains(':') && !authority.starts_with('[') {
        format!("[{authority}]")
    } else {
        authority.to_string()
    }
}

fn extract_tls_domain(authority: &str) -> Result<String, String> {
    if authority.starts_with('[') {
        let Some(end) = authority.find(']') else {
            return Err("client.toml 中的 server_host IPv6 地址格式无效。".to_string());
        };
        return Ok(authority[1..end].to_string());
    }

    if let Some((host, port)) = authority.rsplit_once(':')
        && !host.is_empty()
        && !host.contains(':')
        && !port.is_empty()
    {
        return Ok(host.to_string());
    }

    Ok(authority.to_string())
}

async fn connect_client(
    endpoint_config: &GrpcEndpointConfig,
    tx: &mpsc::Sender<ClientEvent>,
    repaint_ctx: &Context,
) -> Result<ClientRelayClient<Channel>, String> {
    let mut endpoint = Endpoint::from_shared(endpoint_config.endpoint.clone())
        .map_err(|err| format!("gRPC 地址无效：{err}"))?
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS));

    if let Some(domain) = &endpoint_config.tls_domain {
        let tls_config = ClientTlsConfig::new()
            .with_webpki_roots()
            .domain_name(domain.clone())
            .timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS));
        endpoint = endpoint
            .tls_config(tls_config)
            .map_err(|err| format!("gRPC TLS 配置失败：{err}"))?;
    }

    let connect_message = format!("正在建立到 {} 的网络连接...", endpoint_config.endpoint);
    info!("{connect_message}");
    if !send_event(
        tx,
        repaint_ctx,
        ClientEvent::ConnectingStatus {
            message: connect_message,
        },
    ) {
        return Err("UI 通道已关闭。".to_string());
    }

    let channel = tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS),
        endpoint.connect(),
    )
    .await
    .map_err(|_| format!("连接服务端超时（>{CONNECT_TIMEOUT_SECS} 秒）"))?
    .map_err(|err| format!("{err}"))?;

    Ok(ClientRelayClient::new(channel))
}

fn send_event(tx: &mpsc::Sender<ClientEvent>, repaint_ctx: &Context, event: ClientEvent) -> bool {
    if tx.send(event).is_err() {
        return false;
    }
    repaint_ctx.request_repaint();
    true
}

fn map_server_event(event: Event) -> Option<ClientEvent> {
    match event {
        Event::SessionToken(t) => Some(ClientEvent::SessionToken {
            token: t.token,
            url: t.url,
        }),
        Event::MobileConnected(m) => Some(ClientEvent::MobileConnected {
            device_info: m.device_info,
        }),
        Event::MobileDisconnected(_) => Some(ClientEvent::MobileDisconnected),
        Event::ClipboardText(c) => Some(ClientEvent::ClipboardText { content: c.content }),
        Event::Ping(_) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatErrorKind {
    Unauthenticated,
    Unimplemented,
    Transient,
}

#[derive(Debug, Clone)]
struct RetryBackoff {
    base: Duration,
    next: Duration,
    max: Duration,
    seed: u64,
    attempt: u32,
}

impl RetryBackoff {
    fn new(base: Duration, max: Duration, seed: u64) -> Self {
        let max = cmp::max(max, base);
        Self {
            base,
            next: base,
            max,
            seed,
            attempt: 0,
        }
    }

    fn reset(&mut self) {
        self.next = self.base;
        self.attempt = 0;
    }

    fn next_delay(&mut self) -> Duration {
        let current = self.next;
        let delay = jitter_duration(current, self.seed, self.attempt);
        self.next = cmp::min(current.mul_f32(1.2), self.max);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }
}

fn spawn_heartbeat_task(
    mut client: ClientRelayClient<Channel>,
    grpc_session_token: String,
    heartbeat_interval_secs: u64,
) -> CancellationToken {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();

    tokio::spawn(async move {
        let heartbeat_interval = Duration::from_secs(heartbeat_interval_secs.max(1));
        let max_interval = Duration::from_secs(HEARTBEAT_RETRY_MAX_SECS);
        let mut consecutive_failures = 0u32;
        let mut backoff = RetryBackoff::new(
            Duration::from_secs(HEARTBEAT_RETRY_INITIAL_SECS),
            max_interval,
            stable_token_seed(&grpc_session_token),
        );

        loop {
            let sleep_duration = if consecutive_failures > 0 {
                backoff.next_delay()
            } else {
                heartbeat_interval
            };

            tokio::select! {
                _ = task_cancellation.cancelled() => break,
                _ = tokio::time::sleep(sleep_duration) => {}
            }

            match client
                .ping(PingRequest {
                    grpc_session_token: grpc_session_token.clone(),
                })
                .await
            {
                Ok(_) => {
                    if consecutive_failures > 0 {
                        info!("gRPC heartbeat recovered.");
                    }
                    consecutive_failures = 0;
                    backoff.reset();
                }
                Err(status) => match classify_heartbeat_error(&status) {
                    HeartbeatErrorKind::Unimplemented => {
                        warn!(
                            code = ?status.code(),
                            message = %status.message(),
                            "gRPC heartbeat ping is not implemented by the server; stopping heartbeat task."
                        );
                        break;
                    }
                    HeartbeatErrorKind::Unauthenticated => {
                        warn!(
                            code = ?status.code(),
                            message = %status.message(),
                            "gRPC heartbeat unauthenticated; stopping heartbeat task until reconnect."
                        );
                        break;
                    }
                    HeartbeatErrorKind::Transient => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        if consecutive_failures >= HEARTBEAT_UNHEALTHY_THRESHOLD {
                            warn!(
                                failures = consecutive_failures,
                                code = ?status.code(),
                                message = %status.message(),
                                "gRPC heartbeat marked the connection unhealthy after repeated transient failures."
                            );
                        } else {
                            warn!(
                                failures = consecutive_failures,
                                code = ?status.code(),
                                message = %status.message(),
                                "gRPC heartbeat failed transiently; retrying with backoff."
                            );
                        }
                    }
                },
            }
        }
    });

    cancellation
}

fn classify_heartbeat_error(status: &Status) -> HeartbeatErrorKind {
    match status.code() {
        Code::Unimplemented => HeartbeatErrorKind::Unimplemented,
        Code::Unauthenticated => HeartbeatErrorKind::Unauthenticated,
        _ => HeartbeatErrorKind::Transient,
    }
}

fn stable_token_seed(token: &str) -> u64 {
    token.bytes().fold(0xcbf29ce484222325, |acc, byte| {
        (acc ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn jitter_duration(base: Duration, seed: u64, attempt: u32) -> Duration {
    let base_ms = base.as_millis();
    if base_ms == 0 {
        return Duration::ZERO;
    }

    let jitter_span_ms = (base_ms / 5) as u64;
    let mixed = seed ^ u64::from(attempt).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let lower_bound_ms = base_ms.saturating_sub(u128::from(jitter_span_ms)) as u64;
    let range_ms = jitter_span_ms.saturating_mul(2);
    let offset_ms = if range_ms == 0 {
        0
    } else {
        mixed % range_ms.saturating_add(1)
    };
    Duration::from_millis(lower_bound_ms.saturating_add(offset_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_host_builds_http_endpoint() {
        let config = build_endpoint_config("relay.example.com", 50051).expect("plain host");
        assert_eq!(
            config,
            GrpcEndpointConfig {
                endpoint: "http://relay.example.com:50051".to_string(),
                tls_domain: None,
            }
        );
    }

    #[test]
    fn https_host_builds_tls_endpoint() {
        let config = build_endpoint_config("https://cg.keivry.ren", 443).expect("https host");
        assert_eq!(
            config,
            GrpcEndpointConfig {
                endpoint: "https://cg.keivry.ren:443".to_string(),
                tls_domain: Some("cg.keivry.ren".to_string()),
            }
        );
    }

    #[test]
    fn explicit_port_in_server_host_is_preserved() {
        let config = build_endpoint_config("https://cg.keivry.ren:8443", 443)
            .expect("explicit port should win");
        assert_eq!(config.endpoint, "https://cg.keivry.ren:8443");
        assert_eq!(config.tls_domain.as_deref(), Some("cg.keivry.ren"));
    }

    #[test]
    fn path_in_server_host_is_rejected() {
        let error = build_endpoint_config("https://cg.keivry.ren/grpc", 443)
            .expect_err("path should be rejected");
        assert!(error.contains("server_host"));
    }

    #[test]
    fn classify_heartbeat_error_distinguishes_status_codes() {
        assert_eq!(
            classify_heartbeat_error(&Status::new(Code::Unimplemented, "missing rpc")),
            HeartbeatErrorKind::Unimplemented
        );
        assert_eq!(
            classify_heartbeat_error(&Status::new(Code::Unauthenticated, "bad token")),
            HeartbeatErrorKind::Unauthenticated
        );
        assert_eq!(
            classify_heartbeat_error(&Status::new(Code::Unavailable, "retry later")),
            HeartbeatErrorKind::Transient
        );
    }

    #[test]
    fn retry_backoff_grows_with_jitter_and_respects_cap() {
        let mut backoff = RetryBackoff::new(Duration::from_secs(1), Duration::from_secs(60), 7);

        let first = backoff.next_delay();
        let second = backoff.next_delay();
        let third = backoff.next_delay();
        let fourth = backoff.next_delay();

        assert!(first >= Duration::from_millis(800));
        assert!(first <= Duration::from_millis(1200));
        assert!(second >= Duration::from_millis(960));
        assert!(second <= Duration::from_millis(1440));
        assert!(third >= Duration::from_millis(1152));
        assert!(third <= Duration::from_millis(1728));
        assert!(fourth >= Duration::from_millis(1382));
        assert!(fourth <= Duration::from_millis(2074));

        backoff.reset();
        let reset = backoff.next_delay();
        assert!(reset >= Duration::from_millis(800));
        assert!(reset <= Duration::from_millis(1200));
    }
}
