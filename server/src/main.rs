// SPDX-License-Identifier: MIT OR Apache-2.0

mod config;
mod grpc;
mod pairing;
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
    let shutdown = CancellationToken::new();
    let _cleanup = pairing::spawn_cleanup_task(
        Arc::new(cfg.clone()),
        store.clone(),
        pairing_store.clone(),
        browser_session_store.clone(),
        ws_ticket_store.clone(),
        shutdown.clone(),
    );

    tokio::select! {
        res = web::serve(
            addr_http,
            store.clone(),
            pairing_store.clone(),
            browser_session_store.clone(),
            ws_ticket_store.clone(),
            cfg.clone(),
        ) => {
            shutdown.cancel();
            error!("HTTP server exited: {:?}", res);
            res?
        },
        res = grpc::serve(addr_grpc, store.clone(), pairing_store.clone(), cfg.clone()) => {
            shutdown.cancel();
            error!("gRPC server exited: {:?}", res);
            res?
        },
    }

    Ok(())
}
