//! Orchestratore: collega il lato radiod (multicast RTP + control TLV)
//! con il lato TCI (WebSocket server).
//!
//! Responsabilità:
//! - Gestire la mappa (trx, vfo) → SSRC
//! - Creare/distruggere canali radiod on-demand (preset iq)
//! - Inoltrare IQ da RTP → frame TCI binari ai client connessi
//! - Tradurre comandi TCI (VFO, DDS, ...) in COMMAND TLV verso radiod

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, info, warn};

use crate::radiod::control::ControlClient;
use crate::radiod::multicast::join_multicast;
use crate::radiod::rtp;
use crate::radiod::tlv::{Encoding, StatusType, TlvField, TlvValue};
use crate::tci::protocol::build_iq_frame;
use crate::tci::state::{IqFrame, SharedState};

/// Prefisso dello SSRC: identifica i canali creati da ka9q-tci.
/// Usiamo un valore "TCI" stilizzato in nibble.
pub const SSRC_PREFIX: u32 = 0x7C10_0000;
pub const SSRC_TRX_SHIFT: u32 = 4;
pub const SSRC_VFO_MASK: u32 = 0x0F;
pub const SSRC_TRX_MASK: u32 = 0x0F;

/// Codifica deterministica (trx, vfo) → SSRC.
///
/// Layout (32 bit, big-endian dentro RTP):
///   [SSRC_PREFIX (24 bit)] [trx (4 bit)] [vfo (4 bit)]
///
/// `trx` accettato 0..=15 (4 bit), `vfo` accettato 0..=15 (4 bit).
/// Valori più grandi vengono troncati con `& 0xF`.
pub fn ssrc_encode(trx: u8, vfo: u8) -> u32 {
    SSRC_PREFIX | ((trx as u32 & SSRC_TRX_MASK) << SSRC_TRX_SHIFT) | (vfo as u32 & SSRC_VFO_MASK)
}

/// Decodifica SSRC → (trx, vfo). Restituisce None se non matcha il nostro prefix.
pub fn ssrc_decode(ssrc: u32) -> Option<(u8, u8)> {
    if (ssrc & 0xFFFF_FF00) != SSRC_PREFIX {
        return None;
    }
    let trx = ((ssrc >> SSRC_TRX_SHIFT) & SSRC_TRX_MASK) as u8;
    let vfo = (ssrc & SSRC_VFO_MASK) as u8;
    Some((trx, vfo))
}

/// Stato di un canale ka9q-radio noto al bridge.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub ssrc: u32,
    pub trx: u8,
    pub vfo: u8,
    pub freq_hz: u64,
    pub samprate: u32,
    /// Gruppo multicast su cui radiod pubblica i dati RTP per questo SSRC.
    /// Scoperto da STATUS (campo non ancora wire-mappato — TODO Step 3/5).
    pub data_group: Option<Ipv4Addr>,
    /// True se abbiamo ricevuto almeno uno STATUS che conferma l'esistenza.
    pub created: bool,
}

impl ChannelInfo {
    fn new(trx: u8, vfo: u8) -> Self {
        Self {
            ssrc: ssrc_encode(trx, vfo),
            trx,
            vfo,
            freq_hz: 0,
            samprate: 0,
            data_group: None,
            created: false,
        }
    }
}

/// Mappa SSRC → ChannelInfo. Logica pura, niente I/O.
#[derive(Debug, Default)]
pub struct SsrcTable {
    by_ssrc: HashMap<u32, ChannelInfo>,
}

