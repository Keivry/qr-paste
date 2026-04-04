// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::grpc::relay::ServerEvent,
    dashmap::DashMap,
    std::{sync::Arc, time::Instant},
    tokio::sync::mpsc,
    uuid::Uuid,
};

/// 服务端会话，代表一次 PC 客户端发起的 gRPC Subscribe 调用。
///
/// 每次 Subscribe 调用会生成一个新令牌并创建一个 Session，存储在 [`SessionStore`] 中。
/// 手机扫码后通过 WebSocket 与该 Session 绑定的 PC 客户端通信。
pub struct Session {
    /// 会话令牌（UUID v4 字符串），同时也是 SessionStore 的 key。
    #[allow(dead_code)]
    pub token: String,
    /// 会话创建时刻，用于计算是否超过 `token_expiry_secs`。
    pub created_at: Instant,
    /// 兼容旧流程保留的遗留字段，当前已不再写入 `true`；新逻辑应使用 `pairing_id` 跟踪配对关系。
    #[allow(dead_code)]
    pub scanned: bool,
    /// 手机端发起 WebSocket 升级时记录的预留时刻。
    ///
    /// 用于在升级完成前阻止其他扫码请求，超过 `UPGRADE_RESERVATION_TIMEOUT_SECS` 后预留自动失效。
    pub upgrade_reserved_at: Option<Instant>,
    /// WebSocket 连接是否活跃。连接建立后置为 `true`，断开时仅回写为 `false`，会话继续保留，
    /// 由 TTL 清理任务按生命周期统一回收。
    pub ws_active: bool,
    /// 向 PC 客户端 gRPC 流发送事件的通道发送端。
    ///
    /// 手机端发来消息时，服务端通过此通道将事件推送给 PC 客户端。
    pub client_tx: Option<mpsc::Sender<Result<ServerEvent, tonic::Status>>>,
    /// 向手机端 WebSocket 发送控制指令的无界通道发送端。
    ///
    /// 目前用于在 PC 客户端断开时向手机端推送断开通知。`None` 表示手机端尚未建立 WebSocket。
    pub mobile_control_tx: Option<mpsc::UnboundedSender<String>>,
    /// 手机端设备信息（通常为握手时采集的 User-Agent）。
    #[allow(dead_code)]
    pub device_info: Option<String>,
    /// 稳定 pairing 标识；旧客户端为空。
    pub pairing_id: Option<Uuid>,
    /// 服务端为本次 gRPC Subscribe 会话生成的随机令牌，用于 Ping RPC 鉴权。
    pub grpc_session_token: String,
}

/// 线程安全的会话存储，key 为令牌字符串，value 为 [`Session`]。
///
/// 由 `Arc<DashMap>` 组成，可在多个 tokio 任务间安全共享。
pub type SessionStore = Arc<DashMap<String, Session>>;

/// 创建一个空的 [`SessionStore`]。
pub fn new_store() -> SessionStore { Arc::new(DashMap::new()) }
