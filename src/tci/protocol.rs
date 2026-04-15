//! Parser e serializzatore per messaggi TCI v2.0 (text + binary).
//!
//! Formato text: "COMMAND:arg1,arg2,...;"  (case-insensitive)
//! Formato binary: header 64 byte LE + payload float32 LE interleaved
//!
//! Riferimento: "TCI Protocol v2.0", Expert Electronics, 12 Jan 2024.
//! Copia locale: docs/TCI Protocol.pdf

use thiserror::Error;

// ── Errori ──────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum TciParseError {
    #[error("empty message")]
    Empty,
    #[error("missing semicolon terminator")]
    NoTerminator,
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("wrong arg count for {cmd}: expected {expected}, got {got}")]
    BadArgCount {
        cmd: String,
        expected: usize,
        got: usize,
    },
    #[error("invalid argument: {0}")]
    BadArg(String),
}

// ── Stream types (per header binario) ───────────────────────────────

/// Tipo di stream (campo `type` nell'header binario).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum StreamType {
    IqStream = 0,
    RxAudioStream = 1,
    TxAudioStream = 2,
    TxChrono = 3,
    LineoutStream = 4,
}

/// Tipo di campione (campo `format` nell'header binario).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SampleType {
    Int16 = 0,
    Int24 = 1,
    Int32 = 2,
    Float32 = 3,
}

// ── Comandi TCI text ────────────────────────────────────────────────

/// Messaggio TCI text parsato (client → server oppure bidirezionale).
///
/// Copre tutti i comandi della spec v2.0 rilevanti per un bridge IQ RX-only.
/// Comandi TX-only (DRIVE, TUNE_DRIVE, TRX set, TX_STREAM_*, ecc.) sono
/// raggruppati in `Other` per log/ack senza logica.
#[derive(Debug, Clone)]
pub enum TciCommand {
    // ── Bidirezionali — lettura (Read) ──
    /// VFO:trx,vfo;  (read request)
    VfoRead { trx: u32, vfo: u32 },
    /// DDS:trx;  (read request)
    DdsRead { trx: u32 },
    /// IF:trx,vfo;  (read request)
    IfRead { trx: u32, vfo: u32 },
    /// MODULATION:trx;
    ModulationRead { trx: u32 },
    /// RX_CHANNEL_ENABLE:trx,channel;
    RxChannelEnableRead { trx: u32, channel: u32 },
    /// RX_FILTER_BAND:trx;
    RxFilterBandRead { trx: u32 },

    // ── Bidirezionali — scrittura (Set) ──
    /// VFO:trx,vfo,freq_hz;
    Vfo { trx: u32, vfo: u32, freq_hz: u64 },
    /// DDS:trx,freq_hz;
    Dds { trx: u32, freq_hz: u64 },
    /// IF:trx,vfo,offset_hz;
    If { trx: u32, vfo: u32, offset_hz: i64 },
    /// MODULATION:trx,mode;
    Modulation { trx: u32, mode: String },
    /// RX_CHANNEL_ENABLE:trx,channel,bool;
    RxChannelEnable { trx: u32, channel: u32, enable: bool },
    /// RX_FILTER_BAND:trx,low_hz,high_hz;
    RxFilterBand { trx: u32, low: i32, high: i32 },

    // ── Unidirezionali (client → server) ──
    /// IQ_SAMPLERATE:rate;
    IqSamplerate { rate: u32 },
    /// IQ_START:trx;
    IqStart { trx: u32 },
    /// IQ_STOP:trx;
    IqStop { trx: u32 },
    /// AUDIO_SAMPLERATE:rate;
    AudioSamplerate { rate: u32 },
    /// AUDIO_START:trx;
    AudioStart { trx: u32 },
    /// AUDIO_STOP:trx;
    AudioStop { trx: u32 },
    /// SPOT:callsign,mode,freq,color,text;
    Spot {
        callsign: String,
        mode: String,
        freq_hz: u64,
        color: u32,
        text: String,
    },
    /// SPOT_DELETE:callsign;
    SpotDelete { callsign: String },
    /// SPOT_CLEAR;
    SpotClear,
    /// RX_SENSORS_ENABLE:bool[,interval_ms];
    RxSensorsEnable { enable: bool, interval_ms: Option<u32> },
    /// AUDIO_STREAM_SAMPLE_TYPE:type;
    AudioStreamSampleType { sample_type: String },
    /// AUDIO_STREAM_CHANNELS:n;
    AudioStreamChannels { channels: u32 },
    /// AUDIO_STREAM_SAMPLES:n;
    AudioStreamSamples { count: u32 },

