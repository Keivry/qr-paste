// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::grpc::relay::{FileReceived, StreamFileRequest, TransferMode},
    sha2::{Digest, Sha256},
    std::{
        io::{Read, Write},
        path::{Path, PathBuf},
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    },
    tonic::transport::Channel,
    tracing::{info, warn},
    uuid::Uuid,
};

#[cfg(target_os = "windows")]
use crate::clipboard;

pub struct FileJob {
    pub event: FileReceived,
    pub file_save_dir: PathBuf,
    pub download_timeout_secs: u64,
    pub download_max_retries: u32,
    pub public_base_url: Option<String>,
    pub auto_paste: bool,
    pub mime_type: String,
    pub image_clipboard_max_decoded_bytes: u64,
    pub simulate_key_after_paste: Option<String>,
    pub paste_delay_ms: u64,
    pub key_after_paste_delay_ms: u64,
    pub grpc_channel: Option<Channel>,
    /// 文件保存锁：序列化"选择目标路径 + rename"步骤，避免并发 worker 产生文件名冲突。
    pub file_save_lock: Arc<Mutex<()>>,
    /// 图片剪贴板粘贴锁：序列化"写剪贴板 + 模拟按键"步骤，避免并发 worker 相互打架。
    pub paste_lock: Arc<Mutex<()>>,
}

pub fn start_file_workers(
    job_rx: mpsc::Receiver<FileJob>,
    notice_tx: mpsc::Sender<String>,
    repaint_ctx: egui::Context,
    num_workers: usize,
) {
    let job_rx = Arc::new(Mutex::new(job_rx));
    for _ in 0..num_workers.max(1) {
        let job_rx = Arc::clone(&job_rx);
        let notice_tx = notice_tx.clone();
        let repaint_ctx = repaint_ctx.clone();
        std::thread::spawn(move || loop {
            let job = {
                let rx = job_rx.lock().unwrap();
                match rx.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                }
            };
            let notice = process_file_job(job);
            let _ = notice_tx.send(notice);
            repaint_ctx.request_repaint();
        });
    }
}

