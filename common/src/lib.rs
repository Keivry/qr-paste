// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

/// 手机端发送给服务端的 WebSocket 消息。
///
/// 使用带 `type` 标签的 JSON 格式序列化，例如：
/// ```json
/// {"type":"clipboard_text","content":"hello"}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MobileMessage {
    /// 用户在手机页面输入并发送的文本内容，目标是写入 PC 剪贴板。
    ClipboardText { content: String },
}

/// 服务端发送给手机端的 WebSocket 消息。
///
/// 使用带 `type` 标签的 JSON 格式序列化，例如：
/// ```json
/// {"type":"client_disconnected"}
/// {"type":"error","message":"PC client not connected"}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToMobileMessage {
    /// PC 客户端已断开 gRPC 连接，会话不再可用。
    ClientDisconnected,
    /// 发生错误，`message` 字段包含人类可读的描述。
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_message_clipboard_text_roundtrip() {
        let msg = MobileMessage::ClipboardText {
            content: "hello".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"clipboard_text","content":"hello"}"#);
        let parsed: MobileMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            MobileMessage::ClipboardText { content } => assert_eq!(content, "hello"),
        }
    }

    #[test]
    fn server_to_mobile_client_disconnected_roundtrip() {
        let msg = ServerToMobileMessage::ClientDisconnected;
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"client_disconnected"}"#);
        let parsed: ServerToMobileMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, ServerToMobileMessage::ClientDisconnected));
    }

    #[test]
    fn server_to_mobile_error_roundtrip() {
        let msg = ServerToMobileMessage::Error {
            message: "something went wrong".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"error","message":"something went wrong"}"#);
        let parsed: ServerToMobileMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            ServerToMobileMessage::Error { message } => {
                assert_eq!(message, "something went wrong")
            }
            _ => panic!("unexpected variant"),
        }
    }
}
