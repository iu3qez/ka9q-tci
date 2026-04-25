//! Codec TLV per il protocollo di controllo di ka9q-radio (status.h / status.c).
//!
//! Formato wire:
//!   byte 0     — pkt_type: STATUS=0, CMD=1
//!   byte 1..N  — sequenza di TLV, terminata da EOL (0x00)
//!
//! Ogni TLV:
//!   1 byte  type  (enum StatusType)
//!   1+ byte length (se bit 7 set → i 7 bit bassi indicano quanti byte
//!                    seguenti codificano la lunghezza in big-endian)
//!   N byte  value
//!
//! Interi: big-endian, leading zero bytes soppressi. 0 → length=0.
//! Float:  reinterpret come u32, poi big-endian (4 byte).
//! Double: reinterpret come u64, poi big-endian (8 byte).

use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;

// ── Packet types ────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PktType {
    Status = 0,
    Command = 1,
}

impl TryFrom<u8> for PktType {
    type Error = TlvError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::Status),
            1 => Ok(Self::Command),
            _ => Err(TlvError::UnknownPktType(v)),
        }
    }
}

// ── TLV type IDs (sottoinsieme usato dal bridge) ───────────────────
/// Valori da ka9q-radio src/status.h (enum status_type, ordinato).
/// SOURCE OF TRUTH: `../ka9q-radio/src/status.h`. Verifica con:
///   awk '/enum status_type/,/^};/' src/status.h | grep -E "^\\s*[A-Z]"
/// I valori sono indici nell'enum C, mai aggiunti/rimossi nel mezzo
/// (vedi commento in status.h: "I try not to delete or rearrange").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[allow(non_camel_case_types)]
pub enum StatusType {
    EOL = 0,
    COMMAND_TAG = 1,
    INPUT_SAMPRATE = 10,
    OUTPUT_DATA_SOURCE_SOCKET = 16,
    OUTPUT_DATA_DEST_SOCKET = 17,
    OUTPUT_SSRC = 18,
    OUTPUT_SAMPRATE = 20,
    RADIO_FREQUENCY = 33,
    FIRST_LO_FREQUENCY = 34,
    SECOND_LO_FREQUENCY = 35,
    SHIFT_FREQUENCY = 36,
    LOW_EDGE = 39,
    HIGH_EDGE = 40,
    DEMOD_TYPE = 48,
    OUTPUT_CHANNELS = 49,
    PRESET = 85,
    RTP_PT = 105,
    OUTPUT_ENCODING = 107,
}

/// Valori dell'enum `encoding` di ka9q-radio (`src/rtp.h`).
/// Usato come Int nei TLV `OUTPUT_ENCODING`.
#[allow(dead_code)]
#[repr(u64)]
pub enum Encoding {
    NoEncoding = 0,
    S16le = 1,
    S16be = 2,
    Opus = 3,
    /// Float32 little-endian — formato richiesto da TCI per i frame IQ.
    F32le = 4,
}

impl TryFrom<u8> for StatusType {
    type Error = TlvError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::EOL),
            1 => Ok(Self::COMMAND_TAG),
            10 => Ok(Self::INPUT_SAMPRATE),
            16 => Ok(Self::OUTPUT_DATA_SOURCE_SOCKET),
            17 => Ok(Self::OUTPUT_DATA_DEST_SOCKET),
            18 => Ok(Self::OUTPUT_SSRC),
            20 => Ok(Self::OUTPUT_SAMPRATE),
            33 => Ok(Self::RADIO_FREQUENCY),
            34 => Ok(Self::FIRST_LO_FREQUENCY),
            35 => Ok(Self::SECOND_LO_FREQUENCY),
            36 => Ok(Self::SHIFT_FREQUENCY),
            39 => Ok(Self::LOW_EDGE),
            40 => Ok(Self::HIGH_EDGE),
            48 => Ok(Self::DEMOD_TYPE),
            49 => Ok(Self::OUTPUT_CHANNELS),
            85 => Ok(Self::PRESET),
            105 => Ok(Self::RTP_PT),
            107 => Ok(Self::OUTPUT_ENCODING),
            _ => Err(TlvError::UnknownStatusType(v)),
        }
    }
}

// ── Errors ──────────────────────────────────────────────────────────
#[derive(Debug, Error)]
pub enum TlvError {
    #[error("unknown packet type: {0}")]
    UnknownPktType(u8),
    #[error("unknown status type: {0}")]
    UnknownStatusType(u8),
    #[error("unexpected end of buffer")]
    Truncated,
    #[error("length encoding too large ({0} meta-bytes)")]
    LengthTooLarge(u8),
}

// ── Decoded TLV value ───────────────────────────────────────────────
#[derive(Debug, Clone)]
pub enum TlvValue {
    Int(u64),
    Float(f32),
    Double(f64),
    Bytes(Vec<u8>),
}

/// Un singolo campo TLV decodificato.
#[derive(Debug, Clone)]
pub struct TlvField {
    pub tag: u8, // raw, per non perdere tipi sconosciuti
    pub value: TlvValue,
}

// ── Decode ──────────────────────────────────────────────────────────