    // ── Controlli globali ──
    /// START;
    Start,
    /// STOP;
    Stop,

    // ── CW (pass-through, log only) ──
    /// CW_MACROS:trx,text;
    CwMacros { trx: u32, text: String },
    /// CW_MACROS_STOP;
    CwMacrosStop,

    /// Comando riconosciuto ma non gestito attivamente (TX, volume, ecc.)
    /// Contiene il nome uppercase e gli argomenti raw.
    Other { name: String, args: Vec<String> },
}

// ── Parser ──────────────────────────────────────────────────────────

/// Parsa un messaggio TCI text.
///
/// Gestisce sia i comandi Set (3 args) sia Read (2 args) per VFO, DDS, IF, ecc.
pub fn parse_command(msg: &str) -> Result<TciCommand, TciParseError> {
    let msg = msg.trim();
    if msg.is_empty() {
        return Err(TciParseError::Empty);
    }
    let msg = msg.strip_suffix(';').ok_or(TciParseError::NoTerminator)?;

    let (cmd, args_str) = match msg.split_once(':') {
        Some((c, a)) => (c, Some(a)),
        None => (msg, None),
    };

    let cmd_upper = cmd.to_ascii_uppercase();
    let args: Vec<&str> = args_str
        .map(|s| s.split(',').collect())
        .unwrap_or_default();

    match cmd_upper.as_str() {
        // ── VFO ──
        "VFO" if args.len() == 3 => Ok(TciCommand::Vfo {
            trx: pu32(args[0])?,
            vfo: pu32(args[1])?,
            freq_hz: pu64(args[2])?,
        }),
        "VFO" if args.len() == 2 => Ok(TciCommand::VfoRead {
            trx: pu32(args[0])?,
            vfo: pu32(args[1])?,
        }),

        // ── DDS ──
        "DDS" if args.len() == 2 => Ok(TciCommand::Dds {
            trx: pu32(args[0])?,
            freq_hz: pu64(args[1])?,
        }),
        "DDS" if args.len() == 1 => Ok(TciCommand::DdsRead {
            trx: pu32(args[0])?,
        }),

        // ── IF ──
        "IF" if args.len() == 3 => Ok(TciCommand::If {
            trx: pu32(args[0])?,
            vfo: pu32(args[1])?,
            offset_hz: pi64(args[2])?,
        }),
        "IF" if args.len() == 2 => Ok(TciCommand::IfRead {
            trx: pu32(args[0])?,
            vfo: pu32(args[1])?,
        }),

        // ── MODULATION ──
        "MODULATION" if args.len() == 2 => Ok(TciCommand::Modulation {
            trx: pu32(args[0])?,
            mode: args[1].to_string(),
        }),
        "MODULATION" if args.len() == 1 => Ok(TciCommand::ModulationRead {
            trx: pu32(args[0])?,
        }),

        // ── RX_CHANNEL_ENABLE ──
        "RX_CHANNEL_ENABLE" if args.len() == 3 => Ok(TciCommand::RxChannelEnable {
            trx: pu32(args[0])?,
            channel: pu32(args[1])?,
            enable: pbool(args[2])?,
        }),
        "RX_CHANNEL_ENABLE" if args.len() == 2 => Ok(TciCommand::RxChannelEnableRead {
            trx: pu32(args[0])?,
            channel: pu32(args[1])?,
        }),

        // ── RX_FILTER_BAND ──
        "RX_FILTER_BAND" if args.len() == 3 => Ok(TciCommand::RxFilterBand {
            trx: pu32(args[0])?,
            low: pi32(args[1])?,
            high: pi32(args[2])?,
        }),
        "RX_FILTER_BAND" if args.len() == 1 => Ok(TciCommand::RxFilterBandRead {
            trx: pu32(args[0])?,
        }),

        // ── IQ / Audio stream control ──
        "IQ_SAMPLERATE" => {
            need("IQ_SAMPLERATE", &args, 1)?;
            Ok(TciCommand::IqSamplerate { rate: pu32(args[0])? })
        }
        "IQ_START" => {
            need("IQ_START", &args, 1)?;
            Ok(TciCommand::IqStart { trx: pu32(args[0])? })
        }
        "IQ_STOP" => {
            need("IQ_STOP", &args, 1)?;
            Ok(TciCommand::IqStop { trx: pu32(args[0])? })
        }
        "AUDIO_SAMPLERATE" => {
            need("AUDIO_SAMPLERATE", &args, 1)?;
            Ok(TciCommand::AudioSamplerate { rate: pu32(args[0])? })
        }
        "AUDIO_START" => {
            need("AUDIO_START", &args, 1)?;
            Ok(TciCommand::AudioStart { trx: pu32(args[0])? })
        }
        "AUDIO_STOP" => {
            need("AUDIO_STOP", &args, 1)?;
            Ok(TciCommand::AudioStop { trx: pu32(args[0])? })
        }
        "AUDIO_STREAM_SAMPLE_TYPE" => {
            need("AUDIO_STREAM_SAMPLE_TYPE", &args, 1)?;
            Ok(TciCommand::AudioStreamSampleType {
                sample_type: args[0].to_string(),
            })
        }
        "AUDIO_STREAM_CHANNELS" => {
            need("AUDIO_STREAM_CHANNELS", &args, 1)?;
            Ok(TciCommand::AudioStreamChannels { channels: pu32(args[0])? })
        }
        "AUDIO_STREAM_SAMPLES" => {
            need("AUDIO_STREAM_SAMPLES", &args, 1)?;
            Ok(TciCommand::AudioStreamSamples { count: pu32(args[0])? })
        }

        // ── Spots ──
        "SPOT" if args.len() == 5 => Ok(TciCommand::Spot {
            callsign: args[0].to_string(),
            mode: args[1].to_string(),
            freq_hz: pu64(args[2])?,
            color: pu32(args[3])?,
            text: args[4].to_string(),
        }),
        "SPOT_DELETE" => {
            need("SPOT_DELETE", &args, 1)?;
            Ok(TciCommand::SpotDelete { callsign: args[0].to_string() })
        }
        "SPOT_CLEAR" => Ok(TciCommand::SpotClear),

        // ── Sensors ──
        "RX_SENSORS_ENABLE" if args.len() >= 1 => Ok(TciCommand::RxSensorsEnable {
            enable: pbool(args[0])?,
            interval_ms: args.get(1).map(|s| pu32(s)).transpose()?,
        }),

        // ── CW ──
        "CW_MACROS" if args.len() >= 2 => Ok(TciCommand::CwMacros {
            trx: pu32(args[0])?,
            text: args[1..].join(","), // il testo può contenere virgole sostituite
        }),
        "CW_MACROS_STOP" => Ok(TciCommand::CwMacrosStop),

        // ── Globali ──
        "START" => Ok(TciCommand::Start),
        "STOP" => Ok(TciCommand::Stop),

        // ── Tutto il resto: accettato come Other (log, ack, no-op) ──
        _ => Ok(TciCommand::Other {
            name: cmd_upper,
            args: args.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

// ── Serializzazione ─────────────────────────────────────────────────

/// Formatta un messaggio TCI da inviare al client.
pub fn format_msg(cmd: &str, args: &[&str]) -> String {
    if args.is_empty() {
        format!("{cmd};")
    } else {
        format!("{}:{};", cmd, args.join(","))
    }
}

/// Sequenza di handshake inviata al client appena connesso.
///
/// Parametri configurabili dal bridge; i default riflettono un RX-only
/// a banda larga (ka9q-radio RX888).
pub fn handshake_messages(
    device_name: &str,
    trx_count: u8,
    channel_count: u8,
    vfo_min_hz: u64,
    vfo_max_hz: u64,
    if_min_hz: i64,
    if_max_hz: i64,
    iq_samplerate: u32,
    modulations: &[&str],
) -> Vec<String> {
    let mut msgs = Vec::new();
    msgs.push(format_msg("protocol", &["ExpertSDR3", "1.9"]));
    msgs.push(format_msg("device", &[device_name]));
    msgs.push(format_msg(
        "receive_only",
        &["true"],
    ));
    msgs.push(format_msg(
        "trx_count",
        &[&trx_count.to_string()],
    ));
    msgs.push(format_msg(
        "channels_count",
        &[&channel_count.to_string()],
    ));
    msgs.push(format_msg(
        "vfo_limits",
        &[&vfo_min_hz.to_string(), &vfo_max_hz.to_string()],
    ));
    msgs.push(format_msg(
        "if_limits",
        &[&if_min_hz.to_string(), &if_max_hz.to_string()],
    ));
    msgs.push(format_msg(
        "modulations_list",
        modulations,
    ));
    msgs.push(format_msg(
        "iq_samplerate",
        &[&iq_samplerate.to_string()],
    ));
    // TODO: inviare stato corrente (frequenze VFO, modulazione, ecc.)
    msgs.push(format_msg("ready", &[]));
    msgs
}

// ── Header binario stream (spec v2.0 §3.4) ─────────────────────────

/// Header stream binario TCI: 16 x u32 LE = 64 byte.
///
/// ```text
/// offset  field          descrizione
/// 0       receiver       numero TRX
/// 4       sample_rate    sample rate Hz
/// 8       format         SampleType enum (FLOAT32=3)
/// 12      codec          compressione (sempre 0)
/// 16      crc            checksum (sempre 0)
/// 20      length         numero di campioni (vedi nota)
/// 24      type           StreamType enum (IQ_STREAM=0)
/// 28      channels       numero di canali (2 per IQ)
/// 32..63  reserved       8 x u32 padding
/// ```
///
/// **Nota su `length`**: contiene il numero totale di campioni reali
/// nel payload. Per IQ (channels=2), il numero di campioni complessi
/// è `length / channels`. Il payload contiene `length` valori float32.
pub const STREAM_HEADER_SIZE: usize = 64;

/// Costruisce un frame IQ binario TCI.
///
/// `samples`: coppie (I, Q) float32.
/// `length` nel header = `samples.len() * 2` (campioni reali totali).
pub fn build_iq_frame(trx: u32, sample_rate: u32, samples: &[(f32, f32)]) -> Vec<u8> {
    let real_sample_count = (samples.len() * 2) as u32;
    let payload_bytes = real_sample_count as usize * 4;
    let mut buf = Vec::with_capacity(STREAM_HEADER_SIZE + payload_bytes);

    // Header: 16 x u32 LE
    buf.extend_from_slice(&trx.to_le_bytes());                          // [0]  receiver
    buf.extend_from_slice(&sample_rate.to_le_bytes());                  // [4]  sample_rate
    buf.extend_from_slice(&(SampleType::Float32 as u32).to_le_bytes()); // [8]  format
    buf.extend_from_slice(&0u32.to_le_bytes());                         // [12] codec
    buf.extend_from_slice(&0u32.to_le_bytes());                         // [16] crc
    buf.extend_from_slice(&real_sample_count.to_le_bytes());            // [20] length
    buf.extend_from_slice(&(StreamType::IqStream as u32).to_le_bytes()); // [24] type
    buf.extend_from_slice(&2u32.to_le_bytes());                         // [28] channels
    buf.extend_from_slice(&[0u8; 32]);                                  // [32..63] reserved (8 x u32)

    // Payload: interleaved I,Q,I,Q... float32 LE
    for &(i, q) in samples {
        buf.extend_from_slice(&i.to_le_bytes());
        buf.extend_from_slice(&q.to_le_bytes());
    }

    buf
}

// ── Helpers ─────────────────────────────────────────────────────────

fn need(cmd: &str, args: &[&str], expected: usize) -> Result<(), TciParseError> {
    if args.len() != expected {
        return Err(TciParseError::BadArgCount {
            cmd: cmd.to_string(),
            expected,
            got: args.len(),
        });
    }
    Ok(())
}

fn pu32(s: &str) -> Result<u32, TciParseError> {
    s.trim().parse().map_err(|_| TciParseError::BadArg(s.to_string()))
}

fn pu64(s: &str) -> Result<u64, TciParseError> {
    s.trim().parse().map_err(|_| TciParseError::BadArg(s.to_string()))
}

fn pi32(s: &str) -> Result<i32, TciParseError> {
    s.trim().parse().map_err(|_| TciParseError::BadArg(s.to_string()))
}

fn pi64(s: &str) -> Result<i64, TciParseError> {
    s.trim().parse().map_err(|_| TciParseError::BadArg(s.to_string()))
}

fn pbool(s: &str) -> Result<bool, TciParseError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(TciParseError::BadArg(s.to_string())),
    }
}

// ── Test ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vfo_set() {
        let cmd = parse_command("vfo:0,0,14074000;").unwrap();
        match cmd {
            TciCommand::Vfo { trx, vfo, freq_hz } => {
                assert_eq!(trx, 0);
                assert_eq!(vfo, 0);
                assert_eq!(freq_hz, 14_074_000);
            }
            _ => panic!("expected Vfo set"),
        }
    }

    #[test]
    fn parse_vfo_read() {
        let cmd = parse_command("VFO:0,1;").unwrap();
        assert!(matches!(cmd, TciCommand::VfoRead { trx: 0, vfo: 1 }));
    }

    #[test]
    fn parse_dds_set() {
        let cmd = parse_command("DDS:0,7100000;").unwrap();
        assert!(matches!(cmd, TciCommand::Dds { trx: 0, freq_hz: 7_100_000 }));
    }

    #[test]
    fn parse_dds_read() {
        let cmd = parse_command("DDS:0;").unwrap();
        assert!(matches!(cmd, TciCommand::DdsRead { trx: 0 }));
    }

    #[test]
    fn parse_if_set() {
        let cmd = parse_command("IF:0,1,-17550;").unwrap();
        match cmd {
            TciCommand::If { trx, vfo, offset_hz } => {
                assert_eq!(trx, 0);
                assert_eq!(vfo, 1);
                assert_eq!(offset_hz, -17550);
            }
            _ => panic!("expected If set"),
        }
    }

    #[test]
    fn parse_rx_filter_band() {
        let cmd = parse_command("RX_FILTER_BAND:1,-2900,-70;").unwrap();
        assert!(matches!(
            cmd,
            TciCommand::RxFilterBand { trx: 1, low: -2900, high: -70 }
        ));
    }

    #[test]
    fn parse_modulation_set() {
        let cmd = parse_command("MODULATION:0,LSB;").unwrap();
        match cmd {
            TciCommand::Modulation { trx, mode } => {
                assert_eq!(trx, 0);
                assert_eq!(mode, "LSB");
            }
            _ => panic!("expected Modulation"),
        }
    }

    #[test]
    fn parse_iq_start() {
        let cmd = parse_command("IQ_START:0;").unwrap();
        assert!(matches!(cmd, TciCommand::IqStart { trx: 0 }));
    }

    #[test]
    fn parse_start_stop() {
        assert!(matches!(parse_command("START;").unwrap(), TciCommand::Start));
        assert!(matches!(parse_command("stop;").unwrap(), TciCommand::Stop));
    }

    #[test]
    fn parse_unknown_goes_to_other() {
        let cmd = parse_command("VOLUME:-12;").unwrap();
        match cmd {
            TciCommand::Other { name, args } => {
                assert_eq!(name, "VOLUME");
                assert_eq!(args, vec!["-12"]);
            }
            _ => panic!("expected Other"),
        }
    }

    #[test]
    fn parse_cw_macros() {
        let cmd = parse_command("cw_macros:0,TU RA6LH 599;").unwrap();
        match cmd {
            TciCommand::CwMacros { trx, text } => {
                assert_eq!(trx, 0);
                assert_eq!(text, "TU RA6LH 599");
            }
            _ => panic!("expected CwMacros"),
        }
    }

    #[test]
    fn format_messages() {
        assert_eq!(format_msg("ready", &[]), "ready;");
        assert_eq!(
            format_msg("protocol", &["ExpertSDR3", "1.9"]),
            "protocol:ExpertSDR3,1.9;"
        );
        assert_eq!(
            format_msg("vfo", &["0", "0", "14074000"]),
            "vfo:0,0,14074000;"
        );
    }

    #[test]
    fn iq_frame_layout() {
        let samples = vec![(1.0f32, -1.0), (0.5, -0.5)];
        let frame = build_iq_frame(0, 48000, &samples);

        // Header = 64 byte + payload = 2 campioni * 2 canali * 4 byte = 16
        assert_eq!(frame.len(), STREAM_HEADER_SIZE + 16);

        // Verifica campi header
        let hdr = |off: usize| u32::from_le_bytes(frame[off..off + 4].try_into().unwrap());
        assert_eq!(hdr(0), 0);                             // receiver
        assert_eq!(hdr(4), 48000);                         // sample_rate
        assert_eq!(hdr(8), SampleType::Float32 as u32);    // format = 3
        assert_eq!(hdr(12), 0);                            // codec
        assert_eq!(hdr(16), 0);                            // crc
        assert_eq!(hdr(20), 4);                            // length = 2*2 real samples
        assert_eq!(hdr(24), StreamType::IqStream as u32);  // type = 0
        assert_eq!(hdr(28), 2);                            // channels

        // Verifica primo campione I
        let i0 = f32::from_le_bytes(frame[64..68].try_into().unwrap());
        assert_eq!(i0, 1.0);
    }

    #[test]
    fn handshake_sequence() {
        let msgs = handshake_messages(
            "ka9q-RX888",
            2,
            2,
            10_000,
            30_000_000,
            -24_000,
            24_000,
            48_000,
            &["AM", "LSB", "USB", "CW", "NFM"],
        );
        assert!(msgs.first().unwrap().starts_with("protocol:"));
        assert!(msgs.last().unwrap().starts_with("ready;"));
        assert!(msgs.iter().any(|m| m.starts_with("device:")));
        assert!(msgs.iter().any(|m| m.contains("receive_only:true")));
    }
}
