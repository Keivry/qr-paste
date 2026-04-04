// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::{
        clipboard,
        config::ClientConfig,
        grpc::{self, ClientEvent},
        tray,
    },
    egui::{Color32, Context, TextureHandle, ViewportCommand},
    std::{
        sync::{mpsc, mpsc::TryRecvError},
        time::{Duration, Instant},
    },
};

#[derive(Clone)]
enum RevokeState {
    Idle,
    Running,
    Success(Instant),
    Error(String),
}

impl RevokeState {
    const fn idle() -> Self { Self::Idle }
}

impl Default for RevokeState {
    fn default() -> Self { Self::idle() }
}

enum AppState {
    Connecting,
    WaitingScan {
        qr_texture: TextureHandle,
        url: String,
    },
    Connected {
        device_info: String,
    },
    Error {
        message: String,
    },
}

struct ClipboardJob {
    content: String,
    auto_paste: bool,
    emulation_key_after_paste: Option<String>,
    delete_clipboard_after_paste: bool,
    paste_delay_ms: u64,
    notice_content: String,
}

/// egui 应用主状态，实现 [`eframe::App`] 接口。
///
/// 持有配置、gRPC 事件通道、系统托盘句柄及剪贴板操作通道。
pub struct App {
    config: ClientConfig,
    state: AppState,
    connecting_target: Option<String>,
    last_connect_status: Option<String>,
    tray: Option<tray::Tray>,
    allow_close: bool,
    startup_visibility_pending: bool,
    /// gRPC 事件接收端；gRPC 线程退出后置为 `None`。
    grpc_rx: Option<mpsc::Receiver<ClientEvent>>,
    tray_rx: mpsc::Receiver<tray::TrayEvent>,
    clipboard_job_tx: mpsc::Sender<ClipboardJob>,
    clipboard_notice_rx: mpsc::Receiver<String>,
    paste_notice: Option<(String, Instant)>,
    last_session_token: Option<String>,
    last_grpc_session_token: Option<String>,
    last_public_base_url: Option<String>,
    revoke_state: RevokeState,
    /// 当前重连退避间隔（指数增长，上限 reconnect_max_interval_secs）。
    reconnect_delay: Duration,
    /// 防止重复调度重连：`true` 表示已调度，下一帧执行 `start_grpc()`。
    reconnect_pending: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, config: ClientConfig) -> Self {
        egui_cjk_font::load_cjk_font(&cc.egui_ctx);

        let (grpc_tx, grpc_rx) = mpsc::channel();
        grpc::start(
            grpc::StartOptions {
                host: config.server_host.clone(),
                port: config.grpc_port,
                auth_token: config.grpc_auth_token.clone(),
                pairing_id: config.pairing_id.clone(),
                heartbeat_interval_secs: config.heartbeat_interval_secs,
            },
            grpc_tx,
            cc.egui_ctx.clone(),
        );

        let (tray_tx, tray_rx) = mpsc::channel();
        let tray = match tray::start(tray_tx, cc.egui_ctx.clone()) {
            Ok(tray) => Some(tray),
            Err(err) => {
                tracing::warn!("Failed to start tray icon: {err}");
                None
            }
        };

        let (clipboard_notice_tx, clipboard_notice_rx) = mpsc::channel();
        let (clipboard_job_tx, clipboard_job_rx) = mpsc::channel();
        start_clipboard_worker(
            clipboard_job_rx,
            clipboard_notice_tx.clone(),
            cc.egui_ctx.clone(),
        );

        Self {
            startup_visibility_pending: config.start_minimized,
            config,
            state: AppState::Connecting,
            connecting_target: None,
            last_connect_status: None,
            tray,
            allow_close: false,
            grpc_rx: Some(grpc_rx),
            tray_rx,
            clipboard_job_tx,
            paste_notice: None,
            clipboard_notice_rx,
            last_session_token: None,
            last_grpc_session_token: None,
            last_public_base_url: None,
            revoke_state: RevokeState::Idle,
            reconnect_delay: Duration::from_secs(1),
            reconnect_pending: false,
        }
    }

    fn start_grpc(&mut self, ctx: &Context) {
        let (grpc_tx, grpc_rx) = mpsc::channel();
        grpc::start(
            grpc::StartOptions {
                host: self.config.server_host.clone(),
                port: self.config.grpc_port,
                auth_token: self.config.grpc_auth_token.clone(),
                pairing_id: self.config.pairing_id.clone(),
                heartbeat_interval_secs: self.config.heartbeat_interval_secs,
            },
            grpc_tx,
            ctx.clone(),
        );
        self.grpc_rx = Some(grpc_rx);
        self.state = AppState::Connecting;
        self.connecting_target = None;
        self.last_connect_status = None;
        self.last_session_token = None;
        self.last_grpc_session_token = None;
        self.last_public_base_url = None;
        self.revoke_state = RevokeState::Idle;
    }

    fn revoke_endpoint_url(&self) -> Option<String> {
        let base = self.last_public_base_url.as_deref()?.trim_end_matches('/');
        if base.starts_with("http://") || base.starts_with("https://") {
            Some(format!(
                "{base}/api/pairing/{}/revoke",
                self.config.pairing_id
            ))
        } else {
            Some(format!(
                "http://{base}/api/pairing/{}/revoke",
                self.config.pairing_id
            ))
        }
    }

    fn trigger_revoke(&mut self, ctx: &Context) {
        let Some(grpc_session_token) = self.last_grpc_session_token.clone() else {
            self.revoke_state =
                RevokeState::Error("当前还没有可用会话，无法撤销访问。".to_string());
            return;
        };

        let Some(url) = self.revoke_endpoint_url() else {
            self.revoke_state =
                RevokeState::Error("尚未获取到服务端地址，无法撤销访问。".to_string());
            return;
        };
        let repaint_ctx = ctx.clone();
        self.revoke_state = RevokeState::Running;

        std::thread::spawn(move || {
            let result = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .and_then(|client| client.post(url).bearer_auth(grpc_session_token).send());

            let next_state = match result {
                Ok(response) if response.status().is_success() => {
                    RevokeState::Success(Instant::now())
                }
                Ok(response) => RevokeState::Error(format!("撤销失败：HTTP {}", response.status())),
                Err(err) => RevokeState::Error(format!("撤销失败：{err}")),
            };

            repaint_ctx.data_mut(|data| {
                data.insert_temp(egui::Id::new("revoke_state"), next_state);
            });
            repaint_ctx.request_repaint();
        });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if let Some(next_state) =
            ctx.data_mut(|data| data.remove_temp::<RevokeState>(egui::Id::new("revoke_state")))
        {
            self.revoke_state = next_state;
        }

        // 重连调度：grpc_rx 已断开且 reconnect_pending，则启动新 gRPC 连接
        if self.reconnect_pending && self.grpc_rx.is_none() {
            self.reconnect_pending = false;
            self.start_grpc(ctx);
        }

        if self.startup_visibility_pending {
            self.startup_visibility_pending = false;
            ctx.send_viewport_cmd(ViewportCommand::Visible(self.tray.is_none()));
        }

        if ctx.input(|i| i.viewport().close_requested())
            && self.config.minimize_on_close
            && self.tray.is_some()
            && !self.allow_close
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }

        if let Some(rx) = &self.grpc_rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => match event {
                        ClientEvent::ConnectingTarget { message } => {
                            self.state = AppState::Connecting;
                            self.connecting_target = Some(message);
                        }
                        ClientEvent::ConnectingStatus { message } => {
                            self.state = AppState::Connecting;
                            self.last_connect_status = Some(message);
                        }
                        ClientEvent::GrpcSessionToken { token } => {
                            self.last_grpc_session_token = Some(token);
                        }
                        ClientEvent::SessionToken { token, url } => {
                            self.connecting_target = None;
                            self.last_connect_status = None;
                            self.last_session_token = Some(token);
                            self.last_public_base_url = public_base_url_from_session_url(&url);
                            // 重连成功，重置退避间隔
                            self.reconnect_delay = Duration::from_secs(1);
                            let texture = generate_qr_texture(ctx, &url);
                            self.state = AppState::WaitingScan {
                                qr_texture: texture,
                                url,
                            };
                        }
                        ClientEvent::MobileConnected { device_info } => {
                            self.connecting_target = None;
                            self.last_connect_status = None;
                            self.state = AppState::Connected { device_info };
                            if self.tray.is_some() {
                                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                            }
                        }
                        ClientEvent::MobileDisconnected => {
                            self.connecting_target = None;
                            self.last_connect_status = None;
                            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                        }
                        ClientEvent::ClipboardText { content } => {
                            if content.is_empty() {
                                continue;
                            }
                            let mut chars = content.chars();
                            let prefix: String = chars.by_ref().take(20).collect();
                            let notice_content = if chars.next().is_some() {
                                format!("{prefix}...")
                            } else {
                                prefix
                            };
                            let _ = self.clipboard_job_tx.send(ClipboardJob {
                                content,
                                auto_paste: self.config.auto_paste,
                                emulation_key_after_paste: self
                                    .config
                                    .emulation_key_after_paste
                                    .clone(),
                                delete_clipboard_after_paste: self
                                    .config
                                    .delete_clipboard_after_paste,
                                paste_delay_ms: self.config.paste_delay_ms,
                                notice_content,
                            });
                        }
                        ClientEvent::Error { message } => {
                            self.connecting_target = None;
                            self.last_connect_status = None;
                            self.state = AppState::Error { message };
                        }
                    },
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.grpc_rx = None;
                        let should_reconnect = !matches!(self.state, AppState::Error { .. });
                        if should_reconnect && !self.reconnect_pending {
                            let max = Duration::from_secs(self.config.reconnect_max_interval_secs);
                            self.state = AppState::Connecting;
                            self.last_connect_status = Some("连接断开，正在重试...".to_string());
                            self.reconnect_pending = true;
                            ctx.request_repaint_after(self.reconnect_delay);
                            self.reconnect_delay = (self.reconnect_delay * 2).min(max);
                        }
                        break;
                    }
                }
            }
        }

        while let Ok(ev) = self.tray_rx.try_recv() {
            match ev {
                tray::TrayEvent::ShowWindow => {
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                tray::TrayEvent::Quit => {
                    self.allow_close = true;
                    self.tray = None;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }

        while let Ok(notice) = self.clipboard_notice_rx.try_recv() {
            self.paste_notice = Some((notice, Instant::now()));
            ctx.request_repaint_after(Duration::from_secs(self.config.notification_duration_secs));
        }

        if let Some((_, t)) = &self.paste_notice
            && t.elapsed().as_secs() >= self.config.notification_duration_secs
        {
            self.paste_notice = None;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.vertical_centered(|ui| {
            ui.add_space(20.0);

            match &self.state {
                AppState::Connecting => {
                    ui.label("连接中...");
                    if let Some(target) = &self.connecting_target {
                        ui.add_space(6.0);
                        ui.label(target);
                    }
                    if let Some(detail) = &self.last_connect_status {
                        ui.add_space(6.0);
                        ui.colored_label(egui::Color32::YELLOW, detail);
                    }
                }
                AppState::WaitingScan { qr_texture, url } => {
                    let sized = egui::load::SizedTexture::from_handle(qr_texture);
                    ui.add(
                        egui::Image::from_texture(sized)
                            .fit_to_exact_size(egui::vec2(256.0, 256.0)),
                    );
                    ui.add_space(8.0);
                    ui.label(url.as_str());
                    ui.add_space(4.0);
                    ui.label("等待扫码...");
                }
                AppState::Connected { device_info } => {
                    ui.label(format!("已连接：{device_info}"));
                    ui.add_space(4.0);
                    if let Some((text, _)) = &self.paste_notice {
                        ui.label(text);
                    } else {
                        ui.label("等待文本...");
                    }
                    ui.add_space(12.0);
                    let running = matches!(self.revoke_state, RevokeState::Running);
                    if ui
                        .add_enabled(!running, egui::Button::new("重新生成凭据（撤销访问）"))
                        .clicked()
                    {
                        self.trigger_revoke(ui.ctx());
                    }
                    match &self.revoke_state {
                        RevokeState::Idle => {}
                        RevokeState::Running => {
                            ui.add_space(4.0);
                            ui.label("正在请求服务端撤销旧访问...");
                        }
                        RevokeState::Success(at) => {
                            if at.elapsed() < Duration::from_secs(5) {
                                ui.add_space(4.0);
                                ui.colored_label(
                                    Color32::LIGHT_GREEN,
                                    "已请求撤销，请让手机重新扫码。",
                                );
                            }
                        }
                        RevokeState::Error(message) => {
                            ui.add_space(4.0);
                            ui.colored_label(Color32::YELLOW, message);
                        }
                    }
                }
                AppState::Error { message } => {
                    ui.colored_label(egui::Color32::RED, format!("错误：{message}"));
                }
            }
        });
    }
}

fn start_clipboard_worker(
    clipboard_job_rx: mpsc::Receiver<ClipboardJob>,
    clipboard_notice_tx: mpsc::Sender<String>,
    repaint_ctx: Context,
) {
    std::thread::spawn(move || {
        while let Ok(job) = clipboard_job_rx.recv() {
            if job.content.is_empty() {
                continue;
            }

            let snapshot = if job.auto_paste && job.delete_clipboard_after_paste {
                Some(clipboard::take_snapshot())
            } else {
                None
            };

            if let Err(err) = clipboard::write_to_clipboard(&job.content) {
                tracing::warn!("Clipboard write failed: {err}");
                let _ = clipboard_notice_tx.send("写入剪贴板失败，请重试。".to_string());
                repaint_ctx.request_repaint();
                continue;
            }

            if job.auto_paste {
                clipboard::simulate_paste(job.paste_delay_ms);
                if let Some(key_spec) = &job.emulation_key_after_paste
                    && let Some((modifier, key)) = clipboard::parse_key_spec(key_spec)
                {
                    clipboard::simulate_key(modifier, key);
                }
            }

            let notice = build_clipboard_notice(
                job.auto_paste,
                job.emulation_key_after_paste.as_deref(),
                &job.notice_content,
            );
            let _ = clipboard_notice_tx.send(notice);
            repaint_ctx.request_repaint();

            if let Some(ref snap) = snapshot
                && let Err(e) = clipboard::restore_clipboard(snap, &job.content)
            {
                tracing::warn!("还原剪贴板失败: {e}");
            }
        }
    });
}

fn build_clipboard_notice(
    auto_paste: bool,
    emulation_key_after_paste: Option<&str>,
    notice_content: &str,
) -> String {
    if auto_paste {
        if let Some(key_spec) = emulation_key_after_paste {
            format!("已自动粘贴（{key_spec}）：{notice_content}")
        } else {
            format!("已自动粘贴：{notice_content}")
        }
    } else {
        format!("已复制到剪贴板：{notice_content}")
    }
}

fn public_base_url_from_session_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    let default_port = parsed.port_or_known_default();
    let explicit_port = parsed.port();
    let authority = match (explicit_port, default_port) {
        (Some(port), Some(default)) if port != default => format!("{host}:{port}"),
        (Some(port), None) => format!("{host}:{port}"),
        _ => host.to_string(),
    };
    Some(format!("{scheme}://{authority}"))
}

fn generate_qr_texture(ctx: &Context, url: &str) -> TextureHandle {
    use qrcode::QrCode;

    let code = match QrCode::new(url.as_bytes()) {
        Ok(code) => code,
        Err(err) => {
            tracing::warn!("Failed to generate QR code for session URL: {err}");
            return placeholder_qr_texture(ctx);
        }
    };
    let image = code
        .render::<image::Luma<u8>>()
        .min_dimensions(256, 256)
        .build();
    let (w, h) = (image.width() as usize, image.height() as usize);

    let pixels: Vec<egui::Color32> = image
        .pixels()
        .map(|p| egui::Color32::from_gray(p.0[0]))
        .collect();

    let color_image = egui::ColorImage {
        size: [w, h],
        pixels,
        source_size: egui::vec2(w as f32, h as f32),
    };

    ctx.load_texture("qr_code", color_image, egui::TextureOptions::LINEAR)
}

fn placeholder_qr_texture(ctx: &Context) -> TextureHandle {
    let size = 256usize;
    let mut pixels = Vec::with_capacity(size * size);

    for y in 0..size {
        for x in 0..size {
            let is_dark = ((x / 16) + (y / 16)) % 2 == 0;
            pixels.push(if is_dark {
                egui::Color32::from_gray(32)
            } else {
                egui::Color32::from_gray(224)
            });
        }
    }

    let color_image = egui::ColorImage {
        size: [size, size],
        pixels,
        source_size: egui::vec2(size as f32, size as f32),
    };

    ctx.load_texture(
        "qr_code_placeholder",
        color_image,
        egui::TextureOptions::LINEAR,
    )
}

#[cfg(test)]
mod tests {
    use super::{build_clipboard_notice, public_base_url_from_session_url};

    #[test]
    fn clipboard_notice_for_auto_paste_with_key_is_generic() {
        assert_eq!(
            build_clipboard_notice(true, Some("ctrl+Return"), "hello"),
            "已自动粘贴（ctrl+Return）：hello"
        );
    }

    #[test]
    fn clipboard_notice_for_auto_paste_without_key_omits_enter_wording() {
        assert_eq!(
            build_clipboard_notice(true, None, "hello"),
            "已自动粘贴：hello"
        );
    }

    #[test]
    fn clipboard_notice_for_copy_only_is_copy_message() {
        assert_eq!(
            build_clipboard_notice(false, Some("Return"), "hello"),
            "已复制到剪贴板：hello"
        );
    }

    #[test]
    fn public_base_url_from_session_url_extracts_https_origin() {
        assert_eq!(
            public_base_url_from_session_url("https://relay.example.com/m/abc#ps=secret")
                .as_deref(),
            Some("https://relay.example.com")
        );
    }

    #[test]
    fn public_base_url_from_session_url_keeps_non_default_port() {
        assert_eq!(
            public_base_url_from_session_url("https://relay.example.com:8443/m/abc#ps=secret")
                .as_deref(),
            Some("https://relay.example.com:8443")
        );
    }
}
