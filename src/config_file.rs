//! Configurazione caricata da file YAML.
//!
//! Il file è opzionale. Se assente o se i campi mancano, valgono i default
//! hardcoded del bridge (v. `TrxState::default`). Le opzioni CLI hanno
//! comunque la precedenza sul file (vedi `main.rs`).
//!
//! Esempio minimo (`config.yaml`):
//!
//! ```yaml
//! trx:
//!   - freq: 7074000      # primo RX, VFO A — FT8 20 m
//!     modulation: USB
//!   - freq: 14074000     # secondo RX, VFO A
//!     modulation: USB
//! ```

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

/// Configurazione globale caricabile da YAML.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct FileConfig {
    /// Lista di TRX. Lunghezza ignorata se eccede `--max-trx`.
    /// Se vuota o assente, usa i default hardcoded.
    #[serde(default)]
    pub trx: Vec<TrxConfig>,
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
}
