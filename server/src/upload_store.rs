// SPDX-License-Identifier: MIT OR Apache-2.0

//! 服务端文件上传临时存储：`UploadStore` 及关联类型。
//!
//! 负责管理上传文件的内存状态索引、磁盘文件路径映射、全局并发计数器，以及
//! pairing 级别的文件生命周期管理。

use {
    bytes::Bytes,
    dashmap::DashMap,
    rand::RngExt,
    sha2::Sha256,
    std::{
        path::{Path, PathBuf},
        sync::{
            Arc,
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    },
    tokio::{
        io::AsyncWriteExt,
        sync::mpsc::{Receiver, Sender},
    },
    tracing::{info, warn},
    uuid::Uuid,
};

/// gRPC 流式推送的单个传输单元。
///
/// STREAMING 路径下，upload handler 按顺序发送 `Data` 帧，
/// 最后发送 `End` 或 `Abort` 终止帧。
#[derive(Debug)]
pub enum StreamItem {
    /// 文件数据块。
    Data(Bytes),
    /// 正常终止帧，携带完整文件的 SHA-256 摘要（32 字节原始数据）。
    End { sha256: [u8; 32] },
    /// 异常中止帧，携带可读的错误原因。
    Abort { reason: String },
}

/// STREAMING 路径的单文件流状态，存储在 `UploadStore::streaming_states` 中。
struct StreamingState {
    /// 发送端，由 upload handler 持有并推送数据帧。
    tx: Sender<StreamItem>,
    /// 接收端，`attach_stream()` 调用时被 take 走交给 gRPC handler。
    rx: Option<Receiver<StreamItem>>,
    /// 是否已被 gRPC handler attach（原子量，防止重复 attach）。
    attached: AtomicBool,
}

/// `attach_stream()` 可能返回的错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// 指定 file_id 没有对应的 StreamingState。
    NotFound,
    /// 该 file_id 的流已被另一个 gRPC handler attach。
    AlreadyAttached,
}

/// `send_chunk()` 的返回值，指示发送结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendChunkResult {
    /// 发送成功。
    Ok,
    /// Receiver 已关闭（gRPC handler 断连）。
    Disconnected,
    /// 5 秒内未能成功发送（backpressure 超时）。
    Timeout,
}

/// `send_end` / `send_abort` 的发送结果。
pub enum SendTerminalResult {
    /// 终态帧已送达 receiver。
    Sent,
    /// Receiver 已关闭（gRPC handler 断连），终态帧未送达。
    Disconnected,
    /// 5 秒内 channel 未就绪，终态帧未送达。
    Timeout,
}

/// 文件在服务端生命周期内的状态流转。
///
/// 状态流转路径（正常）：
/// `Uploading` → `StoredUnnotified` → `Notifying` → `Notified` → `Acked`
///
/// 异常路径：
/// - 上传失败：从 `Uploading` 直接移除（文件计数与字节计数同步回滚）
/// - 推送失败：从 `Notifying` 直接移除并删除磁盘文件
/// - Pairing 清理：仅删除 `StoredUnnotified` 状态文件；跳过 `Uploading`、`Notifying`；保留
///   `Notified` 等待 ACK
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FileStatus {
    /// 文件正在上传中，临时占位（已计入全局计数器）。
    Uploading,
    /// 文件已写入磁盘，尚未向 PC 客户端推送 `FileReceived` 事件。
    StoredUnnotified,
    /// 正在向 PC 客户端推送 `FileReceived` gRPC 事件，防止与清理并发。
    Notifying,
    /// `FileReceived` 事件已成功推送，等待客户端 ACK 或超时兜底清理。
    Notified,
    /// 客户端已确认（ACK），文件可被安全移除。
    Acked,
}

/// 服务端为每个上传文件维护的内存元数据。
#[derive(Debug)]
pub struct FileMeta {
    /// 所属配对的唯一标识。
    pub pairing_id: Uuid,
    /// 文件当前状态。
    pub status: FileStatus,
    /// 文件写入完成的时刻（`Uploading` 期间为占位时刻，完成后不更新）。
    #[allow(dead_code)]
    pub uploaded_at: Instant,
    /// 磁盘文件路径（在 `StoredUnnotified` 之后有效）。
    pub file_path: PathBuf,
    /// 文件实际字节大小（`Uploading` 期间为悲观预留大小，完成后修正为实际值）。
    pub size_bytes: u64,
    /// 每文件独立生成的 32 字节随机访问令牌，通过 gRPC `FileReceived` 事件下发给 PC 客户端。
    pub file_access_token: [u8; 32],
    /// 清洗后的安全文件名（用于磁盘存储和客户端保存路径）。
    pub file_name: String,
    /// 客户端上传时提供的原始文件名（未清洗，仅用于向上传方回显）。
    #[allow(dead_code)]
    pub raw_filename: String,
    /// MIME 类型。
    pub mime_type: String,
    /// 文件 SHA-256 哈希（十六进制字符串，`Uploading` 期间为空）。
    pub sha256: String,
}

/// 上传容量超限错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadLimitError {
    /// 单个 pairing 内 pending 文件数已达上限。
    PerPairingFileLimitReached,
    /// 全局 pending 文件数已达上限。
    GlobalFileLimitReached,
    /// 全局 pending 字节数已达上限。
    GlobalByteLimitReached,
    /// Pairing 已关闭，不接受新上传。
    PairingClosed,
}

/// 配额检查与占位的内部共享状态。
///
/// 所有读写必须在 `UploadStore::reservation` Mutex 保护下进行，
/// 确保"检查 + 占位"操作的原子性，防止并发请求绕过容量上限。
#[derive(Debug, Default)]
struct ReservationState {
    /// pairing ID → pairing 状态（文件列表 + 关闭标志）。
    pairings: std::collections::HashMap<Uuid, PairingState>,
    /// 全局 pending 文件数（含 `Uploading`/`StoredUnnotified`/`Notifying`/`Notified`）。
    global_pending_files: u32,
    /// 全局 pending 文件总字节数（`Uploading` 期间为悲观预留值）。
    global_pending_bytes: u64,
}

