// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::{
        clipboard,
        config::ClientConfig,
        grpc::{self, ClientEvent},
        tray,
    },
    egui::{
        Align2, Color32, ColorImage, Context, FontId, Rect, Sense, TextureHandle, ViewportCommand,
        vec2,
    },
    std::{
        sync::mpsc,
        sync::mpsc::TryRecvError,
        time::{Duration, Instant},
    },
};

enum AppState {
    Connecting,
    WaitingScan {
        qr_texture: TextureHandle,
        url: String,
    },
    Connected {
        device_info: String,
    },
    Expired {
        blurred_texture: TextureHandle,
    },
    Error {
        message: String,
    },
}

struct ClipboardJob {
    content: String,
    auto_paste: bool,
    enter_after_paste: bool,
    paste_delay_ms: u64,
    notice_content: String,
}

pub struct App {
    config: ClientConfig,
    state: AppState,
    connecting_target: Option<String>,
    last_connect_status: Option<String>,
    last_qr_image: Option<ColorImage>,
    tray: Option<tray::Tray>,
    allow_close: bool,
    startup_visibility_pending: bool,
    /// gRPC 事件接收端；gRPC 线程退出后置为 `None`。
    grpc_rx: Option<mpsc::Receiver<ClientEvent>>,
    tray_rx: mpsc::Receiver<tray::TrayEvent>,
    clipboard_job_tx: mpsc::Sender<ClipboardJob>,
    clipboard_notice_rx: mpsc::Receiver<String>,
    paste_notice: Option<(String, Instant)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, config: ClientConfig) -> Self {
        egui_cjk_font::load_cjk_font(&cc.egui_ctx);

        let (grpc_tx, grpc_rx) = mpsc::channel();
        grpc::start(
            config.server_host.clone(),
            config.grpc_port,
            config.grpc_auth_token.clone(),
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
            last_qr_image: None,
            tray,
            allow_close: false,
            grpc_rx: Some(grpc_rx),
            tray_rx,
            clipboard_job_tx,
            paste_notice: None,
            clipboard_notice_rx,
        }
    }

    fn start_grpc(&mut self, ctx: &Context) {
        let (grpc_tx, grpc_rx) = mpsc::channel();
        grpc::start(
            self.config.server_host.clone(),
            self.config.grpc_port,
            self.config.grpc_auth_token.clone(),
            grpc_tx,
            ctx.clone(),
        );
        self.grpc_rx = Some(grpc_rx);
        self.state = AppState::Connecting;
        self.connecting_target = None;
        self.last_connect_status = None;
        self.last_qr_image = None;
    }
}

impl eframe::App for App {
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
                        ClientEvent::SessionToken { url, .. } => {
                            self.connecting_target = None;
                            self.last_connect_status = None;
                            let (texture, color_image) = generate_qr_texture(ctx, &url);
                            self.last_qr_image = Some(color_image);
                            self.state = AppState::WaitingScan {
                                qr_texture: texture,
                                url,
                            };
                        }
                        ClientEvent::MobileConnected { device_info } => {
                            self.connecting_target = None;
                            self.last_connect_status = None;
                            self.last_qr_image = None;
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
                            self.last_qr_image = None;
                            self.state = AppState::Error { message };
                        }
                    },
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.grpc_rx = None;
                        let waiting_scan =
                            matches!(self.state, AppState::WaitingScan { .. });
                        if waiting_scan {
                            if let Some(qr_image) = self.last_qr_image.take() {
                                let blurred = blur_color_image(qr_image);
                                let blurred_texture = ctx.load_texture(
                                    "qr_code_blurred",
                                    blurred,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.state = AppState::Expired { blurred_texture };
                                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                            } else {
                                self.state = AppState::Connecting;
                                self.last_connect_status =
                                    Some("连接已断开，请点击刷新重试。".to_string());
                            }
                        } else if !matches!(self.state, AppState::Error { .. }) {
                            self.state = AppState::Connecting;
                            self.last_connect_status =
                                Some("连接已断开，请点击刷新重试。".to_string());
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
        let mut reconnect = false;

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
                AppState::Expired { blurred_texture } => {
                    let (rect, response) =
                        ui.allocate_exact_size(vec2(256.0, 256.0), Sense::click());

                    let painter = ui.painter();

                    painter.image(
                        blurred_texture.id(),
                        rect,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );

                    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(120));
                    painter.text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "已过期\n\n点击刷新二维码",
                        FontId::proportional(18.0),
                        Color32::WHITE,
                    );

                    if response.clicked() {
                        reconnect = true;
                    }
                }
                AppState::Error { message } => {
                    ui.colored_label(egui::Color32::RED, format!("错误：{message}"));
                }
            }
        });

        if reconnect {
            self.start_grpc(ui.ctx());
        }
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

fn generate_qr_texture(ctx: &Context, url: &str) -> (TextureHandle, ColorImage) {
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

    let color_image = ColorImage {
        size: [w, h],
        pixels,
        source_size: egui::vec2(w as f32, h as f32),
    };

    let texture = ctx.load_texture("qr_code", color_image.clone(), egui::TextureOptions::LINEAR);
    (texture, color_image)
}

fn placeholder_qr_texture(ctx: &Context) -> (TextureHandle, ColorImage) {
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

    let texture = ctx.load_texture(
        "qr_code_placeholder",
        color_image.clone(),
        egui::TextureOptions::LINEAR,
    );
    (texture, color_image)
}

fn blur_color_image(src: ColorImage) -> ColorImage {
    let [w, h] = src.size;
    let luma_buf: image::ImageBuffer<image::Luma<u8>, Vec<u8>> =
        image::ImageBuffer::from_fn(w as u32, h as u32, |x, y| {
            image::Luma([src.pixels[y as usize * w + x as usize].r()])
        });
    let blurred = image::imageops::blur(&luma_buf, 4.0);
    let pixels: Vec<egui::Color32> = blurred
        .pixels()
        .map(|p| egui::Color32::from_gray(p.0[0]))
        .collect();
    ColorImage {
        size: [w, h],
        pixels,
        source_size: egui::vec2(w as f32, h as f32),
    }
}
