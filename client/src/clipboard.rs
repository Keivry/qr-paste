// SPDX-License-Identifier: MIT OR Apache-2.0

/// 将文本写入系统剪贴板。
///
/// 使用 `arboard` crate 访问系统剪贴板。
///
/// # Errors
///
/// 剪贴板初始化失败或写入失败时返回包含原因的 `String`。
/// 传入空字符串时直接返回 `Ok(())`，不执行任何操作。
pub fn write_to_clipboard(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("Clipboard init failed: {e}"))?;
    clipboard
        .set_text(text)
        .map_err(|e| format!("Clipboard write failed: {e}"))
}

/// 模拟按下 `Ctrl+V` 组合键，将剪贴板内容粘贴到当前焦点输入框。
///
/// 在按键前等待 `delay_ms` 毫秒，以确保剪贴板写入已生效。
/// 使用 `enigo` crate 发送虚拟键盘事件。
///
/// 若 `enigo` 初始化失败（例如在无桌面环境下运行），则记录告警并退出。
pub fn simulate_paste(delay_ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    use enigo::{Enigo, Key, Keyboard, Settings};
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!("Paste simulation unavailable: {err}");
            return;
        }
    };
    if let Err(err) = enigo.key(Key::Control, enigo::Direction::Press) {
        tracing::warn!("Failed to press Ctrl for paste: {err}");
        return;
    }
    if let Err(err) = enigo.key(Key::Unicode('v'), enigo::Direction::Click) {
        tracing::warn!("Failed to send V key for paste: {err}");
    }
    if let Err(err) = enigo.key(Key::Control, enigo::Direction::Release) {
        tracing::warn!("Failed to release Ctrl for paste: {err}");
    }
}
