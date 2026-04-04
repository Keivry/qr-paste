// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    ipnet::IpNet,
    serde::Deserialize,
    std::{fmt, fs, net::IpAddr, sync::Arc},
    url::{Host, Url},
};

#[derive(Clone)]
pub struct TrustedProxyCidrs {
    networks: Arc<[IpNet]>,
}

impl Default for TrustedProxyCidrs {
    fn default() -> Self {
        Self {
            networks: Arc::from([]),
        }
    }
}

impl fmt::Debug for TrustedProxyCidrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TrustedProxyCidrs")
            .field(&self.networks)
            .finish()
    }
}

impl TrustedProxyCidrs {
    pub fn parse(values: &[String]) -> anyhow::Result<Self> {
        let mut networks = Vec::with_capacity(values.len());
        for value in values {
            let cidr = value.trim();
            if cidr.is_empty() {
                anyhow::bail!("trusted_proxy_cidrs 不能包含空字符串");
            }
            let network = cidr.parse::<IpNet>().map_err(|err| {
                anyhow::anyhow!("trusted_proxy_cidrs 包含非法 CIDR `{cidr}`：{err}")
            })?;
            networks.push(network);
        }
        Ok(Self {
            networks: Arc::from(networks),
        })
    }

    pub fn trusts_peer(&self, peer_ip: IpAddr) -> bool {
        self.networks
            .iter()
            .any(|network| network.contains(&peer_ip))
    }

    pub fn resolve_client_ip(
        &self,
        peer_ip: IpAddr,
        x_forwarded_for: Option<&str>,
        x_real_ip: Option<&str>,
    ) -> IpAddr {
        if !self.trusts_peer(peer_ip) {
            return peer_ip;
        }

        match x_forwarded_for {
            Some(value) => parse_rightmost_forwarded_ip(value).unwrap_or(peer_ip),
            None => parse_ip_header(x_real_ip).unwrap_or(peer_ip),
        }
    }
}

