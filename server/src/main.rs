// SPDX-License-Identifier: MIT OR Apache-2.0

mod config;
mod grpc;
mod pairing;
mod persist;
mod session;
mod upload_store;
mod web;

use {
    std::{net::SocketAddr, sync::Arc},
    tokio_util::sync::CancellationToken,
    tracing::{error, info},
    tracing_subscriber::EnvFilter,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = config::ServerConfig::load()?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.log_level))
        .init();

    let addr_http = SocketAddr::new(cfg.http_bind_host, cfg.http_port);
    let addr_grpc = SocketAddr::new(cfg.grpc_bind_host, cfg.grpc_port);

    let store = session::new_store();
    let pairing_store = pairing::new_pairing_store();
    let browser_session_store = pairing::new_browser_session_store();
    let ws_ticket_store = pairing::new_ws_ticket_store();

    let persist = if cfg.persistence_path.is_empty() {
        None
    } else {
        let (store_arc, loaded_pairings, loaded_sessions) =
            persist::PersistenceStore::open(&cfg.persistence_path)?;
        for lp in loaded_pairings {
            pairing_store.insert(
                lp.pairing_id,
                pairing::PairingEntry {
                    pairing_id: lp.pairing_id,
                    pairing_secret: lp.pairing_secret,
                    epoch: lp.epoch,
                    online: false,
                    last_seen: lp.last_seen,
                    expires_at: lp.expires_at,
                    active_session_token: None,
                    active_mobile_ws: None,
                    revision: lp.revision,
                },
            );
        }
        for ls in loaded_sessions {
            browser_session_store.insert(
                ls.session_id,
                pairing::BrowserSession {
                    session_id: ls.session_id,
                    pairing_id: ls.pairing_id,
                    pairing_epoch: ls.pairing_epoch,
                    created_at: ls.created_at,
                    last_seen: ls.last_seen,
                    expires_at: ls.expires_at,
                    revoked: ls.revoked,
                },
            );
        }
        Some(store_arc)
    };

    validate_upload_dir(&cfg.upload_dir)?;
    info!("upload_dir 就绪: {}", cfg.upload_dir.display());

    let upload_store = upload_store::new_upload_store(
        cfg.max_upload_size_bytes,
        cfg.max_pending_upload_files_per_pairing,
        cfg.max_pending_upload_files_global,
        cfg.max_pending_upload_bytes_global,
    );
    let stats = upload_store.rebuild_baseline(&cfg.upload_dir, cfg.upload_file_retention_secs);
    info!(
        "upload_store 基线重建完成：扫描 {} 个文件，纳入基线 {} 个（{} 字节）",
        stats.scanned, stats.accepted, stats.accepted_bytes
    );

    let shutdown = CancellationToken::new();
    let _cleanup = pairing::spawn_cleanup_task(
        Arc::new(cfg.clone()),
        store.clone(),
        pairing_store.clone(),
        browser_session_store.clone(),
        ws_ticket_store.clone(),
        shutdown.clone(),
        persist.clone(),
        upload_store.clone(),
    );

    {
        let upload_store_bg = upload_store.clone();
        let upload_dir = cfg.upload_dir.clone();
        let retention_secs = cfg.upload_file_retention_secs;
        let cleanup_interval = cfg.upload_cleanup_interval_secs.max(1);
        let cancel = shutdown.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(cleanup_interval));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => {
                        upload_store_bg.cleanup_expired(&upload_dir, retention_secs).await;
                    }
                }
            }
        });
    }

    tokio::select! {
        res = web::serve(
            addr_http,
            store.clone(),
            pairing_store.clone(),
            browser_session_store.clone(),
            ws_ticket_store.clone(),
            cfg.clone(),
            persist.clone(),
            upload_store.clone(),
        ) => {
            shutdown.cancel();
            error!("HTTP server exited: {:?}", res);
            res?
        },
        res = grpc::serve(addr_grpc, store.clone(), pairing_store.clone(), cfg.clone(), persist.clone()) => {
            shutdown.cancel();
            error!("gRPC server exited: {:?}", res);
            res?
        },
    }

    Ok(())
}

fn validate_upload_dir(upload_dir: &std::path::Path) -> anyhow::Result<()> {
    if !upload_dir.exists() {
        std::fs::create_dir_all(upload_dir)
            .map_err(|e| anyhow::anyhow!("无法创建 upload_dir {}: {e}", upload_dir.display()))?;
    }

    let meta = std::fs::symlink_metadata(upload_dir)
        .map_err(|e| anyhow::anyhow!("无法读取 upload_dir 元数据 {}: {e}", upload_dir.display()))?;

    if !meta.is_dir() {
        anyhow::bail!(
            "upload_dir {} 不是普通目录（可能是文件或符号链接）",
            upload_dir.display()
        );
    }

    let probe_path = upload_dir.join(".qr_paste_write_probe");
    std::fs::write(&probe_path, b"probe").map_err(|e| {
        anyhow::anyhow!(
            "upload_dir {} 不可写（权限或配额错误）: {e}",
            upload_dir.display()
        )
    })?;
    let _ = std::fs::remove_file(&probe_path);

    Ok(())
}
