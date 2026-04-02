// SPDX-License-Identifier: MIT OR Apache-2.0

/// gRPC 接口生成代码，由 `build.rs` 通过 `tonic-prost-build` 从 `proto/relay.proto` 生成。
pub mod relay {
    tonic::include_proto!("relay");
}

use {
    egui::Context,
    relay::{
        ServerEvent,
        SubscribeRequest,
        client_relay_client::ClientRelayClient,
        server_event::Event,
    },
    std::{sync::mpsc, time::Duration},
    tonic::{
        Code,
        transport::{Channel, ClientTlsConfig, Endpoint},
    },
    tracing::{info, warn},
};

const CONNECT_TIMEOUT_SECS: u64 = 10;
const SUBSCRIBE_TIMEOUT_SECS: u64 = 10;
const FIRST_EVENT_TIMEOUT_SECS: u64 = 10;

/// 客户端从 gRPC 流接收到的事件，传递给 UI 线程处理。
#[derive(Debug)]
pub enum ClientEvent {
    /// 本轮连接尝试的目标地址（格式为「正在连接 <address> ...」）。
    ConnectingTarget { message: String },
    /// 当前连接/重连的阶段性进度描述。
    ConnectingStatus { message: String },
    /// 服务端分配了新的会话令牌和对应的手机扫码 URL。
    SessionToken {
        #[allow(dead_code)]
        token: String,
        url: String,
    },
    /// 手机端已通过 WebSocket 连接到服务端并与本次会话绑定。
    MobileConnected { device_info: String },
    /// 手机端 WebSocket 连接已断开。
    MobileDisconnected,
    /// 手机端发送了剪贴板文本内容。
    ClipboardText { content: String },
    /// gRPC 连接发生不可恢复的错误（达到最大重连次数）。
    Error { message: String },
}

