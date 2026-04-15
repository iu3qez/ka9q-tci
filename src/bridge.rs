//! Orchestratore: collega il lato radiod (multicast RTP + control TLV)
//! con il lato TCI (WebSocket server).
//!
//! Responsabilità:
//! - Gestire la mappa (trx, vfo) → SSRC
//! - Creare/distruggere canali radiod on-demand (preset iq)
//! - Inoltrare IQ da RTP → frame TCI binari ai client connessi
//! - Tradurre comandi TCI (VFO, DDS, ...) in COMMAND TLV verso radiod

// TODO: struct Bridge, metodo run(), canali tokio tra componenti
