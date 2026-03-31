// SPDX-License-Identifier: MIT OR Apache-2.0

use {crate::icon, std::sync::mpsc};

/// 系统托盘触发的用户事件。
pub enum TrayEvent {
    /// 用户点击托盘图标或菜单中的"显示窗口"项。
    ShowWindow,
    /// 用户点击菜单中的"退出"项。
    Quit,
}

pub struct Tray {
    _icon: tray_icon::TrayIcon,
}

impl Drop for Tray {
    fn drop(&mut self) {
        tray_icon::TrayIconEvent::set_event_handler(None::<fn(tray_icon::TrayIconEvent)>);
        tray_icon::menu::MenuEvent::set_event_handler(None::<fn(tray_icon::menu::MenuEvent)>);
    }
}

pub fn start(tx: mpsc::Sender<TrayEvent>, repaint_ctx: egui::Context) -> Result<Tray, String> {
    use tray_icon::{
        MouseButton,
        MouseButtonState,
        TrayIconBuilder,
        TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem},
    };

    let menu = Menu::new();
    let show_item = MenuItem::new("显示窗口", true, None);
    let quit_item = MenuItem::new("退出", true, None);
    menu.append(&show_item)
        .map_err(|err| format!("failed to append tray menu item 'show': {err}"))?;
    menu.append(&quit_item)
        .map_err(|err| format!("failed to append tray menu item 'quit': {err}"))?;

    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("qr-paste")
        .with_icon(load_icon()?)
        .build()
        .map_err(|err| format!("failed to create tray icon: {err}"))?;

    let tray_tx = tx.clone();
    let tray_repaint_ctx = repaint_ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = send_event(&tray_tx, &tray_repaint_ctx, TrayEvent::ShowWindow);
        }
    }));

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            let _ = send_event(&tx, &repaint_ctx, TrayEvent::ShowWindow);
        } else if event.id == quit_id {
            let _ = send_event(&tx, &repaint_ctx, TrayEvent::Quit);
        }
    }));

    Ok(Tray { _icon: tray })
}

/// 加载托盘图标。
fn load_icon() -> Result<tray_icon::Icon, String> { icon::load_tray_icon() }

fn send_event(tx: &mpsc::Sender<TrayEvent>, repaint_ctx: &egui::Context, event: TrayEvent) -> bool {
    if tx.send(event).is_err() {
        return false;
    }
    repaint_ctx.request_repaint();
    true
}