/// 在独立线程中启动 gRPC 客户端，并将接收到的事件发送至 `tx`。
///
/// 内部使用独立的 tokio 多线程 runtime 运行异步 gRPC 客户端。
/// 连接断开后按 `reconnect_interval_secs` 等待后自动重连，
/// 直到达到 `max_reconnect_attempts`（0 表示无限重试）。
///
/// 当 `tx` 的接收端已关闭（UI 退出）时，函数立即返回。
pub fn start(
    host: String,
    port: u16,
    auth_token: String,
    reconnect_interval_secs: u64,
    max_reconnect_attempts: u32,
    tx: mpsc::Sender<ClientEvent>,
    repaint_ctx: Context,
) {
    std::thread::spawn(move || {
        let endpoint_config = match build_endpoint_config(&host, port) {
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
            let mut attempts: u32 = 0;

            loop {
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

                let last_failure_message =
                    match connect_client(&endpoint_config, &tx, &repaint_ctx).await {
                    Err(e) => {
                        let message = format!(
                            "连接服务端失败：{e}；将在 {reconnect_interval_secs} 秒后重试。"
                        );
                        warn!("{message}");
                        if !send_event(
                            &tx,
                            &repaint_ctx,
                            ClientEvent::ConnectingStatus {
                                message: message.clone(),
                            },
                        ) {
                            return;
                        }
                        message
                    }
                    Ok(mut client) => {
                        match tokio::time::timeout(
                            Duration::from_secs(SUBSCRIBE_TIMEOUT_SECS),
                            client.subscribe(SubscribeRequest {
                                auth_token: auth_token.clone(),
                            }),
                        )
                        .await
                        {
                            Err(_) => {
                                let message = format!(
                                    "订阅 gRPC 流超时（>{SUBSCRIBE_TIMEOUT_SECS} 秒）；将在 {reconnect_interval_secs} 秒后重试。"
                                );
                                warn!("{message}");
                                if !send_event(
                                    &tx,
                                    &repaint_ctx,
                                    ClientEvent::ConnectingStatus {
                                        message: message.clone(),
                                    },
                                ) {
                                    return;
                                }
                                message
                            }
                            Ok(Err(e)) if e.code() == Code::Unauthenticated => {
                                let _ = send_event(
                                    &tx,
                                    &repaint_ctx,
                                    ClientEvent::Error {
                                        message:
                                            "gRPC 鉴权失败，请检查 client.toml 中的 grpc_auth_token"
                                                .to_string(),
                                    },
                                );
                                return;
                            }
                            Ok(Err(e)) => {
                                let message =
                                    format!("订阅 gRPC 流失败：{e}；将在 {reconnect_interval_secs} 秒后重试。");
                                warn!("{message}");
                                if !send_event(
                                    &tx,
                                    &repaint_ctx,
                                    ClientEvent::ConnectingStatus {
                                        message: message.clone(),
                                    },
                                ) {
                                    return;
                                }
                                message
                            }
                            Ok(Ok(response)) => {
                                attempts = 0;
                                let mut stream = response.into_inner();
                                let first_event_result = loop {
                                    match tokio::time::timeout(
                                        Duration::from_secs(FIRST_EVENT_TIMEOUT_SECS),
                                        stream.message(),
                                    )
                                    .await
                                    {
                                        Err(_) => {
                                            let message = format!(
                                                "等待服务端首条 gRPC 事件超时（>{FIRST_EVENT_TIMEOUT_SECS} 秒）；将在 {reconnect_interval_secs} 秒后重试。"
                                            );
                                            warn!("{message}");
                                            if !send_event(
                                                &tx,
                                                &repaint_ctx,
                                                ClientEvent::ConnectingStatus {
                                                    message: message.clone(),
                                                },
                                            ) {
                                                return;
                                            }
                                            break Err(message);
                                        }
                                        Ok(Err(e)) => {
                                            let message =
                                                format!("gRPC 数据流中断：{e}；正在重新连接。");
                                            warn!("{message}");
                                            if !send_event(
                                                &tx,
                                                &repaint_ctx,
                                                ClientEvent::ConnectingStatus {
                                                    message: message.clone(),
                                                },
                                            ) {
                                                return;
                                            }
                                            break Err(message);
                                        }
                                        Ok(Ok(None)) => {
                                            let message = "gRPC 连接已关闭，正在重新连接。".to_string();
                                            info!("{message}");
                                            if !send_event(
                                                &tx,
                                                &repaint_ctx,
                                                ClientEvent::ConnectingStatus {
                                                    message: message.clone(),
                                                },
                                            ) {
                                                return;
                                            }
                                            break Err(message);
                                        }
                                        Ok(Ok(Some(ServerEvent { event: Some(ev) }))) => {
                                            if let Some(client_event) = map_server_event(ev) {
                                                break Ok(client_event);
                                            }

                                            let message = format!(
                                                "服务端连接已建立，正在等待首条业务事件；将在 {reconnect_interval_secs} 秒后继续观察。"
                                            );
                                            if !send_event(
                                                &tx,
                                                &repaint_ctx,
                                                ClientEvent::ConnectingStatus { message },
                                            ) {
                                                return;
                                            }
                                        }
                                        Ok(Ok(Some(ServerEvent { event: None }))) => {}
                                    }
                                };

                                match first_event_result {
                                    Ok(first_event) => {
                                        if !send_event(&tx, &repaint_ctx, first_event) {
                                            return;
                                        }

                                        loop {
                                            match stream.message().await {
                                                Err(e) => {
                                                    let message =
                                                        format!("gRPC 数据流中断：{e}；正在重新连接。");
                                                    warn!("{message}");
                                                    if !send_event(
                                                        &tx,
                                                        &repaint_ctx,
                                                        ClientEvent::ConnectingStatus {
                                                            message: message.clone(),
                                                        },
                                                    ) {
                                                        return;
                                                    }
                                                    break message;
                                                }
                                                Ok(None) => {
                                                    let message =
                                                        "gRPC 连接已关闭，正在重新连接。".to_string();
                                                    info!("{message}");
                                                    if !send_event(
                                                        &tx,
                                                        &repaint_ctx,
                                                        ClientEvent::ConnectingStatus {
                                                            message: message.clone(),
                                                        },
                                                    ) {
                                                        return;
                                                    }
                                                    break message;
                                                }
                                                Ok(Some(ServerEvent { event: Some(ev) })) => {
                                                    let Some(client_event) = map_server_event(ev)
                                                    else {
                                                        continue;
                                                    };
                                                    if !send_event(&tx, &repaint_ctx, client_event) {
                                                        // UI 线程已退出，终止 gRPC 循环
                                                        return;
                                                    }
                                                }
                                                Ok(Some(ServerEvent { event: None })) => {}
                                            }
                                        }
                                    }
                                    Err(message) => message,
                                }
                            }
                        }
                    }
                };

                attempts += 1;
                if max_reconnect_attempts > 0 && attempts >= max_reconnect_attempts {
                    let message = format!(
                        "连接服务端失败，已重试 {max_reconnect_attempts} 次。最近一次错误：{last_failure_message}"
                    );
                    let _ = send_event(
                        &tx,
                        &repaint_ctx,
                        ClientEvent::Error {
                            message,
                        },
                    );
                    return;
                }

                tokio::time::sleep(Duration::from_secs(reconnect_interval_secs)).await;
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
}
