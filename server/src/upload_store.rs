// SPDX-License-Identifier: MIT OR Apache-2.0

//! 服务端文件上传临时存储：`UploadStore` 及关联类型。
//!
//! 负责管理上传文件的内存状态索引、磁盘文件路径映射、全局并发计数器，以及
//! pairing 级别的文件生命周期管理。

use {
    dashmap::DashMap,
    rand::RngExt,
    sha2::Sha256,
    std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::Instant,
    },
    tokio::io::AsyncWriteExt,
    tracing::{info, warn},
    uuid::Uuid,
};

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
        })
    }

    /// 流式接收上传文件并写入磁盘，同步计算 SHA-256。
    ///
    /// # 并发安全
    ///
    /// "检查 + 占位"在同一 Mutex 临界区内原子完成，防止并发请求绕过容量上限。
    /// 占位后释放锁，再执行磁盘 I/O（不持锁跨越 await）。
    /// 写入失败时自动回滚计数器并删除临时文件。
    pub async fn save_file(
        &self,
        upload_dir: &Path,
        pairing_id: Uuid,
        mut stream: axum::extract::Multipart,
        max_upload_size_bytes: u64,
        chunk_timeout: std::time::Duration,
    ) -> Result<(Uuid, FileMeta), SaveFileError> {
        let file_id = Uuid::new_v4();

        let field = match stream
            .next_field()
            .await
            .map_err(|e| SaveFileError::Io(format!("multipart 读取失败: {e}")))?
        {
            Some(f) => f,
            None => return Err(SaveFileError::NoFileField),
        };

        let raw_filename = field.file_name().unwrap_or_default().to_string();
        let file_name = sanitize_file_name(&raw_filename, &file_id);
        let mime_type = field
            .content_type()
            .map(|ct| ct.to_string())
            .unwrap_or_else(|| {
                mime_guess::from_path(&file_name)
                    .first_or_octet_stream()
                    .to_string()
            });

        // 在同一 Mutex 临界区内原子执行"检查 + 占位"，防止并发绕过配额。
        {
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

            let meta = FileMeta {
                pairing_id,
                status: FileStatus::Uploading,
                uploaded_at: Instant::now(),
                file_path: PathBuf::new(),
                size_bytes: self.max_upload_size_bytes,
                file_access_token: [0u8; 32],
                file_name: file_name.clone(),
                raw_filename: raw_filename.clone(),
                mime_type: mime_type.clone(),
                sha256: String::new(),
            };
            self.files.insert(file_id, meta);
            res.reserve(file_id, pairing_id, self.max_upload_size_bytes);
        }

        // Mutex 已释放，执行磁盘 I/O。
        let tmp_name = format!(".{file_id}.tmp");
        let tmp_path = upload_dir.join(&tmp_name);

        let result = self
            .write_stream_to_file(&tmp_path, field, max_upload_size_bytes, chunk_timeout)
            .await;

        if result.is_ok()
            && let Ok(Some(_)) = stream.next_field().await
        {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            self.rollback_upload(file_id, pairing_id);
            return Err(SaveFileError::TooManyFields);
        }

        match result {
            Ok((actual_size, sha256_hex)) => {
                let mut token = [0u8; 32];
                rand::rng().fill(&mut token);
                let final_path = upload_dir.join(file_id.to_string());
                if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
                    warn!("重命名临时文件失败 file_id={file_id}: {e}");
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    self.rollback_upload(file_id, pairing_id);
                    return Err(SaveFileError::Io(format!("重命名失败: {e}")));
                }

                // 写盘成功后，在 Mutex 保护下二次检查 pairing 是否已关闭（High-3 修复）。
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
                            entry.file_access_token = token;
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

                let meta_snapshot = self.files.get(&file_id).map(|e| FileMeta {
                    pairing_id: e.pairing_id,
                    status: e.status.clone(),
                    uploaded_at: e.uploaded_at,
                    file_path: e.file_path.clone(),
                    size_bytes: e.size_bytes,
                    file_access_token: e.file_access_token,
                    file_name: e.file_name.clone(),
                    raw_filename: e.raw_filename.clone(),
                    mime_type: e.mime_type.clone(),
                    sha256: e.sha256.clone(),
                });
                if let Some(meta) = meta_snapshot {
                    Ok((file_id, meta))
                } else {
                    Err(SaveFileError::Io("内存记录丢失".to_string()))
                }
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                self.rollback_upload(file_id, pairing_id);
                Err(e)
            }
        }
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
    fn rollback_upload(&self, file_id: Uuid, pairing_id: Uuid) {
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
    /// 用于 ACK 收到后的主动清理路径。
    pub fn remove_file(&self, file_id: Uuid) {
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
                    {
                        let mut res = self.reservation.lock().unwrap();
                        res.release(fid, m.pairing_id, m.size_bytes);
                    }
                    info!("cleanup_expired 删除超时文件 file_id={fid} age={age_secs}s");
                } else {
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
    /// multipart 中未找到文件字段。
    NoFileField,
    /// multipart 中包含多余字段（只允许一个文件 part）。
    TooManyFields,
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
