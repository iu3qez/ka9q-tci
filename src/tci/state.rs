//! Stato condiviso TCI — aggiornato dal bridge, letto/scritto dai client WS.
//!
//! Ogni TRX ha la propria frequenza DDS (centro panorama), VFO A/B,
//! modulazione, filtro, e flag di streaming IQ/audio attivo.

use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::bridge::BridgeCmd;

/// Stato di un singolo canale VFO (A o B).
#[derive(Debug, Clone)]
pub struct VfoState {
    pub freq_hz: u64,
    pub if_offset_hz: i64,
    pub enabled: bool,
}

impl Default for VfoState {
    fn default() -> Self {
        Self {
            freq_hz: 7_074_000,
            if_offset_hz: 0,
            enabled: true, // VFO A è sempre attivo per spec
        }
    }
}

/// Stato di un singolo TRX (receiver software).
#[derive(Debug, Clone)]
pub struct TrxState {
    pub dds_freq_hz: u64,
    pub vfo: [VfoState; 2], // A=0, B=1
    pub modulation: String,
    pub filter_low: i32,
    pub filter_high: i32,
}

impl Default for TrxState {
    fn default() -> Self {
        Self {
            dds_freq_hz: 7_074_000,
            vfo: [VfoState::default(), VfoState {
                enabled: false,
                ..VfoState::default()
            }],
            modulation: "USB".to_string(),
            filter_low: 0,
            filter_high: 3000,
        }
    }
}

/// Frame IQ pronto per l'invio ai client (già serializzato come bytes TCI).
#[derive(Debug, Clone)]
pub struct IqFrame {
    pub trx: u32,
    pub data: Vec<u8>, // header 64B + payload float32 LE
}

/// Stato globale condiviso tra bridge e client WS.
pub struct SharedState {
    /// Stato per-TRX, protetto da RwLock per letture concorrenti.
    pub trx: RwLock<Vec<TrxState>>,

    /// Parametri globali.
    pub iq_samplerate: RwLock<u32>,

    /// Canale broadcast per frame IQ (bridge → client).
    /// I client si iscrivono con `iq_tx.subscribe()`.
    pub iq_tx: broadcast::Sender<IqFrame>,

    /// Canale comandi verso il bridge (server WS → cmd_task).
    /// Bounded: in caso di Full, il caller logga e droppa (no backpressure).
    pub cmd_tx: mpsc::Sender<BridgeCmd>,
}

impl SharedState {
    pub fn new(
        trx_count: usize,
        iq_samplerate: u32,
        cmd_tx: mpsc::Sender<BridgeCmd>,
    ) -> Arc<Self> {
        let (iq_tx, _) = broadcast::channel(64); // buffer 64 frame
        let trx = (0..trx_count).map(|_| TrxState::default()).collect();
        Arc::new(Self {
            trx: RwLock::new(trx),
            iq_samplerate: RwLock::new(iq_samplerate),
            iq_tx,
            cmd_tx,
        })
    }

    /// Genera le righe di stato corrente da inviare dopo l'handshake.
    /// Include i `notification` standard (TX_ENABLE, TRX) richiesti da
    /// alcuni client (es. SDC) per uscire dallo stato "wait start", e
    /// chiude con `start;` per dichiarare che il device è running.
    pub async fn current_state_messages(&self) -> Vec<String> {
        use super::protocol::format_msg;
        let trx_vec = self.trx.read().await;
        let mut msgs = Vec::new();
        for (i, trx) in trx_vec.iter().enumerate() {
            let si = i.to_string();

            // RX-only: dichiara TX disabilitato per ogni TRX.
            msgs.push(format_msg("tx_enable", &[&si, "false"]));
            msgs.push(format_msg("trx", &[&si, "false"]));

            msgs.push(format_msg("dds", &[&si, &trx.dds_freq_hz.to_string()]));
            for (vi, vfo) in trx.vfo.iter().enumerate() {
                let sv = vi.to_string();
                msgs.push(format_msg("vfo", &[&si, &sv, &vfo.freq_hz.to_string()]));
                if vi > 0 {
                    msgs.push(format_msg(
                        "rx_channel_enable",
                        &[&si, &sv, if vfo.enabled { "true" } else { "false" }],
                    ));
                }
            }
            msgs.push(format_msg("modulation", &[&si, &trx.modulation]));
            msgs.push(format_msg(
                "rx_filter_band",
                &[&si, &trx.filter_low.to_string(), &trx.filter_high.to_string()],
            ));
        }
        // Annuncio finale: device running. SDC esce da "wait start" qui.
        msgs.push(format_msg("start", &[]));
        msgs
    }
}