impl SsrcTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserisce (o restituisce) il ChannelInfo per (trx, vfo).
    /// Se non esiste, viene creato con `created=false` e SSRC deterministico.
    pub fn get_or_insert(&mut self, trx: u8, vfo: u8) -> &mut ChannelInfo {
        let ssrc = ssrc_encode(trx, vfo);
        self.by_ssrc.entry(ssrc).or_insert_with(|| ChannelInfo::new(trx, vfo))
    }

    /// Lookup per SSRC esatto.
    pub fn get(&self, ssrc: u32) -> Option<&ChannelInfo> {
        self.by_ssrc.get(&ssrc)
    }

    /// Aggiorna lo stato a partire dai TLV di uno STATUS packet.
    /// I campi attesi sono interpretati come segue:
    ///   - OUTPUT_SSRC (21): identifica il canale
    ///   - RADIO_FREQUENCY (40): Double in Hz
    ///   - OUTPUT_SAMPRATE (23): Int (samples/s)
    ///
    /// Se OUTPUT_SSRC non è presente nei field, fa nulla.
    /// Se OUTPUT_SSRC non matcha il prefix di ka9q-tci, fa nulla
    /// (è un canale gestito da altri client).
    /// I field mancanti vengono ignorati (non resettano i valori esistenti).
    pub fn update_from_status(&mut self, fields: &[TlvField]) {
        // estrai SSRC
        let ssrc = fields.iter().find_map(|f| {
            if f.tag == StatusType::OUTPUT_SSRC as u8 {
                if let TlvValue::Int(v) = &f.value {
                    return Some(*v as u32);
                }
            }
            None
        });
        let ssrc = match ssrc {
            Some(s) => s,
            None => return,
        };
        let (trx, vfo) = match ssrc_decode(ssrc) {
            Some(x) => x,
            None => return, // non nostro
        };

        let entry = self.by_ssrc.entry(ssrc).or_insert_with(|| ChannelInfo::new(trx, vfo));
        // NB: `created` non viene toccato qui. Semantica: `created=true`
        // significa "il bridge ha già inviato un COMMAND con PRESET per
        // questo SSRC". Se radiod ha il canale aperto da un run precedente
        // (stesso SSRC, preset diverso), non possiamo dedurlo dallo STATUS
        // — anzi, vogliamo che il prossimo Tune ri-mandi PRESET per
        // riportarlo al nostro preset corrente. mark_created() viene
        // chiamato esplicitamente dal dispatch_cmd dopo il primo invio.

        for f in fields {
            match (f.tag, &f.value) {
                (t, TlvValue::Double(d)) if t == StatusType::RADIO_FREQUENCY as u8 => {
                    entry.freq_hz = d.round() as u64;
                }
                (t, TlvValue::Float(d)) if t == StatusType::RADIO_FREQUENCY as u8 => {
                    entry.freq_hz = d.round() as u64;
                }
                (t, TlvValue::Int(v)) if t == StatusType::OUTPUT_SAMPRATE as u8 => {
                    entry.samprate = *v as u32;
                }
                (t, TlvValue::Bytes(b)) if t == StatusType::OUTPUT_DATA_DEST_SOCKET as u8 => {
                    // Wire format da ka9q-radio src/status.c (encode_socket):
                    //   AF_INET  → 6 byte:  [ip:4][port:2]
                    //   AF_INET6 → 18 byte: [ip6:16][port:2]
                    // Family inferita dalla length (NON presente sul wire).
                    match b.len() {
                        6 => {
                            if let Ok(arr) = <[u8; 4]>::try_from(&b[..4]) {
                                entry.data_group = Some(Ipv4Addr::from(arr));
                            }
                        }
                        18 => {
                            // IPv6 non supportato per ora: il bridge fa solo IPv4 multicast.
                            tracing::warn!(
                                ssrc = format!("{:#010x}", ssrc),
                                "OUTPUT_DATA_DEST_SOCKET IPv6 ignorato"
                            );
                        }
                        n => {
                            tracing::warn!(
                                ssrc = format!("{:#010x}", ssrc),
                                len = n,
                                "OUTPUT_DATA_DEST_SOCKET: lunghezza inattesa"
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Marca un canale come creato (ottimisticamente, dopo aver inviato un
    /// COMMAND di creazione, senza attendere lo STATUS di conferma).
    /// Evita PRESET/OUTPUT_SAMPRATE ridondanti su Tune ravvicinati.
    pub fn mark_created(&mut self, ssrc: u32) {
        if let Some(ch) = self.by_ssrc.get_mut(&ssrc) {
            ch.created = true;
        }
    }

    /// Invalida lo stato di un canale dopo un cambio di preset (SetSr).
    /// radiod può ricreare il canale e cambiare `data_group`; resettiamo
    /// `created=false` e `data_group=None` per forzare:
    ///   - re-invio di PRESET sul prossimo Tune
    ///   - ri-discovery del data multicast nel rtp_manager_task
    pub fn invalidate(&mut self, ssrc: u32) {
        if let Some(ch) = self.by_ssrc.get_mut(&ssrc) {
            ch.created = false;
            ch.data_group = None;
        }
    }

    /// Iter immutabile su tutti i canali noti.
    pub fn iter(&self) -> impl Iterator<Item = &ChannelInfo> {
        self.by_ssrc.values()
    }

    pub fn len(&self) -> usize {
        self.by_ssrc.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ssrc.is_empty()
    }
}

// ────────────────────────────────────────────────────────────────────
// Bridge supervisor: orchestrazione control plane + SsrcTable.
// ────────────────────────────────────────────────────────────────────

/// Comandi inviati dai client TCI al bridge tramite `SharedState::cmd_tx`.
/// Il bridge li traduce in COMMAND TLV verso radiod.
#[derive(Debug, Clone)]
pub enum BridgeCmd {
    /// Accorda il canale (trx, vfo) sulla frequenza data, creando il
    /// canale ka9q se non esiste ancora.
    Tune { trx: u8, vfo: u8, freq_hz: u64 },
    /// Cambia il sample rate IQ per tutti i canali noti.
    SetSr { samprate: u32 },
    /// Abilita/disabilita un canale (per ora: solo log; teardown TODO).
    EnableRx { trx: u8, vfo: u8, enable: bool },
}

/// Configurazione runtime del bridge.
pub struct BridgeConfig {
    pub status_name: String,
    pub iface: Option<Ipv4Addr>,
    pub poll_interval: Duration,
    pub default_samprate: u32,
    pub max_trx: u8,
    /// Mapping (sample-rate Hz) → (nome preset ka9q-radio).
    /// Il preset deve esistere in /usr/local/share/ka9q-radio/presets.conf
    /// con `demod = linear` e `samprate` coincidente.
    /// Default: 12000→iq, 48000→iq48, 96000→iq96.
    pub preset_map: HashMap<u32, String>,
    /// Preset usato se il samprate richiesto dal client TCI non è in
    /// `preset_map` (warn + fallback).
    pub default_preset: String,
}

/// Mapping di default samprate→preset.
pub fn default_preset_map() -> HashMap<u32, String> {
    let mut m = HashMap::new();
    m.insert(12_000, "iq".to_string());
    m.insert(48_000, "iq48".to_string());
    m.insert(96_000, "iq96".to_string());
    m
}

/// Avvia il bridge: connette il control plane, lancia POLL periodico,
/// dispatch comandi TCI→radiod e aggiornamento dello stato SSRC.
/// Ritorna solo su errore fatale.
pub async fn run(
    cfg: BridgeConfig,
    state: Arc<SharedState>,
    cmd_rx: mpsc::Receiver<BridgeCmd>,
) -> anyhow::Result<()> {
    if cfg.poll_interval.is_zero() {
        anyhow::bail!("poll_interval must be > 0");
    }
    info!(
        status = %cfg.status_name,
        interval_s = cfg.poll_interval.as_secs(),
        "bridge starting"
    );

    let mut control = ControlClient::connect(&cfg.status_name, cfg.iface).await?;
    let status_rx = control
        .take_status_rx()
        .ok_or_else(|| anyhow::anyhow!("status_rx already taken"))?;
    let control = Arc::new(control);

    let ssrc_table: Arc<Mutex<SsrcTable>> = Arc::new(Mutex::new(SsrcTable::new()));

    // ── poll_task: invia POLL ogni `poll_interval` ──
    let poll_client = Arc::clone(&control);
    let poll_interval = cfg.poll_interval;
    let poll_task = tokio::spawn(async move {
        let mut tick = time::interval(poll_interval);
        // Il primo tick è immediato: vogliamo popolare la SsrcTable ASAP.
        loop {
            tick.tick().await;
            match poll_client.poll().await {
                Ok(()) => debug!("POLL sent"),
                Err(e) => warn!(err = %e, "POLL failed"),
            }
        }
    });

    // ── status_task: consuma STATUS, aggiorna SsrcTable ──
    let table_for_status = Arc::clone(&ssrc_table);
    let status_task = tokio::spawn(async move {
        let mut rx = status_rx;
        while let Some(pkt) = rx.recv().await {
            let mut table = match table_for_status.lock() {
                Ok(g) => g,
                Err(poisoned) => {
                    warn!("ssrc_table mutex poisoned, recovering");
                    poisoned.into_inner()
                }
            };
            let before = table.len();
            table.update_from_status(&pkt.fields);
            let after = table.len();
            if after > before {
                if let Some(ssrc) = pkt.ssrc {
                    info!(
                        ssrc = format!("{:#010x}", ssrc),
                        total = after,
                        "discovered new SSRC"
                    );
                }
            }
        }
        debug!("status channel closed, status_task exiting");
    });

    // ── cmd_task: dispatch comandi TCI → COMMAND TLV ──
    //
    // Il task è un singolo consumer del canale mpsc, quindi ogni `BridgeCmd`
    // viene processato sequenzialmente. Tune legge `state.iq_samplerate`
    // tramite RwLock, scritto da server.rs su IqSamplerate; nel deployment
    // single-client la sequenza IqSamplerate→…→Vfo arriva ordinata.
    //
    // Caso multi-client: due client che concorrono su Iq_SAMPLERATE/VFO
    // possono interleave, e un Tune può vincere la corsa contro un SetSr
    // da altro client → primo create con preset vecchio. Mitigato perché
    // SetSr re-tune in seguito tutti i canali noti col nuovo preset.
    let cmd_client = Arc::clone(&control);
    let table_for_cmd = Arc::clone(&ssrc_table);
    let state_for_cmd = Arc::clone(&state);
    let preset_map: Arc<HashMap<u32, String>> = Arc::new(cfg.preset_map.clone());
    let default_preset: Arc<str> = Arc::from(cfg.default_preset.as_str());
    let cmd_task = tokio::spawn(async move {
        let mut rx = cmd_rx;
        while let Some(cmd) = rx.recv().await {
            if let Err(e) = dispatch_cmd(
                &cmd_client,
                &table_for_cmd,
                &state_for_cmd,
                &preset_map,
                &default_preset,
                cmd,
            )
            .await
            {
                warn!(err = %e, "cmd dispatch failed");
            }
        }
        debug!("cmd channel closed, cmd_task exiting");
    });

    // ── rtp_manager_task: scopre data_group nuovi e spawna ingest ──
    let table_for_rtp = Arc::clone(&ssrc_table);
    let state_for_rtp = Arc::clone(&state);
    let iface_for_rtp = cfg.iface;
    let rtp_task = tokio::spawn(async move {
        rtp_manager(table_for_rtp, state_for_rtp, iface_for_rtp).await;
    });

    // ── attesa del primo task che termina ──
    // TODO Step 6: al momento del select! i task perdenti non vengono
    // abort()ati; restano orfani fino al teardown del runtime. Acceptable
    // perché main esce subito dopo via try_join!, ma da sistemare con
    // CancellationToken in Step 6.
    tokio::select! {
        r = poll_task => match r {
            Ok(()) => Err(anyhow::anyhow!("poll_task ended unexpectedly")),
            Err(e) => Err(anyhow::anyhow!("poll_task panicked: {e}")),
        },
        r = status_task => match r {
            Ok(()) => Err(anyhow::anyhow!("status_task ended unexpectedly")),
            Err(e) => Err(anyhow::anyhow!("status_task panicked: {e}")),
        },
        r = cmd_task => match r {
            Ok(()) => Err(anyhow::anyhow!("cmd_task ended unexpectedly")),
            Err(e) => Err(anyhow::anyhow!("cmd_task panicked: {e}")),
        },
        r = rtp_task => match r {
            Ok(()) => Err(anyhow::anyhow!("rtp_task ended unexpectedly")),
            Err(e) => Err(anyhow::anyhow!("rtp_task panicked: {e}")),
        },
    }
}

/// Esegue un singolo `BridgeCmd` traducendolo in COMMAND TLV verso radiod.
///
/// Lock policy: la SsrcTable viene letta/modificata in lock brevi che NON
/// attraversano `.await`. I dati necessari per la build TLV sono raccolti
/// dentro lo scope del lock e poi rilasciati prima di `send_command`.
async fn dispatch_cmd(
    control: &ControlClient,
    table: &Arc<Mutex<SsrcTable>>,
    state: &Arc<SharedState>,
    preset_map: &HashMap<u32, String>,
    default_preset: &str,
    cmd: BridgeCmd,
) -> anyhow::Result<()> {
    match cmd {
        BridgeCmd::Tune { trx, vfo, freq_hz } => {
            // Sample rate corrente richiesto dal client TCI.
            // Determina il preset ka9q-radio da chiedere a radiod.
            let current_sr = *state.iq_samplerate.read().await;
            let preset = preset_map
                .get(&current_sr)
                .map(|s| s.as_str())
                .unwrap_or_else(|| {
                    warn!(
                        samprate = current_sr,
                        fallback = default_preset,
                        "no preset mapped for samprate, using fallback"
                    );
                    default_preset
                });

            // Decide se serve creazione o solo retune, dentro lock breve
            // (std::sync::Mutex, niente .await dentro lo scope).
            let (ssrc, needs_create) = {
                let mut t = match table.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                let ch = t.get_or_insert(trx, vfo);
                let needs_create = !ch.created;
                (ch.ssrc, needs_create)
            };

            // Al primo create mandiamo solo SSRC + PRESET + RADIO_FREQUENCY,
            // come fa il `tune` di ka9q-radio. Specificare OUTPUT_SAMPRATE o
            // OUTPUT_ENCODING incompatibili col preset fa rifiutare radiod
            // silenziosamente. Il samprate è quindi quello del preset.
            let mut fields = Vec::with_capacity(3);
            fields.push((StatusType::OUTPUT_SSRC, TlvValue::Int(ssrc as u64)));
            if needs_create {
                fields.push((
                    StatusType::PRESET,
                    TlvValue::Bytes(preset.as_bytes().to_vec()),
                ));
            }
            fields.push((
                StatusType::RADIO_FREQUENCY,
                TlvValue::Double(freq_hz as f64),
            ));

            debug!(
                ssrc = format!("{:#010x}", ssrc),
                freq_hz,
                create = needs_create,
                preset = needs_create.then_some(preset),
                samprate = current_sr,
                "Tune → COMMAND"
            );
            control.send_command(&fields).await?;

            // Optimistic flag: evita PRESET/OUTPUT_SAMPRATE ridondanti su
            // Tune ravvicinati prima che lo STATUS confermi la creazione.
            if needs_create {
                let mut t = match table.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                t.mark_created(ssrc);
            }
        }
        BridgeCmd::SetSr { samprate } => {
            // Lookup preset corrispondente al nuovo samprate. Se non mappato,
            // log warn e usa default. Risolto qui per logging coerente.
            let preset = preset_map
                .get(&samprate)
                .map(|s| s.as_str())
                .unwrap_or_else(|| {
                    warn!(
                        samprate,
                        fallback = default_preset,
                        "no preset mapped for SetSr samprate, using fallback"
                    );
                    default_preset
                });

            // Snapshot dei canali esistenti per re-tune con il nuovo preset.
            // Solo i canali GIÀ tunati (freq_hz != 0) vengono ri-accordati;
            // canali con freq_hz=0 sono entry preallocate ma non ancora usate
            // dal client e verranno tunate dal prossimo Tune con il nuovo SR.
            //
            // Cambio del preset implica: cambio samprate/passband, e — dato che
            // radiod può scegliere un data_group diverso e rigenerare lo SSRC
            // socket — invalidiamo `created` e `data_group` così che:
            //   - il prossimo Tune ri-mandi PRESET (se serve)
            //   - rtp_manager rifaccia la discovery del nuovo gruppo dallo STATUS.
            let snapshot: Vec<(u32, u64)> = {
                let mut t = match table.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                let snap: Vec<(u32, u64)> = t
                    .iter()
                    .filter(|c| c.freq_hz != 0)
                    .map(|c| (c.ssrc, c.freq_hz))
                    .collect();
                for (ssrc, _) in &snap {
                    t.invalidate(*ssrc);
                }
                snap
            };
            if snapshot.is_empty() {
                debug!(samprate, preset, "SetSr: nessun canale tunato, skip");
                return Ok(());
            }
            for (ssrc, freq_hz) in snapshot {
                let fields = vec![
                    (StatusType::OUTPUT_SSRC, TlvValue::Int(ssrc as u64)),
                    (
                        StatusType::PRESET,
                        TlvValue::Bytes(preset.as_bytes().to_vec()),
                    ),
                    (
                        StatusType::RADIO_FREQUENCY,
                        TlvValue::Double(freq_hz as f64),
                    ),
                ];
                debug!(
                    ssrc = format!("{:#010x}", ssrc),
                    samprate, preset, freq_hz, "SetSr → COMMAND"
                );
                control.send_command(&fields).await?;
            }
        }
        BridgeCmd::EnableRx { trx, vfo, enable } => {
            // MVP: solo log. Il teardown lato radiod richiede COMMAND specifico
            // ancora da definire (vedi ka9q-radio: rimozione canale via SSRC).
            debug!(trx, vfo, enable, "EnableRx (no-op MVP)");
        }
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// RTP ingest: scoperta dei data_group e pump RTP → IqFrame broadcast
// ────────────────────────────────────────────────────────────────────

const RTP_DATA_PORT: u16 = 5004;
const RTP_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Loop che osserva la SsrcTable e spawna un task ingest per ogni nuovo
/// `data_group` mai joinato in precedenza. Un singolo task copre tutti gli
/// SSRC che pubblicano sullo stesso gruppo (radiod tipicamente bundle un
/// preset → un gruppo data).
async fn rtp_manager(
    table: Arc<Mutex<SsrcTable>>,
    state: Arc<SharedState>,
    iface: Option<Ipv4Addr>,
) {
    let mut joined: HashMap<Ipv4Addr, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut tick = time::interval(RTP_POLL_INTERVAL);
    loop {
        tick.tick().await;

        // Snapshot dei data_group correntemente noti (lock breve).
        let new_groups: Vec<Ipv4Addr> = {
            let t = match table.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            t.iter()
                .filter_map(|c| c.data_group)
                .filter(|g| !joined.contains_key(g))
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect()
        };

        for group in new_groups {
            info!(group = %group, port = RTP_DATA_PORT, "joining RTP data group");
            let table_for_ingest = Arc::clone(&table);
            let state_for_ingest = Arc::clone(&state);
            let handle = tokio::spawn(async move {
                if let Err(e) = rtp_ingest(group, iface, table_for_ingest, state_for_ingest).await {
                    warn!(%group, err = %e, "rtp_ingest terminated");
                }
            });
            // TODO Step 6: in caso di terminazione del task ingest (errore di
            // join_multicast o socket chiuso) l'entry resta nella mappa →
            // nessun respawn. Aggiungere watchdog con backoff.
            joined.insert(group, handle);
        }
    }
}

/// Riceve RTP da un gruppo data, parsifica header, mappa SSRC → (trx, vfo)
/// via SsrcTable, costruisce IqFrame e li pubblica su `state.iq_tx`.
///
/// Assunzione: encoding del payload = S16BE stereo interleaved (default di
/// ka9q-radio per preset `iq`). Convertito a f32 nell'intervallo [-1.0, +1.0)
/// prima di costruire il frame TCI binario, che vuole f32 little-endian.
async fn rtp_ingest(
    group: Ipv4Addr,
    iface: Option<Ipv4Addr>,
    table: Arc<Mutex<SsrcTable>>,
    state: Arc<SharedState>,
) -> anyhow::Result<()> {
    let sock = join_multicast(group, RTP_DATA_PORT, iface).await?;
    // 8192 copre MTU ethernet standard (1500) e jumbo frame (9000) — solo
    // jumbo > 8192 viene troncato. radiod tipicamente < 1500B.
    let mut buf = vec![0u8; 8192];

    loop {
        let (n, _src) = match sock.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(e) => {
                warn!(%group, err = %e, "RTP recv error");
                continue;
            }
        };
        if n == buf.len() {
            warn!(%group, "RTP packet at buffer limit, possible truncation");
        }
        let pkt = &buf[..n];

        let (hdr, payload_off) = match rtp::parse(pkt) {
            Ok(x) => x,
            Err(e) => {
                warn!(%group, err = %e, "RTP parse failed");
                continue;
            }
        };

        // Mappa SSRC → trx (lock breve).
        let trx = {
            let t = match table.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            match t.get(hdr.ssrc) {
                Some(ch) => ch.trx as u32,
                None => continue, // SSRC non nostro o non ancora mappato
            }
        };

        // Sample rate annunciato al client TCI: autoritativo, NON il
        // valore della SsrcTable. Il preset attivo è scelto in funzione
        // di `state.iq_samplerate` (vedi preset_map), quindi i due
        // coincidono per costruzione. Mettere qui il valore della
        // SsrcTable rischierebbe sample_rate=0 nel frame se lo STATUS
        // non avesse ancora popolato OUTPUT_SAMPRATE.
        let samprate = *state.iq_samplerate.read().await;

        let payload = &pkt[payload_off..];
        let samples = decode_s16be_stereo(payload);
        if samples.is_empty() {
            continue;
        }

        // Log diagnostico del PRIMO frame su questo gruppo: aiuta a
        // verificare header TCI e contenuto IQ contro la spec ExpertSDR3.
        static FIRST_FRAME_LOGGED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !FIRST_FRAME_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let (i0, q0) = samples[0];
            info!(
                ssrc = format!("{:#010x}", hdr.ssrc),
                trx,
                samprate,
                n_samples = samples.len(),
                rtp_payload_len = payload.len(),
                first_i = i0,
                first_q = q0,
                "first IQ frame to TCI clients"
            );
        }

        let frame_bytes = build_iq_frame(trx, samprate, &samples);
        let frame = IqFrame {
            trx,
            data: frame_bytes,
        };
        // Errore = nessun subscriber, non è fatale.
        let _ = state.iq_tx.send(frame);
    }
}

/// Decodifica payload S16BE stereo interleaved → Vec<(I, Q)> in float [-1, +1).
/// Drop incompleto se il numero di byte non è multiplo di 4 (2 × i16).
///
/// Layout wire (per frame): [I_hi, I_lo, Q_hi, Q_lo, ...] big-endian i16.
/// Normalizzazione: `f = i16_value / 32768.0`. Range: [-1.0, +1.0).
fn decode_s16be_stereo(payload: &[u8]) -> Vec<(f32, f32)> {
    const SCALE: f32 = 1.0 / 32768.0;
    let n_pairs = payload.len() / 4;
    let mut out = Vec::with_capacity(n_pairs);
    for i in 0..n_pairs {
        let off = i * 4;
        let i_raw = i16::from_be_bytes([payload[off], payload[off + 1]]);
        let q_raw = i16::from_be_bytes([payload[off + 2], payload[off + 3]]);
        out.push((i_raw as f32 * SCALE, q_raw as f32 * SCALE));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test 1: roundtrip encode/decode per tutti i valori (trx, vfo) validi ──

    #[test]
    fn encode_decode_roundtrip() {
        for trx in 0u8..=15 {
            for vfo in 0u8..=15 {
                let ssrc = ssrc_encode(trx, vfo);
                let decoded = ssrc_decode(ssrc);
                assert_eq!(
                    decoded,
                    Some((trx, vfo)),
                    "roundtrip fallito per trx={trx} vfo={vfo}: ssrc={ssrc:#010x}"
                );
            }
        }
    }

    // ── Test 2: SSRC con prefix diverso viene rifiutato ──

    #[test]
    fn decode_rejects_foreign_ssrc() {
        assert_eq!(ssrc_decode(0xDEAD_BEEF), None);
        assert_eq!(ssrc_decode(0x0000_0000), None);
        assert_eq!(ssrc_decode(0xFFFF_FFFF), None);
        // SSRC_PREFIX + 1 byte sbagliato
        assert_eq!(ssrc_decode(0x7C11_0000), None);
    }

    // ── Test 3: get_or_insert è idempotente ──

    #[test]
    fn get_or_insert_idempotent() {
        let mut table = SsrcTable::new();
        let ssrc_a = table.get_or_insert(3, 1).ssrc;
        let ssrc_b = table.get_or_insert(3, 1).ssrc;
        assert_eq!(ssrc_a, ssrc_b, "SSRC deve essere lo stesso alla seconda chiamata");
        assert_eq!(table.len(), 1, "table deve avere un solo elemento");
    }

    // ── Test 4: update_from_status aggiorna freq, lascia samprate=0 ──

    #[test]
    fn update_from_status_partial() {
        let mut table = SsrcTable::new();
        let ssrc = ssrc_encode(0, 0);

        let fields = vec![
            TlvField {
                tag: StatusType::OUTPUT_SSRC as u8,
                value: TlvValue::Int(ssrc as u64),
            },
            TlvField {
                tag: StatusType::RADIO_FREQUENCY as u8,
                value: TlvValue::Double(14_074_000.0),
            },
        ];

        table.update_from_status(&fields);

        assert_eq!(table.len(), 1);
        let ch = table.get(ssrc).expect("il canale deve esistere dopo update");
        assert_eq!(ch.freq_hz, 14_074_000, "freq_hz deve essere aggiornata");
        assert_eq!(ch.samprate, 0, "samprate deve restare 0 (non presente nei fields)");
        assert!(
            !ch.created,
            "update_from_status NON deve toccare `created` — solo mark_created lo fa"
        );
    }

    #[test]
    fn update_from_status_does_not_imply_created() {
        // Anche con tutti i field popolati, update_from_status non deve mai
        // settare created=true. Solo mark_created() (chiamata esplicita dopo
        // un nostro PRESET) ha quel diritto.
        let mut table = SsrcTable::new();
        let ssrc = ssrc_encode(0, 0);
        let fields = vec![
            TlvField {
                tag: StatusType::OUTPUT_SSRC as u8,
                value: TlvValue::Int(ssrc as u64),
            },
            TlvField {
                tag: StatusType::RADIO_FREQUENCY as u8,
                value: TlvValue::Double(14_074_000.0),
            },
            TlvField {
                tag: StatusType::OUTPUT_SAMPRATE as u8,
                value: TlvValue::Int(48_000),
            },
        ];
        table.update_from_status(&fields);
        let ch = table.get(ssrc).expect("canale presente");
        assert!(!ch.created);
        // mark_created esplicito → created passa a true
        table.mark_created(ssrc);
        let ch = table.get(ssrc).unwrap();
        assert!(ch.created);
    }

    // ── Test 5: SSRC senza il nostro prefix viene ignorato ──

    #[test]
    fn update_from_status_ignores_foreign() {
        let mut table = SsrcTable::new();

        let fields = vec![
            TlvField {
                tag: StatusType::OUTPUT_SSRC as u8,
                value: TlvValue::Int(0xDEAD_BEEF),
            },
            TlvField {
                tag: StatusType::RADIO_FREQUENCY as u8,
                value: TlvValue::Double(7_074_000.0),
            },
        ];

        table.update_from_status(&fields);
        assert!(table.is_empty(), "table deve restare vuota per SSRC estranei");
    }

    // ── Test: decode S16BE stereo ──

    #[test]
    fn decode_s16be_stereo_basic() {
        // Due coppie (I,Q) = 8 byte
        // I=+1 (0x0001), Q=-1 (0xFFFF), I=+16384 (0x4000), Q=-16384 (0xC000)
        let payload: Vec<u8> = vec![
            0x00, 0x01, 0xFF, 0xFF,
            0x40, 0x00, 0xC0, 0x00,
        ];
        let samples = decode_s16be_stereo(&payload);
        assert_eq!(samples.len(), 2);
        // 1/32768 ≈ 3.05e-5
        assert!((samples[0].0 - (1.0 / 32768.0)).abs() < 1e-9);
        assert!((samples[0].1 - (-1.0 / 32768.0)).abs() < 1e-9);
        assert!((samples[1].0 - 0.5).abs() < 1e-9);
        assert!((samples[1].1 - (-0.5)).abs() < 1e-9);
    }

    #[test]
    fn decode_s16be_stereo_full_scale() {
        // i16::MIN → -1.0 (esatto), i16::MAX → +0.99997
        let payload: Vec<u8> = vec![
            0x80, 0x00, 0x7F, 0xFF, // I=-32768, Q=+32767
        ];
        let samples = decode_s16be_stereo(&payload);
        assert_eq!(samples.len(), 1);
        assert!((samples[0].0 - (-1.0)).abs() < 1e-9);
        assert!((samples[0].1 - (32767.0 / 32768.0)).abs() < 1e-9);
    }

    #[test]
    fn update_from_status_extracts_data_group_ipv4() {
        let mut table = SsrcTable::new();
        let ssrc = ssrc_encode(1, 0);
        // sockaddr_in wire: [239, 22, 92, 109, 0x13, 0x88] → 239.22.92.109:5000
        let sockaddr_in = vec![239u8, 22, 92, 109, 0x13, 0x88];
        let fields = vec![
            TlvField {
                tag: StatusType::OUTPUT_SSRC as u8,
                value: TlvValue::Int(ssrc as u64),
            },
            TlvField {
                tag: StatusType::OUTPUT_DATA_DEST_SOCKET as u8,
                value: TlvValue::Bytes(sockaddr_in),
            },
        ];
        table.update_from_status(&fields);
        let ch = table.get(ssrc).expect("canale presente");
        assert_eq!(ch.data_group, Some(Ipv4Addr::new(239, 22, 92, 109)));
    }

    #[test]
    fn update_from_status_data_group_anomalous_length_ignored() {
        let mut table = SsrcTable::new();
        let ssrc = ssrc_encode(0, 0);
        let fields = vec![
            TlvField {
                tag: StatusType::OUTPUT_SSRC as u8,
                value: TlvValue::Int(ssrc as u64),
            },
            TlvField {
                tag: StatusType::OUTPUT_DATA_DEST_SOCKET as u8,
                // 5 byte: né AF_INET (6) né AF_INET6 (18) → ignorato con warn
                value: TlvValue::Bytes(vec![1, 2, 3, 4, 5]),
            },
        ];
        table.update_from_status(&fields);
        let ch = table.get(ssrc).expect("canale presente");
        assert_eq!(ch.data_group, None, "lunghezza anomala non deve popolare data_group");
    }

    #[test]
    fn decode_s16be_stereo_drops_partial() {
        // 6 byte = 1 coppia (4) + 2 byte residui (drop)
        let payload: Vec<u8> = vec![
            0x40, 0x00, 0xC0, 0x00, // I=+16384, Q=-16384 → ±0.5
            0x12, 0x34,             // residuo, scartato
        ];
        let samples = decode_s16be_stereo(&payload);
        assert_eq!(samples.len(), 1);
        assert!((samples[0].0 - 0.5).abs() < 1e-9);
        assert!((samples[0].1 - (-0.5)).abs() < 1e-9);
    }

    // ── Test 6: nessun campo OUTPUT_SSRC → nessuna modifica alla table ──

    #[test]
    fn update_from_status_no_ssrc_field() {
        let mut table = SsrcTable::new();

        // Solo RADIO_FREQUENCY, niente OUTPUT_SSRC
        let fields = vec![TlvField {
            tag: StatusType::RADIO_FREQUENCY as u8,
            value: TlvValue::Double(3_573_000.0),
        }];

        table.update_from_status(&fields);
        assert!(table.is_empty(), "table deve restare vuota se manca OUTPUT_SSRC");
    }
}