/// 服务端配置，从 `server.toml` 中读取。
///
/// 所有字段均有默认值（`public_base_url` 和 `grpc_auth_token` 除外）。
/// 参考 `server.example.toml` 查看每个字段的详细说明。
#[derive(Clone, Deserialize)]
#[allow(dead_code)]
pub struct ServerConfig {
    /// 【必填】服务端对外可访问的基础 URL，不含尾部斜线。
    /// 例如 `"https://example.com"`，用于拼接二维码链接。
    pub public_base_url: String,
    /// 【必填】gRPC 认证令牌，客户端配置中的 `grpc_auth_token` 必须与此值完全匹配。
    pub grpc_auth_token: String,
    /// gRPC 监听端口，PC 客户端连接到此端口。默认 50051。
    #[serde(default = "default_grpc_port")]
    pub grpc_port: u16,
    /// HTTP/WebSocket 监听端口，手机浏览器访问此端口。默认 8080。
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// HTTP/WebSocket 监听地址。默认 `127.0.0.1`，便于置于反向代理之后安全暴露。
    #[serde(default = "default_bind_host")]
    pub http_bind_host: IpAddr,
    /// gRPC 监听地址。默认 `127.0.0.1`，避免裸明文 gRPC 直接暴露到公网。
    #[serde(default = "default_bind_host")]
    pub grpc_bind_host: IpAddr,
    /// 令牌有效期（秒）。超过此时间未使用的令牌会在清理时被删除。默认 300。
    #[serde(default = "default_token_expiry_secs")]
    pub token_expiry_secs: u64,
    /// pairing 记录 TTL（秒）。客户端断线后保留该时长以支持免扫码重连。默认 86400。
    #[serde(default = "default_pairing_ttl_secs")]
    pub pairing_ttl_secs: u64,
    /// 过期令牌清理任务的执行间隔（秒）。默认 60。
    #[serde(default = "default_token_cleanup_interval_secs")]
    pub token_cleanup_interval_secs: u64,
    /// 每个 IP 每分钟允许建立的最大 WebSocket 连接次数。默认 10。超出后返回 429。
    #[serde(default = "default_ws_rate_limit_per_ip_per_min")]
    pub ws_rate_limit_per_ip_per_min: u32,
    /// 每个 IP 每分钟允许的最大 HTTP 请求次数（不含 WebSocket 升级）。默认 20。超出后返回 429。
    #[serde(default = "default_http_rate_limit_per_ip_per_min")]
    pub http_rate_limit_per_ip_per_min: u32,
    /// 服务端允许同时存在的最大 WebSocket 连接数。默认 100。超出后拒绝新连接。
    #[serde(default = "default_max_ws_connections")]
    pub max_ws_connections: usize,
    /// 单条 WebSocket 消息允许的最大字节数。超过此大小的消息会导致连接断开。默认 65536（64 KB）。
    #[serde(default = "default_max_message_size_bytes")]
    pub max_message_size_bytes: usize,
    /// WebSocket 连接最大空闲时间（秒）。超时后服务端主动断开连接。默认 90。
    #[serde(default = "default_ws_idle_timeout_secs")]
    pub ws_idle_timeout_secs: u64,
    /// gRPC HTTP/2 keepalive 心跳发送间隔（秒）。默认 60。
    #[serde(default = "default_grpc_keepalive_interval_secs")]
    pub grpc_keepalive_interval_secs: u64,
    /// gRPC HTTP/2 keepalive 超时（秒）。超过此时间未收到 ACK 则断开连接。默认 20。
    #[serde(default = "default_grpc_keepalive_timeout_secs")]
    pub grpc_keepalive_timeout_secs: u64,
    /// 日志级别：`trace` / `debug` / `info` / `warn` / `error`。默认 `"info"`。
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 可信反向代理 CIDR 白名单。默认空列表，表示不信任任何代理头。
    ///
    /// 仅当 TCP 对端 IP 落在这些 CIDR 内时，才会解析 `X-Forwarded-For`
    /// 最右侧 IP；若该头缺失，则尝试 `X-Real-IP`。若头部缺失或格式非法，
    /// 则回退到 TCP 对端 IP。
    #[serde(default)]
    pub trusted_proxy_cidrs: Vec<String>,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("public_base_url", &self.public_base_url)
            .field("grpc_auth_token", &"[REDACTED]")
            .field("grpc_port", &self.grpc_port)
            .field("http_port", &self.http_port)
            .field("http_bind_host", &self.http_bind_host)
            .field("grpc_bind_host", &self.grpc_bind_host)
            .field("token_expiry_secs", &self.token_expiry_secs)
            .field("pairing_ttl_secs", &self.pairing_ttl_secs)
            .field(
                "token_cleanup_interval_secs",
                &self.token_cleanup_interval_secs,
            )
            .field(
                "ws_rate_limit_per_ip_per_min",
                &self.ws_rate_limit_per_ip_per_min,
            )
            .field(
                "http_rate_limit_per_ip_per_min",
                &self.http_rate_limit_per_ip_per_min,
            )
            .field("max_ws_connections", &self.max_ws_connections)
            .field("max_message_size_bytes", &self.max_message_size_bytes)
            .field("ws_idle_timeout_secs", &self.ws_idle_timeout_secs)
            .field(
                "grpc_keepalive_interval_secs",
                &self.grpc_keepalive_interval_secs,
            )
            .field(
                "grpc_keepalive_timeout_secs",
                &self.grpc_keepalive_timeout_secs,
            )
            .field("log_level", &self.log_level)
            .field("trusted_proxy_cidrs", &self.trusted_proxy_cidrs)
            .finish()
    }
}

