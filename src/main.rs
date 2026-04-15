mod config;
mod radiod;
mod tci;
mod bridge;

use clap::Parser;
use tracing::{info, error};

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
        "ka9q-tci starting"
    );

    // TODO: avviare bridge::run(cfg).await
    info!("scaffolding OK — nessuna logica ancora, solo struttura");
    Ok(())
}
