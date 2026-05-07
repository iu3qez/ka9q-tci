mod config;
mod config_file;
mod radiod;
mod tci;
mod bridge;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use futures_util::future::try_join_all;
use tokio::sync::mpsc;
use tracing::{info, warn};

use std::collections::HashMap;

use bridge::BridgeConfig;
use config_file::{EndpointConfig, FileConfig, TrxConfig};
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

/// Endpoint risolto pronto per lo spawn. Tutti i campi opzionali nel YAML
/// sono già stati riempiti con i fallback dai flag CLI.
struct ResolvedEndpoint {
    port: u16,
    label: String,
    iq_samplerate: u32,
    trx: Vec<TrxConfig>,
}

/// Risolve la lista di endpoint da configurare. Tre casi:
///
/// - YAML con `endpoints:` → ogni entry diventa un `ResolvedEndpoint`,
///   campi opzionali ereditano dai flag CLI.
/// - YAML con solo `trx:` flat (forma legacy) → un singolo endpoint sulla
///   porta `--bind-addr` con `iq_samplerate=--iq-samplerate`.
/// - Nessun YAML → un singolo endpoint con i default CLI e nessuna freq
///   iniziale (i TRX usano i default hardcoded di `TrxState`).
fn resolve_endpoints(
    file_cfg: Option<&FileConfig>,
    cli: &config::Args,
) -> anyhow::Result<Vec<ResolvedEndpoint>> {
    let bind_port = cli
        .bind_addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("--bind-addr {} non ha una porta valida", cli.bind_addr))?;

    if let Some(cfg) = file_cfg {
        if cfg.has_endpoints() {
            return Ok(cfg
                .endpoints
                .iter()
                .map(|ep: &EndpointConfig| ResolvedEndpoint {
                    port: ep.port,
                    label: ep
                        .label
                        .clone()
                        .unwrap_or_else(|| ep.port.to_string()),
                    iq_samplerate: ep.iq_samplerate.unwrap_or(cli.iq_samplerate),
                    trx: ep.trx.clone(),
                })
                .collect());
        }
        // Forma legacy: trx flat → singolo endpoint sulla porta CLI.
        return Ok(vec![ResolvedEndpoint {
            port: bind_port,
            label: bind_port.to_string(),
            iq_samplerate: cli.iq_samplerate,
            trx: cfg.trx.clone(),
        }]);
    }

    // Niente YAML: singolo endpoint con default CLI e nessuna initial trx.
    Ok(vec![ResolvedEndpoint {
        port: bind_port,
        label: bind_port.to_string(),
        iq_samplerate: cli.iq_samplerate,
        trx: Vec::new(),
    }])
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ka9q_tci=info".into()),
        )
        .init();

    let cli = config::Args::parse();
    info!(
        status = %cli.status_name,
        bind = %cli.bind_addr,
        iq_sr = cli.iq_samplerate,
        max_trx = cli.max_trx,
        "ka9q-tci starting"
    );

    // Carica YAML opzionale.
    let file_cfg = match cli.config.as_deref() {
        Some(p) => match FileConfig::load(p) {
            Ok(Some(c)) => {
                let mode = if c.has_endpoints() { "multi-endpoint" } else { "legacy" };
                info!(
                    path = %p.display(),
                    mode,
                    n_endpoints = if c.has_endpoints() { c.endpoints.len() } else { 1 },
                    "config file loaded"
                );
                Some(c)
            }
            Ok(None) => {
                warn!(path = %p.display(), "config path non esiste, uso defaults");
                None
            }
            Err(e) => {
                warn!(err = %e, "errore parsing config file, uso defaults");
                None
            }
        },
        None => None,
    };

    let endpoints = resolve_endpoints(file_cfg.as_ref(), &cli)?;
    if endpoints.len() > u8::MAX as usize {
        anyhow::bail!(
            "troppi endpoint ({}): max {} (limite SSRC encoding)",
            endpoints.len(),
            u8::MAX
        );
    }

    let iface_v4 = match cli.mcast_iface {
        Some(IpAddr::V4(v4)) => Some(v4),
        Some(IpAddr::V6(_)) => {
            warn!("--mcast-iface IPv6 non supportato, uso INADDR_ANY");
            None
        }
        None => None,
    };

    let preset_map = {
        let parsed = parse_preset_map(&cli.preset_map);
        if parsed.is_empty() {
            warn!(
                input = %cli.preset_map,
                "--preset-map vuota dopo il parsing; uso il default builtin"
            );
            bridge::default_preset_map()
        } else {
            parsed
        }
    };
    info!(
        preset_map = ?preset_map,
        default_preset = %cli.default_preset,
        "preset configuration loaded"
    );

    // Costruisce N SharedState (uno per endpoint) e spawna N server TCI.
    // Tutti gli endpoint condividono lo stesso `cmd_tx` → un unico bridge
    // riceve i comandi e li dispatcha tramite il campo `BridgeCmd::endpoint`.
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let mut states: Vec<Arc<SharedState>> = Vec::with_capacity(endpoints.len());
    let mut server_handles = Vec::with_capacity(endpoints.len());
    // Tune iniziali: invio a radiod le freq configurate nel YAML PRIMA che
    // qualsiasi client si connetta. Senza questo, il client TCI riceve nel
    // handshake `vfo:0,0,FREQ;` (dal TCI state) ma radiod resta sul default
    // dell'instance (es. usb 12k) finché il client non manda un Tune
    // esplicito → disallineamento silenzioso, il client mostra dati che non
    // corrispondono alla freq pubblicizzata.
    let mut initial_tunes: Vec<bridge::BridgeCmd> = Vec::new();

    for (idx, ep) in endpoints.iter().enumerate() {
        let endpoint_id = idx as u8;
        info!(
            endpoint_id,
            port = ep.port,
            label = %ep.label,
            iq_sr = ep.iq_samplerate,
            n_trx = ep.trx.len(),
            "spawning TCI endpoint"
        );

        // trx_count per-endpoint = numero di TRX effettivamente configurati
        // nel YAML, non `cli.max_trx` globale. Annunciare TRX "fantasma"
        // (es. trx_count=3 con 1 solo TRX in config) sporca l'init handshake
        // con stati di default che il client non userà mai e che possono
        // confondere logiche client-side. Clamp a 1 per sicurezza.
        let trx_count = ep.trx.len().max(1) as u8;
        let state = SharedState::new_with_initial(
            trx_count as usize,
            ep.iq_samplerate,
            cmd_tx.clone(),
            &ep.trx,
        );
        states.push(Arc::clone(&state));

        // if_limits = ±(rate/2): l'IF passband per IQ baseband copre tutto
        // il sample rate / 2. Hardcoded a ±24k ignora gli endpoint a 96k+
        // → SDC pensa di avere 48k IQ utilizzabile e renderizza solo metà.
        let if_half = (ep.iq_samplerate / 2) as i64;
        let server_config = ServerConfig {
            endpoint_id,
            device_name: format!("ka9q-tci/{}", ep.label),
            trx_count,
            if_min_hz: -if_half,
            if_max_hz: if_half,
            ..ServerConfig::default()
        };
        let bind = format!("0.0.0.0:{}", ep.port);
        let handle = tokio::spawn(async move {
            tci::server::run(&bind, state, server_config).await
        });
        server_handles.push(handle);

        // Accumula Tune iniziali per i TRX con freq > 0.
        for (trx_idx, trx_cfg) in ep.trx.iter().enumerate() {
            if trx_cfg.freq > 0 {
                initial_tunes.push(bridge::BridgeCmd::Tune {
                    endpoint: endpoint_id,
                    trx: trx_idx as u8,
                    vfo: 0,
                    freq_hz: trx_cfg.freq,
                });
            }
        }
    }

    // Spedisco i Tune iniziali. Il bridge sta partendo in parallelo: i Tune
    // vanno nel canale e vengono dispatched non appena il `cmd_task` è
    // pronto. Se il canale è pieno (capacità 64), un YAML con > 64 TRX
    // farebbe await; nel deploy reale gli endpoint sono <= 16 quindi non
    // succede.
    let n_initial = initial_tunes.len();
    for cmd in initial_tunes {
        if let Err(e) = cmd_tx.send(cmd).await {
            warn!(err = %e, "initial Tune send failed");
        }
    }
    if n_initial > 0 {
        info!(n_initial, "initial Tune commands queued to bridge");
    }
    // cmd_tx originale rilasciato qui: i clones nei SharedState restano,
    // così cmd_rx non chiude prematuramente.
    drop(cmd_tx);

    let bridge_cfg = BridgeConfig {
        status_name: cli.status_name.clone(),
        iface: iface_v4,
        poll_interval: Duration::from_secs(cli.poll_interval_secs),
        default_samprate: cli.iq_samplerate,
        max_trx: cli.max_trx,
        preset_map,
        default_preset: cli.default_preset.clone(),
    };

    let bridge_fut = bridge::run(bridge_cfg, states, cmd_rx);

    // Trasforma le JoinHandle in Future<Output=Result<()>> "piatte".
    let server_fut = try_join_all(server_handles.into_iter().map(|h| async move {
        match h.await {
            Ok(r) => r,
            Err(e) => Err(anyhow::anyhow!("server task panicked: {e}")),
        }
    }));

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

    fn cli_default() -> config::Args {
        config::Args {
            status_name: "hf.local".into(),
            bind_addr: "0.0.0.0:40001".into(),
            mcast_iface: None,
            iq_samplerate: 48_000,
            preset_map: "48000:iq48".into(),
            default_preset: "iq48".into(),
            max_trx: 2,
            poll_interval_secs: 5,
            config: None,
        }
    }

    #[test]
    fn resolve_no_yaml_yields_single_default_endpoint() {
        let cli = cli_default();
        let eps = resolve_endpoints(None, &cli).unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].port, 40001);
        assert_eq!(eps[0].iq_samplerate, 48_000);
        assert!(eps[0].trx.is_empty());
    }

    #[test]
    fn resolve_legacy_yaml_yields_single_endpoint_with_trx() {
        let yaml = "trx:\n  - { freq: 7074000 }\n  - { freq: 14074000 }\n";
        let file: FileConfig = serde_yaml::from_str(yaml).unwrap();
        let cli = cli_default();
        let eps = resolve_endpoints(Some(&file), &cli).unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].port, 40001);
        assert_eq!(eps[0].iq_samplerate, 48_000);
        assert_eq!(eps[0].trx.len(), 2);
    }

    #[test]
    fn resolve_multi_endpoint_yaml_propagates_overrides() {
        let yaml = r#"
endpoints:
  - port: 40010
    label: 40m
    iq_samplerate: 96000
    trx: [{ freq: 7074000 }]
  - port: 40020
    trx: [{ freq: 14074000 }]
"#;
        let file: FileConfig = serde_yaml::from_str(yaml).unwrap();
        let cli = cli_default();
        let eps = resolve_endpoints(Some(&file), &cli).unwrap();
        assert_eq!(eps.len(), 2);
        // ep0: tutti i campi specificati nel YAML
        assert_eq!(eps[0].port, 40010);
        assert_eq!(eps[0].label, "40m");
        assert_eq!(eps[0].iq_samplerate, 96_000);
        // ep1: label e iq_samplerate fallback su CLI / port
        assert_eq!(eps[1].port, 40020);
        assert_eq!(eps[1].label, "40020");
        assert_eq!(eps[1].iq_samplerate, 48_000);
    }

    #[test]
    fn resolve_rejects_invalid_bind_port() {
        let mut cli = cli_default();
        cli.bind_addr = "no-port-here".into();
        assert!(resolve_endpoints(None, &cli).is_err());
        // Anche una "porta" non numerica dopo il `:` deve fallire.
        cli.bind_addr = "host:abc".into();
        assert!(resolve_endpoints(None, &cli).is_err());
    }
}
