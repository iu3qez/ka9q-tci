mod config;
mod radiod;
mod tci;
mod bridge;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;
use tracing::{info, warn};

use bridge::BridgeConfig;
use tci::server::ServerConfig;
use tci::state::SharedState;

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

    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let state = SharedState::new(cfg.max_trx as usize, cfg.iq_samplerate, cmd_tx);

    let server_config = ServerConfig {
        trx_count: cfg.max_trx,
        ..ServerConfig::default()
    };

    let iface_v4 = match cfg.mcast_iface {
        Some(IpAddr::V4(v4)) => Some(v4),
        Some(IpAddr::V6(_)) => {
            warn!("--mcast-iface IPv6 non supportato, uso INADDR_ANY");
            None
        }
        None => None,
    };

    let bridge_cfg = BridgeConfig {
        status_name: cfg.status_name.clone(),
        iface: iface_v4,
        poll_interval: Duration::from_secs(cfg.poll_interval_secs),
        default_samprate: cfg.iq_samplerate,
        max_trx: cfg.max_trx,
        preset: cfg.preset.clone(),
    };

    let bridge_fut = bridge::run(bridge_cfg, Arc::clone(&state), cmd_rx);
    let server_fut = tci::server::run(&cfg.bind_addr, state, server_config);

    tokio::try_join!(bridge_fut, server_fut)?;
    Ok(())
}
