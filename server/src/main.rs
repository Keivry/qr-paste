// SPDX-License-Identifier: MIT OR Apache-2.0

mod config;
mod grpc;
mod pairing;
mod persist;
mod session;
mod web;

use {
    std::{net::SocketAddr, sync::Arc},
    tokio_util::sync::CancellationToken,
    tracing::error,
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

    let shutdown = CancellationToken::new();
    let _cleanup = pairing::spawn_cleanup_task(
        Arc::new(cfg.clone()),
        store.clone(),
        pairing_store.clone(),
        browser_session_store.clone(),
        ws_ticket_store.clone(),
        shutdown.clone(),
        persist.clone(),
    );

    tokio::select! {
        res = web::serve(
            addr_http,
            store.clone(),
            pairing_store.clone(),
            browser_session_store.clone(),
            ws_ticket_store.clone(),
            cfg.clone(),
            persist.clone(),
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
