// SPDX-License-Identifier: MIT OR Apache-2.0

/// 粘贴前剪贴板内容的快照，用于粘贴后还原。
pub enum ClipboardSnapshot {
    /// 剪贴板包含非空文本内容，保存文本以便还原。
    Text(String),
    /// 剪贴板为空文本（`""`），还原时写回空字符串。
    EmptyText,
    /// 当前剪贴板不可作为文本读取（如图片），跳过还原。
    NonText,
    /// 剪贴板不可读（如初始化失败），跳过还原。
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 剪贴板还原流程的稳定错误码枚举。
pub enum ClipboardRestoreError {
    OpenFailed,
    SetFailed,
}

impl std::fmt::Display for ClipboardRestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = match self {
            Self::OpenFailed => "clipboard_open_failed",
            Self::SetFailed => "clipboard_restore_failed",
        };
        f.write_str(code)
    }
}

impl std::error::Error for ClipboardRestoreError {}

/// 读取当前剪贴板内容并返回快照。
pub fn take_snapshot() -> ClipboardSnapshot {
    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(_) => return ClipboardSnapshot::Unreadable,
    };
    match cb.get_text() {
        Ok(s) if s.is_empty() => ClipboardSnapshot::EmptyText,
        Ok(s) => ClipboardSnapshot::Text(s),
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("not available as text") {
                ClipboardSnapshot::NonText
            } else {
                ClipboardSnapshot::Unreadable
            }
        }
    }
}

/// 将剪贴板还原为快照内容，仅当当前内容仍等于 `injected_content` 时执行（竞争检测）。
///
/// `NonText` 和 `Unreadable` 快照直接跳过，不写入任何内容。
pub fn restore_clipboard(
    snapshot: &ClipboardSnapshot,
    injected_content: &str,
) -> Result<(), ClipboardRestoreError> {
    let restore_text = match snapshot {
        ClipboardSnapshot::Text(s) => s.as_str(),
        ClipboardSnapshot::EmptyText => "",
        ClipboardSnapshot::NonText | ClipboardSnapshot::Unreadable => return Ok(()),
    };
    let mut cb = arboard::Clipboard::new().map_err(|_| ClipboardRestoreError::OpenFailed)?;
    match cb.get_text() {
        Ok(current) if current != injected_content => {
            tracing::debug!(
                injected_len = injected_content.len(),
                current_len = current.len(),
                "Restore skipped: clipboard modified externally"
            );
            return Ok(());
        }
        Err(_) => {
            tracing::warn!("Restore skipped: clipboard read failed");
            return Ok(());
        }
        Ok(_) => {}
    }
    cb.set_text(restore_text)
        .map_err(|_| ClipboardRestoreError::SetFailed)
}

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

/// 解析配置中的按键规格字符串，返回可选修饰键与主键。
pub fn parse_key_spec(s: &str) -> Option<(Option<enigo::Key>, enigo::Key)> {
    use enigo::Key;
    let (modifier_str, key_str) = match s.split_once('+') {
        Some((m, k)) => (Some(m), k),
        None => (None, s),
    };
    let modifier = match modifier_str {
        Some("ctrl") => Some(Key::Control),
        Some("shift") => Some(Key::Shift),
        Some("alt") => Some(Key::Alt),
        Some("meta") => Some(Key::Meta),
        Some(unknown) => {
            tracing::warn!("Unknown modifier in key spec '{s}': '{unknown}'");
            return None;
        }
        None => None,
    };
    let key = match key_str {
        "Return" => Key::Return,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "Space" => Key::Space,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Up" => Key::UpArrow,
        "Down" => Key::DownArrow,
        "Left" => Key::LeftArrow,
        "Right" => Key::RightArrow,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        unknown => {
            tracing::warn!("Unknown key name in key spec '{s}': '{unknown}'");
            return None;
        }
    };
    Some((modifier, key))
}

/// 模拟按下指定按键（可选修饰键）。
///
/// 若 `enigo` 初始化失败则记录告警并退出。
pub fn simulate_key(modifier: Option<enigo::Key>, key: enigo::Key) {
    use enigo::{Enigo, Keyboard, Settings};
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!("Key simulation unavailable: {err}");
            return;
        }
    };
    if let Some(m) = modifier
        && let Err(err) = enigo.key(m, enigo::Direction::Press)
    {
        tracing::warn!("Failed to press modifier key: {err}");
        return;
    }
    if let Err(err) = enigo.key(key, enigo::Direction::Click) {
        tracing::warn!("Failed to send key: {err}");
    }
    if let Some(m) = modifier
        && let Err(err) = enigo.key(m, enigo::Direction::Release)
    {
        tracing::warn!("Failed to release modifier key: {err}");
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{ClipboardRestoreError, parse_key_spec},
        enigo::Key,
    };

    #[test]
    fn parse_key_spec_accepts_extended_allowlist() {
        assert_eq!(parse_key_spec("Delete"), Some((None, Key::Delete)));
        assert_eq!(parse_key_spec("Up"), Some((None, Key::UpArrow)));
        assert_eq!(parse_key_spec("PageDown"), Some((None, Key::PageDown)));
        assert_eq!(parse_key_spec("F12"), Some((None, Key::F12)));
        assert_eq!(
            parse_key_spec("ctrl+Home"),
            Some((Some(Key::Control), Key::Home))
        );
        assert_eq!(
            parse_key_spec("meta+Right"),
            Some((Some(Key::Meta), Key::RightArrow))
        );
    }

    #[test]
    fn parse_key_spec_rejects_unknown_or_multi_modifier_keys() {
        assert_eq!(parse_key_spec("enter"), None);
        assert_eq!(parse_key_spec("ctrl+shift+Tab"), None);
        assert_eq!(parse_key_spec("super+Return"), None);
        assert_eq!(parse_key_spec("A"), None);
    }

    #[test]
    fn restore_error_uses_fixed_codes() {
        assert_eq!(
            ClipboardRestoreError::OpenFailed.to_string(),
            "clipboard_open_failed"
        );
        assert_eq!(
            ClipboardRestoreError::SetFailed.to_string(),
            "clipboard_restore_failed"
        );
    }
}