fn default_grpc_port() -> u16 { 50051 }
fn default_http_port() -> u16 { 8080 }
fn default_bind_host() -> IpAddr { IpAddr::from([127, 0, 0, 1]) }
fn default_token_expiry_secs() -> u64 { 300 }
fn default_pairing_ttl_secs() -> u64 { 86400 }
fn default_token_cleanup_interval_secs() -> u64 { 60 }
fn default_ws_rate_limit_per_ip_per_min() -> u32 { 10 }
fn default_http_rate_limit_per_ip_per_min() -> u32 { 20 }
fn default_max_ws_connections() -> usize { 100 }
fn default_max_message_size_bytes() -> usize { 65536 }
fn default_ws_idle_timeout_secs() -> u64 { 90 }
fn default_grpc_keepalive_interval_secs() -> u64 { 60 }
fn default_grpc_keepalive_timeout_secs() -> u64 { 20 }
fn default_log_level() -> String { "info".to_string() }

impl ServerConfig {
    /// 从当前工作目录下的 `server.toml` 加载配置。
    ///
    /// # Errors
    ///
    /// - 文件不存在时返回错误
    /// - TOML 解析失败时返回错误
    pub fn load() -> anyhow::Result<Self> {
        let content = fs::read_to_string("server.toml")
            .map_err(|_| anyhow::anyhow!("未找到 server.toml。请先复制 server.example.toml。"))?;
        let cfg: Self = toml::from_str(&content)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.public_base_url.trim().is_empty() {
            anyhow::bail!("server.toml 中必须填写 public_base_url");
        }
        if self.grpc_auth_token.trim().is_empty() {
            anyhow::bail!("server.toml 中必须填写 grpc_auth_token");
        }
        if self.grpc_auth_token.len() < 16 {
            anyhow::bail!(
                "server.toml 中的 grpc_auth_token 长度不足 16 字符，请使用高熵随机值以确保安全性。"
            );
        }
        validate_public_base_url(&self.public_base_url)?;
        if self.ws_rate_limit_per_ip_per_min == 0 {
            anyhow::bail!("ws_rate_limit_per_ip_per_min 必须大于 0");
        }
        if self.http_rate_limit_per_ip_per_min == 0 {
            anyhow::bail!("http_rate_limit_per_ip_per_min 必须大于 0");
        }
        if self.max_ws_connections == 0 {
            anyhow::bail!("max_ws_connections 必须大于 0");
        }
        if self.max_message_size_bytes == 0 {
            anyhow::bail!("max_message_size_bytes 必须大于 0");
        }
        if self.token_cleanup_interval_secs == 0 {
            anyhow::bail!("token_cleanup_interval_secs 必须大于 0");
        }
        if self.token_expiry_secs == 0 {
            anyhow::bail!("token_expiry_secs 必须大于 0");
        }
        if self.pairing_ttl_secs == 0 {
            anyhow::bail!("pairing_ttl_secs 必须大于 0");
        }
        if self.ws_idle_timeout_secs == 0 {
            anyhow::bail!("ws_idle_timeout_secs 必须大于 0");
        }
        let _ = self.trusted_proxy_ranges()?;
        Ok(())
    }

    pub fn normalized_public_origin(&self) -> anyhow::Result<String> {
        normalize_origin(&self.public_base_url)
    }

    pub fn trusted_proxy_ranges(&self) -> anyhow::Result<TrustedProxyCidrs> {
        TrustedProxyCidrs::parse(&self.trusted_proxy_cidrs)
    }
}

fn parse_rightmost_forwarded_ip(value: &str) -> Option<IpAddr> {
    value
        .split(',')
        .next_back()
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .and_then(|entry| entry.parse::<IpAddr>().ok())
}

fn parse_ip_header(value: Option<&str>) -> Option<IpAddr> {
    value
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .and_then(|entry| entry.parse::<IpAddr>().ok())
}