/// Decodifica la lunghezza in formato ka9q (BER-like).
fn decode_length(buf: &mut &[u8]) -> Result<usize, TlvError> {
    if buf.is_empty() {
        return Err(TlvError::Truncated);
    }
    let first = buf[0];
    *buf = &buf[1..];
    if first & 0x80 == 0 {
        return Ok(first as usize);
    }
    let n_bytes = (first & 0x7F) as usize;
    if n_bytes > 4 || buf.len() < n_bytes {
        return Err(TlvError::LengthTooLarge(n_bytes as u8));
    }
    let mut len: usize = 0;
    for &b in &buf[..n_bytes] {
        len = (len << 8) | b as usize;
    }
    *buf = &buf[n_bytes..];
    Ok(len)
}

/// Decodifica un intero big-endian con leading zeros soppressi.
fn decode_int(data: &[u8]) -> u64 {
    let mut v: u64 = 0;
    for &b in data {
        v = (v << 8) | b as u64;
    }
    v
}

/// Decodifica un pacchetto STATUS/CMD in una lista di TlvField.
pub fn decode_packet(data: &[u8]) -> Result<(PktType, Vec<TlvField>), TlvError> {
    if data.is_empty() {
        return Err(TlvError::Truncated);
    }
    let pkt_type = PktType::try_from(data[0])?;
    let mut buf: &[u8] = &data[1..];
    let mut fields = Vec::new();

    while !buf.is_empty() {
        let tag = buf[0];
        buf = &buf[1..];
        if tag == 0 {
            break; // EOL
        }
        let len = decode_length(&mut buf)?;
        if buf.len() < len {
            return Err(TlvError::Truncated);
        }
        let val_bytes = &buf[..len];

        // Euristica tipo → valore
        let value = match StatusType::try_from(tag) {
            Ok(StatusType::RADIO_FREQUENCY
            | StatusType::FIRST_LO_FREQUENCY
            | StatusType::SECOND_LO_FREQUENCY
            | StatusType::SHIFT_FREQUENCY) => {
                // double (o float se ≤ 4 byte)
                if len == 8 {
                    TlvValue::Double(f64::from_bits(decode_int(val_bytes)))
                } else if len == 4 {
                    TlvValue::Float(f32::from_bits(decode_int(val_bytes) as u32))
                } else {
                    TlvValue::Int(decode_int(val_bytes))
                }
            }
            Ok(StatusType::LOW_EDGE | StatusType::HIGH_EDGE) => {
                if len <= 4 {
                    TlvValue::Float(f32::from_bits(decode_int(val_bytes) as u32))
                } else {
                    TlvValue::Double(f64::from_bits(decode_int(val_bytes)))
                }
            }
            Ok(StatusType::PRESET
            | StatusType::OUTPUT_DATA_SOURCE_SOCKET
            | StatusType::OUTPUT_DATA_DEST_SOCKET) => TlvValue::Bytes(val_bytes.to_vec()),
            _ => TlvValue::Int(decode_int(val_bytes)),
        };

        buf = &buf[len..];
        fields.push(TlvField { tag, value });
    }
    Ok((pkt_type, fields))
}

// ── Encode ──────────────────────────────────────────────────────────

/// Codifica la lunghezza in formato ka9q (BER-like).
fn encode_length(buf: &mut BytesMut, len: usize) {
    if len < 0x80 {
        buf.put_u8(len as u8);
    } else if len <= 0xFF {
        buf.put_u8(0x81);
        buf.put_u8(len as u8);
    } else {
        buf.put_u8(0x82);
        buf.put_u16(len as u16);
    }
}

/// Codifica un intero big-endian sopprimendo i leading zero bytes.
fn encode_int(buf: &mut BytesMut, v: u64) {
    if v == 0 {
        encode_length(buf, 0);
        return;
    }
    let be = v.to_be_bytes();
    let start = be.iter().position(|&b| b != 0).unwrap_or(7);
    let data = &be[start..];
    encode_length(buf, data.len());
    buf.put_slice(data);
}

/// Costruisce un pacchetto COMMAND con i campi dati.
pub fn build_command(fields: &[(StatusType, TlvValue)]) -> BytesMut {
    let mut buf = BytesMut::with_capacity(256);
    buf.put_u8(PktType::Command as u8);
    for (tag, val) in fields {
        buf.put_u8(*tag as u8);
        match val {
            TlvValue::Int(v) => encode_int(&mut buf, *v),
            TlvValue::Float(f) => {
                let bits = f.to_bits().to_be_bytes();
                encode_length(&mut buf, 4);
                buf.put_slice(&bits);
            }
            TlvValue::Double(d) => {
                let bits = d.to_bits().to_be_bytes();
                encode_length(&mut buf, 8);
                buf.put_slice(&bits);
            }
            TlvValue::Bytes(b) => {
                encode_length(&mut buf, b.len());
                buf.put_slice(b);
            }
        }
    }
    buf.put_u8(StatusType::EOL as u8); // terminatore
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_command_tag() {
        let pkt = build_command(&[
            (StatusType::COMMAND_TAG, TlvValue::Int(0xDEAD)),
            (StatusType::RADIO_FREQUENCY, TlvValue::Double(14_074_000.0)),
        ]);
        let (ptype, fields) = decode_packet(&pkt).unwrap();
        assert_eq!(ptype, PktType::Command);
        assert_eq!(fields.len(), 2);
        match &fields[0].value {
            TlvValue::Int(v) => assert_eq!(*v, 0xDEAD),
            _ => panic!("expected Int"),
        }
        match &fields[1].value {
            TlvValue::Double(v) => assert!((v - 14_074_000.0).abs() < 0.01),
            _ => panic!("expected Double"),
        }
    }
}
