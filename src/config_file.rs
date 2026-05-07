//! Configurazione caricata da file YAML.
//!
//! Due formati supportati:
//!
//! 1. **Multi-endpoint (preferito)**: lista di server TCI separati, uno per
//!    banda HF, ognuno sulla propria porta WebSocket. Aggira il limite
//!    empirico `TRX_COUNT ≤ 2` per server di alcuni client (es. SDC).
//!
//!    ```yaml
//!    endpoints:
//!      - port: 40001
//!        label: 40m
//!        iq_samplerate: 48000
//!        trx:
//!          - { freq: 7074000, modulation: USB }
//!      - port: 40002
//!        label: 20m
//!        trx:
//!          - { freq: 14074000, modulation: USB }
//!    ```
//!
//! 2. **Legacy (singolo endpoint)**: solo lista TRX, retrocompat con i
//!    config pre-multi-endpoint. Il bridge promuove la lista a un singolo
//!    `EndpointConfig` usando i flag CLI (`--bind-addr`, `--iq-samplerate`).
//!
//!    ```yaml
//!    trx:
//!      - { freq: 7074000, modulation: USB }
//!    ```
//!
//! Se entrambi i campi sono presenti, `endpoints` prevale e `trx` viene
//! ignorato (con warn al caller). Le opzioni CLI restano override globali.

use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("IO leggendo {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("YAML invalido in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
}

/// Configurazione di un singolo TRX caricata da YAML.
#[derive(Debug, Deserialize, Clone)]
pub struct TrxConfig {
    /// Frequenza iniziale del VFO A in Hz.
    pub freq: u64,
    /// Modulazione iniziale (es. "USB", "LSB", "CW"). Default "USB".
    #[serde(default = "default_modulation")]
    pub modulation: String,
    /// Frequenza iniziale del VFO B (default = stessa di `freq`).
    #[serde(default)]
    pub freq_b: Option<u64>,
}

fn default_modulation() -> String {
    "USB".to_string()
}

/// Configurazione di un singolo endpoint TCI (porta WS + lista TRX).
#[derive(Debug, Deserialize, Clone)]
pub struct EndpointConfig {
    /// Porta WebSocket dell'endpoint (es. 40001).
    pub port: u16,
    /// Etichetta umana, usata in logging e nel campo `device:` TCI.
    /// Se assente, default al numero di porta.
    #[serde(default)]
    pub label: Option<String>,
    /// IQ sample rate iniziale annunciato ai client di questo endpoint.
    /// Se assente, eredita il default globale (`--iq-samplerate`).
    #[serde(default)]
    pub iq_samplerate: Option<u32>,
    /// TRX esposti dall'endpoint. Almeno uno richiesto.
    pub trx: Vec<TrxConfig>,
}

/// Configurazione globale caricabile da YAML. Supporta due formati
/// (multi-endpoint e legacy single-list); usa
/// [`FileConfig::has_endpoints`] per scegliere il path di promozione
/// nel caller.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct FileConfig {
    /// Forma legacy: lista TRX flat (singolo endpoint implicito).
    /// Lunghezza ignorata se eccede `--max-trx`.
    #[serde(default)]
    pub trx: Vec<TrxConfig>,
    /// Forma multi-endpoint: N server TCI distinti.
    /// Se non vuota, ha precedenza su `trx`.
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
}

impl FileConfig {
    /// `true` se l'utente ha definito `endpoints:` (forma nuova).
    /// Quando false, il caller deve promuovere `self.trx` a un singolo
    /// `EndpointConfig` usando i default da CLI.
    pub fn has_endpoints(&self) -> bool {
        !self.endpoints.is_empty()
    }
}

