//! Parser per header RTP (RFC 3550) usato da ka9q-radio.
//!
//! Wire format (big-endian):
//!   0                   1                   2                   3
//!   0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//!  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!  |V=2|P|X| CC    |M| PT          |      sequence number          |
//!  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!  |                           timestamp                           |
//!  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!  |                             SSRC                              |
//!  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

use thiserror::Error;

pub const RTP_HEADER_MIN: usize = 12;

#[derive(Debug, Error)]
pub enum RtpError {
    #[error("packet too short ({0} bytes)")]
    TooShort(usize),
    #[error("RTP version {0}, expected 2")]
    BadVersion(u8),
}

#[derive(Debug, Clone)]
pub struct RtpHeader {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub csrc_count: u8,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    /// Header size in byte, inclusi CSRC ed eventuale estensione.
    pub fn header_len(&self) -> usize {
        RTP_HEADER_MIN + (self.csrc_count as usize) * 4
        // L'extension viene gestita a parte in parse()
    }
}

/// Parsa un pacchetto RTP e restituisce (header, offset del payload).
pub fn parse(data: &[u8]) -> Result<(RtpHeader, usize), RtpError> {
    if data.len() < RTP_HEADER_MIN {
        return Err(RtpError::TooShort(data.len()));
    }

    let b0 = data[0];
    let version = (b0 >> 6) & 0x03;
    if version != 2 {
        return Err(RtpError::BadVersion(version));
    }

    let padding = (b0 >> 5) & 1 != 0;
    let extension = (b0 >> 4) & 1 != 0;
    let csrc_count = b0 & 0x0F;

    let b1 = data[1];
    let marker = (b1 >> 7) & 1 != 0;
    let payload_type = b1 & 0x7F;

    let sequence = u16::from_be_bytes([data[2], data[3]]);
    let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    let mut offset = RTP_HEADER_MIN + (csrc_count as usize) * 4;

    // Skip RTP header extension if present
    if extension {
        if data.len() < offset + 4 {
            return Err(RtpError::TooShort(data.len()));
        }
        // 2 byte profile, 2 byte length (in 32-bit words)
        let ext_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        offset += 4 + ext_len * 4;
    }

    if offset > data.len() {
        return Err(RtpError::TooShort(data.len()));
    }

    let hdr = RtpHeader {
        version,
        padding,
        extension,
        csrc_count,
        marker,
        payload_type,
        sequence,
        timestamp,
        ssrc,
    };

    Ok((hdr, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        // V=2, no padding/ext/csrc, PT=97, seq=1, ts=160, ssrc=0x12345678
        let pkt: Vec<u8> = vec![
            0x80, 97, 0x00, 0x01, // V=2, PT=97, seq=1
            0x00, 0x00, 0x00, 0xA0, // timestamp=160
            0x12, 0x34, 0x56, 0x78, // ssrc
            // payload...
            0x01, 0x02, 0x03, 0x04,
        ];
        let (hdr, off) = parse(&pkt).unwrap();
        assert_eq!(hdr.version, 2);
        assert_eq!(hdr.payload_type, 97);
        assert_eq!(hdr.sequence, 1);
        assert_eq!(hdr.timestamp, 160);
        assert_eq!(hdr.ssrc, 0x12345678);
        assert_eq!(off, 12);
        assert_eq!(&pkt[off..], &[1, 2, 3, 4]);
    }
}
