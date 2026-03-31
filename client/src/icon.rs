// SPDX-License-Identifier: MIT OR Apache-2.0

const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

struct IconRgba {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

fn load_icon_rgba() -> Result<IconRgba, String> {
    let image = image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png)
        .map_err(|err| format!("failed to decode app icon PNG: {err}"))?
        .into_rgba8();

    Ok(IconRgba {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

pub fn load_app_icon() -> Result<egui::IconData, String> {
    let icon = load_icon_rgba()?;
    Ok(egui::IconData {
        rgba: icon.rgba,
        width: icon.width,
        height: icon.height,
    })
}

pub fn load_tray_icon() -> Result<tray_icon::Icon, String> {
    let icon = load_icon_rgba()?;
    tray_icon::Icon::from_rgba(icon.rgba, icon.width, icon.height).map_err(|err| err.to_string())
}