fn validate_public_base_url(public_base_url: &str) -> anyhow::Result<()> {
    let url = Url::parse(public_base_url)
        .map_err(|err| anyhow::anyhow!("public_base_url 不是合法 URL：{err}"))?;

    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("public_base_url 不能包含 userinfo");
    }
    if url.query().is_some() {
        anyhow::bail!("public_base_url 不能包含 query");
    }
    if url.fragment().is_some() {
        anyhow::bail!("public_base_url 不能包含 fragment");
    }
    if !url.path().is_empty() && url.path() != "/" {
        anyhow::bail!("public_base_url 不能包含非根路径");
    }

    let host = url.host_str().unwrap_or("");
    let is_loopback = match url.host() {
        Some(Host::Ipv4(v4)) => v4.is_loopback(),
        Some(Host::Ipv6(v6)) => v6.is_loopback(),
        Some(Host::Domain(name)) => matches!(name, "localhost"),
        None => false,
    };
    let is_non_public = match url.host() {
        Some(Host::Ipv4(v4)) => is_non_public_ip(IpAddr::V4(v4)),
        Some(Host::Ipv6(v6)) => is_non_public_ip(IpAddr::V6(v6)),
        Some(Host::Domain(name)) => matches!(name, "localhost"),
        None => false,
    };

    if url.scheme() == "http" {
        if !is_loopback {
            anyhow::bail!(
                "public_base_url 必须使用 https:// 协议（生产环境）；\
                 检测到非 loopback 地址的 http:// 配置"
            );
        }
    } else if url.scheme() == "https" && is_non_public {
        anyhow::bail!(
            "public_base_url 不能设为内网/loopback 地址（检测到 {host}）；\
             请配置手机可访问的公网地址"
        );
    }

    let _ = normalize_origin(public_base_url)?;
    Ok(())
}

fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // Unique Local Address (ULA): fc00::/7 — stable API 暂无，手动检测
            let first_octet = v6.octets()[0];
            if first_octet & 0xfe == 0xfc {
                return true;
            }
            // Link-local unicast: fe80::/10
            let first_two = u16::from_be_bytes([v6.octets()[0], v6.octets()[1]]);
            if first_two & 0xffc0 == 0xfe80 {
                return true;
            }
            false
        }
    }
}