fn process_file_job(job: FileJob) -> String {
    let event = &job.event;
    let file_name = sanitize_file_name_for_save(&event.file_name);

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(job.download_timeout_secs.max(1)))
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("⚠ 文件下载失败（构建 HTTP 客户端）：{e}"),
    };

    if let Err(e) = std::fs::create_dir_all(&job.file_save_dir) {
        send_ack(&client, event, &job.public_base_url, false);
        return format!("⚠ 无法创建保存目录 {}：{e}", job.file_save_dir.display());
    }

    let transfer_mode = TransferMode::try_from(event.transfer_mode).unwrap_or(TransferMode::Relay);

    if transfer_mode == TransferMode::HttpStreaming {
        match download_via_http_stream(event, &client) {
            Ok((tmp_path, total_bytes)) => {
                #[cfg(target_os = "windows")]
                if job.auto_paste && job.mime_type.starts_with("image/") {
                    match process_image_from_tmp(
                        &job,
                        &tmp_path,
                        &job.file_save_dir,
                        &file_name,
                        total_bytes,
                    ) {
                        Ok(notice) => {
                            send_ack(&client, event, &job.public_base_url, true);
                            return notice;
                        }
                        Err(DownloadError::Terminal(msg)) | Err(DownloadError::Transient(msg)) => {
                            send_ack(&client, event, &job.public_base_url, false);
                            return msg;
                        }
                    }
                }

                match persist_tmp_locked(&tmp_path, &job.file_save_dir, &file_name, total_bytes, &job.file_save_lock) {
                    Ok(notice) => {
                        send_ack(&client, event, &job.public_base_url, true);
                        return notice;
                    }
                    Err(DownloadError::Terminal(msg)) | Err(DownloadError::Transient(msg)) => {
                        send_ack(&client, event, &job.public_base_url, false);
                        return msg;
                    }
                }
            }

            Err(msg) => {
                send_ack(&client, event, &job.public_base_url, false);
                return msg;
            }
        }
    }

    if transfer_mode == TransferMode::Streaming {
        if let Some(ref channel) = job.grpc_channel {
            match download_via_grpc_stream(event, channel.clone(), &job.file_save_dir) {
                Ok((tmp_path, total_bytes)) => {
                    #[cfg(target_os = "windows")]
                    if job.auto_paste && job.mime_type.starts_with("image/") {
                        match process_image_from_tmp(
                            &job,
                            &tmp_path,
                            &job.file_save_dir,
                            &file_name,
                            total_bytes,
                        ) {
                            Ok(notice) => {
                                send_ack(&client, event, &job.public_base_url, true);
                                return notice;
                            }
                            Err(DownloadError::Terminal(msg))
                            | Err(DownloadError::Transient(msg)) => {
                                send_ack(&client, event, &job.public_base_url, false);
                                return msg;
                            }
                        }
                    }

                    match persist_tmp_locked(&tmp_path, &job.file_save_dir, &file_name, total_bytes, &job.file_save_lock)
                    {
                        Ok(notice) => {
                            send_ack(&client, event, &job.public_base_url, true);
                            return notice;
                        }
                        Err(DownloadError::Terminal(msg)) | Err(DownloadError::Transient(msg)) => {
                            send_ack(&client, event, &job.public_base_url, false);
                            return msg;
                        }
                    }
                }
                Err(msg) => {
                    send_ack(&client, event, &job.public_base_url, false);
                    return msg;
                }
            }
        } else {
            warn!(file_id = %event.file_id, "STREAMING 模式但 grpc_channel 未就绪，回退 RELAY");
        }
    }

    let bearer = hex::encode(&event.file_access_token);
    let max_attempts = job.download_max_retries.saturating_add(1);

    let mut last_err = String::new();
    for attempt in 1..=max_attempts {
        let attempt_result = {
            #[cfg(target_os = "windows")]
            {
                if job.auto_paste && job.mime_type.starts_with("image/") {
                    process_image_job(&job, &client, &bearer, &file_name)
                } else {
                    process_file_save_job(&job, &client, &bearer, &file_name)
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (
                    &job.auto_paste,
                    &job.mime_type,
                    job.image_clipboard_max_decoded_bytes,
                    &job.simulate_key_after_paste,
                    job.paste_delay_ms,
                    job.key_after_paste_delay_ms,
                );
                process_file_save_job(&job, &client, &bearer, &file_name)
            }
        };

        match attempt_result {
            Ok(notice) => {
                send_ack(&client, event, &job.public_base_url, true);
                return notice;
            }
            Err(DownloadError::Terminal(msg)) => {
                send_ack(&client, event, &job.public_base_url, false);
                return msg;
            }
            Err(DownloadError::Transient(msg)) => {
                last_err = msg;
                if attempt < max_attempts {
                    warn!(
                        file_name = %file_name,
                        attempt,
                        max_attempts,
                        "文件下载瞬时失败，准备重试：{last_err}"
                    );
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    send_ack(&client, event, &job.public_base_url, false);
    format!(
        "⚠ 文件下载失败（已重试 {} 次）：{last_err}",
        max_attempts - 1
    )
}

enum DownloadError {
    Transient(String),
    Terminal(String),
}

fn process_file_save_job(
    job: &FileJob,
    client: &reqwest::blocking::Client,
    bearer: &str,
    file_name: &str,
) -> Result<String, DownloadError> {
    let (tmp_path, total_bytes) = try_download(client, &job.event, bearer, &job.file_save_dir)?;
    persist_tmp_locked(&tmp_path, &job.file_save_dir, file_name, total_bytes, &job.file_save_lock)
}

#[cfg(target_os = "windows")]
fn process_image_job(
    job: &FileJob,
    client: &reqwest::blocking::Client,
    bearer: &str,
    file_name: &str,
) -> Result<String, DownloadError> {
    let (tmp_path, total_bytes) =
        try_download_to_tmp(client, &job.event, bearer, &job.file_save_dir)?;
    process_image_from_tmp(job, &tmp_path, &job.file_save_dir, file_name, total_bytes)
}

#[cfg(target_os = "windows")]
fn process_image_from_tmp(
    job: &FileJob,
    tmp_path: &Path,
    save_dir: &Path,
    file_name: &str,
    total_bytes: u64,
) -> Result<String, DownloadError> {
    let reader = match open_image_reader(tmp_path) {
        Ok(reader) => reader,
        Err(err) => {
            info!(
                file_name = %file_name,
                path = %tmp_path.display(),
                error = %err,
                "图片格式嗅探失败，回退为文件保存"
            );
            return persist_tmp_locked(&tmp_path, save_dir, file_name, total_bytes, &job.file_save_lock);
        }
    };

    let (width, height) = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(err) => {
            info!(
                file_name = %file_name,
                path = %tmp_path.display(),
                error = %err,
                "图片尺寸探测失败，回退为文件保存"
            );
            return persist_tmp_locked(&tmp_path, save_dir, file_name, total_bytes, &job.file_save_lock);
        }
    };

    let Some(decoded_bytes) = estimate_decoded_rgba_bytes(width, height) else {
        info!(
            file_name = %file_name,
            width,
            height,
            "图片解码内存预估溢出，回退为文件保存"
        );
        return persist_tmp_locked(&tmp_path, save_dir, file_name, total_bytes, &job.file_save_lock);
    };

    if decoded_bytes > job.image_clipboard_max_decoded_bytes {
        info!(
            file_name = %file_name,
            width,
            height,
            decoded_bytes,
            limit = job.image_clipboard_max_decoded_bytes,
            "图片解码内存超过上限，回退为文件保存"
        );
        return persist_tmp_locked(&tmp_path, save_dir, file_name, total_bytes, &job.file_save_lock);
    }

    let image = match open_image_reader(&tmp_path)
        .and_then(|reader| reader.decode().map_err(|err| err.to_string()))
    {
        Ok(image) => image,
        Err(err) => {
            info!(
                file_name = %file_name,
                path = %tmp_path.display(),
                error = %err,
                "图片解码失败，回退为文件保存"
            );
            return persist_tmp_locked(&tmp_path, save_dir, file_name, total_bytes, &job.file_save_lock);
        }
    };

    let _paste_guard = job.paste_lock.lock().unwrap();
    match write_image_to_clipboard_win32(image) {
        Ok(()) => {
            if let Err(err) = std::fs::remove_file(&tmp_path) {
                warn!(
                    path = %tmp_path.display(),
                    error = %err,
                    "图片已写入剪贴板，但删除临时文件失败"
                );
            }
            info!(
                file_name = %file_name,
                width,
                height,
                size = total_bytes,
                "图片已写入剪贴板并准备自动粘贴"
            );
            clipboard::simulate_paste(job.paste_delay_ms);
            if let Some(key_spec) = &job.simulate_key_after_paste
                && let Some((modifier, key)) = clipboard::parse_key_spec(key_spec)
            {
                clipboard::simulate_key(modifier, key, job.key_after_paste_delay_ms);
            }
            let notice = if let Some(key_spec) = &job.simulate_key_after_paste {
                format!("已自动粘贴图片（{key_spec}）：{file_name}")
            } else {
                format!("已自动粘贴图片：{file_name}")
            };
            Ok(notice)
        }
        Err(err) => {
            warn!(
                file_name = %file_name,
                path = %tmp_path.display(),
                error = %err,
                "图片写入剪贴板失败，回退为文件保存"
            );
            drop(_paste_guard);
            persist_tmp_locked(&tmp_path, save_dir, file_name, total_bytes, &job.file_save_lock)
        }
    }
}

fn try_download(
    client: &reqwest::blocking::Client,
    event: &FileReceived,
    bearer: &str,
    tmp_dir: &Path,
) -> Result<(PathBuf, u64), DownloadError> {
    download_to_tmp(client, event, bearer, tmp_dir)
}

#[cfg(target_os = "windows")]
fn try_download_to_tmp(
    client: &reqwest::blocking::Client,
    event: &FileReceived,
    bearer: &str,
    tmp_dir: &Path,
) -> Result<(PathBuf, u64), DownloadError> {
    download_to_tmp(client, event, bearer, tmp_dir)
}

fn download_via_grpc_stream(
    event: &FileReceived,
    channel: Channel,
    tmp_dir: &Path,
) -> Result<(PathBuf, u64), String> {
    use crate::grpc::relay::client_relay_client::ClientRelayClient;

    let file_id = event.file_id.clone();
    let token = event.file_access_token.clone();
    let file_name = event.file_name.clone();

    let tmp_path = tmp_dir.join(format!(".{}.tmp", Uuid::new_v4()));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("⚠ gRPC 运行时初始化失败：{e}"))?;

    rt.block_on(async move {
        let mut client = ClientRelayClient::new(channel);

        let request = StreamFileRequest {
            file_id: file_id.clone(),
            file_access_token: token,
        };

        let mut stream = client
            .stream_file(request)
            .await
            .map_err(|s| format!("⚠ StreamFile RPC 失败：{} {}", s.code(), s.message()))?
            .into_inner();

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
            .map_err(|e| format!("⚠ 无法创建临时文件 {}：{e}", tmp_path.display()))?;

        let mut hasher = Sha256::new();
        let mut total_bytes: u64 = 0;

        loop {
            let chunk = match stream.message().await {
                Ok(Some(c)) => c,
                Ok(None) => {
                    drop(file);
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(format!("⚠ gRPC 流提前关闭，未收到终止帧：{file_name}"));
                }
                Err(s) => {
                    drop(file);
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(format!("⚠ gRPC 流错误：{} {}", s.code(), s.message()));
                }
            };

            if chunk.is_last {
                drop(file);
                let expected_sha256 = chunk.sha256;
                let computed = hasher.finalize();
                if expected_sha256 != computed.as_slice() {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    warn!(
                        file_id = %file_id,
                        expected = %hex::encode(&expected_sha256),
                        computed = %hex::encode(computed),
                        "gRPC 流 SHA-256 校验失败"
                    );
                    return Err(format!("⚠ 文件 {file_name} sha256 mismatch，已丢弃"));
                }
                info!(file_id = %file_id, size = total_bytes, "gRPC 流接收完成，SHA-256 校验通过");
                return Ok((tmp_path, total_bytes));
            }

            let data = &chunk.data;
            hasher.update(data);
            total_bytes = match total_bytes.checked_add(data.len() as u64) {
                Some(v) => v,
                None => {
                    drop(file);
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err("⚠ gRPC 流文件大小溢出".to_string());
                }
            };
            use tokio::io::AsyncWriteExt as _;
            if let Err(e) = file.write_all(data).await {
                drop(file);
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(format!("⚠ gRPC 流写入临时文件失败：{e}"));
            }
        }
    })
}

fn download_via_http_stream(
    event: &FileReceived,
    regular_client: &reqwest::blocking::Client,
) -> Result<(PathBuf, u64), String> {
    let bearer = hex::encode(&event.file_access_token);
    let file_name = event.file_name.clone();
    let tmp_dir = std::env::temp_dir();

    let no_redirect_client = match reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("⚠ HTTP 流式下载失败（构建客户端）：{e}")),
    };

    let response = match no_redirect_client
        .get(&event.download_url)
        .bearer_auth(&bearer)
        .send()
    {
        Ok(r) => r,
        Err(e) => return Err(format!("⚠ HTTP 流式下载失败：{e}")),
    };

    let status = response.status();

    if status == reqwest::StatusCode::FOUND {
        let sha256_from_header = match response
            .headers()
            .get("x-file-sha256")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
        {
            Some(s)
                if !s.is_empty() && s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                s
            }
            _ => return Err("⚠ HTTP 流式下载：302 响应缺少有效的 X-File-Sha256 头".to_string()),
        };

        let redirect_url = match response.headers().get(reqwest::header::LOCATION) {
            Some(v) => match v.to_str() {
                Ok(u) => u.to_string(),
                Err(_) => return Err("⚠ HTTP 流式下载：302 Location 头无效".to_string()),
            },
            None => return Err("⚠ HTTP 流式下载：302 但无 Location 头".to_string()),
        };

        let mut relay_event = event.clone();
        relay_event.download_url = redirect_url;
        relay_event.sha256 = sha256_from_header;

        return download_to_tmp(regular_client, &relay_event, &bearer, &tmp_dir).map_err(
            |e| match e {
                DownloadError::Terminal(msg) | DownloadError::Transient(msg) => msg,
            },
        );
    }

    if status == reqwest::StatusCode::NOT_FOUND || !status.is_success() {
        return Err(format!("⚠ HTTP 流式下载失败：HTTP {status}"));
    }

    let tmp_path = tmp_dir.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
    {
        Ok(f) => f,
        Err(e) => {
            return Err(format!(
                "⚠ HTTP 流式下载：无法创建临时文件 {}：{e}",
                tmp_path.display()
            ));
        }
    };

    let mut hasher = Sha256::new();
    let mut total_bytes: u64 = 0;
    let mut ring_buf: Vec<u8> = Vec::with_capacity(32);
    let mut response = response;
    let mut read_buf = [0u8; 65536];

    loop {
        let n = match response.read(&mut read_buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!("⚠ HTTP 流式下载读取失败：{e}"));
            }
        };

        let incoming = &read_buf[..n];
        let combined_len = ring_buf.len() + incoming.len();

        if combined_len <= 32 {
            ring_buf.extend_from_slice(incoming);
        } else {
            let flush_count = combined_len - 32;
            let from_ring = flush_count.min(ring_buf.len());
            let flush_data: Vec<u8> = ring_buf
                .drain(..from_ring)
                .chain(
                    incoming[..flush_count.saturating_sub(from_ring)]
                        .iter()
                        .copied(),
                )
                .collect();

            hasher.update(&flush_data);
            total_bytes = match total_bytes.checked_add(flush_data.len() as u64) {
                Some(v) => v,
                None => {
                    drop(file);
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err("⚠ HTTP 流式下载：文件大小溢出".to_string());
                }
            };
            if let Err(e) = file.write_all(&flush_data) {
                drop(file);
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!("⚠ HTTP 流式下载写入临时文件失败：{e}"));
            }

            ring_buf.extend_from_slice(&incoming[flush_count.saturating_sub(from_ring)..]);
        }
    }

    drop(file);

    if ring_buf.len() != 32 {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "⚠ HTTP 流式下载：响应体长度不足，预期末尾 32 字节 SHA-256，实际残余 {} 字节（文件 {file_name}）",
            ring_buf.len()
        ));
    }

    let computed: [u8; 32] = hasher.finalize().into();
    let sha256_from_body: [u8; 32] = ring_buf.as_slice().try_into().unwrap();
    if computed != sha256_from_body {
        let _ = std::fs::remove_file(&tmp_path);
        warn!(
            expected = %hex::encode(sha256_from_body),
            computed = %hex::encode(computed),
            "HTTP 流式下载 SHA-256 校验失败"
        );
        return Err(format!(
            "⚠ HTTP 流式下载：文件 {file_name} SHA-256 校验失败，已丢弃"
        ));
    }

    info!(file_name = %file_name, size = total_bytes, "HTTP 流式下载完成，SHA-256 校验通过");
    Ok((tmp_path, total_bytes))
}

