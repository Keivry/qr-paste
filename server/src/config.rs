// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    serde::Deserialize,
    std::{fmt, fs, net::IpAddr},
};

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
    /// gRPC HTTP/2 keepalive 心跳发送间隔（秒）。默认 30。
    #[serde(default = "default_grpc_keepalive_interval_secs")]
    pub grpc_keepalive_interval_secs: u64,
    /// gRPC HTTP/2 keepalive 超时（秒）。超过此时间未收到 ACK 则断开连接。默认 20。
    #[serde(default = "default_grpc_keepalive_timeout_secs")]
    pub grpc_keepalive_timeout_secs: u64,
    /// 日志级别：`trace` / `debug` / `info` / `warn` / `error`。默认 `"info"`。
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 是否部署在可信反向代理之后（nginx/caddy 等）。
    ///
    /// 设为 `true` 时，限速器使用 `X-Forwarded-For` / `X-Real-IP` 头中的 IP；
    /// 设为 `false`（默认）时，使用直连的对端 IP，防止伪造来源绕过限速。
    /// 如果不确定，保持默认 `false`。
    #[serde(default)]
    pub behind_trusted_proxy: bool,
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
            .field("behind_trusted_proxy", &self.behind_trusted_proxy)
            .finish()
    }
}

fn default_grpc_port() -> u16 { 50051 }
fn default_http_port() -> u16 { 8080 }
fn default_bind_host() -> IpAddr { IpAddr::from([127, 0, 0, 1]) }
fn default_token_expiry_secs() -> u64 { 300 }
fn default_token_cleanup_interval_secs() -> u64 { 60 }
fn default_ws_rate_limit_per_ip_per_min() -> u32 { 10 }
fn default_http_rate_limit_per_ip_per_min() -> u32 { 20 }
fn default_max_ws_connections() -> usize { 100 }
fn default_max_message_size_bytes() -> usize { 65536 }
fn default_ws_idle_timeout_secs() -> u64 { 90 }
fn default_grpc_keepalive_interval_secs() -> u64 { 30 }
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
            tracing::warn!(
                "警告：grpc_auth_token 长度不足 16 字符，建议使用高熵随机值以确保安全性。"
            );
        }
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
        if self.ws_idle_timeout_secs == 0 {
            anyhow::bail!("ws_idle_timeout_secs 必须大于 0");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            grpc_auth_token = "shared-secret"
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
        assert_eq!(cfg.grpc_keepalive_interval_secs, 30);
        assert_eq!(cfg.grpc_keepalive_timeout_secs, 20);
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn custom_values_override_defaults() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = "another-secret"
            grpc_port = 9090
            http_port = 8443
            grpc_bind_host = "0.0.0.0"
            http_bind_host = "0.0.0.0"
            token_expiry_secs = 600
            log_level = "debug"
            "#,
        );
        assert_eq!(cfg.public_base_url, "https://relay.example.com");
        assert_eq!(cfg.grpc_auth_token, "another-secret");
        assert_eq!(cfg.grpc_port, 9090);
        assert_eq!(cfg.http_port, 8443);
        assert_eq!(cfg.grpc_bind_host, IpAddr::from([0, 0, 0, 0]));
        assert_eq!(cfg.http_bind_host, IpAddr::from([0, 0, 0, 0]));
        assert_eq!(cfg.token_expiry_secs, 600);
        assert_eq!(cfg.log_level, "debug");
        // 未覆盖的字段仍使用默认值
        assert_eq!(cfg.ws_idle_timeout_secs, 90);
    }

    #[test]
    fn empty_public_base_url_rejected_at_validation() {
        // parse 本身不报错，但 load() 中的验证应拒绝空 URL
        // 此处直接测试验证逻辑
        let cfg = parse(
            r#"
            public_base_url = ""
            grpc_auth_token = "shared-secret"
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
    fn zero_token_cleanup_interval_rejected_at_validation() {
        let cfg = parse(
            r#"
            public_base_url = "https://relay.example.com"
            grpc_auth_token = "shared-secret"
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
            grpc_auth_token = "shared-secret"
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
            grpc_auth_token = "shared-secret"
            ws_idle_timeout_secs = 0
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("zero ws_idle_timeout_secs should fail validation");
        assert!(error.to_string().contains("ws_idle_timeout_secs"));
    }
}
