//! Control plane verso radiod: invio POLL/COMMAND, ricezione STATUS.
//!
//! Il gruppo status/command (es. hf.local → 239.135.38.120:5006) è usato
//! sia per ricevere STATUS (heartbeat + risposte a POLL) sia per inviare
//! COMMAND (tune, set samprate, ecc.).

use super::tlv::{self, PktType, StatusType, TlvField, TlvValue};
use std::net::Ipv4Addr;
use thiserror::Error;
use tokio::net::UdpSocket;
use tracing::{debug, trace, warn};

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLV decode: {0}")]
    Tlv(#[from] tlv::TlvError),
    #[error("DNS resolve failed for {0}")]
    Resolve(String),
}

/// Risolve un nome mDNS in Ipv4Addr (usa getaddrinfo di sistema).
pub fn resolve_mdns(name: &str) -> Result<Ipv4Addr, ControlError> {
    use std::net::ToSocketAddrs;
    let addr_str = format!("{name}:0");
    let mut addrs = addr_str
        .to_socket_addrs()
        .map_err(|_| ControlError::Resolve(name.to_string()))?;
    addrs
        .find_map(|a| match a {
            std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
            _ => None,
        })
        .ok_or_else(|| ControlError::Resolve(name.to_string()))
}

// TODO: struct ControlClient con metodi per:
//   - poll_all() → Vec<TlvField> per ogni SSRC attivo
//   - set_frequency(ssrc, freq_hz)
//   - create_channel(ssrc, preset, freq_hz, samprate)
//   - task di ascolto STATUS in background (tokio::spawn)