fn download_to_tmp(
    client: &reqwest::blocking::Client,
    event: &FileReceived,
    bearer: &str,
    tmp_dir: &Path,
) -> Result<(PathBuf, u64), DownloadError> {
    let response = client
        .get(&event.download_url)
        .bearer_auth(bearer)
        .send()
        .map_err(|e| DownloadError::Transient(format!("⚠ 文件下载失败：{e}")))?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(DownloadError::Terminal(format!(
            "⚠ 文件下载失败：HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(DownloadError::Transient(format!(
            "⚠ 文件下载失败：HTTP {status}"
        )));
    }

    let tmp_path = tmp_dir.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(|e| {
            DownloadError::Terminal(format!("⚠ 无法创建临时文件 {}：{e}", tmp_path.display()))
        })?;

    let mut hasher = Sha256::new();
    let mut total_bytes: u64 = 0;

    let mut buf = [0u8; 65536];
    let mut response = response;
    loop {
        let n = match response.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&tmp_path);
                return Err(DownloadError::Transient(format!("⚠ 文件读取失败：{e}")));
            }
        };
        hasher.update(&buf[..n]);
        total_bytes = match total_bytes.checked_add(n as u64) {
            Some(total_bytes) => total_bytes,
            None => {
                drop(file);
                let _ = std::fs::remove_file(&tmp_path);
                return Err(DownloadError::Terminal(
                    "⚠ 文件下载失败：文件大小超出可处理范围".to_string(),
                ));
            }
        };
        if let Err(e) = file.write_all(&buf[..n]) {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(DownloadError::Terminal(format!("⚠ 写入文件失败：{e}")));
        }
    }

    drop(file);

    if !event.sha256.is_empty() {
        let computed = hex::encode(hasher.finalize());
        if computed != event.sha256 {
            let _ = std::fs::remove_file(&tmp_path);
            warn!(
                expected = %event.sha256,
                computed = %computed,
                "文件 SHA-256 校验失败，丢弃"
            );
            return Err(DownloadError::Terminal(format!(
                "⚠ 文件 {} 校验失败，已丢弃",
                event.file_name.as_str()
            )));
        }
    }

    Ok((tmp_path, total_bytes))
}

