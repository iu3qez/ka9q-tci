# ka9q-tci

Bridge che espone i flussi IQ di **ka9q-radio** (by KA9Q, Phil Karn) come
server **TCI** (Expert Electronics *Transceiver Control Interface*), in
modo che client TCI su qualunque host della LAN (SparkSDR, SkimSrv,
CW Skimmer, Log4OM, …) possano:

- **consumare IQ** dei canali del ricevitore
- **controllarne l'accordo** (frequenza, eventuale shaping di banda) via
  `VFO:`, `DDS:`, `RX_ENABLE:`, `IQ_SAMPLERATE:` ecc.

Contesto di deploy primario: Raspberry Pi 5 con **RX888 MkII**, stesso host
dove gira `radiod`. Il binario deve restare LAN-generic: può girare su
qualsiasi macchina che possa raggiungere il gruppo multicast di `radiod`.

## Stack

- **Linguaggio: Rust** (scelta consolidata; anche didattica per l'utente).
- Async runtime: `tokio`.
- WebSocket server: `tokio-tungstenite`.
- UDP / multicast: `tokio::net::UdpSocket` + `socket2` per le opzioni
  `IP_ADD_MEMBERSHIP` / `IP_MULTICAST_IF`.
- Parsing RTP e TLV ka9q: moduli nostri, niente dipendenze esterne (il
  protocollo di controllo di `radiod` è semplice e documentato).

Non ci sono ancora `Cargo.toml` / sorgenti: greenfield.

## Network discovery

Documentazione completa della sessione di discovery multicast (TTL, gruppi,
porte, gotcha) in [`docs/network-discovery.md`](docs/network-discovery.md).

Punti chiave per lo sviluppo:

- **TTL radiod** deve essere ≥ 1 in config (`/etc/radio/radiod@rx888-generic.conf`,
  `[global]`, `ttl = 1`) altrimenti tutto resta su loopback.
- **Gruppi multicast** risolti via mDNS: `hf.local` (status/cmd :5006),
  `*-pcm.local` (data RTP :5004, status heartbeat :5006).
- **Join multicast**: specificare sempre l'interfaccia esplicita
  (`IP_ADD_MEMBERSHIP` con IP di eth0), non `INADDR_ANY` — essenziale su
  host multi-homed.
- **Control plane**: request/response — il bridge deve inviare POLL per
  ricevere STATUS sul gruppo receiver-level (`hf.local`); i per-channel
  heartbeat arrivano spontanei sui gruppi data.

## Repo layout

```
/home/sf/src/ka9q-tci/        # questo progetto
/home/sf/src/ka9q-radio/      # sorgente ka9q-radio (read-only, riferimento)
```

**Non modificare** `../ka9q-radio` da questo progetto. Lo trattiamo come
dipendenza esterna; è lì come riferimento per:

- `src/status.h`, `src/status.c` — definizione dei tipi TLV del protocollo
  di controllo di `radiod`.
- `src/multicast.c` — join dei gruppi, gestione TTL/interfacce.
- `src/rx888.c` — specifiche del front-end (parametri, sample rate).
- `docs/ka9q-api.md`, `docs/ka9q-radio.md`, `docs/NETWORK-NOTES.md` —
  documentazione ufficiale del protocollo.
- `share/presets.conf` — preset `[iq]` (demod=linear) usato come template
  per i canali creati dinamicamente.
- `share/radiod@rx888-*.conf` — esempi di config RX888.

## Architettura

```
TCI clients (WebSocket, ws://host:40001/)
    │   text:   VFO:, DDS:, RX_ENABLE:, IQ_SAMPLERATE:, TRX:, ...
    │   binary: header + IQ float32 LE interleaved
    ▼
ka9q-tci bridge (Rust, tokio)
    │   - WS server + TCI state machine
    │   - SSRC manager (mappa (rx,vfo) -> SSRC)
    │   - RTP ingest per SSRC (multicast join)
    │   - comandi TLV al control plane di radiod (UDP mcast :5006)
    ▼
radiod (ka9q-radio) — RX888 MkII
```

### Mapping TCI ↔ ka9q-radio

| TCI | ka9q-radio |
|---|---|
| RX index + VFO index | **SSRC** deterministico (es. `0xTCI0 \| rx<<4 \| vfo`) |
| `VFO:<rx>,<vfo>,<hz>` | TLV `RADIO_FREQUENCY` sull'SSRC |
| `IQ_SAMPLERATE:<sr>` | `samprate` del canale |
| `RX_ENABLE:<rx>,true` | crea SSRC se non esiste (preset `iq`) |
| `MODE:...` | in flusso IQ linear non cambia demod; accettato e logged |

La creazione dinamica di canali sfrutta il fatto che `radiod` accetta un
*nuovo* SSRC in un COMMAND packet e istanzia il canale al volo a partire
dal preset indicato.

## Convenzioni di lavoro

- **Utente parla italiano** — rispondere in italiano.
- Non toccare `../ka9q-radio`. Se serve leggerci dentro, farlo in sola
  lettura.
- Repo GitHub: `iu3qez/ka9q-tci`.
- Evitare dipendenze crate pesanti: preferire crate piccole e ben
  mantenute (tokio, tokio-tungstenite, socket2, bytes, thiserror, tracing).

## Comandi

- Build: `cargo build` (debug) / `cargo build --release`
- Run: `cargo run -- --status-name hf.local --bind-addr 0.0.0.0:40001`
- Test: `cargo test`
- Lint: `RUSTFLAGS="-D warnings" cargo check` (CI-ready)

## Riferimenti esterni

- ka9q-radio: <https://github.com/ka9q/ka9q-radio>
- Protocollo TCI: spec Expert Electronics (ExpertSDR2/3). Il formato dei
  messaggi text (`CMD:params;`) e il frame binario IQ vanno documentati in
  `docs/tci-wire.md` prima di scrivere il parser.
