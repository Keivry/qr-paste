// SPDX-License-Identifier: MIT OR Apache-2.0

use {
    crate::grpc::relay::FileReceived,
    sha2::{Digest, Sha256},
    std::{
        io::{Read, Write},
        path::{Path, PathBuf},
        sync::mpsc,
        time::Duration,
    },
    tracing::{info, warn},
};

pub struct FileJob {
    pub event: FileReceived,
    pub file_save_dir: PathBuf,
    pub download_timeout_secs: u64,
    pub download_max_retries: u32,
    pub public_base_url: Option<String>,
}

pub fn start_file_worker(
    job_rx: mpsc::Receiver<FileJob>,
    notice_tx: mpsc::Sender<String>,
    repaint_ctx: egui::Context,
) {
    std::thread::spawn(move || {
        while let Ok(job) = job_rx.recv() {
            let notice = process_file_job(job);
            let _ = notice_tx.send(notice);
            repaint_ctx.request_repaint();
        }
    });
}

fn process_file_job(job: FileJob) -> String {
    let event = &job.event;
    let file_name = sanitize_file_name_for_save(&event.file_name);

    let dest_path = unique_dest_path(&job.file_save_dir, &file_name);

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

    let bearer = hex::encode(&event.file_access_token);
    let max_attempts = job.download_max_retries.saturating_add(1);

    let mut last_err = String::new();
    for attempt in 1..=max_attempts {
        match try_download(&client, event, &bearer, &dest_path) {
            Ok(total_bytes) => {
                info!(
                    file_name = %file_name,
                    path = %dest_path.display(),
                    size = total_bytes,
                    "文件已保存"
                );
                send_ack(&client, event, &job.public_base_url, true);
                let display_name = dest_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&file_name);
                return format!("已保存文件：{display_name}");
            }
            Err(DownloadError::Terminal(msg)) => {
                // 终态错误（404、401、SHA-256 校验失败等），不重试。
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

fn try_download(
    client: &reqwest::blocking::Client,
    event: &FileReceived,
    bearer: &str,
    dest_path: &std::path::Path,
) -> Result<u64, DownloadError> {
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

    let mut file = std::fs::File::create(dest_path).map_err(|e| {
        DownloadError::Terminal(format!("⚠ 无法创建文件 {}：{e}", dest_path.display()))
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
                let _ = std::fs::remove_file(dest_path);
                return Err(DownloadError::Transient(format!("⚠ 文件读取失败：{e}")));
            }
        };
        hasher.update(&buf[..n]);
        total_bytes += n as u64;
        if let Err(e) = file.write_all(&buf[..n]) {
            drop(file);
            let _ = std::fs::remove_file(dest_path);
            return Err(DownloadError::Terminal(format!("⚠ 写入文件失败：{e}")));
        }
    }

    drop(file);

    if !event.sha256.is_empty() {
        let computed = hex::encode(hasher.finalize());
        if computed != event.sha256 {
            let _ = std::fs::remove_file(dest_path);
            warn!(
                expected = %event.sha256,
                computed = %computed,
                "文件 SHA-256 校验失败，丢弃"
            );
            return Err(DownloadError::Terminal(format!(
                "⚠ 文件 {} 校验失败，已丢弃",
                dest_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
            )));
        }
    }

    Ok(total_bytes)
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
    let cleaned = cleaned.trim_matches('.').trim();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned.to_string()
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
            format!("{stem} ({i})")
        } else {
            format!("{stem} ({i}).{ext}")
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
