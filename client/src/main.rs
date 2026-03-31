// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod clipboard;
mod config;
mod grpc;
mod icon;
mod tray;
mod ui;

use {
    egui::ViewportCommand,
    rustls::crypto::aws_lc_rs,
    tracing_subscriber::{EnvFilter, prelude::*},
};

fn main() -> eframe::Result<()> {
    let _ = aws_lc_rs::default_provider().install_default();

    init_tracing();

    let app_icon = icon::load_app_icon().ok();

    let cfg = match config::ClientConfig::load() {
        Ok(c) => c,
        Err(error) => {
            tracing::error!("客户端配置加载失败：{error}");
            return show_startup_error(error, app_icon);
        }
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("qr-paste")
        .with_inner_size([360.0, 480.0])
        .with_visible(!cfg.start_minimized);
    if let Some(icon) = app_icon {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "qr-paste",
        options,
        Box::new(|cc| Ok(Box::new(ui::App::new(cc, cfg)))),
    )
}

fn init_tracing() {
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init();
}

fn show_startup_error(message: String, app_icon: Option<egui::IconData>) -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("qr-paste 配置错误")
        .with_inner_size([420.0, 200.0])
        .with_resizable(false);
    if let Some(icon) = app_icon {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "qr-paste 配置错误",
        options,
        Box::new(move |cc| {
            egui_cjk_font::load_cjk_font(&cc.egui_ctx);
            Ok(Box::new(StartupErrorApp { message }))
        }),
    )
}

struct StartupErrorApp {
    message: String,
}

impl eframe::App for StartupErrorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.vertical_centered(|ui| {
            ui.add_space(16.0);
            ui.heading("启动失败");
            ui.add_space(12.0);
            ui.label(self.message.as_str());
            ui.add_space(12.0);
            if ui.button("退出").clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
        });
    }
}