pub fn send_ack(
    client: &reqwest::blocking::Client,
    event: &FileReceived,
    base_url: &Option<String>,
    success: bool,
) {
    let Some(base) = base_url else { return };
    let ack_url = format!(
        "{}/api/files/{}/ack",
        base.trim_end_matches('/'),
        event.file_id
    );
    let bearer = hex::encode(&event.file_access_token);
    match client
        .post(&ack_url)
        .bearer_auth(&bearer)
        .json(&serde_json::json!({"success": success}))
        .send()
    {
        Ok(r) if r.status().is_success() => {
            info!(file_id = %event.file_id, success, "文件 ACK 已发送");
        }
        Ok(r) => {
            warn!(file_id = %event.file_id, status = %r.status(), success, "文件 ACK 失败");
        }
        Err(e) => {
            warn!(file_id = %event.file_id, error = %e, success, "文件 ACK 请求出错");
        }
    }
}

fn sanitize_file_name_for_save(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "file".to_string();
    }
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_end_matches(char::is_whitespace);
    let cleaned = cleaned.trim_end_matches('.');
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        let mut cleaned = cleaned.to_string();
        const RESERVED: &[&str] = &[
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        let (stem_check, _) = split_stem_ext(&cleaned);
        if RESERVED
            .iter()
            .any(|reserved| stem_check.eq_ignore_ascii_case(reserved))
        {
            cleaned = format!("_{cleaned}");
        }
        truncate_file_name_to_limit(cleaned, 255)
    }
}