impl ReservationState {
    fn is_pairing_closed(&self, pairing_id: Uuid) -> bool {
        self.pairings.get(&pairing_id).is_some_and(|ps| ps.closed)
    }

    fn count_pending_for_pairing(&self, pairing_id: Uuid) -> u32 {
        self.pairings
            .get(&pairing_id)
            .map(|ps| ps.pending_count)
            .unwrap_or(0)
    }

    /// 登记占位：将 file_id 加入 pairing，更新全局计数。
    fn reserve(&mut self, file_id: Uuid, pairing_id: Uuid, reserved_bytes: u64) {
        let ps = self.pairings.entry(pairing_id).or_default();
        ps.file_ids.push(file_id);
        ps.pending_count += 1;
        self.global_pending_files += 1;
        self.global_pending_bytes = self.global_pending_bytes.saturating_add(reserved_bytes);
    }

    /// 回滚占位（上传失败或 pairing 已关闭时调用）。
    fn release(&mut self, file_id: Uuid, pairing_id: Uuid, reserved_bytes: u64) {
        if let Some(ps) = self.pairings.get_mut(&pairing_id)
            && ps.file_ids.contains(&file_id)
        {
            ps.file_ids.retain(|id| *id != file_id);
            ps.pending_count = ps.pending_count.saturating_sub(1);
            self.global_pending_files = self.global_pending_files.saturating_sub(1);
            self.global_pending_bytes = self.global_pending_bytes.saturating_sub(reserved_bytes);
        }
    }

    /// 修正字节预留（从悲观预留修正为实际大小）。
    fn correct_bytes(&mut self, old_reserved: u64, actual_size: u64) {
        if actual_size > old_reserved {
            self.global_pending_bytes = self
                .global_pending_bytes
                .saturating_add(actual_size - old_reserved);
        } else {
            self.global_pending_bytes = self
                .global_pending_bytes
                .saturating_sub(old_reserved - actual_size);
        }
    }
}

/// `UploadStore` 的内部 pairing 状态。
#[derive(Debug, Default)]
struct PairingState {
    /// 该 pairing 关联的所有文件 ID 列表。
    file_ids: Vec<Uuid>,
    /// 是否已关闭（调用 `cleanup_pairing` 后置为 true）。
    closed: bool,
    /// 当前 pending 文件计数（含 Uploading 状态）。
    pending_count: u32,
}

/// 服务端文件上传临时存储。
///
/// 管理内存中的文件状态索引与全局并发计数器，线程安全。
pub struct UploadStore {
    /// 文件 ID → 文件元数据（DashMap 仅用于文件内容读写，不承担配额计数）。
    files: DashMap<Uuid, FileMeta>,
    /// 配额状态（含 pairing 映射和全局计数器），由 Mutex 保护确保原子性。
    reservation: Mutex<ReservationState>,
    /// 服务端配置快照（用于容量上限检查）。
    max_upload_size_bytes: u64,
    max_pending_upload_files_per_pairing: u32,
    max_pending_upload_files_global: u32,
    max_pending_upload_bytes_global: u64,
    /// STREAMING 路径：file_id → 流状态（发送/接收端 + attached 标志）。
    streaming_states: DashMap<Uuid, StreamingState>,
}

