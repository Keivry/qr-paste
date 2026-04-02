// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    serde::Deserialize,
    std::{env, fs, path::PathBuf},
};

/// 客户端配置，从 `client.toml` 中读取。
///
/// 所有字段均有默认值（`server_host` 与 `grpc_auth_token` 除外）。
/// 参考 `client.example.toml` 查看每个字段的详细说明。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// 【必填】服务端主机名 / IP，或完整 gRPC 入口 URL。
    ///
    /// 示例：
    /// - `"relay.example.com"`（明文直连，实际地址会组合为 `http://relay.example.com:<grpc_port>`）
    /// - `"https://grpc.example.com"`（经 HTTPS/TLS 反向代理接入）
    pub server_host: String,
    /// gRPC 鉴权令牌，与服务端 `grpc_auth_token` 保持一致。
    ///
    /// 建议使用至少 16 字符的高熵随机字符串以确保连接安全。
    pub grpc_auth_token: String,
    /// gRPC 端口。
    ///
    /// 当 `server_host` 未显式包含端口时，会拼接到最终连接地址中。
    /// 对裸主机名 / `http://` 地址默认使用 `50051`，对 `https://` 地址默认使用 `443`。
    pub grpc_port: u16,
    pub auto_paste: bool,
    /// `auto_paste` 为 `true` 时，粘贴完成后是否自动按下回车键。默认 `false`。
    pub enter_after_paste: bool,
    /// 自动粘贴前的等待毫秒数，给剪贴板写入操作留出时间。默认 150 ms。
    pub paste_delay_ms: u64,
    /// gRPC 断开后的重连等待时间（秒）。默认 5 秒。
    pub reconnect_interval_secs: u64,
    /// 最大重连尝试次数。`0` 表示无限重试。默认 0。
    pub max_reconnect_attempts: u32,
    /// 启动时是否隐藏主窗口（仅在托盘显示）。默认 `false`。
    pub start_minimized: bool,
    /// 点击窗口关闭按钮时是否最小化到托盘而不是退出。默认 `false`。
    pub minimize_on_close: bool,
    /// 粘贴通知在 UI 中显示的时长（秒）。默认 3 秒。
    pub notification_duration_secs: u64,
}

fn default_grpc_port() -> u16 { 50051 }
fn default_https_grpc_port() -> u16 { 443 }
fn default_auto_paste() -> bool { false }
fn default_enter_after_paste() -> bool { false }
fn default_paste_delay_ms() -> u64 { 150 }
fn default_reconnect_interval_secs() -> u64 { 5 }
fn default_max_reconnect_attempts() -> u32 { 0 }
fn default_start_minimized() -> bool { false }
fn default_minimize_on_close() -> bool { false }
fn default_notification_duration_secs() -> u64 { 3 }

#[derive(Debug, Deserialize)]
struct RawClientConfig {
    server_host: String,
    grpc_auth_token: String,
    #[serde(default)]
    grpc_port: Option<u16>,
    #[serde(default = "default_auto_paste")]
    auto_paste: bool,
    #[serde(default = "default_enter_after_paste")]
    enter_after_paste: bool,
    #[serde(default = "default_paste_delay_ms")]
    paste_delay_ms: u64,
    #[serde(default = "default_reconnect_interval_secs")]
    reconnect_interval_secs: u64,
    #[serde(default = "default_max_reconnect_attempts")]
    max_reconnect_attempts: u32,
    #[serde(default = "default_start_minimized")]
    start_minimized: bool,
    #[serde(default = "default_minimize_on_close")]
    minimize_on_close: bool,
    #[serde(default = "default_notification_duration_secs")]
    notification_duration_secs: u64,
}

impl From<RawClientConfig> for ClientConfig {
    fn from(raw: RawClientConfig) -> Self {
        let grpc_port = raw
            .grpc_port
            .unwrap_or_else(|| default_grpc_port_for_server_host(&raw.server_host));

        Self {
            server_host: raw.server_host,
            grpc_auth_token: raw.grpc_auth_token,
            grpc_port,
            auto_paste: raw.auto_paste,
            enter_after_paste: raw.enter_after_paste,
            paste_delay_ms: raw.paste_delay_ms,
            reconnect_interval_secs: raw.reconnect_interval_secs,
            max_reconnect_attempts: raw.max_reconnect_attempts,
            start_minimized: raw.start_minimized,
            minimize_on_close: raw.minimize_on_close,
            notification_duration_secs: raw.notification_duration_secs,
        }
    }
}

impl ClientConfig {
    /// # Errors
    ///
    /// - 文件不存在时返回错误
    /// - TOML 解析失败或必填字段缺失时返回中文错误字符串
    pub fn load() -> Result<Self, String> {
        let config_path = resolve_config_path()?;
        let content = fs::read_to_string(&config_path)
            .map_err(|err| format!("读取 {} 失败：{err}", config_path.display()))?;
        let raw: RawClientConfig =
            toml::from_str(&content).map_err(|e| format!("client.toml 解析失败：{e}"))?;
        let cfg: Self = raw.into();
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        if self.server_host.trim().is_empty() {
            return Err("client.toml 中必须填写 server_host。".to_string());
        }
        validate_server_host(&self.server_host)?;
        if self.grpc_auth_token.trim().is_empty() {
            return Err("client.toml 中必须填写 grpc_auth_token。".to_string());
        }
        if self.grpc_auth_token.len() < 16 {
            tracing::warn!("grpc_auth_token 长度不足 16 字符，建议使用高熵随机值以确保安全性。");
        }
        Ok(())
    }
}

