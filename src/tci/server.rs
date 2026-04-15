//! WebSocket server TCI.
//!
//! Ascolta su ws://bind_addr/ e gestisce la connessione di ogni client TCI.
//! Al connect invia la sequenza di handshake, poi smista comandi text e
//! invia frame IQ binari.

// TODO: implementare
//   - accept loop con tokio-tungstenite
//   - handshake sequence (protocol, device, vfo_limits, ready)
//   - dispatch comandi → bridge
//   - invio IQ frames dai canali attivi
