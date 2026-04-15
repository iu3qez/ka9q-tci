//! WebSocket server TCI.
//!
//! Ascolta su `bind_addr`, accetta connessioni, gestisce handshake e
//! smista comandi text / frame IQ binari per ogni client.

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn, debug};

use super::protocol::{self, TciCommand, format_msg, handshake_messages};
use super::state::SharedState;

/// Parametri del server TCI, derivati dalla config.
pub struct ServerConfig {
    pub device_name: String,
    pub trx_count: u8,
    pub channel_count: u8,
    pub vfo_min_hz: u64,
    pub vfo_max_hz: u64,
    pub if_min_hz: i64,
    pub if_max_hz: i64,
    pub modulations: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            device_name: "ka9q-RX888".to_string(),
            trx_count: 2,
            channel_count: 2,
            vfo_min_hz: 10_000,
            vfo_max_hz: 30_000_000,
            if_min_hz: -24_000,
            if_max_hz: 24_000,
            modulations: vec![
                "AM", "SAM", "DSB", "LSB", "USB", "CW", "NFM", "DIGL", "DIGU",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }
}

/// Avvia il server WS TCI. Non ritorna finché il runtime è attivo.
pub async fn run(
    bind_addr: &str,
    state: Arc<SharedState>,
    config: ServerConfig,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    let config = Arc::new(config);
    info!(bind = bind_addr, "TCI WebSocket server listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let state = Arc::clone(&state);
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, peer, state, config).await {
                warn!(%peer, err = %e, "client disconnected with error");
            } else {
                info!(%peer, "client disconnected");
            }
        });
    }
}

