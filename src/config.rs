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
    /// Default 12000 = sample rate del preset `iq` di ka9q-radio.
    /// Cambiare richiede resampling lato bridge (non implementato).
    #[arg(long, default_value_t = 12000)]
    pub iq_samplerate: u32,

    /// Numero massimo di receiver TCI esposti
    #[arg(long, default_value_t = 2)]
    pub max_trx: u8,

    /// Intervallo POLL del control plane verso radiod (secondi)
    #[arg(long, default_value_t = 5)]
    pub poll_interval_secs: u64,
}