fn persist_tmp_locked(
    tmp_path: &Path,
    dir: &Path,
    file_name: &str,
    total_bytes: u64,
    lock: &Mutex<()>,
) -> Result<String, DownloadError> {
    let _guard = lock.lock().unwrap();
    let dest_path = unique_dest_path(dir, file_name);
    move_tmp_to_dest(tmp_path, &dest_path)?;
    info!(
        file_name = %file_name,
        path = %dest_path.display(),
        size = total_bytes,
        "文件已保存"
    );
    Ok(format!(
        "已保存文件：{}",
        display_name(&dest_path, file_name)
    ))
}

fn move_tmp_to_dest(tmp_path: &Path, dest_path: &Path) -> Result<(), DownloadError> {
    std::fs::rename(tmp_path, dest_path).map_err(|e| {
        let _ = std::fs::remove_file(tmp_path);
        DownloadError::Terminal(format!("⚠ 无法保存文件 {}：{e}", dest_path.display()))
    })
}

fn display_name(path: &Path, fallback: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| fallback.to_string(), ToString::to_string)
}

fn truncate_file_name_to_limit(mut cleaned: String, max_bytes: usize) -> String {
    if cleaned.len() <= max_bytes {
        return cleaned;
    }

    let original_ext = split_stem_ext(&cleaned).1.to_string();
    let boundary = cleaned.floor_char_boundary(max_bytes);
    cleaned.truncate(boundary);

    if original_ext.is_empty() {
        return cleaned;
    }

    let (_, truncated_ext) = split_stem_ext(&cleaned);
    if truncated_ext == original_ext.as_str() {
        return cleaned;
    }

    let ext = truncate_utf8_to_boundary(&original_ext, 16);
    if ext.is_empty() {
        return cleaned;
    }

    let extension = format!(".{ext}");
    let (truncated_stem, _) = split_stem_ext(&cleaned);
    let stem_limit = max_bytes.saturating_sub(extension.len());
    let truncated_stem = truncate_utf8_to_boundary(truncated_stem, stem_limit);
    format!("{truncated_stem}{extension}")
}