fn normalize_origin(value: &str) -> anyhow::Result<String> {
    let url = Url::parse(value).map_err(|err| anyhow::anyhow!("origin 解析失败：{err}"))?;
    let scheme = url.scheme().to_ascii_lowercase();
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("origin 缺少主机名"))?
        .to_ascii_lowercase();
    let port = url.port();
    let normalized = match (scheme.as_str(), port) {
        ("http", None | Some(80)) | ("https", None | Some(443)) => {
            format!("{scheme}://{host}")
        }
        (_, Some(port)) => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    };
    Ok(normalized.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_GRPC_AUTH_TOKEN: &str = "shared-secret-xx";
    const ANOTHER_VALID_GRPC_AUTH_TOKEN: &str = "another-secret-yy";

    fn parse(toml: &str) -> ServerConfig { toml::from_str(toml).expect("parse failed") }

    #[test]
    fn load_requires_public_base_url() {
        // 缺少 server.toml 文件时应返回错误
        let result = ServerConfig::load();
        assert!(result.is_err());
    }

    #[test]
    fn defaults_are_correct() {
        let cfg = parse(
            r#"
            public_base_url = "https://example.com"
            grpc_auth_token = "shared-secret-xx"
            "#,
        );
        assert_eq!(cfg.grpc_port, 50051);
        assert_eq!(cfg.http_port, 8080);
        assert_eq!(cfg.http_bind_host, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(cfg.grpc_bind_host, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(cfg.token_expiry_secs, 300);
        assert_eq!(cfg.token_cleanup_interval_secs, 60);
        assert_eq!(cfg.ws_rate_limit_per_ip_per_min, 10);
        assert_eq!(cfg.http_rate_limit_per_ip_per_min, 20);
        assert_eq!(cfg.max_ws_connections, 100);
        assert_eq!(cfg.max_message_size_bytes, 65536);
        assert_eq!(cfg.ws_idle_timeout_secs, 90);
        assert_eq!(cfg.grpc_keepalive_interval_secs, 60);
        assert_eq!(cfg.grpc_keepalive_timeout_secs, 20);
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.trusted_proxy_cidrs.is_empty());
    }

    #[test]
    fn custom_values_override_defaults() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = "another-secret-yy"
            grpc_port = 9090
            http_port = 8443
            grpc_bind_host = "0.0.0.0"
            http_bind_host = "0.0.0.0"
            token_expiry_secs = 600
            log_level = "debug"
            trusted_proxy_cidrs = ["127.0.0.1/32", "10.0.0.0/8"]
            "#,
        );
        assert_eq!(cfg.public_base_url, "https://relay.example.com");
        assert_eq!(cfg.grpc_auth_token, ANOTHER_VALID_GRPC_AUTH_TOKEN);
        assert_eq!(cfg.grpc_port, 9090);
        assert_eq!(cfg.http_port, 8443);
        assert_eq!(cfg.grpc_bind_host, IpAddr::from([0, 0, 0, 0]));
        assert_eq!(cfg.http_bind_host, IpAddr::from([0, 0, 0, 0]));
        assert_eq!(cfg.token_expiry_secs, 600);
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(
            cfg.trusted_proxy_cidrs,
            vec!["127.0.0.1/32".to_string(), "10.0.0.0/8".to_string()]
        );
        // 未覆盖的字段仍使用默认值
        assert_eq!(cfg.ws_idle_timeout_secs, 90);
    }

    #[test]
    fn invalid_trusted_proxy_cidr_rejected_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = "shared-secret-xx"
            trusted_proxy_cidrs = ["not-a-cidr"]
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("invalid trusted_proxy_cidrs should fail validation");
        assert!(error.to_string().contains("trusted_proxy_cidrs"));
    }

    #[test]
    fn trusted_proxy_ranges_use_rightmost_x_forwarded_for_for_trusted_peer() {
        let trusted = TrustedProxyCidrs::parse(&["127.0.0.0/8".to_string()])
            .expect("trusted proxies should parse");
        let resolved = trusted.resolve_client_ip(
            IpAddr::from([127, 0, 0, 1]),
            Some("198.51.100.10, 203.0.113.7"),
            Some("192.0.2.5"),
        );
        assert_eq!(resolved, IpAddr::from([203, 0, 113, 7]));
    }

    #[test]
    fn trusted_proxy_ranges_fall_back_to_x_real_ip_only_when_xff_missing() {
        let trusted = TrustedProxyCidrs::parse(&["127.0.0.0/8".to_string()])
            .expect("trusted proxies should parse");
        let resolved =
            trusted.resolve_client_ip(IpAddr::from([127, 0, 0, 1]), None, Some("192.0.2.5"));
        assert_eq!(resolved, IpAddr::from([192, 0, 2, 5]));
    }

    #[test]
    fn trusted_proxy_ranges_ignore_headers_for_untrusted_peer() {
        let trusted = TrustedProxyCidrs::parse(&["127.0.0.1/32".to_string()])
            .expect("trusted proxies should parse");
        let resolved = trusted.resolve_client_ip(
            IpAddr::from([10, 0, 0, 5]),
            Some("203.0.113.7"),
            Some("192.0.2.5"),
        );
        assert_eq!(resolved, IpAddr::from([10, 0, 0, 5]));
    }

    #[test]
    fn trusted_proxy_ranges_fall_back_to_peer_ip_on_invalid_xff() {
        let trusted = TrustedProxyCidrs::parse(&["127.0.0.0/8".to_string()])
            .expect("trusted proxies should parse");
        let resolved = trusted.resolve_client_ip(
            IpAddr::from([127, 0, 0, 1]),
            Some("not-an-ip"),
            Some("192.0.2.5"),
        );
        assert_eq!(resolved, IpAddr::from([127, 0, 0, 1]));
    }

    #[test]
    fn empty_public_base_url_rejected_at_validation() {
        // parse 本身不报错，但 load() 中的验证应拒绝空 URL
        // 此处直接测试验证逻辑
        let cfg = parse(
            r#"
            public_base_url = ""
            grpc_auth_token = "shared-secret-xx"
            "#,
        );
        assert!(cfg.public_base_url.is_empty());
        // 模拟 load() 中的验证
        let error = cfg
            .validate()
            .expect_err("empty public_base_url should fail validation");
        assert!(error.to_string().contains("public_base_url"));
    }

    #[test]
    fn empty_grpc_auth_token_rejected_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = ""
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("empty grpc_auth_token should fail validation");
        assert!(error.to_string().contains("grpc_auth_token"));
    }

    #[test]
    fn short_grpc_auth_token_rejected_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = "short-token"
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("short grpc_auth_token should fail validation");
        assert!(error.to_string().contains("grpc_auth_token"));
    }

    #[test]
    fn zero_token_cleanup_interval_rejected_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = "shared-secret-xx"
            token_cleanup_interval_secs = 0
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("zero token_cleanup_interval_secs should fail validation");
        assert!(error.to_string().contains("token_cleanup_interval_secs"));
    }

    #[test]
    fn zero_token_expiry_secs_rejected_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = "shared-secret-xx"
            token_expiry_secs = 0
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("zero token_expiry_secs should fail validation");
        assert!(error.to_string().contains("token_expiry_secs"));
    }

    #[test]
    fn zero_ws_idle_timeout_secs_rejected_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = "shared-secret-xx"
            ws_idle_timeout_secs = 0
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("zero ws_idle_timeout_secs should fail validation");
        assert!(error.to_string().contains("ws_idle_timeout_secs"));
    }

    #[test]
    fn http_non_loopback_rejected_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "http://relay.example.com"
            grpc_auth_token = "shared-secret-xx"
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("http:// non-loopback should fail validation");
        assert!(error.to_string().contains("https://"));
    }

    #[test]
    fn http_localhost_allowed_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "http://localhost:8080"
            grpc_auth_token = "shared-secret-xx"
            "#,
        );
        cfg.validate().expect("http://localhost should be allowed");
    }

    #[test]
    fn http_ipv6_loopback_allowed_at_validation() {
        let toml = format!(
            "public_base_url = \"http://[::1]:8080\"\ngrpc_auth_token = \"{VALID_GRPC_AUTH_TOKEN}\""
        );
        let cfg = parse(&toml);
        cfg.validate()
            .expect("http://[::1] should be allowed for local dev");
    }

    #[test]
    fn https_loopback_rejected_at_validation() {
        for url in &["https://127.0.0.1", "https://localhost", "https://[::1]"] {
            let toml = format!(
                "public_base_url = \"{url}\"\ngrpc_auth_token = \"{VALID_GRPC_AUTH_TOKEN}\""
            );
            let cfg = parse(&toml);
            cfg.validate()
                .expect_err(&format!("https loopback {url} should be rejected"));
        }
    }

    #[test]
    fn https_private_rejected_at_validation() {
        for url in &[
            "https://10.0.0.1",
            "https://192.168.1.1",
            "https://172.16.0.1",
            "https://172.31.255.255",
            "https://169.254.1.1",
        ] {
            let toml = format!(
                "public_base_url = \"{url}\"\ngrpc_auth_token = \"{VALID_GRPC_AUTH_TOKEN}\""
            );
            let cfg = parse(&toml);
            cfg.validate()
                .expect_err(&format!("https private {url} should be rejected"));
        }
    }

    #[test]
    fn https_ipv6_non_public_rejected_at_validation() {
        for url in &[
            "https://[fd00::1]",
            "https://[fc00::1]",
            "https://[fe80::1]",
            "https://[febf::1]",
        ] {
            let toml = format!(
                "public_base_url = \"{url}\"\ngrpc_auth_token = \"{VALID_GRPC_AUTH_TOKEN}\""
            );
            let cfg = parse(&toml);
            cfg.validate()
                .expect_err(&format!("https IPv6 non-public {url} should be rejected"));
        }
    }
}
