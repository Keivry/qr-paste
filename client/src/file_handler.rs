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
        let attempt_result = {
            #[cfg(target_os = "windows")]
            {
                if job.auto_paste && job.mime_type.starts_with("image/") {
                    process_image_job(&job, &client, &bearer, &dest_path, &file_name)
                } else {
                    process_file_save_job(&job, &client, &bearer, &dest_path, &file_name)
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (
                    &job.auto_paste,
                    &job.mime_type,
                    job.image_clipboard_max_decoded_bytes,
                );
                process_file_save_job(&job, &client, &bearer, &dest_path, &file_name)
            }
        };

        match attempt_result {
            Ok(notice) => {
                send_ack(&client, event, &job.public_base_url, true);
                return notice;
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
    dest_path: &Path,
    file_name: &str,
) -> Result<String, DownloadError> {
    let (tmp_path, total_bytes) = try_download(client, &job.event, bearer, &job.file_save_dir)?;
    persist_tmp_as_saved_file(&tmp_path, dest_path, file_name, total_bytes)
}

#[cfg(target_os = "windows")]
fn process_image_job(
    job: &FileJob,
    client: &reqwest::blocking::Client,
    bearer: &str,
    dest_path: &Path,
    file_name: &str,
) -> Result<String, DownloadError> {
    let (tmp_path, total_bytes) =
        try_download_to_tmp(client, &job.event, bearer, &job.file_save_dir)?;

    let reader = match open_image_reader(&tmp_path) {
        Ok(reader) => reader,
        Err(err) => {
            info!(
                file_name = %file_name,
                path = %tmp_path.display(),
                error = %err,
                "图片格式嗅探失败，回退为文件保存"
            );
            return persist_tmp_as_saved_file(&tmp_path, dest_path, file_name, total_bytes);
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
            return persist_tmp_as_saved_file(&tmp_path, dest_path, file_name, total_bytes);
        }
    };

    let Some(decoded_bytes) = estimate_decoded_rgba_bytes(width, height) else {
        info!(
            file_name = %file_name,
            width,
            height,
            "图片解码内存预估溢出，回退为文件保存"
        );
        return persist_tmp_as_saved_file(&tmp_path, dest_path, file_name, total_bytes);
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
        return persist_tmp_as_saved_file(&tmp_path, dest_path, file_name, total_bytes);
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
            return persist_tmp_as_saved_file(&tmp_path, dest_path, file_name, total_bytes);
        }
    };

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
            clipboard::simulate_paste(0);
            Ok(format!("已自动粘贴图片：{file_name}"))
        }
        Err(err) => {
            warn!(
                file_name = %file_name,
                path = %tmp_path.display(),
                error = %err,
                "图片写入剪贴板失败，回退为文件保存"
            );
            persist_tmp_as_saved_file(&tmp_path, dest_path, file_name, total_bytes)
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

fn persist_tmp_as_saved_file(
    tmp_path: &Path,
    dest_path: &Path,
    file_name: &str,
    total_bytes: u64,
) -> Result<String, DownloadError> {
    move_tmp_to_dest(tmp_path, dest_path)?;
    info!(
        file_name = %file_name,
        path = %dest_path.display(),
        size = total_bytes,
        "文件已保存"
    );
    Ok(format!(
        "已保存文件：{}",
        display_name(dest_path, file_name)
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

    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    let header_size = 124usize;
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
        let src = pixel_data.as_ptr();
        let row_bytes = (width as usize).saturating_mul(4);
        for dst_row in 0..(height as usize) {
            let src_row = (height as usize) - 1 - dst_row;
            let source = src.add(src_row * row_bytes);
            let target = dst.add(dst_row * row_bytes);
            for col in 0..(width as usize) {
                let s = source.add(col * 4);
                let t = target.add(col * 4);
                *t.add(0) = *s.add(2);
                *t.add(1) = *s.add(1);
                *t.add(2) = *s.add(0);
                *t.add(3) = *s.add(3);
            }
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