impl UploadStore {
    /// 创建新的 `UploadStore`。
    pub fn new(
        max_upload_size_bytes: u64,
        max_pending_upload_files_per_pairing: u32,
        max_pending_upload_files_global: u32,
        max_pending_upload_bytes_global: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            files: DashMap::new(),
            reservation: Mutex::new(ReservationState::default()),
            max_upload_size_bytes,
            max_pending_upload_files_per_pairing,
            max_pending_upload_files_global,
            max_pending_upload_bytes_global,
            streaming_states: DashMap::new(),
        })
    }

    async fn write_stream_to_file(
        &self,
        path: &Path,
        field: axum::extract::multipart::Field<'_>,
        max_bytes: u64,
        chunk_timeout: std::time::Duration,
    ) -> Result<(u64, String), SaveFileError> {
        use {futures::StreamExt as _, sha2::Digest as _};

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map_err(|e| SaveFileError::Io(format!("创建临时文件失败: {e}")))?;

        let mut hasher = Sha256::new();
        let mut total_bytes: u64 = 0;
        let mut field = field;

        loop {
            let next = tokio::time::timeout(chunk_timeout, field.next()).await;
            let chunk = match next {
                Err(_) => return Err(SaveFileError::Timeout),
                Ok(None) => break,
                Ok(Some(res)) => {
                    res.map_err(|e| SaveFileError::Io(format!("读取 multipart chunk 失败: {e}")))?
                }
            };
            let chunk_len = chunk.len() as u64;
            total_bytes = total_bytes.saturating_add(chunk_len);
            if total_bytes > max_bytes {
                let _ = file.flush().await;
                return Err(SaveFileError::TooLarge);
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| SaveFileError::Io(format!("写入磁盘失败: {e}")))?;
        }

        file.flush()
            .await
            .map_err(|e| SaveFileError::Io(format!("flush 失败: {e}")))?;
        drop(file);

        let hash = hasher.finalize();
        let sha256_hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        Ok((total_bytes, sha256_hex))
    }

    /// 回滚上传占位：从内存索引中移除文件记录并还原全局计数器。
    ///
    /// 同时幂等清理可能残留的 StreamingState（STREAMING 路径失败时的 orphan 清理）。
    pub fn rollback_upload(&self, file_id: Uuid, pairing_id: Uuid) {
        self.streaming_states.remove(&file_id);
        let reserved_bytes = self
            .files
            .remove(&file_id)
            .map(|(_, m)| m.size_bytes)
            .unwrap_or(self.max_upload_size_bytes);
        self.reservation
            .lock()
            .unwrap()
            .release(file_id, pairing_id, reserved_bytes);
    }

    #[allow(dead_code)]
    pub fn get_file_path(&self, file_id: Uuid) -> Option<PathBuf> {
        self.files
            .get(&file_id)
            .filter(|m| {
                matches!(
                    m.status,
                    FileStatus::StoredUnnotified | FileStatus::Notifying | FileStatus::Notified
                )
            })
            .map(|m| m.file_path.clone())
    }

    /// 获取文件元数据快照（供 gRPC 推送和下载鉴权使用）。
    ///
    /// 返回 `Option<(file_access_token, file_name, mime_type, size_bytes, sha256, status)>`。
    pub fn get_file_meta(&self, file_id: Uuid) -> Option<FileMetaSnapshot> {
        self.files.get(&file_id).map(|m| FileMetaSnapshot {
            pairing_id: m.pairing_id,
            status: m.status.clone(),
            file_path: m.file_path.clone(),
            file_access_token: m.file_access_token,
            file_name: m.file_name.clone(),
            mime_type: m.mime_type.clone(),
            size_bytes: m.size_bytes,
            sha256: m.sha256.clone(),
        })
    }

    /// 将文件状态从 `StoredUnnotified` 切换为 `Notifying`（gRPC 推送前调用）。
    ///
    /// 在同一临界区内原子完成"pairing closed 检查 + 状态迁移"，消除 `cleanup_pairing`
    /// 与通知启动之间的竞态窗口。
    ///
    /// 返回 `true` 表示切换成功，`false` 表示 pairing 已关闭或文件状态不符合预期。
    pub fn begin_notifying(&self, file_id: Uuid, pairing_id: Uuid) -> bool {
        // 持有 reservation 锁期间同步检查 closed 并迁移 DashMap 状态，
        // 确保 cleanup_pairing 的 closed 标记与通知启动之间不存在竞态窗口。
        let res = self.reservation.lock().unwrap();
        if res.is_pairing_closed(pairing_id) {
            return false;
        }
        if let Some(mut entry) = self.files.get_mut(&file_id)
            && entry.status == FileStatus::StoredUnnotified
            && entry.pairing_id == pairing_id
        {
            entry.status = FileStatus::Notifying;
            return true;
        }
        false
    }

    /// 推送成功后将文件状态切换为 `Notified`。
    pub fn mark_notified(&self, file_id: Uuid) {
        if let Some(mut entry) = self.files.get_mut(&file_id)
            && entry.status == FileStatus::Notifying
        {
            entry.status = FileStatus::Notified;
        }
    }

    /// 推送失败后删除内存记录、回滚计数器，并异步删除磁盘文件。
    ///
    /// `Notifying` 状态文件不得遗留；失败时立即清除。
    pub fn mark_notify_failed(&self, file_id: Uuid) {
        if let Some((_, meta)) = self.files.remove(&file_id) {
            {
                let mut res = self.reservation.lock().unwrap();
                res.release(file_id, meta.pairing_id, meta.size_bytes);
            }
            // 异步删除磁盘文件，避免无活跃订阅或推送失败路径留下孤儿文件。
            let path = meta.file_path.clone();
            tokio::spawn(async move {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!(
                        "mark_notify_failed 删除文件失败 path={}: {e}",
                        path.display()
                    );
                }
            });
        }
    }

    /// Pairing 会话结束时清理关联的临时文件。
    ///
    /// 先在 Mutex 保护下原子标记 pairing 为 closed 并获取文件快照，
    /// 再依据快照**仅**删除 `StoredUnnotified` 状态的文件。
    /// - `Uploading`：跳过（由 `save_file()` 完成后检查 pairing 状态自行清理）
    /// - `Notifying`：跳过（由推送完成回调处理）
    /// - `Notified`：保留（等待 ACK 或超时兜底）
    pub fn cleanup_pairing(&self, pairing_id: Uuid) {
        // 在 Mutex 保护下原子标记 closed 并取出文件 ID 快照。
        let file_ids_snapshot: Vec<Uuid> = {
            let mut res = self.reservation.lock().unwrap();
            let ps = res.pairings.entry(pairing_id).or_default();
            ps.closed = true;
            ps.file_ids.clone()
        };

        // 仅清理 StoredUnnotified 状态的文件。
        for fid in file_ids_snapshot {
            let should_remove = self
                .files
                .get(&fid)
                .is_some_and(|m| m.status == FileStatus::StoredUnnotified);
            if should_remove && let Some((_, meta)) = self.files.remove(&fid) {
                {
                    let mut res = self.reservation.lock().unwrap();
                    res.release(fid, pairing_id, meta.size_bytes);
                }
                // 异步删除磁盘文件（fire-and-forget）。
                let path = meta.file_path.clone();
                tokio::spawn(async move {
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        warn!("cleanup_pairing 删除文件失败 path={}: {e}", path.display());
                    }
                });
            }
        }
    }

    /// 立即删除单个文件并同步减少全局计数。
    ///
    /// 用于 ACK 收到后的主动清理路径。同时幂等清理可能残留的 StreamingState。
    pub fn remove_file(&self, file_id: Uuid) {
        self.streaming_states.remove(&file_id);
        if let Some((_, meta)) = self.files.remove(&file_id) {
            {
                let mut res = self.reservation.lock().unwrap();
                res.release(file_id, meta.pairing_id, meta.size_bytes);
            }
            // 异步删除磁盘文件。
            let path = meta.file_path.clone();
            tokio::spawn(async move {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!("remove_file 删除文件失败 path={}: {e}", path.display());
                }
            });
        }
    }

    /// 扫描 `upload_dir` 删除超过 TTL 的文件，同步更新内存索引和全局计数器。
    ///
    /// - 使用 `symlink_metadata()` 避免跟随符号链接。
    /// - 仅处理 `is_file() == true` 的普通文件。
    /// - 以文件系统 mtime 为清理判据，不依赖内存中的 `Instant`。
    /// - **`Uploading` 状态文件**：跳过，不删磁盘文件，由 `save_file()` 完成后自行处理。
    /// - **孤儿文件**（无内存记录）：删除磁盘文件，**不**调整全局计数器。
    pub async fn cleanup_expired(&self, upload_dir: &Path, retention_secs: u64) {
        let mut dir = match tokio::fs::read_dir(upload_dir).await {
            Ok(d) => d,
            Err(e) => {
                warn!("cleanup_expired 读取目录失败 {}: {e}", upload_dir.display());
                return;
            }
        };

        let now = std::time::SystemTime::now();

        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path();
            let meta = match tokio::fs::symlink_metadata(&path).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(
                        "cleanup_expired symlink_metadata 失败 {}: {e}",
                        path.display()
                    );
                    continue;
                }
            };

            if !meta.is_file() {
                // 跳过符号链接、目录等。
                warn!("cleanup_expired 跳过非普通文件 {}", path.display());
                continue;
            }

            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(e) => {
                    warn!("cleanup_expired 获取 mtime 失败 {}: {e}", path.display());
                    continue;
                }
            };

            let age_secs = now.duration_since(mtime).unwrap_or_default().as_secs();

            if age_secs <= retention_secs {
                continue; // 未超时，跳过。
            }

            // 通过文件名反查内存记录，决定是否可以清理。
            // 正式文件名：纯 UUID；临时文件名：`.{uuid}.tmp`。
            let file_name_str = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            let file_id = Uuid::parse_str(file_name_str).ok().or_else(|| {
                // 尝试解析临时文件名格式 `.{uuid}.tmp`
                file_name_str
                    .strip_prefix('.')
                    .and_then(|s| s.strip_suffix(".tmp"))
                    .and_then(|s| Uuid::parse_str(s).ok())
            });

            if let Some(fid) = file_id {
                // 若内存中存在该文件且处于 Uploading 状态，跳过——上传仍在进行。
                if self
                    .files
                    .get(&fid)
                    .is_some_and(|m| m.status == FileStatus::Uploading)
                {
                    info!("cleanup_expired 跳过 Uploading 文件 file_id={fid} age={age_secs}s");
                    continue;
                }

                // 超时且非 Uploading，删除磁盘文件。
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!("cleanup_expired 删除文件失败 {}: {e}", path.display());
                    continue;
                }

                if let Some((_, m)) = self.files.remove(&fid) {
                    self.streaming_states.remove(&fid);
                    {
                        let mut res = self.reservation.lock().unwrap();
                        res.release(fid, m.pairing_id, m.size_bytes);
                    }
                    info!("cleanup_expired 删除超时文件 file_id={fid} age={age_secs}s");
                } else {
                    self.streaming_states.remove(&fid);
                    // 孤儿文件，不调整计数器。
                    info!(
                        "cleanup_expired 删除孤儿文件 path={} age={age_secs}s",
                        path.display()
                    );
                }
            } else {
                // 文件名既非 UUID 也非 `.{uuid}.tmp`，直接删除，不调整计数器。
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!("cleanup_expired 删除文件失败 {}: {e}", path.display());
                    continue;
                }
                info!(
                    "cleanup_expired 删除非 UUID 文件 path={} age={age_secs}s",
                    path.display()
                );
            }
        }
    }

    /// 服务启动时重建全局计数器基线。
    ///
    /// 扫描 `upload_dir` 下所有普通文件，将 TTL 内的文件计入基线并在内存中创建
    /// 最小化 `FileMeta` 记录（`StoredUnnotified` 状态），便于后续清理路径安全减法。
    ///
    /// **调用时机**：在启动校验（`upload_dir` 可写探测）通过后、HTTP 上传路由注册前同步调用。
    pub fn rebuild_baseline(&self, upload_dir: &Path, retention_secs: u64) -> BaselineStats {
        let mut scanned = 0u64;
        let mut accepted = 0u64;
        let mut accepted_bytes = 0u64;

        let entries = match std::fs::read_dir(upload_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(
                    "rebuild_baseline 读取目录失败 {}: {e}",
                    upload_dir.display()
                );
                return BaselineStats {
                    scanned: 0,
                    accepted: 0,
                    accepted_bytes: 0,
                };
            }
        };

        let now = std::time::SystemTime::now();

        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    warn!(
                        "rebuild_baseline symlink_metadata 失败 {}: {e}",
                        path.display()
                    );
                    continue;
                }
            };

            if !meta.is_file() {
                continue;
            }

            scanned += 1;

            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(e) => {
                    warn!("rebuild_baseline 获取 mtime 失败 {}: {e}", path.display());
                    continue;
                }
            };

            let age_secs = now.duration_since(mtime).unwrap_or_default().as_secs();
            if age_secs > retention_secs {
                // 超过 TTL，由后续 cleanup_expired 清理，不计入基线。
                continue;
            }

            let size = meta.len();
            // 只处理文件名为有效 UUID 的文件（上传成功后重命名的正式文件）。
            let file_id = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            let Some(fid) = file_id else {
                continue;
            };

            // 创建最小化内存记录（fake pairing_id，仅供计数器减法使用）。
            let placeholder_pairing = Uuid::nil();
            let file_meta = FileMeta {
                pairing_id: placeholder_pairing,
                status: FileStatus::StoredUnnotified,
                uploaded_at: Instant::now(),
                file_path: path.clone(),
                size_bytes: size,
                file_access_token: [0u8; 32],
                file_name: String::new(),
                raw_filename: String::new(),
                mime_type: String::new(),
                sha256: String::new(),
            };
            self.files.insert(fid, file_meta);

            {
                let mut res = self.reservation.lock().unwrap();
                res.reserve(fid, placeholder_pairing, size);
            }

            accepted += 1;
            accepted_bytes += size;
        }

        BaselineStats {
            scanned,
            accepted,
            accepted_bytes,
        }
    }

    /// 在 Mutex 临界区内完成配额检查、file_id 生成、file_access_token 生成，并将 FileMeta
    /// 以 `FileStatus::Uploading` 状态注册到内存索引。
    ///
    /// 调用方应在此返回后再获取 multipart Field 并决定走 STREAMING 还是 RELAY 路径。
    ///
    /// # 返回
    ///
    /// 成功时返回 `(file_id, file_access_token, sanitized_file_name)`。
    pub fn begin_upload(
        &self,
        pairing_id: Uuid,
        raw_filename: &str,
        mime_type: String,
    ) -> Result<(Uuid, [u8; 32], String), SaveFileError> {
        let file_id = Uuid::new_v4();
        let file_name = sanitize_file_name(raw_filename, &file_id);

        let mut token = [0u8; 32];
        let mut res = self.reservation.lock().unwrap();

        if res.is_pairing_closed(pairing_id) {
            return Err(SaveFileError::Limit(UploadLimitError::PairingClosed));
        }

        let per_pairing_count = res.count_pending_for_pairing(pairing_id);
        if per_pairing_count >= self.max_pending_upload_files_per_pairing {
            return Err(SaveFileError::Limit(
                UploadLimitError::PerPairingFileLimitReached,
            ));
        }

        if res.global_pending_files >= self.max_pending_upload_files_global {
            return Err(SaveFileError::Limit(
                UploadLimitError::GlobalFileLimitReached,
            ));
        }

        if res
            .global_pending_bytes
            .checked_add(self.max_upload_size_bytes)
            .is_none_or(|v| v > self.max_pending_upload_bytes_global)
        {
            return Err(SaveFileError::Limit(
                UploadLimitError::GlobalByteLimitReached,
            ));
        }

        // 在 Mutex 临界区内生成 CSPRNG token（不依赖时间戳或自增 ID）。
        rand::rng().fill(&mut token);

        let meta = FileMeta {
            pairing_id,
            status: FileStatus::Uploading,
            uploaded_at: Instant::now(),
            file_path: PathBuf::new(),
            size_bytes: self.max_upload_size_bytes,
            file_access_token: token,
            file_name: file_name.clone(),
            raw_filename: raw_filename.to_string(),
            mime_type,
            sha256: String::new(),
        };
        self.files.insert(file_id, meta);
        res.reserve(file_id, pairing_id, self.max_upload_size_bytes);

        Ok((file_id, token, file_name))
    }

    /// 为指定 file_id 创建新的 StreamingState（mpsc channel 容量 4）。
    ///
    /// 若同一 file_id 已存在 StreamingState（僵尸），先清理再插入新的。
    pub fn create_streaming_state(&self, file_id: Uuid) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let state = StreamingState {
            tx,
            rx: Some(rx),
            attached: AtomicBool::new(false),
        };
        self.streaming_states.insert(file_id, state);
    }

    /// 将 Receiver 从 StreamingState 取走并返回给 gRPC handler。
    ///
    /// 通过 `AtomicBool::compare_exchange` 原子标记 attached，防止重复 attach。
    pub fn attach_stream(&self, file_id: Uuid) -> Result<Receiver<StreamItem>, AttachError> {
        let mut entry = self
            .streaming_states
            .get_mut(&file_id)
            .ok_or(AttachError::NotFound)?;

        entry
            .attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| AttachError::AlreadyAttached)?;

        let rx = entry
            .rx
            .take()
            .expect("rx must be Some when attached=false");
        Ok(rx)
    }

    /// 向 STREAMING 通道发送一个数据块，带 5 秒 backpressure 超时。
    pub async fn send_chunk(&self, file_id: Uuid, data: Bytes) -> SendChunkResult {
        let tx = {
            let entry = match self.streaming_states.get(&file_id) {
                Some(e) => e,
                None => return SendChunkResult::Disconnected,
            };
            entry.tx.clone()
        };

        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tx.send(StreamItem::Data(data)),
        )
        .await
        {
            Ok(Ok(())) => SendChunkResult::Ok,
            Ok(Err(_)) => SendChunkResult::Disconnected,
            Err(_) => SendChunkResult::Timeout,
        }
    }

    /// STREAMING 路径上传完成后调用：将 FileStatus 直接从 Uploading 置为 Notified，
    /// 并更新 file_path、size_bytes、sha256。
    pub fn mark_streaming_done(
        &self,
        file_id: Uuid,
        final_path: PathBuf,
        actual_size: u64,
        sha256: [u8; 32],
    ) {
        if let Some(mut entry) = self.files.get_mut(&file_id) {
            let old_reserved = entry.size_bytes;
            entry.status = FileStatus::Notified;
            entry.file_path = final_path;
            entry.size_bytes = actual_size;
            entry.sha256 = sha256.iter().map(|b| format!("{b:02x}")).collect();
            let mut res = self.reservation.lock().unwrap();
            res.correct_bytes(old_reserved, actual_size);
        }
    }

    /// 发送正常终止信号；streaming_state 由 gRPC handler 在流结束后负责移除。
    ///
    /// 即使发送失败（receiver 已关闭或 5 秒 backpressure 超时），也保证状态一致。
    pub async fn send_end(&self, file_id: Uuid, sha256: [u8; 32]) -> SendTerminalResult {
        let tx = self
            .streaming_states
            .get(&file_id)
            .map(|entry| entry.tx.clone());
        if let Some(tx) = tx {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tx.send(StreamItem::End { sha256 }),
            )
            .await
            {
                Ok(Ok(())) => SendTerminalResult::Sent,
                Ok(Err(_)) => SendTerminalResult::Disconnected,
                Err(_) => SendTerminalResult::Timeout,
            }
        } else {
            SendTerminalResult::Disconnected
        }
    }

    /// 发送异常中止信号；streaming_state 由 gRPC handler 在流结束后负责移除。
    ///
    /// 即使发送失败（receiver 已关闭或 5 秒 backpressure 超时），也保证状态一致。
    pub async fn send_abort(&self, file_id: Uuid, reason: String) -> SendTerminalResult {
        let tx = self
            .streaming_states
            .get(&file_id)
            .map(|entry| entry.tx.clone());
        if let Some(tx) = tx {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tx.send(StreamItem::Abort { reason }),
            )
            .await
            {
                Ok(Ok(())) => SendTerminalResult::Sent,
                Ok(Err(_)) => SendTerminalResult::Disconnected,
                Err(_) => SendTerminalResult::Timeout,
            }
        } else {
            SendTerminalResult::Disconnected
        }
    }

    /// 幂等清理 StreamingState（不 panic，多次调用安全）。
    pub fn remove_streaming_state(&self, file_id: Uuid) { self.streaming_states.remove(&file_id); }

    /// RELAY 路径：接受外部已解析好的 multipart Field，
    /// 写盘、计算 SHA-256，将状态从 Uploading 更新为 StoredUnnotified。
    ///
    /// file_id 和 file_access_token 必须已由调用方在 Mutex 临界区内生成并注册到 FileMeta。
    pub async fn save_file_from_field(
        &self,
        upload_dir: &Path,
        file_id: Uuid,
        pairing_id: Uuid,
        field: axum::extract::multipart::Field<'_>,
        max_bytes: u64,
        chunk_timeout: std::time::Duration,
    ) -> Result<(), SaveFileError> {
        let tmp_name = format!(".{file_id}.tmp");
        let tmp_path = upload_dir.join(&tmp_name);

        let result = self
            .write_stream_to_file(&tmp_path, field, max_bytes, chunk_timeout)
            .await;

        match result {
            Ok((actual_size, sha256_hex)) => {
                let final_path = upload_dir.join(file_id.to_string());
                if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
                    warn!("重命名临时文件失败 file_id={file_id}: {e}");
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    self.rollback_upload(file_id, pairing_id);
                    return Err(SaveFileError::Io(format!("重命名失败: {e}")));
                }

                let pairing_closed = {
                    let mut res = self.reservation.lock().unwrap();
                    if res.is_pairing_closed(pairing_id) {
                        let old_reserved = self
                            .files
                            .remove(&file_id)
                            .map(|(_, m)| m.size_bytes)
                            .unwrap_or(self.max_upload_size_bytes);
                        res.release(file_id, pairing_id, old_reserved);
                        true
                    } else {
                        if let Some(mut entry) = self.files.get_mut(&file_id) {
                            let old_reserved = entry.size_bytes;
                            entry.status = FileStatus::StoredUnnotified;
                            entry.file_path = final_path.clone();
                            entry.size_bytes = actual_size;
                            entry.sha256 = sha256_hex.clone();
                            res.correct_bytes(old_reserved, actual_size);
                        }
                        false
                    }
                };

                if pairing_closed {
                    let _ = tokio::fs::remove_file(&final_path).await;
                    return Err(SaveFileError::Limit(UploadLimitError::PairingClosed));
                }

                Ok(())
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                self.rollback_upload(file_id, pairing_id);
                Err(e)
            }
        }
    }
}

