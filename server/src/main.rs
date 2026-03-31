// SPDX-License-Identifier: MIT OR Apache-2.0

mod config;
mod grpc;
mod session;
mod web;

use {std::net::SocketAddr, tracing::error, tracing_subscriber::EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = config::ServerConfig::load()?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.log_level))
        .init();

    let addr_http = SocketAddr::new(cfg.http_bind_host, cfg.http_port);
    let addr_grpc = SocketAddr::new(cfg.grpc_bind_host, cfg.grpc_port);

    let store = session::new_store();

    tokio::select! {
        res = web::serve(addr_http, store.clone(), cfg.clone()) => {
            error!("HTTP server exited: {:?}", res);
            res?
        },
        res = grpc::serve(addr_grpc, store.clone(), cfg.clone()) => {
            error!("gRPC server exited: {:?}", res);
            res?
        },
    }

    Ok(())
}
