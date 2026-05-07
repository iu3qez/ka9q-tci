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

/// Numero di sample complessi per RTP packet osservato da ka9q-radio
/// in HF su MTU ethernet standard (1500). Usato solo per dimensionare il
/// buffer del broadcast channel — non influenza il wire format.
const ASSUMED_SAMPLES_PER_FRAME: u32 = 360;

/// Secondi di IQ che il broadcast channel deve poter bufferizzare prima
/// di iniziare a droppare con `Lagged(N)`. Coperto pause tipiche di SDC
/// (spot-batch verso RBN, GC) osservate empiricamente in ~0.6s. 2s
/// lascia ampio margine senza esplodere la memoria a rate alti.
const BUF_TARGET_SECONDS: u32 = 2;

/// Capacità del broadcast channel `iq_tx` in funzione del sample rate.
/// Dimensionato per garantire `BUF_TARGET_SECONDS` di tolleranza al
/// laggy consumer indipendentemente da `iq_samplerate`. A 48 kHz ≈ 266
/// frame; a 384 kHz ≈ 2133 frame. Il bound inferiore evita buffer
/// minuscoli per rate inattesi; il bound superiore evita esplosione
/// memoria se il rate è patologico.
pub(crate) fn iq_buffer_size(iq_samplerate: u32) -> usize {
    const MIN: usize = 128;
    const MAX: usize = 8192;
    let frames_per_sec = iq_samplerate / ASSUMED_SAMPLES_PER_FRAME;
    let target = (frames_per_sec as usize).saturating_mul(BUF_TARGET_SECONDS as usize);
    target.clamp(MIN, MAX)
}

/// Frame IQ pronto per l'invio ai client (già serializzato come bytes TCI).
/// Il bridge popola `endpoint` decodificandolo dall'SSRC; il server TCI
/// inoltra al client solo i frame del proprio `endpoint_id`.
#[derive(Debug, Clone)]
pub struct IqFrame {
    pub endpoint: u8,
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
        Self::new_with_initial(trx_count, iq_samplerate, cmd_tx, &[])
    }

    /// Variante che accetta uno stato iniziale per i primi N TRX.
    /// `initial[i]` configura il TRX `i`; gli indici oltre `initial.len()`
    /// usano `TrxState::default()`.
    pub fn new_with_initial(
        trx_count: usize,
        iq_samplerate: u32,
        cmd_tx: mpsc::Sender<BridgeCmd>,
        initial: &[crate::config_file::TrxConfig],
    ) -> Arc<Self> {
        let (iq_tx, _) = broadcast::channel(iq_buffer_size(iq_samplerate));
        let trx = (0..trx_count)
            .map(|i| {
                let mut s = TrxState::default();
                if let Some(cfg) = initial.get(i) {
                    s.dds_freq_hz = cfg.freq;
                    s.vfo[0].freq_hz = cfg.freq;
                    s.vfo[1].freq_hz = cfg.freq_b.unwrap_or(cfg.freq);
                    s.modulation = cfg.modulation.clone();
                }
                s
            })
            .collect();
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

            // `rx_enable` non è in spec TCI 2.0 ma è emesso da implementazioni
            // di riferimento (es. madpsy/ka9q_ubersdr) ed è richiesto da SDC
            // per attivare la subscription IQ — senza, il client riceve i
            // frame binari ma non li renderizza.
            msgs.push(format_msg("rx_enable", &[&si, "true"]));

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iq_buffer_size_scales_with_rate() {
        // A 48 kHz: 48000/360 * 2 ≈ 266 frame.
        assert!(iq_buffer_size(48_000) >= 200 && iq_buffer_size(48_000) <= 400);
        // A 96 kHz: ~533 frame.
        assert!(iq_buffer_size(96_000) >= 400 && iq_buffer_size(96_000) <= 700);
        // A 384 kHz: ~2133 frame.
        assert!(iq_buffer_size(384_000) >= 1800 && iq_buffer_size(384_000) <= 2400);
        // Monotonia: più alto il rate, più grande il buffer.
        assert!(iq_buffer_size(48_000) < iq_buffer_size(96_000));
        assert!(iq_buffer_size(96_000) < iq_buffer_size(192_000));
        assert!(iq_buffer_size(192_000) < iq_buffer_size(384_000));
    }

    #[test]
    fn iq_buffer_size_clamps_extremes() {
        // Rate inattesi (0, 1Hz) cadono sul minimo.
        assert_eq!(iq_buffer_size(0), 128);
        assert_eq!(iq_buffer_size(1), 128);
        // Rate patologico molto alto cade sul massimo.
        assert_eq!(iq_buffer_size(u32::MAX), 8192);
    }
}