fn truncate_utf8_to_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        value
    } else {
        &value[..value.floor_char_boundary(max_bytes)]
    }
}

fn unique_dest_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }

    let (stem, ext) = split_stem_ext(file_name);
    for i in 1u32..=9999 {
        let new_name = if ext.is_empty() {
            format!("{stem}({i})")
        } else {
            format!("{stem}({i}).{ext}")
        };
        let candidate = dir.join(&new_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(file_name)
}

fn split_stem_ext(file_name: &str) -> (&str, &str) {
    if let Some(dot_pos) = file_name.rfind('.')
        && dot_pos > 0
    {
        return (&file_name[..dot_pos], &file_name[dot_pos + 1..]);
    }
    (file_name, "")
}

#[cfg(target_os = "windows")]
type FsImageReader = image::ImageReader<std::io::BufReader<std::fs::File>>;

#[cfg(target_os = "windows")]
fn open_image_reader(tmp_path: &Path) -> Result<FsImageReader, String> {
    image::ImageReader::open(tmp_path)
        .map_err(|err| err.to_string())?
        .with_guessed_format()
        .map_err(|err| err.to_string())
}

#[cfg(target_os = "windows")]
fn estimate_decoded_rgba_bytes(width: u32, height: u32) -> Option<u64> {
    (width as u64)
        .checked_mul(height as u64)
        .and_then(|value| value.checked_mul(4))
}

#[cfg(target_os = "windows")]
fn write_image_to_clipboard_win32(img: image::DynamicImage) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::GlobalFree,
        System::{
            DataExchange::{
                CloseClipboard,
                EmptyClipboard,
                OpenClipboard,
                RegisterClipboardFormatW,
                SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::CF_DIBV5,
        },
    };

    let mut rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    let header_size = 124usize;
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    let pixel_data = rgba.as_raw();
    let image_size =
        u32::try_from(pixel_data.len()).map_err(|_| "Clipboard image too large".to_string())?;
    let total_size = header_size
        .checked_add(pixel_data.len())
        .ok_or_else(|| "Clipboard image too large".to_string())?;

    unsafe {
        let hmem = GlobalAlloc(GMEM_MOVEABLE, total_size);
        if hmem.is_null() {
            return Err("GlobalAlloc failed".to_string());
        }

        let ptr = GlobalLock(hmem);
        if ptr.is_null() {
            GlobalFree(hmem);
            return Err("GlobalLock failed".to_string());
        }

        let ptr = ptr.cast::<u8>();
        std::ptr::write_bytes(ptr, 0, total_size);

        write_u32_le(ptr, 0, 124);
        write_u32_le(ptr, 4, width);
        write_i32_le(ptr, 8, height as i32);
        write_u16_le(ptr, 12, 1);
        write_u16_le(ptr, 14, 32);
        write_u32_le(ptr, 16, 3);
        write_u32_le(ptr, 20, image_size);
        write_u32_le(ptr, 40, 0x00FF_0000);
        write_u32_le(ptr, 44, 0x0000_FF00);
        write_u32_le(ptr, 48, 0x0000_00FF);
        write_u32_le(ptr, 52, 0xFF00_0000);
        write_u32_le(ptr, 56, 0x5769_6E20);

        let dst = ptr.add(header_size);
        let row_bytes = (width as usize).saturating_mul(4);
        for dst_row in 0..(height as usize) {
            let src_row = (height as usize) - 1 - dst_row;
            std::ptr::copy_nonoverlapping(
                pixel_data.as_ptr().add(src_row * row_bytes),
                dst.add(dst_row * row_bytes),
                row_bytes,
            );
        }

        GlobalUnlock(hmem);

        if OpenClipboard(std::ptr::null_mut()) == 0 {
            GlobalFree(hmem);
            return Err("OpenClipboard failed".to_string());
        }

        let result = (|| -> Result<(), String> {
            if EmptyClipboard() == 0 {
                GlobalFree(hmem);
                return Err("EmptyClipboard failed".to_string());
            }

            if SetClipboardData(CF_DIBV5.into(), hmem.cast()).is_null() {
                GlobalFree(hmem);
                return Err("SetClipboardData failed".to_string());
            }

            let exclude_format_name: Vec<u16> = "ExcludeClipboardContentFromMonitorProcessing"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let exclude_fmt = RegisterClipboardFormatW(exclude_format_name.as_ptr());
            if exclude_fmt != 0 {
                SetClipboardData(exclude_fmt, std::ptr::null_mut());
            }

            Ok(())
        })();

        CloseClipboard();
        result
    }
}

