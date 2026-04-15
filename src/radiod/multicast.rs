//! Utility per creare socket UDP multicast con join esplicito.
//!
//! Usa `socket2` per impostare `IP_ADD_MEMBERSHIP` e `IP_MULTICAST_IF`
//! in modo deterministico (niente INADDR_ANY su host multi-homed).

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::net::{Ipv4Addr, SocketAddrV4};
use thiserror::Error;
use tokio::net::UdpSocket;

#[derive(Debug, Error)]
pub enum McastError {
    #[error("socket setup: {0}")]
    Socket(#[from] std::io::Error),
}

/// Crea un `tokio::net::UdpSocket` iscritto al gruppo multicast dato.
///
/// - `group`: indirizzo multicast (es. 239.135.38.120)
/// - `port`: porta (es. 5004 data, 5006 control)
/// - `iface`: IP dell'interfaccia locale per il join; `None` = INADDR_ANY
pub async fn join_multicast(
    group: Ipv4Addr,
    port: u16,
    iface: Option<Ipv4Addr>,
) -> Result<UdpSocket, McastError> {
    let iface_addr = iface.unwrap_or(Ipv4Addr::UNSPECIFIED);

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    // Linux: SO_REUSEPORT consente più processi sullo stesso gruppo
    #[cfg(target_os = "linux")]
    socket.set_reuse_port(true)?;

    socket.set_nonblocking(true)?;

    // Bind alla porta multicast (su 0.0.0.0, il kernel filtra per membership)
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&SockAddr::from(bind_addr))?;

    // Join esplicito
    socket.join_multicast_v4(&group, &iface_addr)?;

    let std_socket: std::net::UdpSocket = socket.into();
    let tokio_socket = UdpSocket::from_std(std_socket)?;
    Ok(tokio_socket)
}

/// Crea un socket per inviare comandi al control plane multicast.
///
/// - `group`: indirizzo multicast destinazione
/// - `port`: porta (5006)
/// - `iface`: IP dell'interfaccia da cui trasmettere; `None` = default
/// - `ttl`: TTL multicast (1 = LAN)
pub async fn send_multicast(
    group: Ipv4Addr,
    port: u16,
    iface: Option<Ipv4Addr>,
    ttl: u32,
) -> Result<(UdpSocket, std::net::SocketAddrV4), McastError> {
    let iface_addr = iface.unwrap_or(Ipv4Addr::UNSPECIFIED);

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;
    socket.set_multicast_ttl_v4(ttl)?;
    socket.set_multicast_if_v4(&iface_addr)?;

    // Bind a porta effimera
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
    socket.bind(&SockAddr::from(bind_addr))?;

    let dest = SocketAddrV4::new(group, port);
    let std_socket: std::net::UdpSocket = socket.into();
    let tokio_socket = UdpSocket::from_std(std_socket)?;
    Ok((tokio_socket, dest))
}
