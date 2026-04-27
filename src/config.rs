use clap::Parser;
use std::net::IpAddr;
use std::path::PathBuf;

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

    /// IQ sample rate iniziale offerto ai client TCI nell'handshake.
    /// I client (es. SparkSDR) possono cambiarlo runtime con il comando
    /// TCI `IQ_SAMPLERATE:<rate>;`; il bridge selezionerà automaticamente
    /// il preset corrispondente da --preset-map.
    /// Default 48000.
    #[arg(long, default_value_t = 48000)]
    pub iq_samplerate: u32,

    /// Mapping samplerate→preset, formato "rate:preset,rate:preset,...".
    /// I preset devono esistere in /usr/local/share/ka9q-radio/presets.conf
    /// con `demod = linear` e `samprate` coincidente.
    /// Default: 12000:iq,48000:iq48,96000:iq96
    #[arg(long, default_value = "12000:iq,48000:iq48,96000:iq96")]
    pub preset_map: String,

    /// Preset di fallback se il client chiede un samplerate non mappato.
    #[arg(long, default_value = "iq48")]
    pub default_preset: String,

    /// Numero massimo di receiver TCI esposti
    #[arg(long, default_value_t = 2)]
    pub max_trx: u8,

    /// Intervallo POLL del control plane verso radiod (secondi)
    #[arg(long, default_value_t = 5)]
    pub poll_interval_secs: u64,

    /// Path opzionale a un file YAML che imposta lo stato iniziale dei TRX
    /// (frequenze VFO, modulazione). Se non specificato, vengono usati i
    /// default hardcoded (FT8 20m USB).
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,
}