#[cfg(target_os = "windows")]
fn write_u32_le(dst: *mut u8, offset: usize, value: u32) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            value.to_le_bytes().as_ptr(),
            dst.add(offset),
            std::mem::size_of::<u32>(),
        );
    }
}

#[cfg(target_os = "windows")]
fn write_i32_le(dst: *mut u8, offset: usize, value: i32) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            value.to_le_bytes().as_ptr(),
            dst.add(offset),
            std::mem::size_of::<i32>(),
        );
    }
}

#[cfg(target_os = "windows")]
fn write_u16_le(dst: *mut u8, offset: usize, value: u16) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            value.to_le_bytes().as_ptr(),
            dst.add(offset),
            std::mem::size_of::<u16>(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_file_name_for_save;

    #[test]
    fn sanitize_prefixes_windows_reserved_names() {
        assert_eq!(sanitize_file_name_for_save("con.txt"), "_con.txt");
        assert_eq!(sanitize_file_name_for_save("Lpt1"), "_Lpt1");
    }

    #[test]
    fn sanitize_truncates_to_255_bytes_and_keeps_extension() {
        let file_name = format!("{}.png", "你".repeat(100));
        let sanitized = sanitize_file_name_for_save(&file_name);

        assert!(sanitized.len() <= 255);
        assert!(sanitized.ends_with(".png"));
    }
}
