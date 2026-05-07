# Piano refactor multi-endpoint

Stato: proposta — nessun codice scritto.

## Obiettivo

Esporre `ka9q-tci` come **N server TCI distinti**, uno per banda HF, sullo
stesso binario e con un solo backend `ka9q-radio`. Aggira il limite
empirico `TRX_COUNT ≤ 2` per server osservato in SDC e mantiene il
modello mentale "1 endpoint TCI = 1 banda" già usato dagli altri client
del settore (Skimmer, Log4OM).

## Non-scope

- Modifiche a `../ka9q-radio` (read-only).
- Multi-process via systemd template (`ka9q-tci@band.service`).
- Service discovery automatico (mDNS announcement degli endpoint).
- Comandi TCI fuori dal sottoinsieme già supportato (audio stream,
  SPOT, KEYER, RX_SENSORS, ecc.).

## Architettura target

```
SDC #1 ──ws://host:40001──┐
SDC #2 ──ws://host:40002──┤
SDC #3 ──ws://host:40003──┤  ka9q-tci (singolo binario, singolo processo)
   ...                     ├─ N task tokio, ognuno = 1 TCI server
SDC #N ──ws://host:4000N──┘  un solo bridge condiviso → radiod
                                          │
                                          ▼
                                    ka9q-radio (RX888)
```

Ogni endpoint ha:
- la sua porta WebSocket
- la sua etichetta banda (per logging / advertising in `device:`)
- 1-2 TRX con freq/modulation iniziali
- il suo `iq_samplerate` di default

Il bridge è **unico**: un solo `rtp_ingest`, un solo `ControlClient` verso
radiod, un solo `SsrcTable`. Il routing dei frame IQ ai client avviene
per `endpoint_id` codificato dentro l'SSRC.

## Schema YAML proposto

```yaml
# Globals (sostituiscono e complementano i flag CLI attuali)
status_name: web.local
mcast_iface: 192.168.1.20    # opzionale
preset_map:
  48000: iq48
  96000: iq96
default_preset: iq48
poll_interval_secs: 5

# Lista endpoint TCI. Almeno 1 obbligatorio.
endpoints:
  - port: 40001
    label: 160m
    iq_samplerate: 48000
    trx:
      - { freq: 1840000, modulation: USB }
  - port: 40002
    label: 80m
    iq_samplerate: 48000
    trx:
      - { freq: 3573000, modulation: USB }
  # ...altre bande...
```

I flag CLI attuali (`--bind-addr`, `--max-trx`, `--iq-samplerate`,
`--preset-map`, `--default-preset`, `--poll-interval-secs`) restano come
override globali per dev/debug, ma il deploy normale passa tutto via YAML.

## SSRC encoding esteso

Vecchio layout (16 bit): `0x7c10 | trx<<4 | vfo` — max 16 TRX, 16 VFO.
Nuovo layout (32 bit):   `0x7c000000 | endpoint<<16 | trx<<4 | vfo`

| campo    | bits  | range  |
|----------|-------|--------|
| prefix   | 24..31 | 0x7c (marker ka9q-tci) |
| endpoint | 16..23 | 0..255 |
| trx      | 4..15  | 0..4095 (in pratica 0..1) |
| vfo      | 0..3   | 0..1 |

Es:
- endpoint=0, trx=0, vfo=0 → `0x7c000000`
- endpoint=1, trx=0, vfo=0 → `0x7c010000`
- endpoint=8, trx=0, vfo=0 → `0x7c080000`

Compat: per `endpoint=0` il vecchio prefix `0x7c10...` resta unico se
mappiamo endpoint 0 sui nibble alti — non vincolante, ma valutare per
mantenere SSRC stabili tra release.

## Step di implementazione

Ogni step è un commit auto-contenuto, `cargo test` verde, comportamento
precedente preservato salvo dove esplicitamente notato.

### Step 1 — Schema YAML multi-endpoint, retrocompatibile

- `src/config_file.rs`: `FileConfig` accetta `endpoints: Vec<EndpointConfig>`.
  Se il YAML usa la forma vecchia (`trx: [...]` flat), il loader la
  promuove a un singolo `EndpointConfig` con `port` e `iq_samplerate` da
  flag CLI. Mantiene gli unit test esistenti.
- `config.example.yaml`: due esempi side-by-side (vecchio + nuovo).
- Test: `parse_minimal_yaml`, `parse_full_yaml` restano; aggiunti
  `parse_multi_endpoint_yaml` e `parse_legacy_falls_back_to_single`.

### Step 2 — Endpoint-aware bridge + state + server (N=1 ancora)

