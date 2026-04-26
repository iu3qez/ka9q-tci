use clap::Parser;
use std::net::IpAddr;

/// Bridge ka9q-radio IQ streams to a TCI WebSocket server.
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
    /// mDNS name of the radiod status/control group (e.g. "hf.local")
    #[arg(short, long, default_value = "hf.local")]
    pub status_name: String,

    /// WebSocket bind address (ip:port)
    #[arg(short, long, default_value = "0.0.0.0:40001")]
    pub bind_addr: String,

    /// Network interface IP for multicast join (default: INADDR_ANY).
    /// Impostare esplicitamente su host multi-homed (es. 192.168.1.228).
    #[arg(short = 'i', long)]
    pub mcast_iface: Option<IpAddr>,

    /// IQ sample rate offerto ai client TCI.
    /// Deve coincidere con `samprate` del preset usato (vedi --preset),
    /// altrimenti il client TCI riceve dati a velocità sbagliata.
    /// Preset `iq48` → 48000, `iq96` → 96000, `iq` → 12000.
    #[arg(long, default_value_t = 48000)]
    pub iq_samplerate: u32,

    /// Preset ka9q-radio usato per i canali creati dal bridge.
    /// Deve esistere in /usr/local/share/ka9q-radio/presets.conf con
    /// `demod = linear` (IQ raw). Sample rate del preset deve combaciare
    /// con --iq-samplerate.
    #[arg(long, default_value = "iq48")]
    pub preset: String,

    /// Numero massimo di receiver TCI esposti
    #[arg(long, default_value_t = 2)]
    pub max_trx: u8,

    /// Intervallo POLL del control plane verso radiod (secondi)
    #[arg(long, default_value_t = 5)]
    pub poll_interval_secs: u64,
}
