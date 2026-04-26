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

use std::collections::HashMap;

use bridge::BridgeConfig;
use tci::server::ServerConfig;
use tci::state::SharedState;

/// Parsa "12000:iq,48000:iq48,96000:iq96" → HashMap<u32, String>.
/// Skipa entry malformate con warn.
fn parse_preset_map(s: &str) -> HashMap<u32, String> {
    let mut m = HashMap::new();
    for entry in s.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.splitn(2, ':');
        let rate_str = parts.next().unwrap_or("").trim();
        let preset = parts.next().map(|p| p.trim()).unwrap_or("");
        if preset.is_empty() {
            warn!(entry, "preset-map entry without ':preset', skipping");
            continue;
        }
        match rate_str.parse::<u32>() {
            Ok(rate) => {
                m.insert(rate, preset.to_string());
            }
            Err(_) => {
                warn!(entry, "preset-map entry with non-numeric rate, skipping");
            }
        }
    }
    m
}

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

    let preset_map = {
        let parsed = parse_preset_map(&cfg.preset_map);
        if parsed.is_empty() {
            warn!(
                input = %cfg.preset_map,
                "--preset-map vuota dopo il parsing; uso il default builtin"
            );
            bridge::default_preset_map()
        } else {
            parsed
        }
    };
    info!(
        preset_map = ?preset_map,
        default_preset = %cfg.default_preset,
        "preset configuration loaded"
    );

    let bridge_cfg = BridgeConfig {
        status_name: cfg.status_name.clone(),
        iface: iface_v4,
        poll_interval: Duration::from_secs(cfg.poll_interval_secs),
        default_samprate: cfg.iq_samplerate,
        max_trx: cfg.max_trx,
        preset_map,
        default_preset: cfg.default_preset.clone(),
    };

    let bridge_fut = bridge::run(bridge_cfg, Arc::clone(&state), cmd_rx);
    let server_fut = tci::server::run(&cfg.bind_addr, state, server_config);

    tokio::try_join!(bridge_fut, server_fut)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_preset_map_basic() {
        let m = parse_preset_map("12000:iq,48000:iq48,96000:iq96");
        assert_eq!(m.len(), 3);
        assert_eq!(m.get(&12_000).map(String::as_str), Some("iq"));
        assert_eq!(m.get(&48_000).map(String::as_str), Some("iq48"));
        assert_eq!(m.get(&96_000).map(String::as_str), Some("iq96"));
    }

    #[test]
    fn parse_preset_map_handles_whitespace() {
        let m = parse_preset_map(" 48000 : iq48 , 96000:iq96 ");
        assert_eq!(m.get(&48_000).map(String::as_str), Some("iq48"));
        assert_eq!(m.get(&96_000).map(String::as_str), Some("iq96"));
    }

    #[test]
    fn parse_preset_map_skips_malformed() {
        // entry vuote, mancanti separatore, rate non numerico
        let m = parse_preset_map(",,12000:iq,bad,48000:,:iq48,96000:iq96");
        assert_eq!(m.len(), 2);
        assert!(m.contains_key(&12_000));
        assert!(m.contains_key(&96_000));
        assert!(!m.contains_key(&48_000)); // preset vuoto → skip
    }

    #[test]
    fn parse_preset_map_empty_input() {
        assert!(parse_preset_map("").is_empty());
        assert!(parse_preset_map("  ,  , ").is_empty());
    }

    #[test]
    fn parse_preset_map_duplicate_keys_last_wins() {
        let m = parse_preset_map("48000:iq48,48000:iqA");
        assert_eq!(m.get(&48_000).map(String::as_str), Some("iqA"));
    }
}