fn validate_server_host(server_host: &str) -> Result<(), String> {
    let host = server_host.trim();
    let authority = if let Some(rest) = host.strip_prefix("https://") {
        rest
    } else if let Some(rest) = host.strip_prefix("http://") {
        rest
    } else {
        if host.contains("://") {
            return Err(
                "client.toml 中的 server_host 仅支持 http:// 或 https:// 协议。".to_string(),
            );
        }
        host
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

    Ok(())
}

fn default_grpc_port_for_server_host(server_host: &str) -> u16 {
    if server_host.trim().starts_with("https://") {
        default_https_grpc_port()
    } else {
        default_grpc_port()
    }
}

fn resolve_config_path() -> Result<PathBuf, String> {
    if let Ok(exe_path) = env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let exe_config = exe_dir.join("client.toml");
        if exe_config.is_file() {
            return Ok(exe_config);
        }
    }

    let cwd_config = PathBuf::from("client.toml");
    if cwd_config.is_file() {
        return Ok(cwd_config);
    }

    Err(
        "未找到 client.toml。请先复制 client.example.toml 为 client.toml，并将其放在 client.exe 同目录或当前工作目录。"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> ClientConfig {
        let raw: RawClientConfig = toml::from_str(toml).expect("parse failed");
        raw.into()
    }

    #[test]
    fn load_requires_client_toml_file() {
        let result = ClientConfig::load();
        let error = result.expect_err("missing client.toml should fail");
        assert!(error.contains("client.example.toml"));
    }

    #[test]
    fn https_server_host_defaults_to_443() {
        assert_eq!(
            default_grpc_port_for_server_host("https://cg.keivry.ren"),
            443
        );
    }

    #[test]
    fn plain_server_host_defaults_to_50051() {
        assert_eq!(
            default_grpc_port_for_server_host("relay.example.com"),
            50051
        );
    }

    #[test]
    fn defaults_are_correct() {
        let cfg = parse(
            r#"
            server_host = "example.com"
            grpc_auth_token = "shared-secret"
            "#,
        );
        assert_eq!(cfg.grpc_port, 50051);
        assert!(!cfg.auto_paste);
        assert_eq!(cfg.paste_delay_ms, 150);
        assert_eq!(cfg.reconnect_interval_secs, 5);
        assert_eq!(cfg.max_reconnect_attempts, 0);
        assert!(!cfg.start_minimized);
        assert!(!cfg.minimize_on_close);
        assert_eq!(cfg.notification_duration_secs, 3);
    }

    #[test]
    fn custom_values_override_defaults() {
        let cfg = parse(
            r#"
            server_host = "relay.example.com"
            grpc_auth_token = "shared-secret"
            grpc_port = 9090
            auto_paste = false
            paste_delay_ms = 300
            max_reconnect_attempts = 5
            start_minimized = true
            minimize_on_close = true
            notification_duration_secs = 10
            "#,
        );
        assert_eq!(cfg.server_host, "relay.example.com");
        assert_eq!(cfg.grpc_auth_token, "shared-secret");
        assert_eq!(cfg.grpc_port, 9090);
        assert!(!cfg.auto_paste);
        assert_eq!(cfg.paste_delay_ms, 300);
        assert_eq!(cfg.max_reconnect_attempts, 5);
        assert!(cfg.start_minimized);
        assert!(cfg.minimize_on_close);
        assert_eq!(cfg.notification_duration_secs, 10);
        // 未覆盖的字段仍使用默认值
        assert_eq!(cfg.reconnect_interval_secs, 5);
    }

    #[test]
    fn empty_server_host_rejected() {
        let cfg = parse(
            r#"
            server_host = ""
            grpc_auth_token = "shared-secret"
            "#,
        );
        assert!(cfg.server_host.is_empty());
        // 模拟 load() 中的验证
        let error = cfg
            .validate()
            .expect_err("empty host should fail validation");
        assert!(error.contains("server_host"));
    }

    #[test]
    fn empty_grpc_auth_token_rejected() {
        let cfg = parse(
            r#"
            server_host = "relay.example.com"
            grpc_auth_token = ""
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("empty auth token should fail validation");
        assert!(error.contains("grpc_auth_token"));
    }

    #[test]
    fn https_server_host_is_allowed() {
        let cfg = parse(
            r#"
            server_host = "https://cg.keivry.ren"
            grpc_auth_token = "shared-secret"
            "#,
        );
        cfg.validate().expect("https endpoint should be accepted");
        assert_eq!(cfg.grpc_port, 443);
    }

    #[test]
    fn explicit_grpc_port_overrides_https_default() {
        let cfg = parse(
            r#"
            server_host = "https://cg.keivry.ren"
            grpc_auth_token = "shared-secret"
            grpc_port = 8443
            "#,
        );
        assert_eq!(cfg.grpc_port, 8443);
    }

    #[test]
    fn server_host_with_path_is_rejected() {
        let cfg = parse(
            r#"
            server_host = "https://cg.keivry.ren/grpc"
            grpc_auth_token = "shared-secret"
            "#,
        );
        let error = cfg
            .validate()
            .expect_err("server_host with path should fail validation");
        assert!(error.contains("server_host"));
    }
}