/// `UploadStore::get_file_meta` 返回的文件元数据快照（避免持有 DashMap 引用）。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileMetaSnapshot {
    /// 所属配对 ID。
    pub pairing_id: Uuid,
    /// 文件当前状态。
    pub status: FileStatus,
    /// 磁盘文件路径。
    pub file_path: PathBuf,
    /// 文件访问令牌（32 字节原始数据）。
    pub file_access_token: [u8; 32],
    /// 清洗后的安全文件名（用于磁盘存储和 gRPC 推送）。
    pub file_name: String,
    /// MIME 类型。
    pub mime_type: String,
    /// 文件大小（字节）。
    pub size_bytes: u64,
    /// SHA-256 哈希（十六进制字符串）。
    pub sha256: String,
}

/// `rebuild_baseline` 返回的统计数据。
#[derive(Debug, Clone)]
pub struct BaselineStats {
    /// 扫描的总文件数。
    pub scanned: u64,
    /// 纳入基线的文件数（TTL 内）。
    pub accepted: u64,
    /// 纳入基线的总字节数。
    pub accepted_bytes: u64,
}

/// `save_file` 可能返回的错误类型。
#[derive(Debug)]
pub enum SaveFileError {
    /// 超过容量上限。
    Limit(UploadLimitError),
    /// 磁盘 I/O 错误。
    Io(String),
    /// 文件超过大小上限（流式截断）。
    TooLarge,
    /// 上传超时（chunk 间隔超过允许时限）。
    Timeout,
}