/// Gestisce una singola connessione client TCI.
async fn handle_client(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<SharedState>,
    config: Arc<ServerConfig>,
) -> anyhow::Result<()> {
    let ws_stream = tokio_tungstenite::accept_async(stream).await?;
    info!(%peer, "TCI client connected");

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // ── Handshake ───────────────────────────────────────────────────
    let iq_sr = *state.iq_samplerate.read().await;
    let mod_refs: Vec<&str> = config.modulations.iter().map(|s| s.as_str()).collect();
    let init_msgs = handshake_messages(
        &config.device_name,
        config.trx_count,
        config.channel_count,
        config.vfo_min_hz,
        config.vfo_max_hz,
        config.if_min_hz,
        config.if_max_hz,
        iq_sr,
        &mod_refs,
    );

    for msg in &init_msgs {
        debug!(%peer, "> {}", msg.trim_end());
        ws_tx.send(Message::Text(msg.clone().into())).await?;
    }

    // Invio stato corrente (frequenze, modi, filtri)
    let state_msgs = state.current_state_messages().await;
    for msg in &state_msgs {
        debug!(%peer, "> {}", msg.trim_end());
        ws_tx.send(Message::Text(msg.clone().into())).await?;
    }

    // ── Subscribe al broadcast IQ ───────────────────────────────────
    let mut iq_rx = state.iq_tx.subscribe();

    // Tracking: per quali TRX questo client ha attivato IQ streaming
    let mut iq_active = vec![false; config.trx_count as usize];

    // ── Event loop ──────────────────────────────────────────────────
    loop {
        tokio::select! {
            // Frame IQ dal bridge → client
            iq_result = iq_rx.recv() => {
                match iq_result {
                    Ok(frame) => {
                        let trx = frame.trx as usize;
                        if trx < iq_active.len() && iq_active[trx] {
                            ws_tx.send(Message::Binary(frame.data.into())).await?;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(%peer, skipped = n, "IQ frame lag, client too slow");
                    }
                    Err(RecvError::Closed) => {
                        info!(%peer, "IQ broadcast closed, shutting down");
                        break;
                    }
                }
            }

            // Messaggi dal client
            ws_msg = ws_rx.next() => {
                let ws_msg = match ws_msg {
                    Some(Ok(msg)) => msg,
                    Some(Err(e)) => {
                        warn!(%peer, err = %e, "WS read error");
                        break;
                    }
                    None => break, // stream chiuso
                };

                match ws_msg {
                    Message::Text(text) => {
                        let text_str: &str = text.as_ref();
                        debug!(%peer, "< {}", text_str.trim());
                        // Possono arrivare più comandi separati da newline
                        for line in text_str.lines() {
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }
                            match protocol::parse_command(line) {
                                Ok(cmd) => {
                                    let replies = handle_command(
                                        &cmd, &state, &config, &mut iq_active,
                                    ).await;
                                    for reply in replies {
                                        debug!(%peer, "> {}", reply.trim_end());
                                        ws_tx.send(Message::Text(reply.into())).await?;
                                    }
                                    // Broadcast ai peer: il server fa da synchronizer
                                    // (TODO: notifica altri client)
                                }
                                Err(e) => {
                                    warn!(%peer, cmd = line, err = %e, "parse error");
                                }
                            }
                        }
                    }
                    Message::Binary(_) => {
                        // TX audio dal client — RX-only, ignoriamo
                        debug!(%peer, "ignoring binary message (RX-only)");
                    }
                    Message::Ping(data) => {
                        ws_tx.send(Message::Pong(data)).await?;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Processa un comando TCI e restituisce zero o più risposte text.
///
/// Aggiorna lo stato condiviso e genera la reply per il client.
async fn handle_command(
    cmd: &TciCommand,
    state: &SharedState,
    config: &ServerConfig,
    iq_active: &mut [bool],
) -> Vec<String> {
    let mut replies = Vec::new();
    let max_trx = config.trx_count as usize;

    match cmd {
        // ── VFO Set ──
        TciCommand::Vfo { trx, vfo, freq_hz } => {
            let ti = *trx as usize;
            let vi = *vfo as usize;
            if ti < max_trx && vi < 2 {
                let mut trx_vec = state.trx.write().await;
                trx_vec[ti].vfo[vi].freq_hz = *freq_hz;
                // Se VFO A cambia, aggiorna anche DDS (centro = VFO A)
                if vi == 0 {
                    trx_vec[ti].dds_freq_hz = *freq_hz;
                }
                drop(trx_vec);
                // Echo back come conferma (spec: server fa da synchronizer)
                replies.push(format_msg(
                    "vfo",
                    &[&trx.to_string(), &vfo.to_string(), &freq_hz.to_string()],
                ));
                // TODO: inviare COMMAND TLV a radiod per riaccordare l'SSRC
            }
        }

        // ── VFO Read ──
        TciCommand::VfoRead { trx, vfo } => {
            let ti = *trx as usize;
            let vi = *vfo as usize;
            if ti < max_trx && vi < 2 {
                let trx_vec = state.trx.read().await;
                let freq = trx_vec[ti].vfo[vi].freq_hz;
                replies.push(format_msg(
                    "vfo",
                    &[&trx.to_string(), &vfo.to_string(), &freq.to_string()],
                ));
            }
        }

        // ── DDS Set ──
        TciCommand::Dds { trx, freq_hz } => {
            let ti = *trx as usize;
            if ti < max_trx {
                state.trx.write().await[ti].dds_freq_hz = *freq_hz;
                replies.push(format_msg(
                    "dds",
                    &[&trx.to_string(), &freq_hz.to_string()],
                ));
            }
        }

        // ── DDS Read ──
        TciCommand::DdsRead { trx } => {
            let ti = *trx as usize;
            if ti < max_trx {
                let freq = state.trx.read().await[ti].dds_freq_hz;
                replies.push(format_msg(
                    "dds",
                    &[&trx.to_string(), &freq.to_string()],
                ));
            }
        }

        // ── IF Set ──
        TciCommand::If { trx, vfo, offset_hz } => {
            let ti = *trx as usize;
            let vi = *vfo as usize;
            if ti < max_trx && vi < 2 {
                state.trx.write().await[ti].vfo[vi].if_offset_hz = *offset_hz;
                replies.push(format_msg(
                    "if",
                    &[&trx.to_string(), &vfo.to_string(), &offset_hz.to_string()],
                ));
            }
        }

        // ── IF Read ──
        TciCommand::IfRead { trx, vfo } => {
            let ti = *trx as usize;
            let vi = *vfo as usize;
            if ti < max_trx && vi < 2 {
                let off = state.trx.read().await[ti].vfo[vi].if_offset_hz;
                replies.push(format_msg(
                    "if",
                    &[&trx.to_string(), &vfo.to_string(), &off.to_string()],
                ));
            }
        }

        // ── Modulation Set ──
        TciCommand::Modulation { trx, mode } => {
            let ti = *trx as usize;
            if ti < max_trx {
                state.trx.write().await[ti].modulation = mode.clone();
                replies.push(format_msg(
                    "modulation",
                    &[&trx.to_string(), mode],
                ));
            }
        }

        // ── Modulation Read ──
        TciCommand::ModulationRead { trx } => {
            let ti = *trx as usize;
            if ti < max_trx {
                let m = state.trx.read().await[ti].modulation.clone();
                replies.push(format_msg(
                    "modulation",
                    &[&trx.to_string(), &m],
                ));
            }
        }

        // ── RX_CHANNEL_ENABLE Set ──
        TciCommand::RxChannelEnable { trx, channel, enable } => {
            let ti = *trx as usize;
            let ci = *channel as usize;
            if ti < max_trx && ci < 2 {
                state.trx.write().await[ti].vfo[ci].enabled = *enable;
                replies.push(format_msg(
                    "rx_channel_enable",
                    &[&trx.to_string(), &channel.to_string(), if *enable { "true" } else { "false" }],
                ));
            }
        }

        // ── RX_CHANNEL_ENABLE Read ──
        TciCommand::RxChannelEnableRead { trx, channel } => {
            let ti = *trx as usize;
            let ci = *channel as usize;
            if ti < max_trx && ci < 2 {
                let en = state.trx.read().await[ti].vfo[ci].enabled;
                replies.push(format_msg(
                    "rx_channel_enable",
                    &[&trx.to_string(), &channel.to_string(), if en { "true" } else { "false" }],
                ));
            }
        }

        // ── RX_FILTER_BAND Set ──
        TciCommand::RxFilterBand { trx, low, high } => {
            let ti = *trx as usize;
            if ti < max_trx {
                let mut trx_vec = state.trx.write().await;
                trx_vec[ti].filter_low = *low;
                trx_vec[ti].filter_high = *high;
                replies.push(format_msg(
                    "rx_filter_band",
                    &[&trx.to_string(), &low.to_string(), &high.to_string()],
                ));
            }
        }

        // ── RX_FILTER_BAND Read ──
        TciCommand::RxFilterBandRead { trx } => {
            let ti = *trx as usize;
            if ti < max_trx {
                let t = state.trx.read().await;
                replies.push(format_msg(
                    "rx_filter_band",
                    &[&trx.to_string(), &t[ti].filter_low.to_string(), &t[ti].filter_high.to_string()],
                ));
            }
        }

        // ── IQ control ──
        TciCommand::IqSamplerate { rate } => {
            *state.iq_samplerate.write().await = *rate;
            replies.push(format_msg("iq_samplerate", &[&rate.to_string()]));
        }

        TciCommand::IqStart { trx } => {
            let ti = *trx as usize;
            if ti < iq_active.len() {
                iq_active[ti] = true;
                info!(trx = ti, "IQ streaming started");
            }
        }

        TciCommand::IqStop { trx } => {
            let ti = *trx as usize;
            if ti < iq_active.len() {
                iq_active[ti] = false;
                info!(trx = ti, "IQ streaming stopped");
            }
        }

        // ── START/STOP ──
        TciCommand::Start => {
            replies.push(format_msg("start", &[]));
        }
        TciCommand::Stop => {
            replies.push(format_msg("stop", &[]));
        }

        // ── Comandi non gestiti: log e ignora ──
        TciCommand::Other { name, args } => {
            debug!(cmd = %name, ?args, "unhandled TCI command");
        }

        // Il resto (Spot, CW, Audio, Sensors) — log only per ora
        other => {
            debug!(?other, "TCI command acknowledged but not implemented");
        }
    }

    replies
}
