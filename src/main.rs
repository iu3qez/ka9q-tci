mod config;
mod radiod;
mod tci;
mod bridge;

use std::sync::Arc;

use clap::Parser;
use tracing::info;

use tci::state::SharedState;
use tci::server::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ka9q_tci=info".into()),
        )
        .init();

    let cfg = config::Args::parse();
    info!(
        status = %cfg.status_name,
        bind = %cfg.bind_addr,
        iq_sr = cfg.iq_samplerate,
        max_trx = cfg.max_trx,
        "ka9q-tci starting"
    );

    // Stato condiviso
    let state = SharedState::new(cfg.max_trx as usize, cfg.iq_samplerate);

    // Config server TCI
    let server_config = ServerConfig {
        trx_count: cfg.max_trx,
        ..ServerConfig::default()
    };

    // TODO: avviare bridge::run() in parallelo (radiod multicast ↔ state)
    // Per ora solo il WS server, testabile con qualsiasi client TCI.

    tci::server::run(&cfg.bind_addr, state, server_config).await
}