/// 创建 `Arc<UploadStore>` 的便捷函数。
pub fn new_upload_store(
    max_upload_size_bytes: u64,
    max_pending_upload_files_per_pairing: u32,
    max_pending_upload_files_global: u32,
    max_pending_upload_bytes_global: u64,
) -> Arc<UploadStore> {
    UploadStore::new(
        max_upload_size_bytes,
        max_pending_upload_files_per_pairing,
        max_pending_upload_files_global,
        max_pending_upload_bytes_global,
    )
}

/// Windows 保留文件名（大小写不敏感）。
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 清洗文件名，使其在 Windows 和 Linux 上均可安全使用。
///
/// 执行以下处理（顺序重要）：
/// 1. 提取 basename（去除路径前缀）
/// 2. 过滤控制字符、`"` 及 Windows 路径分隔符 `\` `/`（basename 后不会有 `/`，但保险起见）
/// 3. 修剪首尾空白
/// 4. 去除尾随 `.`（Windows 禁止）
/// 5. 若为 Windows 保留名（忽略大小写及扩展名），追加 `_` 前缀
/// 6. 若超过 255 字节，按字节截断并保留扩展名
/// 7. 空名时回退为 file_id 字符串
pub fn sanitize_file_name(raw: &str, fallback_id: &Uuid) -> String {
    let basename = std::path::Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(raw);

    let cleaned: String = basename
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\' && *c != '/')
        .collect();

    let trimmed = cleaned.trim().to_string();

    if trimmed.is_empty() {
        return fallback_id.to_string();
    }

    let no_trailing_dot = trimmed.trim_end_matches('.').to_string();
    if no_trailing_dot.is_empty() {
        return fallback_id.to_string();
    }

    let stem = std::path::Path::new(&no_trailing_dot)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&no_trailing_dot);
    let reserved = WINDOWS_RESERVED_NAMES
        .iter()
        .any(|r| stem.eq_ignore_ascii_case(r));
    let prefixed = if reserved {
        format!("_{no_trailing_dot}")
    } else {
        no_trailing_dot
    };

    if prefixed.len() <= 255 {
        return prefixed;
    }
    let ext = std::path::Path::new(&prefixed)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let max_stem_bytes = 255_usize.saturating_sub(ext.len());
    let mut truncated = prefixed.as_str();
    while truncated.len() > max_stem_bytes {
        truncated = &truncated[..truncated.len() - 1];
        // 确保截断点在合法的 UTF-8 边界
        while !prefixed.is_char_boundary(truncated.len()) {
            truncated = &truncated[..truncated.len() - 1];
        }
    }
    format!("{truncated}{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_limit_1() -> Arc<UploadStore> { UploadStore::new(1024 * 1024 * 1024, 10, 1, u64::MAX) }

    fn store_loose() -> Arc<UploadStore> { UploadStore::new(1024 * 1024, 10, 100, u64::MAX) }

    #[test]
    fn quota_lifecycle_begin_upload_occupies_quota() {
        let store = store_limit_1();
        let pairing_id = Uuid::new_v4();

        assert!(
            store
                .begin_upload(pairing_id, "file.txt", "text/plain".to_string())
                .is_ok()
        );

        let result = store.begin_upload(pairing_id, "file2.txt", "text/plain".to_string());
        assert!(
            matches!(
                result,
                Err(SaveFileError::Limit(
                    UploadLimitError::GlobalFileLimitReached
                ))
            ),
            "second begin_upload should fail with GlobalFileLimitReached, got {result:?}"
        );
    }

    #[test]
    fn quota_lifecycle_rollback_releases_quota() {
        let store = store_limit_1();
        let pairing_id = Uuid::new_v4();

        let (file_id, ..) = store
            .begin_upload(pairing_id, "file.txt", "text/plain".to_string())
            .unwrap();

        store.rollback_upload(file_id, pairing_id);

        assert!(
            store
                .begin_upload(pairing_id, "file2.txt", "text/plain".to_string())
                .is_ok(),
            "after rollback quota should be released"
        );
    }

    #[test]
    fn quota_lifecycle_streaming_done_keeps_quota() {
        let store = store_limit_1();
        let pairing_id = Uuid::new_v4();

        let (file_id, ..) = store
            .begin_upload(pairing_id, "file.txt", "text/plain".to_string())
            .unwrap();

        store.mark_streaming_done(file_id, PathBuf::from("/tmp/fake"), 1024, [0u8; 32]);

        let result = store.begin_upload(pairing_id, "file2.txt", "text/plain".to_string());
        assert!(
            matches!(
                result,
                Err(SaveFileError::Limit(
                    UploadLimitError::GlobalFileLimitReached
                ))
            ),
            "after mark_streaming_done quota should still be occupied"
        );
    }

    #[tokio::test]
    async fn quota_lifecycle_remove_file_releases_quota() {
        let store = store_limit_1();
        let pairing_id = Uuid::new_v4();

        let (file_id, ..) = store
            .begin_upload(pairing_id, "file.txt", "text/plain".to_string())
            .unwrap();

        store.remove_file(file_id);

        assert!(
            store
                .begin_upload(pairing_id, "file2.txt", "text/plain".to_string())
                .is_ok(),
            "after remove_file quota should be released"
        );
    }

    #[test]
    fn streaming_state_remove_idempotent() {
        let store = store_loose();
        let file_id = Uuid::new_v4();

        store.create_streaming_state(file_id);
        store.remove_streaming_state(file_id);
        store.remove_streaming_state(file_id);

        store.remove_streaming_state(Uuid::new_v4());
    }

    #[test]
    fn streaming_state_attach_succeeds_once() {
        let store = store_loose();
        let file_id = Uuid::new_v4();

        store.create_streaming_state(file_id);

        assert!(store.attach_stream(file_id).is_ok());
        assert!(matches!(
            store.attach_stream(file_id),
            Err(AttachError::AlreadyAttached)
        ));
    }

    #[test]
    fn streaming_state_attach_not_found() {
        let store = store_loose();
        assert!(matches!(
            store.attach_stream(Uuid::new_v4()),
            Err(AttachError::NotFound)
        ));
    }

    #[test]
    fn streaming_state_cleanup_after_file_received_failure() {
        let store = store_loose();
        let file_id = Uuid::new_v4();

        store.create_streaming_state(file_id);
        store.remove_streaming_state(file_id);

        assert!(matches!(
            store.attach_stream(file_id),
            Err(AttachError::NotFound)
        ));
    }

    #[tokio::test]
    async fn streaming_state_cleanup_after_send_abort() {
        let store = store_loose();
        let file_id = Uuid::new_v4();

        store.create_streaming_state(file_id);
        let _rx = store.attach_stream(file_id).unwrap();
        store.send_abort(file_id, "test_reason".to_string()).await;

        // send_abort 不移除状态；由 stream_file gRPC handler 结束时显式清理。
        // attach 应返回 AlreadyAttached（状态仍在），而非 NotFound（状态已移除）。
        assert!(matches!(
            store.attach_stream(file_id),
            Err(AttachError::AlreadyAttached)
        ));

        store.remove_streaming_state(file_id);
        assert!(matches!(
            store.attach_stream(file_id),
            Err(AttachError::NotFound)
        ));
    }

    #[tokio::test]
    async fn streaming_state_cleanup_after_send_end() {
        let store = store_loose();
        let file_id = Uuid::new_v4();

        store.create_streaming_state(file_id);
        store.send_end(file_id, [0u8; 32]).await;

        // send_end 不移除状态；由 stream_file gRPC handler 结束时显式清理。
        // 状态仍在，但 rx 尚未被 attach，因此还可以 attach（不过 channel 已关闭）。
        assert!(
            store.attach_stream(file_id).is_ok()
                || matches!(
                    store.attach_stream(file_id),
                    Err(AttachError::AlreadyAttached)
                ),
            "state should still exist after send_end"
        );

        store.remove_streaming_state(file_id);
        assert!(matches!(
            store.attach_stream(file_id),
            Err(AttachError::NotFound)
        ));
    }

    // ── 7.1: 并发 attach 测试 ──────────────────────────────────────────────

    #[tokio::test]
    async fn streaming_concurrent_attach_only_one_succeeds() {
        let store = store_loose();
        let file_id = Uuid::new_v4();
        store.create_streaming_state(file_id);

        let mut handles = Vec::new();
        for _ in 0..10 {
            let s = Arc::clone(&store);
            handles.push(tokio::spawn(async move { s.attach_stream(file_id) }));
        }

        let results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.expect("task should not panic"))
            .collect();

        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        let already_attached_count = results
            .iter()
            .filter(|r| matches!(r, Err(AttachError::AlreadyAttached)))
            .count();

        assert_eq!(ok_count, 1, "exactly one attach should succeed");
        assert_eq!(
            already_attached_count, 9,
            "remaining 9 should be AlreadyAttached"
        );
    }

    // ── 7.4b: token 唯一性 ─────────────────────────────────────────────────

    #[test]
    fn begin_upload_generates_unique_tokens() {
        let store = store_loose();
        let p = Uuid::new_v4();
        let (_, token1, _) = store
            .begin_upload(p, "a.txt", "text/plain".to_string())
            .unwrap();
        let (_, token2, _) = store
            .begin_upload(p, "b.txt", "text/plain".to_string())
            .unwrap();
        assert_ne!(token1, token2, "tokens must be unique");
    }

    // ── 7.5: 配额测试 ─────────────────────────────────────────────────────

    #[test]
    fn quota_global_file_limit_rollback_then_reuse() {
        let store = store_limit_1();
        let pairing_id = Uuid::new_v4();

        let (file_id, ..) = store
            .begin_upload(pairing_id, "file.txt", "text/plain".to_string())
            .unwrap();

        // 第二次超限
        assert!(matches!(
            store.begin_upload(pairing_id, "file2.txt", "text/plain".to_string()),
            Err(SaveFileError::Limit(
                UploadLimitError::GlobalFileLimitReached
            ))
        ));

        // rollback 后配额释放，可再次成功
        store.rollback_upload(file_id, pairing_id);
        assert!(
            store
                .begin_upload(pairing_id, "file3.txt", "text/plain".to_string())
                .is_ok(),
            "after rollback quota should be freed for reuse"
        );
    }

    #[test]
    fn quota_byte_limit_enforced() {
        // max_upload_size_bytes=30 → 每次悲观预留 30 字节
        // max_pending_upload_bytes_global=50 → 第一次 30 ≤ 50 OK，第二次 30+30=60 > 50 FAIL
        let store = UploadStore::new(30, 10, 100, 50);
        let p = Uuid::new_v4();

        assert!(
            store
                .begin_upload(p, "a.txt", "text/plain".to_string())
                .is_ok(),
            "first upload (30 bytes reserved) should fit within 50-byte global limit"
        );

        let result = store.begin_upload(p, "b.txt", "text/plain".to_string());
        assert!(
            matches!(
                result,
                Err(SaveFileError::Limit(
                    UploadLimitError::GlobalByteLimitReached
                ))
            ),
            "second upload should exceed global byte limit, got {result:?}"
        );
    }

    // ── 7.6: 错误状态码矩阵 ───────────────────────────────────────────────

    #[tokio::test]
    async fn streaming_state_removed_after_send_abort_without_attach() {
        let store = store_loose();
        let file_id = Uuid::new_v4();
        store.create_streaming_state(file_id);

        // 不调用 attach_stream，直接 send_abort
        store.send_abort(file_id, "io_error".to_string()).await;

        // send_abort 不再自动移除状态（由 gRPC handler 结束时清理）。
        // 此路径（never-attached）下通过 rollback_upload 或 remove_streaming_state 兜底。
        store.remove_streaming_state(file_id);
        assert!(
            matches!(store.attach_stream(file_id), Err(AttachError::NotFound)),
            "streaming state should be removed after explicit remove_streaming_state"
        );
    }

    #[tokio::test]
    async fn send_chunk_disconnected_when_no_receiver() {
        let store = store_loose();
        let file_id = Uuid::new_v4();
        store.create_streaming_state(file_id);

        let rx = store.attach_stream(file_id).unwrap();
        drop(rx);

        let result = store.send_chunk(file_id, Bytes::from("data")).await;
        assert_eq!(result, SendChunkResult::Disconnected);
    }

    #[test]
    fn mark_streaming_done_sets_status_to_notified() {
        let store = store_loose();
        let pairing_id = Uuid::new_v4();
        let (file_id, ..) = store
            .begin_upload(pairing_id, "f.txt", "text/plain".to_string())
            .unwrap();

        store.mark_streaming_done(file_id, PathBuf::from("/tmp/x"), 1024, [0u8; 32]);

        let meta = store.get_file_meta(file_id).unwrap();
        assert_eq!(meta.status, FileStatus::Notified);
    }
}
