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

Bridge funzionante end-to-end con SDC + CW Skimmer (spot decodificati
verificati contro Reverse Beacon Network).

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

Multi-endpoint: il binario espone N server TCI distinti (uno per banda
HF), ognuno sulla propria porta WebSocket, tutti backed da un singolo
`radiod`. Aggira il limite empirico `TRX_COUNT ≤ 2` per server di SDC
e altri client passive. Configurazione via YAML (vedi
`config.example.yaml` e `docs/multi-endpoint-plan.md`).

```
SDC #1 ──ws://host:40001──┐                          ┌── canale 0x7C000000
SDC #2 ──ws://host:40002──┤  ka9q-tci (1 binario,    ├── canale 0x7C010000
SDC #3 ──ws://host:40003──┤  N task tokio)           ├── canale 0x7C020000
   ...                     ├─→  - WS server per ep   │
SDC #N ──ws://host:4000N──┘     - bridge condiviso ──┴─→ radiod (RX888)
                                  - SsrcTable centrale     UDP mcast :5006
                                  - rtp_ingest unico
```

Forma legacy ancora supportata: senza `endpoints:` nel YAML, il bridge
parte come singolo server sulla porta `--bind-addr` (drop-in per setup
pre-multi-endpoint).

### SSRC layout

```
[0x7C (8 bit)] [endpoint (8 bit)] [riserva (8 bit)] [trx (4 bit)] [vfo (4 bit)]
```

- prefix `0x7C` identifica i canali ka9q-tci (filtra residui di altri
  client/run)
- `endpoint` 0..255 = indice del TCI server endpoint nel YAML
- `trx` 0..15, `vfo` 0..1 (VFO B filtrato lato bridge — vedi sotto)
- bit 8..15 riservati per future estensioni

Es.: endpoint=2 trx=0 vfo=0 → SSRC `0x7C020000`. Funzioni
`ssrc_encode(ep, trx, vfo)` e `ssrc_decode(u32)` in `bridge.rs`.

### Mapping TCI ↔ ka9q-radio

| TCI | ka9q-radio |
|---|---|
| Endpoint + RX index + VFO index | **SSRC** deterministico `0x7C \| ep<<16 \| trx<<4 \| vfo` |
| `VFO:<rx>,<vfo>,<hz>` (VFO A) | TLV `RADIO_FREQUENCY` sull'SSRC |
| `IQ_SAMPLERATE:<sr>` | `samprate` del canale (preset selezionato via `--preset-map`) |
| `IQ_START:<rx>` | no-op (canale già esistente dal Tune iniziale) |
| Init bridge | invia un Tune iniziale per ogni TRX configurato → radiod allineato prima del primo client |

La creazione dinamica di canali sfrutta il fatto che `radiod` accetta un
*nuovo* SSRC in un COMMAND packet e istanzia il canale al volo a partire
dal preset indicato. Il bridge invia un Tune al startup per ogni TRX
nel YAML, così radiod arriva in linea con le freq dichiarate prima
ancora che un client si connetta — essenziale con consumer passive
(SDC) che si fidano dello stato annunciato dal server.

## Trappole del control plane radiod

**Packet shape obbligatorio.** Il packet COMMAND verso radiod deve
replicare l'ordine e i campi del CLI `tune` di ka9q-radio
(`../ka9q-radio/src/tune.c:265-312`):

```
COMMAND_TAG → OUTPUT_SSRC → LIFETIME → PRESET → RADIO_FREQUENCY → EOL
```

Senza `COMMAND_TAG` e `LIFETIME` radiod accetta la retune freq ma salta
silenziosamente `loadpreset(...)`. Il canale ricade sul default
dell'instance (es. `usb` 12k per `radiod@rx888-web.conf`) → il bridge
legge audio mono USB-demodulato come fosse IQ stereo → spettro con
segnali speculari, decodifica casuale. Il bug è invisibile dal lato
bridge (nessun errore, ampiezze plausibili) e si scopre solo
interrogando radiod con `tune -r web.local -s <decimal_ssrc>` (SSRC
in **decimale**, non hex).

**Filtro VFO B.** Quando un client TCI tuna VFO B (`vfo:trx,1,X;`), il
bridge **non** crea un secondo canale ka9q-radio. Se lo facesse, due
SSRC distinti (`vfo=0` e `vfo=1`) produrrebbero RTP in parallelo che
finirebbero entrambi nel broadcast IQ del medesimo TRX TCI, generando
artefatti AGC-like e segnali doppi. VFO B resta puramente client-side:
lo stato si aggiorna ma nessun comando va a radiod. `rtp_ingest` filtra
anche i frame di SSRC con `vfo!=0` per scartare canali residui che
radiod può avere già attivi all'avvio del bridge.

**Single COMMAND_TAG.** `ControlClient::send_command`
(`src/radiod/control.rs`) antepone già un `COMMAND_TAG` monotono come
primo field del packet. I caller (`dispatch_cmd Tune/SetSr/...`) NON
devono aggiungerne un altro: due `COMMAND_TAG` consecutivi fanno
parsare radiod in modo errato e silenziosamente saltare il
`loadpreset(...)` — stesso sintomo del packet shape rotto, ma con
PRESET presente. Bug pescato 2026-05-07 col confronto contro `tune.c`
(che emette un solo COMMAND_TAG).

## Convenzioni di lavoro

- **Utente parla italiano** — rispondere in italiano.
- Non toccare `../ka9q-radio`. Se serve leggerci dentro, farlo in sola
  lettura.
- Repo GitHub: `iu3qez/ka9q-tci`.
- Evitare dipendenze crate pesanti: preferire crate piccole e ben
  mantenute (tokio, tokio-tungstenite, socket2, bytes, thiserror, tracing).

## Comandi

- Build: `cargo build` (debug) / `cargo build --release`
- Run legacy single-endpoint: `cargo run -- --status-name hf.local --bind-addr 0.0.0.0:40001`
- Run multi-endpoint: `cargo run -- --status-name hf.local --config /path/to/endpoints.yaml`
- Test: `cargo test`
- Lint: `RUSTFLAGS="-D warnings" cargo check` (CI-ready)
- Diagnosi canale lato radiod: `tune -r <status_name> -s <decimal_ssrc>`
  (SSRC in **decimale**, non hex; converti `0x7C010000` → `2080440320`)

## Riferimenti esterni

- ka9q-radio: <https://github.com/ka9q/ka9q-radio>
- Protocollo TCI: spec Expert Electronics (ExpertSDR2/3) in
  `docs/tci-protocol.txt`. Audit + cross-check contro
  `madpsy/ka9q_ubersdr` documentati nei commit message.
- Piano refactor multi-endpoint: `docs/multi-endpoint-plan.md`.