impl FileConfig {
    /// Carica un file YAML. Errore se il file esiste ma è malformato.
    /// Se il path non esiste, restituisce `Ok(None)` (= "uso i default").
    pub fn load(path: &Path) -> Result<Option<Self>, ConfigError> {
        if !path.exists() {
            return Ok(None);
        }
        let s = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let cfg: Self = serde_yaml::from_str(&s).map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;
        Ok(Some(cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_yaml() {
        let yaml = r#"
trx:
  - freq: 7074000
    modulation: USB
  - freq: 14074000
    modulation: LSB
    freq_b: 14076000
"#;
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.trx.len(), 2);
        assert_eq!(cfg.trx[0].freq, 7_074_000);
        assert_eq!(cfg.trx[0].modulation, "USB");
        assert_eq!(cfg.trx[0].freq_b, None);
        assert_eq!(cfg.trx[1].freq, 14_074_000);
        assert_eq!(cfg.trx[1].modulation, "LSB");
        assert_eq!(cfg.trx[1].freq_b, Some(14_076_000));
    }

    #[test]
    fn parse_minimal_yaml() {
        // solo freq, modulation default
        let yaml = "trx:\n  - freq: 7074000\n";
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.trx.len(), 1);
        assert_eq!(cfg.trx[0].modulation, "USB");
    }

    #[test]
    fn parse_empty_yaml_means_defaults() {
        let cfg: FileConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.trx.is_empty());
    }

    #[test]
    fn load_missing_file_returns_none() {
        let path = Path::new("/tmp/__definitely_does_not_exist_ka9q.yaml");
        let r = FileConfig::load(path).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn parse_multi_endpoint_yaml() {
        let yaml = r#"
endpoints:
  - port: 40001
    label: 40m
    iq_samplerate: 48000
    trx:
      - { freq: 7074000, modulation: USB }
  - port: 40002
    trx:
      - { freq: 14074000 }
"#;
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.has_endpoints());
        assert_eq!(cfg.endpoints.len(), 2);
        let ep0 = &cfg.endpoints[0];
        assert_eq!(ep0.port, 40001);
        assert_eq!(ep0.label.as_deref(), Some("40m"));
        assert_eq!(ep0.iq_samplerate, Some(48000));
        assert_eq!(ep0.trx.len(), 1);
        assert_eq!(ep0.trx[0].freq, 7_074_000);
        let ep1 = &cfg.endpoints[1];
        assert_eq!(ep1.port, 40002);
        assert!(ep1.label.is_none());
        assert!(ep1.iq_samplerate.is_none());
        assert_eq!(ep1.trx[0].modulation, "USB"); // default
    }

    #[test]
    fn parse_legacy_yaml_no_endpoints() {
        // Forma vecchia: solo `trx` flat. has_endpoints() = false → il
        // caller dovrà promuovere a singolo endpoint.
        let yaml = "trx:\n  - freq: 7074000\n";
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.has_endpoints());
        assert_eq!(cfg.trx.len(), 1);
        assert!(cfg.endpoints.is_empty());
    }

    #[test]
    fn parse_both_present_endpoints_wins() {
        // Se l'utente specifica entrambi (errore di config), `endpoints`
        // prevale: has_endpoints() true e il caller userà quelli.
        let yaml = r#"
trx:
  - freq: 7074000
endpoints:
  - port: 40001
    trx:
      - { freq: 14074000 }
"#;
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.has_endpoints());
        assert_eq!(cfg.endpoints.len(), 1);
        // `trx` flat resta parsato ma il caller lo ignorerà.
        assert_eq!(cfg.trx.len(), 1);
    }

    #[test]
    fn parse_empty_endpoints_falls_back_to_legacy() {
        // `endpoints: []` esplicito vuoto = stesso effetto di campo assente:
        // has_endpoints() false, caller usa il path legacy.
        let yaml = "endpoints: []\ntrx:\n  - freq: 7074000\n";
        let cfg: FileConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.has_endpoints());
        assert_eq!(cfg.trx.len(), 1);
    }

    #[test]
    fn parse_endpoint_requires_port_and_trx() {
        // Senza port, deserialize fallisce.
        let yaml = "endpoints:\n  - label: foo\n    trx:\n      - { freq: 1 }\n";
        assert!(serde_yaml::from_str::<FileConfig>(yaml).is_err());
        // Senza trx, deserialize fallisce.
        let yaml = "endpoints:\n  - port: 40001\n";
        assert!(serde_yaml::from_str::<FileConfig>(yaml).is_err());
    }
}
