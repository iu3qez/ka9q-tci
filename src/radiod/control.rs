//! Control plane verso radiod: invio POLL/COMMAND, ricezione STATUS.
//!
//! Il gruppo status/command (es. hf.local → 239.135.38.120:5006) è usato
//! sia per ricevere STATUS (heartbeat + risposte a POLL) sia per inviare
//! COMMAND (tune, set samprate, ecc.).

use super::multicast::{join_multicast, send_multicast};
use super::tlv::{self, PktType, StatusType, TlvField, TlvValue};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, trace, warn};

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLV decode: {0}")]
    Tlv(#[from] tlv::TlvError),
    #[error("DNS resolve failed for {0}")]
    Resolve(String),
    #[error("multicast: {0}")]
    Mcast(#[from] super::multicast::McastError),
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

// ── Tipi pubblici ───────────────────────────────────────────────────

/// Un pacchetto STATUS decodificato ricevuto da radiod.
pub struct StatusPacket {
    /// SSRC estratto dal campo OUTPUT_SSRC, se presente.
    pub ssrc: Option<u32>,
    /// Tutti i campi TLV decodificati.
    pub fields: Vec<TlvField>,
}

/// Client per il control plane di radiod.
///
/// Tiene un socket RX iscritto al gruppo multicast e uno TX separato
/// per inviare COMMAND/POLL. Il task RX gira in background e invia
/// i pacchetti STATUS sul canale `status_rx`.
pub struct ControlClient {
    send_sock: Arc<UdpSocket>,
    dest: SocketAddrV4,
    status_rx: Option<mpsc::Receiver<StatusPacket>>,
    cmd_tag: AtomicU32,
    _recv_task: JoinHandle<()>,
}

impl ControlClient {
    /// Risolve `status_name` via mDNS, joina il gruppo control :5006 in RX,
    /// apre un socket TX separato (TTL=1), spawna task RX che decodifica
    /// STATUS e li invia su mpsc bound 64.
    pub async fn connect(
        status_name: &str,
        iface: Option<Ipv4Addr>,
    ) -> Result<Self, ControlError> {
        const CONTROL_PORT: u16 = 5006;
        const TX_TTL: u32 = 1;
        const CHAN_BOUND: usize = 64;

        // Risolve il nome mDNS nel gruppo multicast
        let group = resolve_mdns(status_name)?;

        // Socket RX: join multicast
        let recv_sock = join_multicast(group, CONTROL_PORT, iface).await?;

        // Socket TX: separato, TTL=1
        let (tx_sock, dest) = send_multicast(group, CONTROL_PORT, iface, TX_TTL).await?;
        let send_sock = Arc::new(tx_sock);

        // Canale status
        let (status_tx, status_rx) = mpsc::channel::<StatusPacket>(CHAN_BOUND);

        // Task RX in background
        let recv_task = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let n = match recv_sock.recv(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("recv_task: recv error: {e}");
                        continue;
                    }
                };

                // Decodifica pacchetto
                let (pkt_type, fields) = match tlv::decode_packet(&buf[..n]) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("recv_task: TLV decode error: {e}");
                        continue;
                    }
                };

                // Filtra solo STATUS
                if pkt_type != PktType::Status {
                    continue;
                }

                trace!("STATUS ricevuto: {} campi", fields.len());

                // Cerca OUTPUT_SSRC
                let ssrc = fields.iter().find_map(|f| {
                    if f.tag == StatusType::OUTPUT_SSRC as u8 {
                        if let TlvValue::Int(v) = f.value {
                            return Some(v as u32);
                        }
                    }
                    None
                });

                let pkt = StatusPacket { ssrc, fields };

                match status_tx.try_send(pkt) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!("status channel full, dropping");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // Il receiver è stato droppato, usciamo silenziosamente
                        return;
                    }
                }
            }
        });

        info!("ControlClient connesso al gruppo {group}:{CONTROL_PORT}");

        Ok(Self {
            send_sock,
            dest,
            status_rx: Some(status_rx),
            cmd_tag: AtomicU32::new(1),
            _recv_task: recv_task,
        })
    }

    /// Estrae il receiver STATUS (one-shot, consuma il campo).
    pub fn take_status_rx(&mut self) -> Option<mpsc::Receiver<StatusPacket>> {
        self.status_rx.take()
    }

    /// Invia un COMMAND TLV con i campi forniti.
    /// COMMAND_TAG monotono crescente è anteposto automaticamente.
    pub async fn send_command(
        &self,
        fields: &[(StatusType, TlvValue)],
    ) -> Result<(), ControlError> {
        let tag = self.cmd_tag.fetch_add(1, Ordering::Relaxed);

        // Preponi COMMAND_TAG
        let mut all_fields = Vec::with_capacity(fields.len() + 1);
        all_fields.push((StatusType::COMMAND_TAG, TlvValue::Int(tag as u64)));
        all_fields.extend_from_slice(fields);

        let pkt = tlv::build_command(&all_fields);
        self.send_sock.send_to(&pkt, self.dest).await?;
        Ok(())
    }

    /// Invia un POLL: COMMAND con solo COMMAND_TAG.
    /// radiod risponde con STATUS per tutti gli SSRC attivi.
    pub async fn poll(&self) -> Result<(), ControlError> {
        self.send_command(&[]).await?;
        debug!("POLL inviato a {}", self.dest);
        Ok(())
    }
}

// ── Test ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tlv::{build_command, decode_packet};

    /// Verifica il roundtrip build_command → decode_packet per COMMAND_TAG.
    #[tokio::test]
    async fn roundtrip_command_tag_via_control() {
        let pkt = build_command(&[(StatusType::COMMAND_TAG, TlvValue::Int(42))]);
        let (ptype, fields) = decode_packet(&pkt).unwrap();
        assert_eq!(ptype, PktType::Command);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].tag, StatusType::COMMAND_TAG as u8);
        match fields[0].value {
            TlvValue::Int(v) => assert_eq!(v, 42),
            _ => panic!("atteso Int(42)"),
        }
    }

    /// Verifica che resolve_mdns funzioni su un hostname di sistema valido.
    #[test]
    fn resolve_localhost() {
        let ip = resolve_mdns("localhost").unwrap();
        // può essere 127.0.0.1 oppure ::1 filtrata a V4
        assert!(ip.is_loopback(), "expected loopback, got {ip}");
    }

    /// Verifica che StatusPacket estragga correttamente l'SSRC da un pacchetto STATUS.
    #[test]
    fn status_packet_ssrc_extraction() {
        use bytes::BufMut;
        use bytes::BytesMut;

        // Costruiamo un pacchetto STATUS manuale con OUTPUT_SSRC = 0xCAFEBABE
        let mut pkt = BytesMut::new();
        pkt.put_u8(PktType::Status as u8);
        // OUTPUT_SSRC tag=21, valore 0xCAFEBABE (4 byte, big-endian)
        pkt.put_u8(StatusType::OUTPUT_SSRC as u8);
        pkt.put_u8(4); // length
        pkt.put_u32(0xCAFE_BABE);
        pkt.put_u8(0x00); // EOL

        let (ptype, fields) = decode_packet(&pkt).unwrap();
        assert_eq!(ptype, PktType::Status);

        let ssrc = fields.iter().find_map(|f| {
            if f.tag == StatusType::OUTPUT_SSRC as u8 {
                if let TlvValue::Int(v) = f.value {
                    return Some(v as u32);
                }
            }
            None
        });
        assert_eq!(ssrc, Some(0xCAFE_BABE));
    }
}