Fusione dei vecchi Step 2 e 3 del piano (separarli avrebbe lasciato
dead code in mezzo). Tutto retrocompat: endpoint=0 hardcoded ovunque,
comportamento runtime identico.

- `src/bridge.rs`: SSRC encoding esteso `0x7c000000 | endpoint<<16 |
  trx<<4 | vfo`. `SsrcTable.get_or_insert` accetta `(endpoint, trx,
  vfo)`. `rtp_ingest` decodifica endpoint da SSRC.
- `src/bridge.rs`: `BridgeCmd::{Tune, EnableRx, SetSr}` portano un
  campo `endpoint: u8`.
- `src/tci/state.rs`: `IqFrame { endpoint: u8, trx: u32, data: Vec<u8> }`.
- `src/tci/server.rs`: `ServerConfig.endpoint_id: u8 = 0`. Ogni
  `BridgeCmd` emesso dal server porta il proprio `endpoint_id`. Il
  loop subscribe filtra `frame.endpoint == endpoint_id`.
- Test: roundtrip SSRC su `(endpoint, trx, vfo)` per range
  significativi; test esistenti restano verdi con endpoint=0.

### Step 3 — `main.rs`: spawn N TCI server task

- Loop su `config.endpoints`: per ognuno crea `SharedState` dedicato e
  `tokio::spawn(tci::server::run(...))` con la sua porta.
- Bridge unico: riceve `BridgeCmd` da N mpsc canali, uno per endpoint
  (oppure un singolo canale con `endpoint` come campo).
- Decisione design: preferire singolo `mpsc` con `BridgeCmd::*` arricchito
  con `endpoint: u8` — più semplice, meno canali da gestire.
- Test: integration test (manuale) con 2 endpoint che condividono radiod.

### Step 4 — Test end-to-end con SDC

- 2 endpoint configurati (es. 40m + 20m).
- SDC fa "Add server" su entrambe le porte → 2 waterfall.
- Verifica: tuning indipendente, niente cross-talk, decodifica skimmer
  corretta su entrambe.

### Step 5 — Documentazione

- `CLAUDE.md`: aggiornare sezione "Architettura" e "Mapping TCI ↔
  ka9q-radio" con il nuovo SSRC layout.
- `config.example.yaml`: solo forma multi-endpoint (la vecchia resta
  supportata ma non è il default mostrato).
- Eventuale `docs/multi-endpoint.md` con esempi di deploy reali.

## Test invariants

A ogni step, devono rimanere verdi:

- `cargo test` (oggi 40 test, target ≥40)
- `RUSTFLAGS="-D warnings" cargo check`
- `cargo build --release` su RPi5

A test empirico cumulato dopo Step 4:

- Avvio con YAML legacy (`trx:` flat) → 1 endpoint sulla porta CLI default
- Avvio con YAML nuovo (`endpoints:`) → N endpoint
- SDC connesso a 2 endpoint contemporaneamente non vede segnali
  dell'altro (isolamento corretto)

## Limiti noti accettati

- **Panic isolation tra endpoint**: `try_join!(bridge, try_join_all(server_handles))`
  con la corrente `tokio::spawn` non cancella gli altri server task se uno
  panica. Il processo termina comunque (main ritorna), ma lo shutdown è
  disordinato. Sostituire con `JoinSet::abort_all` se servirà restart
  graceful (out of scope per il refactor).
- **Race init**: i server TCI accettano connessioni prima che il bridge
  abbia ricevuto il primo STATUS da radiod. Un client che fa `IQ_START`
  in quella finestra non riceve frame finché il canale non viene creato
  dal primo `Tune`. Comportamento già presente nel pre-refactor (non
  regressione); con N endpoint la finestra si allunga linearmente.

## Rollback

Lo schema YAML resta retrocompatibile: il config attuale dell'utente
continua a funzionare invariato. Se il refactor introduce regressioni,
basta rimuovere il blocco `endpoints:` per tornare al comportamento
precedente. Nessun campo wire-format cambia (il binary IQ TCI rimane
identico, l'SSRC è interno al bridge).

## Review checkpoint

Ad ogni step viene lanciato un agente di review (general-purpose,
read-only) sul diff dello step, con focus su:

- Aderenza al piano (no scope-creep)
- Backward compatibility (config vecchio funziona)
- Test coverage (nuovi test per nuove path)
- Possibili regressioni nel path RTP→TCI binary
- Pulizia API (nessun campo "morto" o pub gratuito)

L'agente produce gap list P0..P3; P0/P1 vanno risolti prima di passare
allo step successivo.
