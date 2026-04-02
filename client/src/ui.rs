// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::{
        clipboard,
        config::ClientConfig,
        grpc::{self, ClientEvent},
        tray,
    },
    egui::{ColorImage, Context, TextureHandle, ViewportCommand},
    std::{
        sync::mpsc,
        time::{Duration, Instant},
    },
};

/// 主窗口的状态机。
///
/// 状态转移路径：
/// `Connecting` → `WaitingScan`（收到 SessionToken）
/// `WaitingScan` → `Connected`（收到 MobileConnected）
/// `Connected` → `Connecting`（收到 MobileDisconnected）
/// 任意状态 → `Error`（gRPC 达到最大重连次数）
enum AppState {
    /// 正在连接 gRPC 服务端，或等待服务端分配令牌。
    Connecting,
    /// 已获取令牌，展示二维码等待手机扫描。
    WaitingScan {
        /// 预生成的二维码纹理，避免每帧重复计算。
        qr_texture: TextureHandle,
        /// 二维码对应的 URL，同时在图码下方以文本显示。
        url: String,
    },
    /// 手机已连接，正在等待用户发送文本。
    Connected {
        /// 手机端设备信息；当服务端无法获取 User-Agent 时可能为空字符串。
        device_info: String,
    },
    /// gRPC 不可恢复错误，展示错误信息。
    Error { message: String },
}

/// 剪贴板操作任务，由 gRPC 接收协程入队，由串行处理器逐个执行以避免乱序。
struct ClipboardJob {
    content: String,
    auto_paste: bool,
    enter_after_paste: bool,
    paste_delay_ms: u64,
    notice_content: String,
}

/// egui 主应用，持有配置、状态机和跨线程通信通道。
pub struct App {
    config: ClientConfig,
    state: AppState,
    connecting_target: Option<String>,
    last_connect_status: Option<String>,
    tray: Option<tray::Tray>,
    allow_close: bool,
    startup_visibility_pending: bool,
    /// 接收来自 gRPC 线程的事件。
    grpc_rx: mpsc::Receiver<ClientEvent>,
    /// 接收来自托盘线程的用户操作事件。
    tray_rx: mpsc::Receiver<tray::TrayEvent>,
    clipboard_job_tx: mpsc::Sender<ClipboardJob>,
    clipboard_notice_rx: mpsc::Receiver<String>,
    /// 粘贴成功通知：`(显示文本, 显示开始时刻)`，过期后置为 `None`。
    paste_notice: Option<(String, Instant)>,
}

impl App {
    /// 创建应用实例，同时启动 gRPC 客户端线程并初始化系统托盘。
    pub fn new(cc: &eframe::CreationContext<'_>, config: ClientConfig) -> Self {
        egui_cjk_font::load_cjk_font(&cc.egui_ctx);

        let (grpc_tx, grpc_rx) = mpsc::channel();
        grpc::start(
            config.server_host.clone(),
            config.grpc_port,
            config.grpc_auth_token.clone(),
            config.reconnect_interval_secs,
            config.max_reconnect_attempts,
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
            grpc_rx,
            tray_rx,
            clipboard_job_tx,
            paste_notice: None,
            clipboard_notice_rx,
        }
    }
}

impl eframe::App for App {
    /// 非 UI 逻辑帧：处理事件、驱动状态机、管理窗口可见性。
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
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

        while let Ok(event) = self.grpc_rx.try_recv() {
            match event {
                ClientEvent::ConnectingTarget { message } => {
                    self.state = AppState::Connecting;
                    self.connecting_target = Some(message);
                }
                ClientEvent::ConnectingStatus { message } => {
                    self.state = AppState::Connecting;
                    self.last_connect_status = Some(message);
                }
                ClientEvent::SessionToken { url, .. } => {
                    self.connecting_target = None;
                    self.last_connect_status = None;
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
                    // 仅在托盘可用时才隐藏主窗口，避免托盘初始化失败后用户失去恢复入口。
                    if self.tray.is_some() {
                        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                    }
                }
                ClientEvent::MobileDisconnected => {
                    self.connecting_target = None;
                    self.state = AppState::Connecting;
                    self.last_connect_status = Some("手机端已断开，等待新的二维码...".to_string());
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                }
                ClientEvent::ClipboardText { content } => {
                    if content.is_empty() {
                        continue;
                    }
                    let notice_content = if content.chars().count() > 20 {
                        format!("{}...", content.chars().take(20).collect::<String>())
                    } else {
                        content.clone()
                    };
                    let _ = self.clipboard_job_tx.send(ClipboardJob {
                        content,
                        auto_paste: self.config.auto_paste,
                        enter_after_paste: self.config.enter_after_paste,
                        paste_delay_ms: self.config.paste_delay_ms,
                        notice_content,
                    });
                }
                ClientEvent::Error { message } => {
                    self.connecting_target = None;
                    self.last_connect_status = None;
                    self.state = AppState::Error { message };
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

        // 清除已过期的粘贴通知
        if let Some((_, t)) = &self.paste_notice
            && t.elapsed().as_secs() >= self.config.notification_duration_secs
        {
            self.paste_notice = None;
        }
    }

    /// UI 渲染帧：根据当前状态机绘制对应界面。
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

            if let Err(err) = clipboard::write_to_clipboard(&job.content) {
                tracing::warn!("Clipboard write failed: {err}");
                let _ = clipboard_notice_tx.send("写入剪贴板失败，请重试。".to_string());
                repaint_ctx.request_repaint();
                continue;
            }

            if job.auto_paste {
                clipboard::simulate_paste(job.paste_delay_ms);
                if job.enter_after_paste {
                    clipboard::simulate_enter_key();
                }
            }

            let notice = if job.auto_paste && job.enter_after_paste {
                format!("已自动粘贴并回车：{}", job.notice_content)
            } else if job.auto_paste {
                format!("已自动粘贴：{}", job.notice_content)
            } else {
                format!("已复制到剪贴板：{}", job.notice_content)
            };
            let _ = clipboard_notice_tx.send(notice);
            repaint_ctx.request_repaint();
        }
    });
}

/// 将 URL 编码为二维码并生成 egui 纹理。
///
/// 使用 `qrcode` crate 生成灰度图像，最小尺寸 256×256，
/// 再转换为 egui `ColorImage`（灰度像素）并上传至 GPU 纹理缓存。
///
/// 若二维码生成失败（URL 过长等），退回为占位图纹理并记录告警。
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

    let mut rgba_pixels: Vec<egui::Color32> = Vec::with_capacity(w * h);
    for pixel in image.pixels() {
        let v = pixel.0[0];
        rgba_pixels.push(egui::Color32::from_gray(v));
    }

    let color_image = ColorImage {
        size: [w, h],
        pixels: rgba_pixels,
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

    let color_image = ColorImage {
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
